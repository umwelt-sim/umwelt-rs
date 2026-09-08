//! Crossing a seam: an entity that reaches the box of a placed region is
//! carried into the placed neighbor by the edge, by the teleport it already
//! has, and nothing at any client tells the two regions apart.
//!
//! **Requires a running `nats-server -js`.** Point `NATS_URL` at it.
//!
//! What it establishes: a region game walking an entity into its box gets it
//! crossed into the neighbor under the same handle and the same name, with
//! `spawned` for the neighbor then `teleported`; its world position is
//! continuous across the seam; a bystander whose view straddles the seam holds
//! that name throughout and is never told to forget it; entity messages sent
//! during the transition reach the neighbor's game in order and never the
//! origin's; a side with nothing placed beyond is a wall; a shadow stands on
//! the origin side once the entity is across; a client walking its own entity
//! across arrives where its last move said; and a teleport into a region
//! nobody serves fails after the timeout with the entity still usable.
//!
//! Also measures the crossing, from the origin's tick that stopped the entity
//! at its box to `teleported` at the client, over ten runs.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{
    ClientGame, ClientLimits, EdgeClient, EdgeGame, EdgeServer, EntityHandle, EntityId,
    EntityKey, EntityKind, Fixed, Flow, Game, Handoff, Overrun, Pacing, Placement,
    RegionId, RegionServer, Step, TickObservation, Wait, WorldConfig, WorldMap, WorldPos,
    WorldSimulation,
};

const PATIENCE: Duration = Duration::from_secs(20);

/// The tag the region games walk east, one meter a tick.
const EAST: u16 = 7;

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

fn config() -> WorldConfig {
    WorldConfig::builder()
        .region_size_m(4096)
        .vertical_extent_m(1024)
        .horizontal_view_radius_m(256)
        .max_horizontal_speed_m_per_sec(40)
        .tick_hz(100)
        .build()
        .expect("config is valid")
}

// -- region -----------------------------------------------------------------

/// The region's game: applies what the edge sent, then walks every entity
/// tagged for it one meter east a tick. The region's own box stops a walker
/// short of its stride; the tick that first does so is noted, which is the
/// tick that reports the boundary collision a crossing starts from.
struct Walker {
    inbound: Arc<Inbound>,
    hit_wall: Arc<Mutex<Vec<Instant>>>,
    stopped: HashSet<EntityId>,
    messages: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Game for Walker {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
        let stride = Fixed::from_meters(1);
        let walkers: Vec<EntityId> =
            step.entities().filter(|&id| step.tag(id) == Some(EAST)).collect();
        for id in walkers {
            let before = step.position(id).expect("live");
            step.translate(id, stride, Fixed::ZERO, Fixed::ZERO);
            let after = step.position(id).expect("live");
            if after.x.raw() - before.x.raw() < stride.raw() && self.stopped.insert(id) {
                self.hit_wall.lock().expect("not poisoned").push(Instant::now());
            }
        }
    }

    fn message_received(&mut self, _from: EntityId, body: &[u8]) {
        self.messages.lock().expect("not poisoned").push(body.to_vec());
    }
}

struct Region {
    cfg: WorldConfig,
    #[allow(dead_code)]
    edges: Arc<Edges>,
    inbound: Arc<Inbound>,
    sink: EdgeSink,
    hit_wall: Arc<Mutex<Vec<Instant>>>,
    messages: Arc<Mutex<Vec<Vec<u8>>>>,
    _server: RegionServer,
}

impl Region {
    fn serve(
        client: &async_nats::Client,
        runtime: &tokio::runtime::Handle,
        id: RegionId,
        cfg: WorldConfig,
    ) -> Region {
        let edges = Arc::new(Edges::new());
        let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
        let server = RegionServer::new(
            client.clone(),
            runtime.clone(),
            id,
            cfg,
            Arc::clone(&inbound),
            Duration::from_secs(5),
        )
        .expect("serves");
        let sink = EdgeSink::new(id, client.clone(), runtime.clone(), Arc::clone(&edges));
        Region {
            cfg,
            edges,
            inbound,
            sink,
            hit_wall: Arc::new(Mutex::new(Vec::new())),
            messages: Arc::new(Mutex::new(Vec::new())),
            _server: server,
        }
    }

