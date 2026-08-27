//! The region-to-edge protocol, over NATS.
//!
//! One region simulation, and the edges relaying for it. See `docs/adr/0001`
//! for why this is NATS rather than a connection this crate implements.
//!
//! - [`protocol`] is the messages and the versions.
//! - [`subjects`] is which subject carries what, and how to read one back.
//! - [`edges`] is the set of edges one region has heard from.
//! - [`session`] is what a running region and its edges say to each other.

mod client;
pub mod edges;
pub mod protocol;
mod server;
pub mod session;
pub mod subjects;

pub use client::{INFO_TIMEOUT, Incoming, Offer, RegionLink};
pub use edges::{ClaimError, EdgeId, EdgeName, EdgeView, Edges};
pub use protocol::{
    DespawnEntities, EntitiesSpawned, EntityKind, MAX_DESPAWN_PER_MESSAGE, MAX_MESSAGE_BYTES,
    MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, PROTOCOL_VERSION,
    ProtocolVersion, RegionId, ServerInfo, ServerVersion, SpawnEntities, WorldParams,
};
pub use server::{EDGE_TIMEOUT, RegionServer};
pub use session::{Applied, EdgeSink, Inbound, Settled};
