//! A game client, an edge and a region, end to end.
//!
//! **Requires a running `nats-server`.** Point `NATS_URL` elsewhere if the
//! broker is not on the default port. The QUIC side needs nothing running: the
//! test binds a loopback endpoint with a certificate it generates.
//!
//! What it establishes: a client that has never heard of a region can ask for
//! entities, move them, and be sent the movement back; the edge refuses a move
//! for a handle that client never spawned; a despawn the region's own game
//! performs reaches the client; and a client that disconnects leaves nothing
//! behind in the region.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::internals::edge::FromClient;
use umwelt::net::{EdgeSink, Edges, Inbound};
use umwelt::{ClientGame, ClientHandle, ClientLimits, EdgeClient, EdgeServer};
use umwelt::{EntityHandle, EntityId, EntityKind, Flow, Game, Handoff, Overrun};
use umwelt::{Pacing, PacketReader, Pos3, RegionId, RegionServer, Step, Wait};
use umwelt::{WorldConfig, WorldSimulation};

/// Entities the client asks for. Small: this is a wiring test, not a load one.
const WANTED: usize = 16;

/// Late enough that the client has its population and is moving it.
const CULL_AT: u32 = 200;

const PATIENCE: Duration = Duration::from_secs(20);

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// Distinct per run, and distinct from the other integration tests'.
fn region_id() -> RegionId {
    RegionId::from_raw(3_000_000 + std::process::id() % 1000)
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

/// The region's game: applies what its edges sent, and after a while despawns
/// one entity of its own accord, which no client asked for.
struct Applier {
    inbound: Arc<Inbound>,
    ticks: u32,
    culled: Arc<Mutex<Option<EntityId>>>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
        self.ticks += 1;
        if self.ticks == CULL_AT
            && let Some(id) = step.live().iter().next()
        {
            step.despawn(id);
            *self.culled.lock().expect("not poisoned") = Some(id);
        }
    }
}

/// What this test's client does with what the edge tells it.
///
/// This is the whole of a consumer's receive side: no polling, no timeout, no
/// decision about what silence means.
struct Watcher {
    region: RegionId,
    /// Handle to entity id, in the order the handles were spent.
    ids: Arc<Mutex<Vec<(EntityHandle, EntityId)>>>,
    gone: Arc<Mutex<Vec<EntityHandle>>>,
    /// Positions that made the whole round trip.
    confirmed: Arc<std::sync::atomic::AtomicU64>,
}

impl ClientGame for Watcher {
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {
        assert_eq!(region, self.region, "an id from a region nobody asked about");
        self.ids.lock().expect("not poisoned").push((handle, entity));
    }

    fn removed(&mut self, handle: EntityHandle) {
        self.gone.lock().expect("not poisoned").push(handle);
    }

