//! A region simulation's listening socket.
//!
//! [`RegionServer`] is one region's front door. It accepts links, authorizes
//! them, tells each one what world it has reached, and gathers the ones that
//! stay into [`Edges`].
//!
//! It is not the simulation. It holds a [`WorldConfig`] so it can describe the
//! world, and holds no world state, no snapshot, and no tick. Wiring an edge to
//! a running [`WorldSimulation`](crate::WorldSimulation) is the next piece of
//! work and is not built.
//!
//! **Threading.** One thread per connected edge, spawned by
//! [`RegionServer::run`] into a [`std::thread::scope`]. A region takes links
//! from a small number of edges rather than from thousands of game clients, so
//! a blocking accept loop is sized for the job and costs no dependency. The
//! design note that Tokio belongs in the edge server is about the
//! client-facing side, which is not this.

use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::config::WorldConfig;
use crate::net::error::{NetError, RejectCode};
use crate::net::region::auth::Authorizer;
use crate::net::region::edges::{Edge, Edges};
use crate::net::region::protocol::{
    ClientIdentification, HANDSHAKE_TIMEOUT, KIND_ACCEPTED, KIND_CLIENT_IDENTIFICATION, KIND_QUIT,
    KIND_REJECTION, KIND_SERVER_INFO, PROTOCOL_VERSION, RegionId, Rejection, ServerInfo,
    ServerVersion, WorldParams,
};
use crate::net::region::wire::{read_frame, write_frame};

/// A region simulation's listening socket.
pub struct RegionServer {
    listener: TcpListener,
    addr: SocketAddr,
    region: RegionId,
    config: WorldConfig,
    auth: Arc<dyn Authorizer>,
    edges: Arc<Edges>,
    stopping: Arc<AtomicBool>,
    refused: AtomicU64,
}

