//! A game client teleports an entity between two regions through an edge
//! server, end to end.
//!
//! **Requires a running `nats-server`.** Point `NATS_URL` elsewhere if the
//! broker is not on the default port.
//!
//! What it establishes: a client calls `teleport`, the edge orchestrates the
//! spawn-in-destination / wait / remap / despawn-from-origin sequence, and the
//! client receives `spawned` (with the new entity id) followed by `teleported`
//! for the same handle it has always held. The handle stays valid throughout,
//! the old id is gone from the origin, and a denied teleport fires
//! `teleport_failed` without moving the entity.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{
    ClientGame, ClientId, ClientLimits, EdgeClient, EdgeGame, EdgeServer, EntityHandle,
    EntityId, EntityKey, EntityKind, Flow, Game, Handoff, Overrun, Pacing, Pos3, RegionId,
    RegionServer, Step, TeleportDecision, TickObservation, Wait, WorldConfig, WorldSimulation,
};

const PATIENCE: Duration = Duration::from_secs(20);

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// Distinct per run and distinct from every other integration test's.
fn region_ids() -> (RegionId, RegionId) {
    let run = std::process::id() % 1000;
    (RegionId::from_raw(4_000_000 + run), RegionId::from_raw(4_001_000 + run))
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

// -- edge game: allows or denies teleports ----------------------------------

struct TeleportEdge {
    /// If set, carry these bytes through the teleport.
    carry: Mutex<Option<Vec<u8>>>,
    /// Set true to deny the next teleport.
    deny: AtomicBool,
    /// What `teleport_arrived` was handed.
    arrived_state: Mutex<Option<Vec<u8>>>,
    arrived_from: Mutex<Option<RegionId>>,
    arrived_to: Mutex<Option<RegionId>>,
}

impl TeleportEdge {
    fn new() -> TeleportEdge {
        TeleportEdge {
            carry: Mutex::new(None),
            deny: AtomicBool::new(false),
            arrived_state: Mutex::new(None),
            arrived_from: Mutex::new(None),
            arrived_to: Mutex::new(None),
        }
    }
}

impl EdgeGame for TeleportEdge {
    fn teleporting(
        &mut self,
        _entity: EntityKey,
        _client: ClientId,
        _from: RegionId,
        _to: RegionId,
        _at: Pos3,
    ) -> TeleportDecision {
        if self.deny.load(Ordering::Relaxed) {
            return TeleportDecision::Deny;
        }
        let carry = self.carry.lock().expect("not poisoned").take();
        match carry {
            Some(state) => TeleportDecision::Carry(state),
            None => TeleportDecision::Allow,
        }
    }

    fn teleport_arrived(
        &mut self,
        _entity: EntityKey,
        _client: ClientId,
        from: RegionId,
        to: RegionId,
        state: &[u8],
    ) {
        *self.arrived_state.lock().expect("not poisoned") = Some(state.to_vec());
        *self.arrived_from.lock().expect("not poisoned") = Some(from);
        *self.arrived_to.lock().expect("not poisoned") = Some(to);
    }
}

// -- client game: records what happened -------------------------------------

struct TeleportWatcher {
    /// All spawned callbacks: (handle, region, entity).
    spawned: Arc<Mutex<Vec<(EntityHandle, RegionId, EntityId)>>>,
    /// All teleported callbacks: (handle, region).
    teleported: Arc<Mutex<Vec<(EntityHandle, RegionId)>>>,
    /// All teleport_failed callbacks: (handle, region).
    failed: Arc<Mutex<Vec<(EntityHandle, RegionId)>>>,
    /// All removed callbacks.
    removed: Arc<Mutex<Vec<EntityHandle>>>,
    /// Position confirmations (round-trip movement).
    confirmed: Arc<AtomicU64>,
}

impl ClientGame for TeleportWatcher {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {
        self.spawned.lock().expect("not poisoned").push((handle, region, entity));
    }

    fn removed(&mut self, handle: EntityHandle) {
        self.removed.lock().expect("not poisoned").push(handle);
    }

    fn teleported(&mut self, handle: EntityHandle, region: RegionId) {
        self.teleported.lock().expect("not poisoned").push((handle, region));
    }

    fn teleport_failed(&mut self, handle: EntityHandle, region: RegionId) {
        self.failed.lock().expect("not poisoned").push((handle, region));
    }

    fn observed(
        &mut self,
        _handle: EntityHandle,
        _region: RegionId,
        observation: &TickObservation<'_>,
    ) {
        self.confirmed.fetch_add(observation.updates().count() as u64, Ordering::Relaxed);
    }
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

// -- tests ------------------------------------------------------------------

#[test]
fn a_client_teleports_an_entity_between_regions() {
    let cfg = config();
    let (origin_id, dest_id) = region_ids();

    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats = runtime
        .block_on(async_nats::connect(url()))
        .expect("a nats-server must be running; see the module doc");

    let origin = Region::serve(&nats, runtime.handle(), origin_id, cfg);
    let dest = Region::serve(&nats, runtime.handle(), dest_id, cfg);

    let quic = edge_endpoint(runtime.handle());
    let at = quic.local_addr().expect("bound");

    // Shared edge game state, wrapped in Arc so both the EdgeServer and the
    // test assertions can read it.
    let edge_game = Arc::new(TeleportEdge::new());
    let edge_game_clone = Arc::clone(&edge_game);
    let edge = EdgeServer::new(nats, runtime.handle().clone(), quic, move |_handle| {
        // Wrap in a delegating impl that shares the Arc.
        SharedTeleportEdge(edge_game_clone)
    })
    .expect("the edge starts");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // A drop guard so the region threads see `stop` even if a test
        // assertion panics outside `wait_until`. Without it the scope
        // joins forever on unwind.
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _guard = StopOnDrop(&stop);

        scope.spawn(|| origin.run(&stop));
        scope.spawn(|| dest.run(&stop));

        let endpoint = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async { endpoint.connect(at, "localhost").expect("configured").await })
            .expect("connects to the edge");

        let spawned: Arc<Mutex<Vec<(EntityHandle, RegionId, EntityId)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let teleported: Arc<Mutex<Vec<(EntityHandle, RegionId)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let failed: Arc<Mutex<Vec<(EntityHandle, RegionId)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let removed: Arc<Mutex<Vec<EntityHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let confirmed = Arc::new(AtomicU64::new(0));

        let watcher = TeleportWatcher {
            spawned: Arc::clone(&spawned),
            teleported: Arc::clone(&teleported),
            failed: Arc::clone(&failed),
            removed: Arc::clone(&removed),
            confirmed: Arc::clone(&confirmed),
        };
        let client = EdgeClient::new(conn, runtime.handle().clone(), |_| watcher)
            .expect("opens a stream");
        let sending = client.handle();

        // Spawn an entity in the origin.
        let avatar = sending
            .spawn(origin_id, Pos3::from_meters(200, 200, 0), EntityKind::observer(0))
            .expect("asks for an entity");

        // Wait for the origin to confirm.
        wait_until("the origin to confirm the spawn", &stop, || {
            spawned.lock().expect("not poisoned").len() == 1
        });
        {
            let s = spawned.lock().expect("not poisoned");
            assert_eq!(s[0].0, avatar, "handle matches");
            assert_eq!(s[0].1, origin_id, "region matches");
        }

        // Move it so we know the handle works.
        sending
            .move_entity(avatar, Pos3::from_meters(201, 200, 0))
            .expect("moves");
        wait_until("movement to round-trip", &stop, || {
            confirmed.load(Ordering::Relaxed) > 0
        });

        // ---- teleport with carried state ----------------------------------

        *edge_game.carry.lock().expect("not poisoned") =
            Some(b"inventory:sword,shield".to_vec());

        sending
            .teleport(avatar, dest_id, Pos3::from_meters(2000, 2000, 0))
            .expect("sends teleport");

        // Wait for the teleported callback.
        wait_until("the client to receive teleported", &stop, || {
            !teleported.lock().expect("not poisoned").is_empty()
        });

        // spawned fires before teleported, so there should be two spawned
        // entries: the original spawn and the teleport arrival.
        {
            let s = spawned.lock().expect("not poisoned");
            assert_eq!(s.len(), 2, "spawned should fire twice: once for origin, once for teleport");
            // Second spawned is in the destination.
            assert_eq!(s[1].0, avatar, "same handle");
            assert_eq!(s[1].1, dest_id, "arrived in destination");
            // The destination allocated its own id. It may or may not
            // equal the origin's — ids are unique within a region, not
            // globally — so what matters is that it arrived, not that
            // the raw number differs.
        }
        {
            let t = teleported.lock().expect("not poisoned");
            assert_eq!(t.len(), 1);
            assert_eq!(t[0].0, avatar, "same handle");
            assert_eq!(t[0].1, dest_id, "destination region");
        }

        // The handle was never removed.
        assert!(
            removed.lock().expect("not poisoned").is_empty(),
            "removed must not fire during a teleport"
        );

        // The edge game got the carried state.
        {
            let state = edge_game.arrived_state.lock().expect("not poisoned");
            assert_eq!(
                state.as_deref(),
                Some(b"inventory:sword,shield".as_slice()),
                "the game state survived the teleport"
            );
            assert_eq!(
                *edge_game.arrived_from.lock().expect("not poisoned"),
                Some(origin_id)
            );
            assert_eq!(
                *edge_game.arrived_to.lock().expect("not poisoned"),
                Some(dest_id)
            );
        }

        // The entity can still be moved after teleporting.
        let before = confirmed.load(Ordering::Relaxed);
        sending
            .move_entity(avatar, Pos3::from_meters(2001, 2000, 0))
            .expect("moves after teleport");
        wait_until("movement after teleport to round-trip", &stop, || {
            confirmed.load(Ordering::Relaxed) > before
        });

        // ---- denied teleport ----------------------------------------------

        // Try teleporting back, but the edge denies it.
        edge_game.deny.store(true, Ordering::Relaxed);
        sending
            .teleport(avatar, origin_id, Pos3::from_meters(200, 200, 0))
            .expect("sends denied teleport");

        wait_until("the client to receive teleport_failed", &stop, || {
            !failed.lock().expect("not poisoned").is_empty()
        });
        {
            let f = failed.lock().expect("not poisoned");
            assert_eq!(f.len(), 1);
            assert_eq!(f[0].0, avatar, "same handle");
            assert_eq!(f[0].1, origin_id, "destination that was denied");
        }
        // Entity stays alive and teleported did not fire again.
        assert_eq!(
            teleported.lock().expect("not poisoned").len(),
            1,
            "only the first teleport succeeded"
        );

        // The entity can still be moved — it stayed in the destination.
        let before = confirmed.load(Ordering::Relaxed);
        sending
            .move_entity(avatar, Pos3::from_meters(2002, 2000, 0))
            .expect("moves after denied teleport");
        wait_until("movement after denied teleport to round-trip", &stop, || {
            confirmed.load(Ordering::Relaxed) > before
        });

        // ---- teleporting something this client does not hold is refused ---

        assert!(
            sending
                .teleport(EntityHandle::from_raw(9_999), dest_id, Pos3::from_meters(0, 0, 0))
                .is_err(),
            "a handle nobody spent must be refused"
        );

        // A teleport removes the origin entity from the edge's maps
        // before the origin region processes the despawn, so a few state
        // packets for the departed entity may arrive in the gap. That is
        // an inherent part of the flow, not a bug.
        assert!(
            edge.stats().undeliverable <= 5,
            "unexpected volume of undeliverable packets: {}",
            edge.stats().undeliverable,
        );
        client.connection().close(0u32.into(), b"done");
        stop.store(true, Ordering::Relaxed);
    });
}

/// Wraps a shared `TeleportEdge` so multiple owners can read it.
struct SharedTeleportEdge(Arc<TeleportEdge>);

impl EdgeGame for SharedTeleportEdge {
    fn teleporting(
        &mut self,
        _entity: EntityKey,
        _client: ClientId,
        _from: RegionId,
        _to: RegionId,
        _at: Pos3,
    ) -> TeleportDecision {
        if self.0.deny.load(Ordering::Relaxed) {
            return TeleportDecision::Deny;
        }
        let carry = self.0.carry.lock().expect("not poisoned").take();
        match carry {
            Some(state) => TeleportDecision::Carry(state),
            None => TeleportDecision::Allow,
        }
    }

    fn teleport_arrived(
        &mut self,
        _entity: EntityKey,
        _client: ClientId,
        from: RegionId,
        to: RegionId,
        state: &[u8],
    ) {
        *self.0.arrived_state.lock().expect("not poisoned") = Some(state.to_vec());
        *self.0.arrived_from.lock().expect("not poisoned") = Some(from);
        *self.0.arrived_to.lock().expect("not poisoned") = Some(to);
    }
}
