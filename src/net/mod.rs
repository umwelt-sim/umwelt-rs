//! Networking, in two protocols that are kept apart.
//!
//! There are two links in this architecture and they have almost nothing in
//! common. Sharing types between them would be the easy mistake, so they are
//! separate modules and this one holds only what is genuinely common.
//!
//! **Region to edge** — [`region`], built, over NATS. A region simulation and
//! the edges relaying for it. Few peers, mutually trusted, deployed together.
//! See `docs/adr/0001`.
//!
//! **Edge to game client** — not built, and it will be its own module. Many
//! peers, none of them trusted, deployed on someone else's machine and updated
//! on their schedule. Game clients do not speak NATS. The payloads
//! [`PacketWriter`](crate::PacketWriter) assembles belong to that link:
//! latest-only, lossy, unordered, MTU-sized.
//!
//! # The pieces, and which side holds them
//!
//! [`RegionServer`] is a region simulation's side: it answers requests for the
//! region's world parameters, reads the commands its edges send, and drops an
//! edge that has gone quiet. It gathers the edges it has heard from into
//! [`Edges`], since a region deals with the set of edges relaying for it rather
//! than with connections one at a time.
//!
//! [`RegionLink`] is an edge's side: one NATS connection through which it talks
//! to any number of regions. An edge server will take a `RegionLink` and start
//! its own socket server on the other side of itself, speaking the
//! client-facing protocol to game clients. It is not built, and it is a
//! different type from this one.
//!
//! # Shape
//!
//! ```no_run
//! use std::sync::Arc;
//! use umwelt::WorldConfig;
//! use umwelt::net::{EdgeName, Edges, Inbound, RegionId, RegionLink, RegionServer};
//!
//! // The region's simulation server binary.
//! let region = RegionId::from_raw(7);
//! let edges = Arc::new(Edges::new());
//! let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
//! let server = RegionServer::connect(
//!     "nats://127.0.0.1:4222",
//!     region,
//!     WorldConfig::default(),
//!     Arc::clone(&inbound),
//! )?;
//!
//! // Whatever relays for it. An edge server will hold one of these.
//! let link = RegionLink::connect("nats://127.0.0.1:4222", EdgeName::new("edge-1")?)?;
//! let offer = link.info(region)?; // the region's world, rebuilt and digest-checked
//! assert_eq!(offer.region, region);
//! # Ok::<(), umwelt::net::NetError>(())
//! ```

mod error;
pub mod region;

pub use error::NetError;
pub use region::{
    Applied, ClaimError, DespawnEntities, EDGE_TIMEOUT, EdgeId, EdgeName, EdgeSink, EdgeView,
    Edges, EntitiesSpawned, EntityKind, INFO_TIMEOUT, Incoming, Inbound, MAX_DESPAWN_PER_MESSAGE,
    MAX_MESSAGE_BYTES, MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, Offer,
    PROTOCOL_VERSION, ProtocolVersion, RegionId, RegionLink, RegionServer,
    ServerInfo, ServerVersion, Settled, SpawnEntities, WorldParams, subjects,
};