impl RegionServer {
    /// Binds a port and takes the identity this region will hand out.
    ///
    /// Binding does not accept anything. Call [`accept`](Self::accept) for one
    /// edge or [`run`](Self::run) for all of them.
    pub fn bind(
        addr: impl ToSocketAddrs,
        region: RegionId,
        config: WorldConfig,
        auth: Arc<dyn Authorizer>,
    ) -> Result<RegionServer, NetError> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        Ok(RegionServer {
            listener,
            addr,
            region,
            config,
            auth,
            edges: Arc::new(Edges::new()),
            stopping: Arc::new(AtomicBool::new(false)),
            refused: AtomicU64::new(0),
        })
    }

    /// The address actually bound, which is what a caller that asked for port
    /// zero needs.
    #[inline]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    #[inline]
    pub fn region(&self) -> RegionId {
        self.region
    }

    #[inline]
    pub fn config(&self) -> &WorldConfig {
        &self.config
    }

    /// The edges relaying for this region.
    #[inline]
    pub fn edges(&self) -> &Arc<Edges> {
        &self.edges
    }

    /// Peers turned away since bind. A refused peer never becomes an edge, so
    /// this is counted here rather than on [`Edges`].
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// Takes one link through the handshake.
    ///
    /// Blocks until a peer arrives. A peer that fails to authorize is refused
    /// and returned as an error, so a caller looping on this decides for itself
    /// whether to keep going.
    pub fn accept(&self) -> Result<Edge, NetError> {
        let (sock, peer) = self.listener.accept()?;
        self.greet(sock, peer)
    }

    /// Accepts until [`Shutdown::stop`] is called, handing each admitted edge
    /// to `on_edge` on its own thread.
    ///
    /// A peer that fails to authorize never reaches `on_edge`; it is counted in
    /// [`refused`](Self::refused) and dropped. `on_edge` owns the edge and the
    /// link closes when it returns, so a caller that wants the edge to stay
    /// attached keeps it alive, which is what [`Edge::wait_for_close`] is for.
    pub fn run(&self, on_edge: impl Fn(Edge) + Sync) -> Result<(), NetError> {
        std::thread::scope(|scope| -> Result<(), NetError> {
            loop {
                let (sock, peer) = match self.listener.accept() {
                    Ok(pair) => pair,
                    Err(e) => {
                        if self.stopping.load(Ordering::SeqCst) {
                            break;
                        }
                        return Err(NetError::Io(e));
                    }
                };
                // Shutdown dials this port to wake the accept above, so the
                // connection that unblocked it may be that dial rather than a
                // peer.
                if self.stopping.load(Ordering::SeqCst) {
                    drop(sock);
                    break;
                }
                let on_edge = &on_edge;
                scope.spawn(move || {
                    if let Ok(edge) = self.greet(sock, peer) {
                        on_edge(edge);
                    }
                });
            }
            Ok(())
        })
    }

    /// A handle that stops [`run`](Self::run).
    ///
    /// Separate from the server so it can be held by whatever is going to do
    /// the stopping while `run` holds the server.
    pub fn shutdown_handle(&self) -> Shutdown {
        Shutdown { stopping: Arc::clone(&self.stopping), addr: self.addr }
    }

    /// The server's half of the handshake.
    ///
    /// The credential is checked before this region says anything about itself,
    /// so a peer that cannot authorize learns neither the region id nor the
    /// configuration of the world.
    fn greet(&self, mut sock: TcpStream, peer: SocketAddr) -> Result<Edge, NetError> {
        sock.set_nodelay(true)?;
        // A peer that connects and then says nothing would otherwise hold this
        // thread for as long as it liked.
        sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        sock.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let mut body = Vec::new();
        let kind = read_frame(&mut sock, &mut body)?;
        if kind != KIND_CLIENT_IDENTIFICATION {
            self.refuse(&mut sock, RejectCode::Malformed);
            return Err(NetError::Unexpected { expected: "client identification", got: kind });
        }
        let ident = match ClientIdentification::decode(&body) {
            Ok(ident) => ident,
            Err(e) => {
                self.refuse(&mut sock, RejectCode::Malformed);
                return Err(e);
            }
        };

        if self.auth.authorize(&ident.credential).is_err() {
            self.refuse(&mut sock, RejectCode::Unauthorized);
            return Err(NetError::Rejected(RejectCode::Unauthorized));
        }
        if ident.protocol != PROTOCOL_VERSION {
            self.refuse(&mut sock, RejectCode::ProtocolMismatch);
            return Err(NetError::ProtocolMismatch {
                ours: PROTOCOL_VERSION,
                theirs: ident.protocol,
            });
        }

        ServerInfo {
            server: ServerVersion::CURRENT,
            region: self.region,
            params: WorldParams::from_config(&self.config),
        }
        .encode(&mut body);
        write_frame(&mut sock, KIND_SERVER_INFO, &body)?;

        let kind = read_frame(&mut sock, &mut body)?;
        match kind {
            KIND_ACCEPTED => {}
            // Not a failure of the link. The peer read the info and left.
            KIND_QUIT => return Err(NetError::Declined),
            other => {
                return Err(NetError::Unexpected { expected: "accepted or quit", got: other });
            }
        }

        // The handshake's deadline does not apply to an attached edge, which is
        // expected to sit idle between packets.
        sock.set_read_timeout(None)?;
        sock.set_write_timeout(None)?;

        self.edges.admit(sock, peer)
    }

    /// Sends a bare rejection and gives up on the connection.
    ///
    /// The write is allowed to fail: a peer that has already gone is refused
    /// either way, and the count is what an operator reads.
    fn refuse(&self, sock: &mut TcpStream, code: RejectCode) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        let mut body = Vec::new();
        Rejection { code }.encode(&mut body);
        let _ = write_frame(sock, KIND_REJECTION, &body);
    }
}

impl core::fmt::Debug for RegionServer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegionServer")
            .field("addr", &self.addr)
            .field("region", &self.region)
            .field("edges", &self.edges.len())
            .finish_non_exhaustive()
    }
}

