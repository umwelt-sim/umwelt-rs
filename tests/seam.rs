//! Seeing across a seam: an observer walking toward the boundary between two
//! placed regions is served the neighbor's border strip before it gets there,
//! by a shadow the edge keeps for it in the neighbor.
//!
//! **Requires a running `nats-server -js`.** Point `NATS_URL` at it.
//!
//! What it establishes: away from the seam a client hears from its own region
//! only; once its view reaches the seam it hears from both, the neighbor's
//! packets arrive under its own handle and carry the neighbor's entities at
//! world positions on the far side, and its own entity is in none of them;
//! walking away, the neighbor's packets stop and the shadow is gone; and a
//! client that disconnects near a seam leaves no shadow behind.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{
    ClientGame, ClientLimits, EdgeClient, EdgeGame, EdgeServer, EntityHandle, EntityId,
    EntityKind, Flow, Game, Handoff, Overrun, Pacing, Placement, Pos3, RegionId,
    RegionServer, Step, TickObservation, Wait, WorldConfig, WorldMap, WorldPos,
    WorldSimulation,
};

const PATIENCE: Duration = Duration::from_secs(20);

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// Distinct per run and distinct from every other integration test's.
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

struct Applier {
    inbound: Arc<Inbound>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
    }
}

struct Region {
    cfg: WorldConfig,
    #[allow(dead_code)]
    edges: Arc<Edges>,
    inbound: Arc<Inbound>,
    sink: EdgeSink,
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
        Region { cfg, edges, inbound, sink, _server: server }
    }

    fn run(&self, stop: &AtomicBool) {
        let mut sim = WorldSimulation::new(
            self.cfg,
            Applier { inbound: Arc::clone(&self.inbound) },
        )
        .with_sink(Handoff::new(self.sink.clone()));
        sim.run(
            Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
            |_, sim| {
                self.inbound.settle(sim, &self.sink, ClientLimits::default());
                if stop.load(Ordering::Relaxed) { Flow::Stop } else { Flow::Continue }
            },
        );
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
    (RegionId::from_raw(7_000_000 + run), RegionId::from_raw(7_001_000 + run))
}

struct Relay;
impl EdgeGame for Relay {}

/// What the edge told this client, by region.
struct Watcher {
    spawned: Arc<Mutex<Vec<(EntityHandle, RegionId, EntityId)>>>,
    /// Packets per region, and every (entity, world position) they carried.
    seen: Arc<Mutex<HashMap<RegionId, (u64, Vec<(EntityHandle, EntityId, WorldPos)>)>>>,
    packets: Arc<AtomicU64>,
}

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {
        self.spawned.lock().expect("not poisoned").push((handle, region, entity));
    }

    fn observed(
        &mut self,
        handle: EntityHandle,
        region: RegionId,
        observation: &TickObservation<'_>,
    ) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        let mut seen = self.seen.lock().expect("not poisoned");
        let entry = seen.entry(region).or_insert((0, Vec::new()));
        entry.0 += 1;
        for (id, at, _) in observation.updates() {
            entry.1.push((handle, id, at));
        }
    }
}