    fn state(
        &mut self,
        _handle: EntityHandle,
        region: RegionId,
        state: &PacketReader<'_>,
    ) {
        assert_eq!(region, self.region, "a packet from a region nobody asked about");
        // Already decoded: no codec here, and no world the region was built
        // with. A packet reaching this client is one built for an avatar it
        // owns, and an avatar always sees itself.
        for (_, pos) in state.updates() {
            if pos.x.floor_meters() > 200 {
                self.confirmed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Where one entity starts. A column, so they are all in view of each other.
fn home(n: usize) -> Pos3 {
    Pos3::from_meters(200, 200 + n as i32, 0)
}

/// Waits, and stops everything before failing.
///
/// A panic inside `std::thread::scope` still joins the scope's threads, so a
/// reader blocked on a message that will never come would hang the failure
/// instead of reporting it.
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

// -- QUIC, generated for this run ------------------------------------------

const ALPN: &[u8] = b"umwelt-test";

fn provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();
    });
}

/// A listening endpoint, with a self-signed certificate. A deployment builds
/// this from whatever its operator trusts and hands it over; the library never
/// touches it.
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

/// Trusts whatever the edge presents, which is right for a test on loopback
/// and wrong everywhere else.
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

#[test]
fn a_game_client_populates_a_region_through_an_edge() {
    let cfg = config();
    let region = region_id();
    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));

    // The test owns every connection, as a deployment would.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let nats = runtime
        .block_on(async_nats::connect(url()))
        .expect("a nats-server must be running; see the module doc");
    let _server = RegionServer::new(
        nats.clone(),
        runtime.handle().clone(),
        region,
        cfg,
        Arc::clone(&inbound),
        Duration::from_secs(5),
    )
    .expect("serves");
    let sink =
        EdgeSink::new(region, nats.clone(), runtime.handle().clone(), Arc::clone(&edges));

    let culled: Arc<Mutex<Option<EntityId>>> = Arc::new(Mutex::new(None));
    let mut sim = WorldSimulation::new(
        cfg,
        Applier { inbound: Arc::clone(&inbound), ticks: 0, culled: Arc::clone(&culled) },
    )
    .with_sink(Handoff::new(sink.clone()));

    let quic = edge_endpoint(runtime.handle());
    let at = quic.local_addr().expect("bound");
    // The consumer's game does nothing here: a client that spawns, moves and
    // despawns needs no callback at all.
    let edge = EdgeServer::new(nats, runtime.handle().clone(), quic, |_handle| NoGame)
        .expect("the edge starts");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let sink_for_loop = sink.clone();
        let inbound_for_loop = Arc::clone(&inbound);
        let stop_for_loop = &stop;
        scope.spawn(move || {
            sim.run(
                Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
                |_, sim| {
                    inbound_for_loop.settle(sim, &sink_for_loop, ClientLimits::default());
                    if stop_for_loop.load(Ordering::Relaxed) {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    }
                },
            )
        });

        let endpoint = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async {
                endpoint.connect(at, "localhost").expect("configured").await
            })
            .expect("connects to the edge");
        // Everything below goes through the library. A game developer never
        // frames a message, picks a datagram, or polls: which of the four
        // commands rides which is a property of the command, and what comes
        // back arrives as calls.
        let ids: Arc<Mutex<Vec<(EntityHandle, EntityId)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let gone: Arc<Mutex<Vec<EntityHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let confirmed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let watcher = Watcher {
            region,
            ids: Arc::clone(&ids),
            gone: Arc::clone(&gone),
            confirmed: Arc::clone(&confirmed),
        };
        let client = EdgeClient::new(conn, runtime.handle().clone(), |_handle| watcher)
            .expect("opens a stream");
        let sending: ClientHandle = client.handle();

        // Giving back something this client is not holding is a mistake in the
        // game, and is reported to it rather than put on the wire.
        assert!(
            sending.despawn(EntityHandle::from_raw(9_999)).is_err(),
            "a handle nobody spent must be refused here"
        );

        // A move for a handle it is not holding is dropped without a word: it
        // may have been despawned a moment ago, and the game has not caught up.
        let quiet = edge.stats().refused;
        sending
            .move_entity(EntityHandle::from_raw(9_998), home(0))
            .expect("dropped, not an error");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(edge.stats().refused, quiet, "a stray move must not reach the edge");

        // The edge still counts one from a client that is not this one. Built
        // by hand and sent raw, which is what a client in another language is.
        let mut body = Vec::new();
        FromClient::Despawn { handle: EntityHandle::from_raw(9_997) }.encode(&mut body);
        client.connection().send_datagram(body.into()).expect("fits");
        wait_until("the edge to refuse a despawn nobody spent", &stop, || {
            edge.stats().refused > quiet
        });

        // The client mints its own handles, spent once and never reused.
        let handles: Vec<EntityHandle> = (0..WANTED)
            .map(|n| {
                sending
                    .spawn(region, home(n), EntityKind::Observer)
                    .expect("asks for an entity")
            })
            .collect();

        let sending = &sending;
        let handles = &handles;
        std::thread::scope(|inner| {
            let ids = &ids;
            let gone = &gone;
            let stop = &stop;
            let confirmed = &confirmed;

            wait_until("the region to report every id", stop, || {
                ids.lock().expect("not poisoned").len() == WANTED
            });
            assert_eq!(
                edges.entity_count(umwelt::net::EdgeId::from_raw(0)),
                WANTED,
                "the region should hold exactly what the client asked for"
            );

            // Move them, and wait for the movement to come back.
            let mover = inner.spawn(move || {
                let mut at = 0i32;
                while !stop.load(Ordering::Relaxed) {
                    at = (at + 1) % 64;
                    let moves: Vec<(EntityHandle, Pos3)> = handles
                        .iter()
                        .enumerate()
                        .map(|(n, handle)| {
                            let base = home(n);
                            let to = Pos3::from_meters(
                                base.x.floor_meters() + at,
                                base.y.floor_meters(),
                                0,
                            );
                            (*handle, to)
                        })
                        .collect();
                    let _ = sending.move_entities(&moves);
                    std::thread::sleep(Duration::from_millis(10));
                }
            });

            wait_until("movement to make the round trip", stop, || {
                confirmed.load(Ordering::Relaxed) >= 100
            });

            // The region's own game despawned one entity that no client asked
            // about. Only a removal can carry that.
            wait_until("the game's own despawn to reach the client", stop, || {
                let culled = *culled.lock().expect("not poisoned");
                let Some(culled) = culled else { return false };
                let held = ids.lock().expect("not poisoned");
                let Some(&(handle, _)) = held.iter().find(|(_, id)| *id == culled) else {
                    return false;
                };
                gone.lock().expect("not poisoned").contains(&handle)
            });

            stop.store(true, Ordering::Relaxed);
            let _ = mover.join();
        });

        // Every packet reached its client, checked while there still is one:
        // the region goes on building packets for a viewer until it hears the
        // entity is gone, and the ones built during a teardown reach nobody by
        // definition. That is what `undeliverable` counts.
        assert_eq!(edge.stats().undeliverable, 0, "every packet reached its client");

        // The client goes. Everything it held goes with it, without anything
        // asking for it.
        client.connection().close(0u32.into(), b"done");
        wait_until("the region to lose everything the client held", &stop, || {
            edges.entity_count(umwelt::net::EdgeId::from_raw(0)) == 0
        });
        assert_eq!(sink.failed(), 0, "no publish failed");
    });
}

struct NoGame;
impl umwelt::EdgeGame for NoGame {}