/// Stops a running [`RegionServer::run`].
pub struct Shutdown {
    stopping: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl Shutdown {
    /// Sets the flag, then dials the port to wake the blocked accept.
    ///
    /// Returns once the loop has been woken, not once it has finished: edges
    /// already attached are left to their own threads, and `run` returns when
    /// they end.
    ///
    /// The dial goes to the address the listener reported. For a server bound
    /// to a wildcard address that is `0.0.0.0`, which most platforms route to
    /// localhost; a deployment that cannot rely on that wants an explicit
    /// address rather than a wildcard.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, HANDSHAKE_TIMEOUT);
    }
}

impl core::fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shutdown").field("addr", &self.addr).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::region::auth::{AllowAll, SharedSecret};
    use crate::net::region::client::{Decision, RegionClient};
    use std::time::{Duration, Instant};

    const KEY: &[u8] = b"region-7-edge-key";

    fn server(auth: Arc<dyn Authorizer>) -> RegionServer {
        RegionServer::bind("127.0.0.1:0", RegionId::from_raw(7), WorldConfig::default(), auth)
            .expect("binds a loopback port")
    }

    fn secret_server() -> RegionServer {
        server(Arc::new(SharedSecret::new(KEY)))
    }

    /// Polls until `done`, so a test does not depend on how fast a thread got
    /// scheduled. Fails the test rather than hanging a CI run.
    fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn a_client_with_the_secret_completes_the_handshake() {
        let s = secret_server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());
            let client = RegionClient::connect(addr, KEY).expect("handshake completes");

            assert_eq!(client.region(), RegionId::from_raw(7));
            assert_eq!(client.config(), &WorldConfig::default());
            assert_eq!(client.server_version(), ServerVersion::CURRENT);

            let edge = accepted.join().expect("thread").expect("server side completes");
            assert_eq!(edge.peer().ip(), addr.ip());
            assert_eq!(edge.id(), crate::net::EdgeId::from_raw(0), "the first edge takes id zero");
        });
    }

    #[test]
    fn a_client_without_the_secret_is_refused() {
        let s = secret_server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());
            let denied = RegionClient::connect(addr, b"wrong-key").expect_err("must be refused");
            assert!(matches!(denied, NetError::Rejected(RejectCode::Unauthorized)));
            assert!(matches!(
                accepted.join().expect("thread"),
                Err(NetError::Rejected(RejectCode::Unauthorized))
            ));
        });
        assert_eq!(s.refused(), 1);
        assert_eq!(s.edges().accepted(), 0);
        assert!(s.edges().is_empty());
    }

    #[test]
    fn a_refused_client_learns_nothing_about_the_region() {
        // The whole reason the credential is checked before the server info.
        let s = secret_server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            scope.spawn(|| s.accept());
            let denied = RegionClient::connect(addr, b"wrong-key").expect_err("must be refused");
            let told = format!("{denied}");
            assert!(!told.contains('7'), "the region id leaked: {told}");
            assert!(!told.contains("4096"), "the region size leaked: {told}");
        });
    }

    #[test]
    fn several_edges_attach_and_stay_attached() {
        const EDGES: usize = 6;

        let s = secret_server();
        let addr = s.local_addr();
        let stop = s.shutdown_handle();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                s.run(|mut edge| {
                    let _ = edge.wait_for_close();
                })
                .expect("the accept loop runs until it is stopped")
            });

            let links: Vec<RegionClient> = (0..EDGES)
                .map(|_| RegionClient::connect(addr, KEY).expect("handshake completes"))
                .collect();

            for link in &links {
                assert_eq!(link.region(), RegionId::from_raw(7));
                assert_eq!(link.config(), &WorldConfig::default());
            }

            wait_until("every edge to be gathered", || s.edges().len() == EDGES);
            assert_eq!(s.edges().accepted(), EDGES as u64);
            assert_eq!(s.refused(), 0);
            assert_eq!(
                s.edges().view().len(),
                EDGES,
                "the region can say who is relaying for it"
            );

            // They are still up: nothing has closed, and the set has not moved.
            std::thread::sleep(Duration::from_millis(50));
            assert_eq!(s.edges().len(), EDGES, "edges must not drop on their own");

            drop(links);
            wait_until("every edge to detach", || s.edges().is_empty());
            stop.stop();
        });
    }

    #[test]
    fn an_unauthorized_client_does_not_stop_the_loop() {
        let s = secret_server();
        let addr = s.local_addr();
        let stop = s.shutdown_handle();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                s.run(|mut edge| {
                    let _ = edge.wait_for_close();
                })
                .expect("a refused peer is not a loop failure")
            });

            assert!(RegionClient::connect(addr, b"wrong").is_err());
            let good = RegionClient::connect(addr, KEY).expect("the next client still connects");
            wait_until("the good client to attach", || s.edges().len() == 1);

            assert_eq!(s.refused(), 1);
            assert_eq!(s.edges().accepted(), 1);

            drop(good);
            wait_until("the edge to detach", || s.edges().is_empty());
            stop.stop();
        });
    }

    #[test]
    fn a_client_that_quits_becomes_no_edge() {
        let s = server(Arc::new(AllowAll));
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());
            let quit = RegionClient::connect_with(addr, b"", |offer| {
                // What a client inspects before committing.
                assert_eq!(offer.region, RegionId::from_raw(7));
                Decision::Quit
            })
            .expect_err("quitting returns no client");
            assert!(matches!(quit, NetError::Declined));
            assert!(matches!(accepted.join().expect("thread"), Err(NetError::Declined)));
        });
        assert!(s.edges().is_empty());
        assert_eq!(s.edges().accepted(), 0, "a quit never became an edge");
        assert_eq!(s.refused(), 0, "and it is not a refusal either");
    }

    #[test]
    fn a_peer_that_says_nothing_does_not_hold_a_thread_forever() {
        let s = server(Arc::new(AllowAll));
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());
            // Connect, then never identify.
            let mute = TcpStream::connect(addr).expect("connects");
            let started = Instant::now();
            let outcome = accepted.join().expect("thread");
            assert!(outcome.is_err(), "a silent peer must not become an edge");
            assert!(
                started.elapsed() < HANDSHAKE_TIMEOUT * 3,
                "the handshake deadline did not fire: waited {:?}",
                started.elapsed()
            );
            drop(mute);
        });
        assert!(s.edges().is_empty());
    }

    #[test]
    fn a_peer_opening_with_the_wrong_message_is_refused() {
        let s = server(Arc::new(AllowAll));
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());
            let mut sock = TcpStream::connect(addr).expect("connects");
            // An acceptance where an identification belongs.
            write_frame(&mut sock, KIND_ACCEPTED, &[]).expect("writes");
            assert!(matches!(
                accepted.join().expect("thread"),
                Err(NetError::Unexpected { expected: "client identification", .. })
            ));
        });
        assert_eq!(s.refused(), 1);
    }

    #[test]
    fn a_detached_edge_frees_its_id_for_the_next_one() {
        let s = secret_server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| s.accept());
            let link = RegionClient::connect(addr, KEY).expect("handshake completes");
            let edge = first.join().expect("thread").expect("attaches");
            assert_eq!(edge.id().raw(), 0);
            drop(edge);
            drop(link);

            let second = scope.spawn(|| s.accept());
            let link = RegionClient::connect(addr, KEY).expect("handshake completes");
            let edge = second.join().expect("thread").expect("attaches");
            assert_eq!(edge.id().raw(), 0, "the freed slot is taken again");
            assert_eq!(s.edges().accepted(), 2);
            drop(link);
        });
    }

    #[test]
    fn shutdown_stops_a_loop_with_no_edges() {
        let s = server(Arc::new(AllowAll));
        let stop = s.shutdown_handle();
        std::thread::scope(|scope| {
            let running = scope.spawn(|| s.run(|_| {}));
            stop.stop();
            running.join().expect("thread").expect("a stopped loop returns cleanly");
        });
    }
}
