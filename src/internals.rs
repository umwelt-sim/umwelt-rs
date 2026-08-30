//! Not an interface. Do not import this.
//!
//! What umwelt's own benchmarks and integration tests reach for. Nothing here
//! is documented, stable, or subject to any compatibility promise.

pub use crate::budget::PacketBudget;
pub use crate::codec::RecordCodec;
pub use crate::gather::{DiscoveredEntities, DiscoveredEntity};
pub use crate::ghost::GhostTable;
pub use crate::odometer::Odometer;
pub use crate::packet::PacketWriter;
pub use crate::select::{NEAR_BAND, Ranked, Selection, select};
pub use crate::sim::Outbound;
pub use crate::snapshot::{CellOccupants, CellSnapshot, SubCells};
pub use crate::subscription::{CellList, Subscription};

/// Reads a payload back into a [`TickObservation`](crate::TickObservation),
/// which is what a client does with one.
///
/// Here rather than on the type because a consumer is handed observations by
/// [`ClientGame::observed`](crate::ClientGame::observed) and never makes one.
/// umwelt's own integration tests do, to model a client from the bytes.
pub fn read_payload<'a>(
    codec: &'a RecordCodec,
    buf: &'a [u8],
) -> Option<crate::TickObservation<'a>> {
    crate::packet::TickObservation::new(codec, buf)
}

/// The region-to-edge wire, and an edge's side of it.
pub mod region {
    pub use crate::net::region::client::{Incoming, Offer, RegionClient};
    pub use crate::net::region::protocol::{
        DespawnEntities, MAX_DESPAWN_PER_MESSAGE, MAX_MESSAGE_BYTES,
        MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, PROTOCOL_VERSION,
        Presence, ServerInfo, Spawn, SpawnEntities, WorldParams,
    };
}

/// The edge-to-client wire.
pub mod edge {
    pub use crate::net::edge::protocol::{
        EdgeInfo, Framer, FromClient, MAX_MESSAGE_BYTES, MAX_MOVES_PER_DATAGRAM, ToClient,
    };
}
