//! Networking, in two protocols that are kept apart.
//!
//! There are two links in this architecture and they have almost nothing in
//! common. Sharing types between them would be the easy mistake, so they are
//! separate modules and this one holds only what is genuinely common.
//!
//! **Region to edge** — [`region`], built, over NATS. A region simulation and
//! the edges relaying for it: few peers, deployed together and updated
//! together. See `docs/adr/0001`.
//!
//! **Edge to game client** — [`edge`], built, over QUIC. Many peers, none of
//! them trusted, running on someone else's machine and updated on someone
//! else's schedule. Game clients do not speak NATS. The payloads
//! [`PacketWriter`](crate::PacketWriter) assembles belong to that link:
//! latest-only, lossy, unordered, MTU-sized, and they reach a client on a
//! datagram. See `docs/adr/0006`.
//!
//! # The pieces, and which side holds them
//!
//! [`RegionServer`] is a region simulation's side: it answers requests for the
//! region's world parameters, reads the commands its edges send, and drops an
//! edge that has gone quiet. It gathers the edges it has heard from into
//! [`Edges`], since a region deals with the set of edges relaying for it rather
//! than with connections one at a time.
//!
//! [`RegionClient`] is an edge's side: the edge's name plus a connection the
//! caller made, through which it talks to any number of regions.
//!
//! [`EdgeServer`] holds one of those and a QUIC endpoint, and relays between
//! them. It is a different type from `RegionClient` and does what that one
//! deliberately does not: game client connections, the client-facing protocol,
//! and the mapping from an entity to whoever owns it.
//!
//! No type here connects to anything or binds anything. Each takes a connected
//! [`async_nats::Client`] and a Tokio handle, and `EdgeServer` also takes a
//! bound `quinn::Endpoint`, so the broker address, credentials, TLS,
//! certificates, the crypto provider and cluster membership all stay the
//! caller's.
//!
//! # Shape
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use umwelt::WorldConfig;
//! use umwelt::net::{EdgeName, Edges, Inbound, RegionClient, RegionId, RegionServer};
//!
//! // The caller connects. Where the broker is, what credentials it wants and
//! // whether it is a cluster are not the library's to decide.
//! let runtime = tokio::runtime::Runtime::new()?;
//! let client = runtime.block_on(async_nats::connect("nats://127.0.0.1:4222"))?;
//!
//! // The region's simulation server binary.
//! let region = RegionId::from_raw(7);
//! let edges = Arc::new(Edges::new());
//! let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
//! let server = RegionServer::new(
//!     client.clone(),
//!     runtime.handle().clone(),
//!     region,
//!     WorldConfig::default(),
//!     Arc::clone(&inbound),
//!     Duration::from_secs(5),
//! )?;
//!
//! // Whatever relays for it. An edge server will hold one of these.
//! let edge = RegionClient::new(client, runtime.handle().clone(), EdgeName::new("edge-1")?)?;
//! let offer = edge.info(region, Duration::from_secs(5))?; // rebuilt and digest-checked
//! assert_eq!(offer.region, region);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod control;
pub mod edge;
mod error;
pub mod region;

pub use control::{EdgeHeartbeat, EdgeLoad, Heartbeat, RegionLoad};
pub use edge::{
    ClientId, EdgeClient, EdgeHandle, EdgeServer, EdgeStats, EntityKey, Framer, FromClient,
    FromEdge, ToClient,
};
pub use error::NetError;
pub use region::{
    Applied, ClaimError, DespawnEntities, EdgeId, EdgeName, EdgeSink, EdgeView,
    Edges, EntityKind, Incoming, Inbound, MAX_DESPAWN_PER_MESSAGE,
    MAX_MESSAGE_BYTES, MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, Offer,
    PROTOCOL_VERSION, Presence, ProtocolVersion, RegionClient, RegionId, RegionServer, Spawn,
    ServerInfo, ServerVersion, Settled, SpawnEntities, WorldParams, subjects,
};
