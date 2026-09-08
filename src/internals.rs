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
pub use crate::subscription::Subscription;

/// A world with `cell_size` overridden, for re-running the cell-size sweep.
///
/// Here rather than on [`WorldConfig`](crate::WorldConfig) because cell size
/// derives from the view radius and no consumer has a reason to set it. It
/// panics on a size the grid cannot take, which is right for a sweep and wrong
/// for anything a consumer calls.
pub fn with_cell_size_m(cfg: &crate::WorldConfig, m: i32) -> crate::WorldConfig {
    cfg.with_cell_size_m(m)
}

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

/// Spawns a shadow, the library's viewer with no presence in the snapshot.
///
/// Here rather than on [`Step`](crate::Step) because a consumer never spawns
/// one: the edge does, over the region link, for an observer whose view
/// reaches a seam. The seam benchmark needs a region full of them without a
/// broker.
pub fn spawn_shadow(step: &mut crate::Step<'_>, at: crate::Pos3) -> crate::EntityId {
    step.spawn_shadow(at)
}

/// The region-to-edge wire, and an edge's side of it.
pub mod region {
    pub use crate::net::region::client::{Incoming, Offer, RegionClient};

    /// Which subject carries what, and how to read one back. Every inbound
    /// message on this link pays the parse, so it is benchmarked.
    pub mod subjects {
        pub use crate::net::region::subjects::{command, origin, sender, state};
    }

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
