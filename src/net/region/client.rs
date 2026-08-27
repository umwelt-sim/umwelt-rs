//! One connection to a region.
//!
//! [`RegionClient`] is the near end of the link a [`RegionServer`](crate::net::RegionServer)
//! answers. It connects, presents a credential, reads what the region says it
//! is, and either takes the offer or leaves.
//!
//! A `RegionClient` is not an edge server. An edge server will take one of
//! these and start its own socket server on the other side of itself, speaking
//! the client-facing protocol to game clients. A `RegionClient` is one link to
//! one region and knows nothing about game clients, fan-out, or relaying.

use std::io::{BufWriter, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Mutex;

use crate::config::WorldConfig;
use crate::entity::EntityId;
use crate::net::error::NetError;
use crate::net::region::protocol::{
    ClientIdentification, DespawnEntities, EntitiesSpawned, HANDSHAKE_TIMEOUT, KIND_ACCEPTED,
    KIND_CLIENT_IDENTIFICATION, KIND_DESPAWN_ENTITIES, KIND_ENTITIES_SPAWNED, KIND_MOVE_ENTITIES,
    KIND_POSITION_UPDATES, KIND_QUIT, KIND_REJECTION, KIND_SERVER_INFO, MAX_DESPAWN_PER_MESSAGE,
    MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, PROTOCOL_VERSION, PositionUpdates,
    EntityKind, KIND_SPAWN_ENTITIES, RegionId, Rejection, ServerInfo, ServerVersion,
    SpawnEntities,
};
use crate::net::region::wire::{read_frame, write_frame};
use crate::pos::Pos3;

/// What a region says it is, before the client commits to it.
///
/// The config here has already been rebuilt from the region's parameters and
/// checked against its digest, so a client holding an `Offer` is holding a
/// world it can decode packets against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The crate version the region is running. Informational.
    pub server: ServerVersion,
    pub region: RegionId,
    pub config: WorldConfig,
}

/// What a client does with an [`Offer`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decision {
    /// Take the session and stay connected.
    #[default]
    Accept,
    /// Leave. The server is told, so it does not count a session it does not
    /// have.
    Quit,
}

/// A connected, handshaken link to one region.
///
/// Dropping it closes the connection.
/// What the region sent, other than the handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
    /// The entities a [`spawn`](RegionClient::spawn) asked for, and the viewer
    /// watching each. The region allocated the ids.
    Spawned(EntitiesSpawned),
    /// One viewer's payload. Decode it with
    /// [`PacketReader`](crate::PacketReader).
    Updates(PositionUpdates<'a>),
}

#[derive(Debug)]
pub struct RegionClient {
    stream: TcpStream,
    /// The write half. Sending takes `&self` so a receive loop and a send loop
    /// can hold the same client, and two frames must not interleave.
    ///
    /// Buffered, so a move batch split across frames by the frame cap costs one
    /// syscall rather than one per frame.
    writer: Mutex<BufWriter<TcpStream>>,
    offer: Offer,
}

impl RegionClient {
    /// Connects and takes whatever the region offers.
    ///
    /// The common case: an edge is pointed at a region because it is meant to
    /// relay for that region, so there is nothing to weigh up. Use
    /// [`connect_with`](Self::connect_with) to inspect the offer first.
    pub fn connect(
        addr: impl ToSocketAddrs,
        credential: &[u8],
    ) -> Result<RegionClient, NetError> {
        RegionClient::connect_with(addr, credential, |_| Decision::Accept)
    }

