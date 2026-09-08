//! Two placed regions, a map in the bucket, and a client that spawns by world
//! position and is routed to the region whose square covers it.
//!
//! **Requires a running `nats-server -js`.** Point `NATS_URL` at it.
//!
//! What it establishes: `WorldMap::write` and `read` round-trip through the
//! bucket; an edge reads the map at startup and reports the placed regions;
//! a `spawn` at a world position inside region B's square lands in B with the
//! right local position; the client is sent that entity back at the same
//! world position; a spawn over an empty square is dropped and counted.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{
    ClientGame, ClientLimits, EdgeClient, EdgeGame, EdgeServer, EntityHandle, EntityKey,
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

/// Distinct per run and distinct from every other integration test's, and
/// under 31 bits, which a placed region's id has to be.
fn region_ids() -> (RegionId, RegionId) {
    let run = std::process::id() % 1000;
    (RegionId::from_raw(5_000_000 + run), RegionId::from_raw(5_001_000 + run))
}

struct Relay;
impl EdgeGame for Relay {}

/// Records what the edge tells this client, in world coordinates.
struct Watcher {
    spawned: Arc<Mutex<Vec<(EntityHandle, RegionId, EntityKey)>>>,
    /// The newest world position the client was sent for each of its own
    /// entities, keyed by the handle the packet was built for.
    seen: Arc<Mutex<Vec<(EntityHandle, RegionId, WorldPos)>>>,
    packets: Arc<AtomicU64>,
}

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, name: EntityKey) {
        self.spawned.lock().expect("not poisoned").push((handle, region, name));
    }

    fn observed(
        &mut self,
        handle: EntityHandle,
        region: RegionId,
        observation: &TickObservation<'_>,
    ) {
        self.packets.fetch_add(1, Ordering::Relaxed);
        for (_, at, _) in observation.updates() {
            self.seen.lock().expect("not poisoned").push((handle, region, at));
        }
    }
}

#[test]
fn a_client_spawns_by_world_position_and_the_map_routes_it() {
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats =
        runtime.block_on(async_nats::connect(url())).expect("connects to the broker");
    let (a, b) = region_ids();
    let cfg = config();
    let size = cfg.region_size();

    // The deployment writes the map: A at (0,0), B east of it.
    let mut map = WorldMap::new();
    map.place(a, Placement::new(0, 0)).expect("placed");
    map.place(b, Placement::new(1, 0)).expect("placed");
    let revision = runtime.block_on(map.write(&nats)).expect("writes the map");
    assert!(revision >= 1, "a write is a revision");
    let read = runtime.block_on(WorldMap::read(&nats)).expect("reads the map");
    assert_eq!(read.as_ref(), Some(&map), "the bucket holds what was written");

    let origin = Region::serve(&nats, runtime.handle(), a, cfg);
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
        scope.spawn(|| origin.run(&stop));
        scope.spawn(|| east.run(&stop));

        // The edge reads the map once, at startup, and learns the region size
        // from the first placed region that answers.
        let edge =
            EdgeServer::new(nats.clone(), runtime.handle().clone(), quic, |_| Relay)
                .expect("the edge starts");
        assert_eq!(edge.stats().placed_regions, 2, "the edge read both placements");

        let endpoint = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async {
                endpoint.connect(at, "localhost").expect("configured").await
            })
            .expect("connects to the edge");
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let packets = Arc::new(AtomicU64::new(0));
        let watcher = Watcher {
            spawned: Arc::clone(&spawned),
            seen: Arc::clone(&seen),
            packets: Arc::clone(&packets),
        };
        let client = EdgeClient::new(conn, runtime.handle().clone(), |_| watcher)
            .expect("opens a stream");
        let sending = client.handle();

        // 200 m east of B's west side, which is 4,296 m east of the origin at
        // the default region size. The client names no region.
        let here = WorldPos::from_local(
            Pos3::from_meters(200, 200, 0),
            Placement::new(1, 0),
            size,
        );
        let farmer =
            sending.spawn(here, EntityKind::observer(0)).expect("asks for an entity");
        wait_until("B to confirm the spawn", &stop, || {
            spawned.lock().expect("not poisoned").len() == 1
        });
        {
            let s = spawned.lock().expect("not poisoned");
            assert_eq!(s[0].0, farmer);
            assert_eq!(s[0].1, b, "the map put the spawn in the east region");
        }

        // The client is sent its own entity back, rebuilt into world
        // coordinates from B's frame: the same position it asked for.
        wait_until("a packet carrying the farmer", &stop, || {
            seen.lock().expect("not poisoned").iter().any(|&(h, _, _)| h == farmer)
        });
        {
            let s = seen.lock().expect("not poisoned");
            let (_, region, at) = s.iter().find(|&&(h, _, _)| h == farmer).expect("seen");
            assert_eq!(*region, b);
            assert_eq!(*at, here, "the wire is lossless at this config");
        }

        // A spawn over an empty square is the edge of the world: dropped and
        // counted, never confirmed.
        let nowhere = WorldPos::from_meters(-1, -1, 0);
        let lost =
            sending.spawn(nowhere, EntityKind::observer(0)).expect("sends the ask");
        wait_until("the edge to count the spawn off the map", &stop, || {
            edge.stats().off_map >= 1
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !spawned.lock().expect("not poisoned").iter().any(|&(h, _, _)| h == lost),
            "nothing confirmed a spawn off the map"
        );

        // A named spawn into A still works, at a position in A's frame.
        let home = sending
            .spawn_into(a, WorldPos::from_meters(100, 100, 0), EntityKind::observer(0))
            .expect("asks for an entity");
        wait_until("A to confirm the named spawn", &stop, || {
            spawned.lock().expect("not poisoned").iter().any(|&(h, _, _)| h == home)
        });
        assert_eq!(
            spawned
                .lock()
                .expect("not poisoned")
                .iter()
                .find(|&&(h, _, _)| h == home)
                .map(|s| s.1),
            Some(a)
        );

        stop.store(true, Ordering::Relaxed);
        drop(client);
    });
}
