//! Networking, in two protocols that are kept apart.
//!
//! There are two links in this architecture and they have almost nothing in
//! common. Sharing types between them would be the easy mistake, so they are
//! separate modules and this one holds only what is genuinely common.
//!
//! **Region to edge** — [`region`], built. A region simulation and the small
//! number of edges relaying for it. Few peers, mutually trusted, deployed
//! together. Reliable and ordered, and the identities on it are regions and
//! edges.
//!
//! **Edge to game client** — not built, and it will be its own module. Many
//! peers, none of them trusted, deployed on someone else's machine and updated
//! on their schedule. The payloads
//! [`PacketWriter`](crate::PacketWriter) assembles belong to that link:
//! latest-only, lossy, unordered, MTU-sized. Nothing about the framing here
//! suits them.
//!
//! They differ in peer count, in reliability, in ordering, in who is trusted,
//! and in what may be disclosed, which is every property that shapes a
//! protocol. So they get their own message types, their own framing, and their
//! own version. A change to one must not be able to break the other.
//!
//! # The pieces, and which side holds them
//!
//! [`RegionServer`] is a region simulation's listening socket. It gathers the
//! links it has accepted into [`Edges`], since a region deals with the set of
//! edges relaying for it rather than with sockets one at a time.
//!
//! [`RegionClient`] is one connection *to* a region. An edge server will take a
//! `RegionClient` and start its own socket server on the other side of itself,
//! speaking the client-facing protocol to game clients. It is not built, and it
//! is a different type from this one: a `RegionClient` is one link to one
//! region and knows nothing about fan-out or relaying.
//!
//! # Shape
//!
//! ```no_run
//! use std::sync::Arc;
//! use umwelt::WorldConfig;
//! use umwelt::net::{RegionClient, RegionId, RegionServer, SharedSecret};
//!
//! // The region's simulation server binary.
//! let region = RegionServer::bind(
//!     "0.0.0.0:7777",
//!     RegionId::from_raw(7),
//!     WorldConfig::default(),
//!     Arc::new(SharedSecret::new(*b"a secret both ends hold")),
//! )?;
//! std::thread::spawn(move || {
//!     region.run(|mut edge| {
//!         let _ = edge.wait_for_close();
//!     })
//! });
//!
//! // Whatever relays for it. An edge server will hold one of these.
//! let link = RegionClient::connect("10.0.0.1:7777", b"a secret both ends hold")?;
//! assert_eq!(link.region(), RegionId::from_raw(7));
//! let cfg = link.config(); // the region's world, rebuilt and digest-checked
//! # Ok::<(), umwelt::net::NetError>(())
//! ```

mod error;
pub mod region;

pub use error::{NetError, RejectCode};
pub use region::{
    AllowAll, Applied, Authorizer, ClaimError, ClientIdentification, Decision, Denied,
    DespawnEntities, Edge, EdgeId, EdgeSink, EdgeView, Edges, EntitiesSpawned,
    EntityKind,
    HANDSHAKE_TIMEOUT,
    Incoming, Inbound,
    MAX_CREDENTIAL_BYTES, MAX_DESPAWN_PER_MESSAGE, MAX_FRAME_BYTES, MAX_MOVES_PER_MESSAGE,
    MAX_SPAWN_PER_MESSAGE, MoveEntities, Offer, PROTOCOL_VERSION, PositionUpdates,
    ProtocolVersion, RegionClient, RegionId, RegionServer, Rejection, ServerInfo, ServerVersion,
    Settled, SharedSecret, Shutdown, SpawnEntities, WorldParams,
};