    /// Connects, then hands the region's offer to `decide`.
    ///
    /// Returns [`NetError::Declined`] if `decide` quits, which is not a failure
    /// of the link: a connect that quits simply has no client to give back.
    ///
    /// A region that refuses the credential returns
    /// [`NetError::Rejected`], and `decide` is never called, since there is
    /// nothing to decide about.
    pub fn connect_with(
        addr: impl ToSocketAddrs,
        credential: &[u8],
        decide: impl FnOnce(&Offer) -> Decision,
    ) -> Result<RegionClient, NetError> {
        let mut sock = TcpStream::connect(addr)?;
        sock.set_nodelay(true)?;
        // A region that accepts the connection and then goes quiet must not
        // hold this thread indefinitely.
        sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        sock.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

        let mut body = Vec::new();
        ClientIdentification { protocol: PROTOCOL_VERSION, credential: credential.to_vec() }
            .encode(&mut body);
        write_frame(&mut sock, KIND_CLIENT_IDENTIFICATION, &body)?;

        let kind = read_frame(&mut sock, &mut body)?;
        let info = match kind {
            KIND_SERVER_INFO => ServerInfo::decode(&body)?,
            KIND_REJECTION => return Err(NetError::Rejected(Rejection::decode(&body)?.code)),
            other => {
                return Err(NetError::Unexpected {
                    expected: "server info or rejection",
                    got: other,
                });
            }
        };

        // Rebuilding the config is also the check that this end decodes the
        // region's packets the way the region encodes them.
        let offer = Offer {
            server: info.server,
            region: info.region,
            config: info.params.to_config()?,
        };

        match decide(&offer) {
            Decision::Accept => write_frame(&mut sock, KIND_ACCEPTED, &[])?,
            Decision::Quit => {
                // Best effort: the server drops the connection either way, and
                // telling it saves it a timeout.
                let _ = write_frame(&mut sock, KIND_QUIT, &[]);
                return Err(NetError::Declined);
            }
        }

        // A session sits idle between packets, so the handshake's deadline
        // would close a healthy connection.
        sock.set_read_timeout(None)?;
        sock.set_write_timeout(None)?;

        let writer = Mutex::new(BufWriter::new(sock.try_clone()?));
        Ok(RegionClient { stream: sock, writer, offer })
    }

    #[inline]
    pub fn offer(&self) -> &Offer {
        &self.offer
    }

    #[inline]
    pub fn region(&self) -> RegionId {
        self.offer.region
    }

    #[inline]
    pub fn server_version(&self) -> ServerVersion {
        self.offer.server
    }

    /// The world this region runs, rebuilt from what it advertised.
    #[inline]
    pub fn config(&self) -> &WorldConfig {
        &self.offer.config
    }

    #[inline]
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    // -- the session ------------------------------------------------------

    /// Asks the region to create entities and record this edge as managing
    /// them.
    ///
    /// Each carries an [`EntityKind`], because whether a viewer is registered
    /// for it is the difference between an entity that costs 12 bytes of
    /// snapshot and one that costs the whole per-viewer pipeline every tick.
    ///
    /// The region allocates the ids and answers with [`Incoming::Spawned`];
    /// nothing is returned here, because the reply arrives on the same stream
    /// as everything else. Sends as many messages as the frame cap needs.
    pub fn spawn(&self, spawns: &[(Pos3, EntityKind)]) -> Result<(), NetError> {
        self.send_all(
            KIND_SPAWN_ENTITIES,
            spawns.chunks(MAX_SPAWN_PER_MESSAGE).map(|chunk| {
                let mut body = Vec::new();
                SpawnEntities { spawns: chunk.to_vec() }.encode(&mut body);
                body
            }),
        )
    }

    /// [`spawn`](Self::spawn) where every entity has a client behind it.
    pub fn spawn_observers(&self, positions: &[Pos3]) -> Result<(), NetError> {
        self.spawn(&SpawnEntities::observers(positions).spawns)
    }

    /// Sends new absolute positions for entities this edge manages.
    ///
    /// The region declines any naming an entity it does not have this edge down
    /// as managing, so a stale id costs a refusal rather than moving somebody
    /// else's entity.
    pub fn move_entities(&self, moves: &[(EntityId, Pos3)]) -> Result<(), NetError> {
        self.send_all(
            KIND_MOVE_ENTITIES,
            moves.chunks(MAX_MOVES_PER_MESSAGE).map(|chunk| {
                let mut body = Vec::new();
                MoveEntities { moves: chunk.to_vec() }.encode(&mut body);
                body
            }),
        )
    }

    /// Gives entities back, because the game clients behind them have gone.
    ///
    /// The region despawns them and drops the viewers watching them. An edge
    /// that disconnects gives up everything it holds without sending this.
    pub fn despawn(&self, ids: &[EntityId]) -> Result<(), NetError> {
        self.send_all(
            KIND_DESPAWN_ENTITIES,
            ids.chunks(MAX_DESPAWN_PER_MESSAGE).map(|chunk| {
                let mut body = Vec::new();
                DespawnEntities { ids: chunk.to_vec() }.encode(&mut body);
                body
            }),
        )
    }