    fn run(&self, stop: &AtomicBool) {
        let mut sim = WorldSimulation::new(
            self.cfg,
            Walker {
                inbound: Arc::clone(&self.inbound),
                hit_wall: Arc::clone(&self.hit_wall),
                stopped: HashSet::new(),
                messages: Arc::clone(&self.messages),
            },
        )
        .with_sink(Handoff::new(self.sink.clone()));
        sim.run(
            Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
            |_, sim| {
                for (from, body) in self.inbound.drain_messages() {
                    sim.deliver_message(from, &body);
                }
                self.inbound.settle(sim, &self.sink, ClientLimits::default());
                if stop.load(Ordering::Relaxed) { Flow::Stop } else { Flow::Continue }
            },
        );
    }

    fn messages(&self) -> Vec<Vec<u8>> {
        self.messages.lock().expect("not poisoned").clone()
    }
}

// -- QUIC -------------------------------------------------------------------

const ALPN: &[u8] = b"umwelt-test";

fn provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    });
}

fn edge_endpoint(runtime: &tokio::runtime::Handle) -> quinn::Endpoint {
    provider();
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("a cert");
    let key = quinn::rustls::pki_types::PrivateKeyDer::try_from(
        cert.signing_key.serialize_der(),
    )
    .expect("a key");
    let mut tls = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)
        .expect("a server config");
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("TLS 1.3"),
    ));
    let _guard = runtime.enter();
    quinn::Endpoint::server(config, "127.0.0.1:0".parse().expect("a valid address"))
        .expect("binds")
}

#[derive(Debug)]
struct TrustAnything;

impl quinn::rustls::client::danger::ServerCertVerifier for TrustAnything {
    fn verify_server_cert(
        &self,
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &[quinn::rustls::pki_types::CertificateDer<'_>],
        _: &quinn::rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: quinn::rustls::pki_types::UnixTime,
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error>
    {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<
        quinn::rustls::client::danger::HandshakeSignatureValid,
        quinn::rustls::Error,
    > {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<quinn::rustls::SignatureScheme> {
        quinn::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn game_endpoint(runtime: &tokio::runtime::Handle) -> quinn::Endpoint {
    provider();
    let mut tls = quinn::rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAnything))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let _guard = runtime.enter();
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("valid")).expect("binds");
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("TLS 1.3"),
    )));
    endpoint
}

fn wait_until(what: &str, stop: &AtomicBool, done: impl Fn() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    panic!("timed out waiting for {what}");
}

/// Distinct per run and under 31 bits.
fn region_ids() -> (RegionId, RegionId) {
    let run = std::process::id() % 1000;
    (RegionId::from_raw(8_000_000 + run), RegionId::from_raw(8_001_000 + run))
}

// -- edge and clients -------------------------------------------------------

struct Relay;
impl EdgeGame for Relay {}

/// One record: whose packet, from which region, which name, where.
type Record = (EntityHandle, RegionId, EntityKey, WorldPos);

/// Everything the edge told one client.
#[derive(Clone, Default)]
struct Log {
    spawned: Arc<Mutex<Vec<(EntityHandle, RegionId, EntityKey)>>>,
    teleported: Arc<Mutex<Vec<(EntityHandle, RegionId, Instant)>>>,
    failed: Arc<Mutex<Vec<(EntityHandle, RegionId)>>>,
    seen: Arc<Mutex<Vec<Record>>>,
    /// Every despawn reported, by name.
    gone: Arc<Mutex<Vec<(RegionId, EntityKey)>>>,
}

impl Log {
    /// Where a handle has been confirmed, in order, with the name each time.
    fn spawned(&self, handle: EntityHandle) -> Vec<(RegionId, EntityKey)> {
        self.spawned
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|s| s.0 == handle)
            .map(|s| (s.1, s.2))
            .collect()
    }

    fn teleported(&self, handle: EntityHandle) -> Option<(RegionId, Instant)> {
        self.teleported
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|t| t.0 == handle)
            .map(|t| (t.1, t.2))
    }

    /// The records for one name in packets built for one handle, by region.
    fn positions(
        &self,
        handle: EntityHandle,
        name: EntityKey,
    ) -> Vec<(RegionId, WorldPos)> {
        self.seen
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|s| s.0 == handle && s.2 == name)
            .map(|s| (s.1, s.3))
            .collect()
    }

    fn forgot(&self, name: EntityKey) -> bool {
        self.gone.lock().expect("not poisoned").iter().any(|g| g.1 == name)
    }
}

