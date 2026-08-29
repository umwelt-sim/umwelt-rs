//! Networking, in two protocols that are kept apart.
//!
//! There are two links in this architecture and they have almost nothing in
//! common. Sharing a *message* between them would be the easy mistake, so they
//! are separate modules and neither reaches into the other's protocol.
//!
//! What they do share is the vocabulary underneath both —
//! [`RegionId`](crate::RegionId), [`EntityId`](crate::EntityId) and
//! [`EntityKind`](crate::EntityKind) — because those name things in the world
//! rather than things on a wire. They live in [`id`](crate::id) and
//! [`entity`](crate::entity), outside this module entirely, so that the traits
//! a consumer implements can be written without reaching in here. The two links
//! also share one decoder, `wire::Cursor`, which belongs to neither.
//!
//! **Region to edge** — over NATS. A region simulation and the edges relaying
//! for it: few peers, deployed together and updated together.
//!
//! **Edge to game client** — over QUIC. Many peers, none of them trusted,
//! running on someone else's machine and updated on someone else's schedule.
//! Game clients do not speak NATS. The payloads a region assembles belong to
//! that link: latest-only, lossy, unordered, MTU-sized, and they reach a
//! client on a datagram.
//!
//! Neither format is public. Subjects, message kinds, versions and caps are
//! umwelt's on both ends.
//!
//! # The pieces, and which side holds them
//!
//! [`RegionServer`] is a region simulation's side: it answers requests for the
//! region's world parameters, reads the commands its edges send, and drops an
//! edge that has gone quiet. It gathers the edges it has heard from into
//! [`Edges`], since a region deals with the set of edges relaying for it rather
//! than with connections one at a time.
//!
//! [`EdgeServer`] is the other side of that link and the whole of an edge: it
//! holds a connection to every region it reaches and a QUIC endpoint facing
//! game clients, and relays between them. Game client connections, the
//! client-facing protocol, and the mapping from an entity to whoever owns it
//! are all its.
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
//! use umwelt::net::{Edges, Inbound, RegionServer};
//! use umwelt::{RegionId, WorldConfig};
//!
//! // The caller connects. Where the broker is, what credentials it wants and
//! // whether it is a cluster are not the library's to decide.
//! let runtime = tokio::runtime::Runtime::new()?;
//! let client = runtime.block_on(async_nats::connect("nats://127.0.0.1:4222"))?;
//!
//! // The region's simulation server binary. `Inbound` is what the tick reads
//! // its edges' commands out of; `Edges` is the set it has heard from.
//! let region = RegionId::from_raw(7);
//! let edges = Arc::new(Edges::new());
//! let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
//! let server = RegionServer::new(
//!     client,
//!     runtime.handle().clone(),
//!     region,
//!     WorldConfig::default(),
//!     Arc::clone(&inbound),
//!     Duration::from_secs(5),
//! )?;
//! assert_eq!(server.region(), region);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! An edge is the other binary, and is [`EdgeServer`] rather than anything
//! assembled out of parts.

pub mod control;
mod error;
pub(crate) mod wire;

pub(crate) mod edge;
pub(crate) mod region;

pub use control::{EdgeHeartbeat, EdgeLoad, Heartbeat, RegionLoad};
pub use edge::{ClientHandle, EdgeClient, EdgeHandle, EdgeServer, EdgeStats};
pub use error::NetError;
pub use region::{
    Applied, ClaimError, EdgeId, EdgeName, EdgeSink, EdgeView, Edges, Inbound,
    RegionServer, Settled,
};