    /// Reads one message from the region, blocking until it arrives.
    ///
    /// `body` is reused across calls, so a receive loop holding one buffer does
    /// not allocate per message. The result borrows it.
    ///
    /// One caller at a time: two threads reading one link would each get part of
    /// the other's frame.
    pub fn receive<'a>(&self, body: &'a mut Vec<u8>) -> Result<Incoming<'a>, NetError> {
        let mut sock = &self.stream;
        let kind = read_frame(&mut sock, body)?;
        match kind {
            KIND_ENTITIES_SPAWNED => Ok(Incoming::Spawned(EntitiesSpawned::decode(body)?)),
            KIND_POSITION_UPDATES => Ok(Incoming::Updates(PositionUpdates::decode(body)?)),
            other => Err(NetError::Unexpected { expected: "a session message", got: other }),
        }
    }

    /// Writes every chunk of one logical message, then pushes once.
    fn send_all(&self, kind: u8, bodies: impl Iterator<Item = Vec<u8>>) -> Result<(), NetError> {
        let mut sock = self.writer.lock().expect("not poisoned");
        for body in bodies {
            write_frame(&mut *sock, kind, &body)?;
        }
        sock.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::error::RejectCode;
    use crate::net::region::auth::AllowAll;
    use crate::net::region::protocol::{ProtocolVersion, WorldParams};
    use crate::net::region::server::RegionServer;
    use std::sync::Arc;

    fn server() -> RegionServer {
        RegionServer::bind(
            "127.0.0.1:0",
            RegionId::from_raw(3),
            WorldConfig::default(),
            Arc::new(AllowAll),
        )
        .expect("binds a loopback port")
    }

    #[test]
    fn a_client_reads_the_offer_before_deciding() {
        let s = server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            scope.spawn(|| s.accept());
            let mut seen = None;
            let client = RegionClient::connect_with(addr, b"", |offer| {
                seen = Some(*offer);
                Decision::Accept
            })
            .expect("handshake completes");

            let seen = seen.expect("decide was called");
            assert_eq!(seen.region, RegionId::from_raw(3));
            assert_eq!(seen.config, WorldConfig::default());
            assert_eq!(client.offer(), &seen);
        });
    }

    #[test]
    fn connecting_to_nothing_is_an_io_error() {
        // Port zero never listens.
        let e = RegionClient::connect("127.0.0.1:0", b"").expect_err("nothing is listening");
        assert!(matches!(e, NetError::Io(_)));
    }

    #[test]
    fn a_client_refuses_a_region_whose_config_does_not_rebuild() {
        // Stands in for a region running a build whose wire layout has moved.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("bound");

        std::thread::scope(|scope| {
            scope.spawn(move || {
                let (mut sock, _) = listener.accept().expect("accepts");
                let mut body = Vec::new();
                read_frame(&mut sock, &mut body).expect("reads the hello");

                let mut params = WorldParams::from_config(&WorldConfig::default());
                params.protocol_hash ^= 0xFF;
                ServerInfo { server: ServerVersion::CURRENT, region: RegionId::from_raw(1), params }
                    .encode(&mut body);
                write_frame(&mut sock, KIND_SERVER_INFO, &body).expect("writes the info");
            });

            let e = RegionClient::connect(addr, b"").expect_err("must not accept the offer");
            assert!(matches!(e, NetError::ConfigMismatch { .. }), "got {e:?}");
        });
    }

    #[test]
    fn a_client_reports_the_reason_it_was_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("bound");

        std::thread::scope(|scope| {
            scope.spawn(move || {
                let (mut sock, _) = listener.accept().expect("accepts");
                let mut body = Vec::new();
                read_frame(&mut sock, &mut body).expect("reads the hello");
                Rejection { code: RejectCode::ProtocolMismatch }.encode(&mut body);
                write_frame(&mut sock, KIND_REJECTION, &body).expect("writes the rejection");
            });

            let e = RegionClient::connect(addr, b"").expect_err("must be refused");
            assert!(matches!(e, NetError::Rejected(RejectCode::ProtocolMismatch)), "got {e:?}");
        });
    }

    #[test]
    fn a_client_speaking_the_wrong_protocol_version_is_refused() {
        let s = server();
        let addr = s.local_addr();
        std::thread::scope(|scope| {
            let accepted = scope.spawn(|| s.accept());

            // Hand-rolled, since RegionClient always sends the current version.
            let mut sock = TcpStream::connect(addr).expect("connects");
            let mut body = Vec::new();
            ClientIdentification {
                protocol: ProtocolVersion::from_raw(PROTOCOL_VERSION.raw() + 1),
                credential: Vec::new(),
            }
            .encode(&mut body);
            write_frame(&mut sock, KIND_CLIENT_IDENTIFICATION, &body).expect("writes");

            let kind = read_frame(&mut sock, &mut body).expect("reads the answer");
            assert_eq!(kind, KIND_REJECTION);
            assert_eq!(
                Rejection::decode(&body).expect("well formed").code,
                RejectCode::ProtocolMismatch
            );

            assert!(matches!(
                accepted.join().expect("thread"),
                Err(NetError::ProtocolMismatch { .. })
            ));
        });
    }
}