#[test]
fn an_observer_near_a_seam_is_served_the_far_side_by_a_shadow() {
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

        let endpoint = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async {
                endpoint.connect(at, "localhost").expect("configured").await
            })
            .expect("connects to the edge");
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(HashMap::new()));
        let packets = Arc::new(AtomicU64::new(0));
        let watcher = Watcher {
            spawned: Arc::clone(&spawned),
            seen: Arc::clone(&seen),
            packets: Arc::clone(&packets),
        };
        let client = EdgeClient::new(conn, runtime.handle().clone(), |_| watcher)
            .expect("opens a stream");
        let sending = client.handle();

        // Something to see on both sides of the seam: five unattended
        // entities in each region's border strip, a little way from the seam.
        let seam_x = size.raw() as i64;
        let m = |v: i64| WorldPos::from_meters(v, 0, 0).x;
        let mut strip: Vec<EntityHandle> = Vec::new();
        for n in 0..5i64 {
            let in_a = WorldPos::from_raw(seam_x - m(40 + n * 10), m(2000 + n * 20), 0);
            let in_b = WorldPos::from_raw(seam_x + m(40 + n * 10), m(2100 + n * 20), 0);
            strip.push(sending.spawn(in_a, EntityKind::unattended(1)).expect("asks"));
            strip.push(sending.spawn(in_b, EntityKind::unattended(2)).expect("asks"));
        }
        wait_until("the strip to be confirmed", &stop, || {
            spawned.lock().expect("not poisoned").len() == 10
        });

        // The observer starts well inside A, out of view of the seam.
        let far = WorldPos::from_local(
            Pos3::from_meters(1000, 2048, 0),
            Placement::new(0, 0),
            size,
        );
        let me = sending.spawn(far, EntityKind::observer(0)).expect("asks for an entity");
        wait_until("the observer to be confirmed", &stop, || {
            spawned
                .lock()
                .expect("not poisoned")
                .iter()
                .any(|&(h, r, _)| h == me && r == a)
        });
        let my_id = spawned
            .lock()
            .expect("not poisoned")
            .iter()
            .find(|&&(h, _, _)| h == me)
            .map(|s| s.2)
            .expect("spawned");
        wait_until("packets from A", &stop, || {
            seen.lock().expect("not poisoned").get(&a).is_some_and(|(n, _)| *n >= 5)
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            seen.lock().expect("not poisoned").get(&b).is_none(),
            "away from the seam, nothing from B"
        );
        assert_eq!(edge.stats().shadows, 0);

        // Within a view radius of the seam: A reports the view collision, the
        // edge keeps a shadow in B, and B's border strip arrives.
        let near = WorldPos::from_raw(seam_x - m(200), m(2048), 0);
        sending.move_entity(me, near).expect("moves");
        wait_until("a shadow to stand in B", &stop, || edge.stats().shadows == 1);
        wait_until("B's border strip to arrive", &stop, || {
            seen.lock().expect("not poisoned").get(&b).is_some_and(|(_, records)| {
                records.iter().any(|&(h, _, at)| h == me && at.x > seam_x)
            })
        });
        {
            let seen = seen.lock().expect("not poisoned");
            let (_, from_b) = seen.get(&b).expect("B packets");
            assert!(
                from_b.iter().all(|&(h, _, _)| h == me),
                "B's packets arrive under the observer's own handle"
            );
            assert!(
                from_b.iter().all(|&(_, _, at)| at.x >= seam_x),
                "everything from B is on B's side of the seam, in world coordinates"
            );
            assert!(
                from_b.iter().all(|&(_, id, _)| id != my_id),
                "the shadow is in nobody's snapshot, and the observer is not in B"
            );
        }

        // Walking away, the shadow goes and B falls silent.
        sending.move_entity(me, far).expect("moves");
        wait_until("the shadow to be dropped", &stop, || edge.stats().shadows == 0);
        std::thread::sleep(Duration::from_millis(200));
        let quiet =
            seen.lock().expect("not poisoned").get(&b).map(|(n, _)| *n).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(300));
        let later =
            seen.lock().expect("not poisoned").get(&b).map(|(n, _)| *n).unwrap_or(0);
        assert_eq!(later, quiet, "nothing more from B once the shadow is gone");

        // Back into the band, then gone: a disconnect sweeps the shadow too.
        sending.move_entity(me, near).expect("moves");
        wait_until("a shadow to stand in B again", &stop, || edge.stats().shadows == 1);
        drop(client);
        wait_until("the disconnect to sweep the shadow", &stop, || {
            edge.stats().shadows == 0
        });

        stop.store(true, Ordering::Relaxed);
    });
}
