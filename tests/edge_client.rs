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

use umwelt::net::{
    EdgeSink, EdgeServer, Edges, EntityKind, Framer, FromClient, Inbound, RegionId,
    RegionServer, ToClient,
};
use umwelt::sim::{ClientLimits, Flow, Handoff, Overrun, Pacing, Step, Wait};
use umwelt::{EntityId, Game, PacketReader, Pos3, RecordCodec, WorldConfig, WorldSimulation};

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

/// How long a reader waits before checking whether it should stop.
const POLL: Duration = Duration::from_millis(200);

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
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("a cert");
    let key =
        quinn::rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
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
    ) -> Result<quinn::rustls::client::danger::ServerCertVerified, quinn::rustls::Error> {
        Ok(quinn::rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error>
    {
        Ok(quinn::rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &quinn::rustls::pki_types::CertificateDer<'_>,
        _: &quinn::rustls::DigitallySignedStruct,
    ) -> Result<quinn::rustls::client::danger::HandshakeSignatureValid, quinn::rustls::Error>
    {
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
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().expect("valid"))
        .expect("binds");
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
    let sink = EdgeSink::new(region, nats.clone(), runtime.handle().clone(), Arc::clone(&edges));

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
            sim.run(Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None }, |_, sim| {
                inbound_for_loop.settle(sim, &sink_for_loop, ClientLimits::default());
                if stop_for_loop.load(Ordering::Relaxed) { Flow::Stop } else { Flow::Continue }
            })
        });

        let client = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async {
                client.connect(at, "localhost").expect("configured").await
            })
            .expect("connects to the edge");
        let (mut send, mut recv) =
            runtime.block_on(conn.open_bi()).expect("opens its stream");

        let mut body = Vec::new();
        let mut framed = Vec::new();
        let post = |message: &FromClient,
                        send: &mut quinn::SendStream,
                        body: &mut Vec<u8>,
                        framed: &mut Vec<u8>| {
            message.encode(body);
            if message.is_latest_only() {
                conn.send_datagram(body.clone().into()).expect("a datagram fits");
            } else {
                Framer::frame(body, framed);
                runtime.block_on(send.write_all(framed)).expect("writes");
            }
        };

        // Spawn before move: a handle this connection never spawned is refused,
        // and counted, rather than reaching a region.
        let before = edge.stats().refused;
        post(
            &FromClient::Move { handle: 9_999, position: home(0) },
            &mut send,
            &mut body,
            &mut framed,
        );
        wait_until("the edge to refuse a move for an unspawned handle", &stop, || {
            edge.stats().refused > before
        });

        for n in 0..WANTED {
            post(
                &FromClient::Spawn {
                    handle: n as u32,
                    region,
                    position: home(n),
                    kind: EntityKind::Observer,
                },
                &mut send,
                &mut body,
                &mut framed,
            );
        }

        // Ids come back on the reliable stream; packets come down as datagrams.
        let ids: Mutex<Vec<Option<EntityId>>> = Mutex::new(vec![None; WANTED]);
        let gone: Mutex<Vec<u32>> = Mutex::new(Vec::new());
        let confirmed = std::sync::atomic::AtomicU64::new(0);
        let codec = RecordCodec::new(&cfg);

        let runtime = &runtime;
        let conn = &conn;
        std::thread::scope(|inner| {
            let ids = &ids;
            let gone = &gone;
            let stop = &stop;
            inner.spawn(move || {
                let mut framer = Framer::new();
                let mut buf = vec![0u8; 16 * 1024];
                while !stop.load(Ordering::Relaxed) {
                    // Built inside `block_on`: a timeout needs a runtime to
                    // exist before it does.
                    let read = match runtime.block_on(async {
                        tokio::time::timeout(POLL, recv.read(&mut buf)).await
                    }) {
                        Ok(Ok(Some(read))) => read,
                        // Nothing yet. Go round and look at `stop` again.
                        Err(_) => continue,
                        _ => return,
                    };
                    framer.push(&buf[..read]);
                    while let Ok(Some(one)) = framer.take() {
                        match ToClient::decode(&one) {
                            Ok(ToClient::Spawned { handle, region: from, entity }) => {
                                assert_eq!(from, region, "an id from a region nobody asked");
                                ids.lock().expect("not poisoned")[handle as usize] =
                                    Some(entity);
                            }
                            Ok(ToClient::Removed { handle }) => {
                                gone.lock().expect("not poisoned").push(handle);
                            }
                            _ => {}
                        }
                    }
                }
            });

            let confirmed = &confirmed;
            inner.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let datagram = match runtime.block_on(async {
                        tokio::time::timeout(POLL, conn.read_datagram()).await
                    }) {
                        Ok(Ok(datagram)) => datagram,
                        Err(_) => continue,
                        _ => return,
                    };
                    let Ok(ToClient::State { packet, .. }) = ToClient::decode(&datagram)
                    else {
                        continue;
                    };
                    let Some(reader) = PacketReader::new(&codec, packet) else { continue };
                    // A packet reaching this client is one built for an avatar
                    // it owns, and an avatar always sees itself.
                    for (_, pos) in reader.updates() {
                        if pos.x.floor_meters() > 200 {
                            confirmed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });

            wait_until("the region to report every id", stop, || {
                ids.lock().expect("not poisoned").iter().all(Option::is_some)
            });
            assert_eq!(
                edges.entity_count(umwelt::net::EdgeId::from_raw(0)),
                WANTED,
                "the region should hold exactly what the client asked for"
            );

            // Move them, and wait for the movement to come back.
            let mover = inner.spawn(move || {
                let mut body = Vec::new();
                let mut framed = Vec::new();
                let mut at = 0i32;
                while !stop.load(Ordering::Relaxed) {
                    at = (at + 1) % 64;
                    for n in 0..WANTED {
                        let base = home(n);
                        let to = Pos3::from_meters(
                            base.x.floor_meters() + at,
                            base.y.floor_meters(),
                            0,
                        );
                        let go = FromClient::Move { handle: n as u32, position: to };
                        go.encode(&mut body);
                        Framer::frame(&body, &mut framed);
                        let _ = conn.send_datagram(body.clone().into());
                    }
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
                let Some(handle) = held.iter().position(|id| *id == Some(culled)) else {
                    return false;
                };
                gone.lock().expect("not poisoned").contains(&(handle as u32))
            });

            stop.store(true, Ordering::Relaxed);
            let _ = mover.join();
        });

        // The client goes. Everything it held goes with it, without anything
        // asking for it.
        conn.close(0u32.into(), b"done");
        drop(client);
        wait_until("the region to lose everything the client held", &stop, || {
            edges.entity_count(umwelt::net::EdgeId::from_raw(0)) == 0
        });
        assert_eq!(edge.stats().undeliverable, 0, "every packet reached its client");
        assert_eq!(sink.failed(), 0, "no publish failed");
    });
}

struct NoGame;
impl umwelt::EdgeGame for NoGame {}
