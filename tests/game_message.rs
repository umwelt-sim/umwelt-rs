//! A game message sent by a client arrives at the region's game, intact.
//!
//! **Requires a running `nats-server`.** Point `NATS_URL` elsewhere if the
//! broker is not on the default port.
//!
//! What it establishes: a game client sends an opaque message, the edge's game
//! forwards it to the region the sender's entity lives in, and the region's
//! game receives it with the correct entity id and unmodified body.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeHandle, EdgeSink, Edges, Inbound};
use umwelt::{ClientGame, ClientHandle, ClientId, ClientLimits, EdgeClient, EdgeGame, EdgeServer};
use umwelt::{EntityId, EntityKey, EntityKind, Flow, Game, Handoff, Overrun};
use umwelt::{Pacing, Pos3, RegionId, RegionServer, Step, Wait};
use umwelt::{WorldConfig, WorldSimulation};

const PATIENCE: Duration = Duration::from_secs(20);

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

fn region_id() -> RegionId {
    RegionId::from_raw(5_000_000 + std::process::id() % 1000)
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

// -- region game: records every message that arrives -------------------------

struct Recorder {
    inbound: Arc<Inbound>,
    received: Arc<Mutex<Vec<(EntityId, Vec<u8>)>>>,
}

impl Game for Recorder {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
    }

    fn message_received(&mut self, from: EntityId, body: &[u8]) {
        self.received.lock().expect("not poisoned").push((from, body.to_vec()));
    }
}

// -- edge game: forwards client messages to the region ----------------------

struct Forwarder {
    handle: EdgeHandle,
    /// The first entity this edge spawned for each client.
    entities: Arc<Mutex<Vec<(ClientId, EntityKey)>>>,
}

impl EdgeGame for Forwarder {
    fn spawned(
        &mut self,
        entity: EntityKey,
        client: Option<ClientId>,
        _region: RegionId,
        _id: EntityId,
    ) {
        if let Some(client) = client {
            self.entities.lock().expect("not poisoned").push((client, entity));
        }
    }

    fn message_received(&mut self, client: ClientId, body: &[u8]) {
        let entities = self.entities.lock().expect("not poisoned");
        if let Some(&(_, key)) = entities.iter().find(|(c, _)| *c == client) {
            let _ = self.handle.send_to_region(key, body);
        }
    }
}

// -- client game: does nothing, the sending side needs no callbacks ----------

struct Silent;
impl ClientGame for Silent {}

// -- QUIC, generated for this run -------------------------------------------

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

// -- helpers ----------------------------------------------------------------

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

// -- test -------------------------------------------------------------------

#[test]
fn a_game_message_travels_from_client_to_sim() {
    let cfg = config();
    let region = region_id();
    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));

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

    let received: Arc<Mutex<Vec<(EntityId, Vec<u8>)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut sim = WorldSimulation::new(
        cfg,
        Recorder { inbound: Arc::clone(&inbound), received: Arc::clone(&received) },
    )
    .with_sink(Handoff::new(sink.clone()));

    let quic = edge_endpoint(runtime.handle());
    let at = quic.local_addr().expect("bound");

    let edge_entities: Arc<Mutex<Vec<(ClientId, EntityKey)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let edge = EdgeServer::new(nats, runtime.handle().clone(), quic, |handle| Forwarder {
        handle,
        entities: Arc::clone(&edge_entities),
    })
    .expect("the edge starts");

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Tick loop: drains messages and settles between ticks.
        let sink_for_loop = sink.clone();
        let inbound_for_loop = Arc::clone(&inbound);
        let received_for_loop = Arc::clone(&received);
        let stop_for_loop = &stop;
        scope.spawn(move || {
            sim.run(
                Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
                |_, sim| {
                    for (from, body) in inbound_for_loop.drain_messages() {
                        sim.deliver_message(from, &body);
                    }
                    inbound_for_loop.settle(sim, &sink_for_loop, ClientLimits::default());
                    if stop_for_loop.load(Ordering::Relaxed) {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    }
                },
            )
        });

        // Connect a client, spawn an entity, wait for it to land.
        let endpoint = game_endpoint(runtime.handle());
        let conn = runtime
            .block_on(async {
                endpoint.connect(at, "localhost").expect("configured").await
            })
            .expect("connects to the edge");
        let client =
            EdgeClient::new(conn, runtime.handle().clone(), |_handle| Silent)
                .expect("opens a stream");
        let sending: ClientHandle = client.handle();

        let _handle = sending
            .spawn(region, Pos3::from_meters(200, 200, 0), EntityKind::observer(0))
            .expect("asks for an entity");

        // Wait for the entity to be registered in the edge's forwarder.
        wait_until("the edge to know the entity", &stop, || {
            !edge_entities.lock().expect("not poisoned").is_empty()
        });

        // Wait for the entity to be claimed in the region, so drain_messages
        // can validate edge ownership.
        wait_until("the entity to be claimed in the region", &stop, || {
            edges.entity_count(umwelt::net::EdgeId::from_raw(0)) > 0
        });

        // Send the message.
        let payload = b"plant lettuce at 12,34";
        sending.send(payload).expect("sends a game message");

        // Wait for it to arrive.
        wait_until("the message to arrive at the sim", &stop, || {
            !received_for_loop.lock().expect("not poisoned").is_empty()
        });

        let messages = received_for_loop.lock().expect("not poisoned");
        assert_eq!(messages.len(), 1, "exactly one message arrived");
        assert_eq!(
            messages[0].1, payload,
            "the body arrived unmodified"
        );

        stop.store(true, Ordering::Relaxed);

        client.connection().close(0u32.into(), b"done");
        assert_eq!(sink.failed(), 0, "no publish failed");
    });

    let _ = edge;
}
