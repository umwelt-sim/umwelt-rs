//! One connection to a region.
//!
//! [`RegionClient`] is the near end of the link a [`RegionServer`](crate::net::RegionServer)
//! answers. It connects, presents a credential, reads what the region says it
//! is, and either takes the offer or leaves.
//!
//! **This is not an edge server.** An edge server will take a `RegionClient`
//! and start its own socket server on the other side of itself, speaking the
//! client-facing protocol to game clients. A `RegionClient` is one link to one
//! region and knows nothing about game clients, fan-out, or relaying.
//! Conflating the two would put per-client work back on the edge tier, which is
//! the mistake the architecture exists to avoid.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};

use crate::config::WorldConfig;
use crate::net::error::NetError;
use crate::net::region::protocol::{
    ClientIdentification, HANDSHAKE_TIMEOUT, KIND_ACCEPTED, KIND_CLIENT_IDENTIFICATION, KIND_QUIT,
    KIND_REJECTION, KIND_SERVER_INFO, PROTOCOL_VERSION, RegionId, Rejection, ServerInfo,
    ServerVersion,
};
use crate::net::region::wire::{read_frame, write_frame};

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
#[derive(Debug)]
pub struct RegionClient {
    stream: TcpStream,
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

        Ok(RegionClient { stream: sock, offer })
    }

    #[inline(always)]
    pub fn offer(&self) -> &Offer {
        &self.offer
    }

    #[inline(always)]
    pub fn region(&self) -> RegionId {
        self.offer.region
    }

    #[inline(always)]
    pub fn server_version(&self) -> ServerVersion {
        self.offer.server
    }

    /// The world this region runs, rebuilt from what it advertised.
    #[inline(always)]
    pub fn config(&self) -> &WorldConfig {
        &self.offer.config
    }

    #[inline(always)]
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    pub fn peer_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.stream.peer_addr()?)
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