struct Watcher(Log);

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, name: EntityKey) {
        self.0.spawned.lock().expect("not poisoned").push((handle, region, name));
    }

    fn teleported(&mut self, handle: EntityHandle, region: RegionId) {
        self.0.teleported.lock().expect("not poisoned").push((
            handle,
            region,
            Instant::now(),
        ));
    }

    fn teleport_failed(&mut self, handle: EntityHandle, region: RegionId) {
        self.0.failed.lock().expect("not poisoned").push((handle, region));
    }

    fn observed(
        &mut self,
        handle: EntityHandle,
        region: RegionId,
        observation: &TickObservation<'_>,
    ) {
        let mut seen = self.0.seen.lock().expect("not poisoned");
        for (name, at, _) in observation.updates() {
            seen.push((handle, region, name, at));
        }
        drop(seen);
        let mut gone = self.0.gone.lock().expect("not poisoned");
        for name in observation.despawns() {
            gone.push((region, name));
        }
    }
}

#[test]
fn an_entity_reaching_a_seam_is_crossed_by_the_edge_under_one_name() {
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats =
        runtime.block_on(async_nats::connect(url())).expect("connects to the broker");
    let (a, b) = region_ids();
    let cfg = config();
    let size = cfg.region_size();

    let mut map = WorldMap::new();
    map.place(a, Placement::new(0, 0)).expect("placed");
    map.place(b, Placement::new(1, 0)).expect("placed");
    runtime.block_on(map.write(&nats)).expect("writes the map");

    let west = Region::serve(&nats, runtime.handle(), a, cfg);
    let east = Region::serve(&nats, runtime.handle(), b, cfg);
    let quic = edge_endpoint(runtime.handle());
    let at = quic.local_addr().expect("bound");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _guard = StopOnDrop(&stop);
        scope.spawn(|| west.run(&stop));
        scope.spawn(|| east.run(&stop));

        let edge =
            EdgeServer::new(nats.clone(), runtime.handle().clone(), quic, |_| Relay)
                .expect("the edge starts");
        assert_eq!(edge.stats().placed_regions, 2);

        let connect = |log: &Log| {
            let endpoint = game_endpoint(runtime.handle());
            let conn = runtime
                .block_on(async {
                    endpoint.connect(at, "localhost").expect("configured").await
                })
                .expect("connects to the edge");
            EdgeClient::new(conn, runtime.handle().clone(), |_| Watcher(log.clone()))
                .expect("opens a stream")
        };
        let crosser_log = Log::default();
        let crosser_client = connect(&crosser_log);
        let crosser = crosser_client.handle();
        let bystander_log = Log::default();
        let bystander_client = connect(&bystander_log);
        let bystander = bystander_client.handle();

        let seam_x = size.raw() as i64;
        let m = |v: i64| WorldPos::from_meters(v, 0, 0).x;
        let lane = m(2048);

        // A bystander in B's border strip, whose view reaches into A: it holds
        // a shadow there and is served both sides of the seam.
        let onlooker = bystander
            .spawn(WorldPos::from_raw(seam_x + m(100), lane, 0), EntityKind::observer(0))
            .expect("asks");
        wait_until("the bystander to be confirmed in B", &stop, || {
            bystander_log.spawned(onlooker).first().is_some_and(|s| s.0 == b)
        });
        wait_until("the bystander's shadow in A", &stop, || edge.stats().shadows == 1);

        // The crosser starts in A, out of view of the seam, and A's game walks
        // it east. Five messages go to A's game while it is on the way.
        let walker = crosser
            .spawn(
                WorldPos::from_raw(seam_x - m(300), lane, 0),
                EntityKind::observer(EAST),
            )
            .expect("asks");
        wait_until("the crosser to be confirmed in A", &stop, || {
            crosser_log.spawned(walker).first().is_some_and(|s| s.0 == a)
        });
        let name = crosser_log.spawned(walker)[0].1;
        for n in 0..5u8 {
            crosser.entity_send(walker, &[b'a', b'0' + n]).expect("sends");
        }

        // The moment the edge starts the crossing, twenty messages are sent.
        // They arrive during the transition and are held for the destination.
        let started = Instant::now();
        // Polled with a pause: a transition lasts several milliseconds, and a
        // tight loop over the edge's locks can starve the thread that takes
        // them to start one.
        while edge.stats().transitions == 0 {
            assert!(Instant::now() < started + PATIENCE, "no crossing started");
            std::thread::sleep(Duration::from_micros(200));
        }
        for n in 0..20u8 {
            crosser.entity_send(walker, format!("t{n:02}").as_bytes()).expect("sends");
        }

        wait_until("the crosser to be teleported", &stop, || {
            crosser_log.teleported(walker).is_some()
        });
        assert_eq!(crosser_log.teleported(walker).map(|t| t.0), Some(b));
        assert_eq!(
            crosser_log.spawned(walker),
            vec![(a, name), (b, name)],
            "confirmed in A, then in B, under the one name"
        );
        let stats = edge.stats();
        assert_eq!(stats.crossings, 1);
        assert_eq!(stats.walls, 0);
        assert_eq!(stats.transitions, 0);

        // Once across, the crosser's view reaches back over the seam, and a
        // shadow stands for it in A: the bystander's and its own. Checked now,
        // since the walk carries it out of the band in a couple of seconds.
        wait_until("a shadow for the crosser in A", &stop, || edge.stats().shadows == 2);

        // Continuous across the seam: the last of it A sent was at A's box,
        // the first of it B sent was just past the seam.
        wait_until("B to send the crosser its own record", &stop, || {
            crosser_log.positions(walker, name).iter().any(|p| p.0 == b)
        });
        let own = crosser_log.positions(walker, name);
        let last_in_a = own.iter().filter(|p| p.0 == a).map(|p| p.1.x).max().expect("A");
        let first_in_b = own.iter().filter(|p| p.0 == b).map(|p| p.1.x).min().expect("B");
        assert!(last_in_a < seam_x, "A never placed it past its box");
        assert!(first_in_b >= seam_x, "B placed it on its own side");
        assert!(
            first_in_b - last_in_a < m(5),
            "the crosser jumped {} m at the seam",
            (first_in_b - last_in_a) / m(1)
        );

        // Messages: the five before went to A, the twenty during the
        // transition reach B in order, and A never sees them. Ahead of those,
        // B is told the last thing the client said before the crossing: that
        // instruction still stands, and B has heard nothing about this
        // entity. Whatever had it walking into the boundary is what carries
        // it on, and the client repeats nothing.
        wait_until("B's game to receive the held messages", &stop, || {
            east.messages().len() >= 21
        });
        let to_a = west.messages();
        let to_b = east.messages();
        assert_eq!(to_a, (0..5u8).map(|n| vec![b'a', b'0' + n]).collect::<Vec<_>>());
        let mut want = vec![b"a4".to_vec()];
        want.extend((0..20u8).map(|n| format!("t{n:02}").into_bytes()));
        assert_eq!(to_b, want, "the standing instruction, then the held messages");

        // The bystander held the name throughout: it was sent the crosser from
        // A through its shadow, then from B, and was never told to forget it.
        std::thread::sleep(Duration::from_millis(300));
        let watched = bystander_log.positions(onlooker, name);
        assert!(watched.iter().any(|p| p.0 == a), "the bystander saw it in A");
        assert!(watched.iter().any(|p| p.0 == b), "the bystander saw it in B");
        assert!(
            !bystander_log.forgot(name),
            "the bystander was told to forget the crosser"
        );

        // A side with nothing placed beyond is a wall: the walker stops at
        // B's east box, and nothing crosses.
        let stuck = crosser
            .spawn(
                WorldPos::from_raw(2 * seam_x - m(50), lane, 0),
                EntityKind::observer(EAST),
            )
            .expect("asks");
        wait_until("the walker to be confirmed in B", &stop, || {
            crosser_log.spawned(stuck).first().is_some_and(|s| s.0 == b)
        });
        wait_until("the wall to be counted", &stop, || edge.stats().walls == 1);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(edge.stats().crossings, 1, "a wall is not a crossing");
        assert_eq!(crosser_log.spawned(stuck).len(), 1, "the walker never left B");

        // A client walking its own entity across: the moves it sends during
        // the transition are held, and the last of them is where it stands
        // in B. The game does not walk this one.
        let mover = crosser
            .spawn(
                WorldPos::from_raw(seam_x - m(20), m(1500), 0),
                EntityKind::observer(0),
            )
            .expect("asks");
        wait_until("the mover to be confirmed in A", &stop, || {
            crosser_log.spawned(mover).first().is_some_and(|s| s.0 == a)
        });
        let mover_name = crosser_log.spawned(mover)[0].1;
        let mut x = seam_x - m(20);
        let deadline = Instant::now() + PATIENCE;
        while crosser_log.teleported(mover).is_none() {
            assert!(Instant::now() < deadline, "the mover never crossed");
            x += m(2);
            crosser.move_entity(mover, WorldPos::from_raw(x, m(1500), 0)).expect("moves");
            std::thread::sleep(Duration::from_millis(5));
        }
        let last_asked = x;
        wait_until("B to place the mover where its last move said", &stop, || {
            crosser_log
                .positions(mover, mover_name)
                .iter()
                .any(|p| p.0 == b && (p.1.x - last_asked).abs() <= m(4))
        });
        assert_eq!(edge.stats().crossings, 2);

        // Ten more crossings, timed from the tick that stopped the entity at
        // A's box to `teleported` at the client.
        let mut latencies: Vec<Duration> = Vec::new();
        for k in 0..10i64 {
            let hits = west.hit_wall.lock().expect("not poisoned").len();
            let runner = crosser
                .spawn(
                    WorldPos::from_raw(seam_x - m(40), m(1000 + k * 10), 0),
                    EntityKind::observer(EAST),
                )
                .expect("asks");
            wait_until("a timed crossing", &stop, || {
                crosser_log.teleported(runner).is_some()
            });
            let (_, arrived) = crosser_log.teleported(runner).expect("just seen");
            let stopped = west.hit_wall.lock().expect("not poisoned")[hits];
            latencies.push(arrived.duration_since(stopped));
            crosser.despawn(runner).expect("gives it back");
        }
        latencies.sort();
        println!(
            "crossing latency, box to teleported, 10 runs: min {:.1} ms, median {:.1} ms, max {:.1} ms",
            latencies[0].as_secs_f64() * 1e3,
            latencies[5].as_secs_f64() * 1e3,
            latencies[9].as_secs_f64() * 1e3,
        );
        assert!(
            latencies[9] < Duration::from_millis(500),
            "a crossing took {:?}",
            latencies[9]
        );
        assert_eq!(edge.stats().crossings, 12);

        // A teleport into a region nobody serves is given up after the
        // timeout, and the entity is still the client's to move.
        let nowhere = RegionId::from_raw(999_999);
        crosser
            .teleport_into(stuck, nowhere, WorldPos::from_meters(10, 10, 0))
            .expect("asks");
        wait_until("the teleport to time out", &stop, || {
            crosser_log.failed.lock().expect("not poisoned").contains(&(stuck, nowhere))
        });
        let stats = edge.stats();
        assert_eq!(stats.teleport_timeouts, 1);
        assert_eq!(stats.transitions, 0);
        let stuck_name = crosser_log.spawned(stuck)[0].1;
        let back = WorldPos::from_raw(2 * seam_x - m(200), lane, 0);
        crosser.move_entity(stuck, back).expect("moves");
        wait_until("the entity to move after the failed teleport", &stop, || {
            crosser_log
                .positions(stuck, stuck_name)
                .iter()
                .any(|p| p.0 == b && p.1.x < 2 * seam_x - m(150))
        });

        stop.store(true, Ordering::Relaxed);
    });
}
