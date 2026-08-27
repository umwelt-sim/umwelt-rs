//! The region-to-edge protocol.
//!
//! One region simulation, and the edges relaying for it. Few peers, mutually
//! trusted, deployed together, over a reliable ordered stream.
//!
//! Nothing here is shared with the client-facing protocol, which is not built.
//! See [`net`](crate::net) for why that separation is deliberate rather than
//! incidental.
//!
//! - [`protocol`] is the messages and the versions.
//! - [`wire`] is how a message becomes bytes on this link, and is specific to
//!   it: a length-prefixed stream is the wrong shape for the client-facing
//!   datagrams.
//! - [`auth`] is who is allowed to open a link.
//! - [`edges`] is the set of edges one region has attached.

pub mod auth;
mod client;
pub mod edges;
pub mod protocol;
mod server;
pub mod session;
mod wire;

pub use auth::{AllowAll, Authorizer, Denied, MAX_CREDENTIAL_BYTES, SharedSecret};
pub use client::{Decision, Incoming, Offer, RegionClient};
pub use edges::{ClaimError, Edge, EdgeId, EdgeView, Edges};
pub use protocol::{
    ClientIdentification, DespawnEntities, EntitiesSpawned, EntityKind, HANDSHAKE_TIMEOUT,
    MAX_DESPAWN_PER_MESSAGE, MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities,
    PROTOCOL_VERSION, PositionUpdates, ProtocolVersion, RegionId, Rejection, ServerInfo,
    ServerVersion, SpawnEntities, WorldParams,
};
pub use server::{RegionServer, Shutdown};
pub use session::{Applied, EdgeSink, Inbound, Settled};
pub use wire::MAX_FRAME_BYTES;
