//! Client registration.
//!
//! A viewer is a logical client: an avatar entity, and the replication state
//! kept for it. Nothing here names a socket, a connection, or an address. The
//! transport carrying a viewer's payloads belongs to the edge, and the
//! simulation only ever knows that a viewer exists and what its connection
//! declared it can take.

use core::fmt;

use crate::budget::PacketBudget;
use crate::entity::EntityId;
use crate::ghost::GhostTable;
use crate::subscription::Subscription;

/// A registered client's identity within one simulation.
///
/// Dense and reusable. Unlike an [`EntityId`], reuse is safe: a recycled
/// viewer is a different client with an empty ghost set, so there is nothing
/// for a stale reference to alias.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewerId(u32);

impl ViewerId {
    /// From the raw value.
    #[inline]
    pub const fn from_raw(raw: u32) -> ViewerId {
        ViewerId(raw)
    }

    /// The raw value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The id as an index into the viewer table.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for ViewerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "V{}", self.0)
    }
}

impl fmt::Display for ViewerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "viewer {}", self.0)
    }
}

/// What a client declared about its connection when it registered.
///
/// Supplied by the consumer when it registers a viewer, not read off a wire: a
/// game client never declares these to a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientLimits {
    /// Largest payload the connection accepts.
    ///
    /// Must leave room for a header and at least one entity record;
    /// [`register_viewer`](crate::WorldSimulation::register_viewer) panics
    /// otherwise.
    pub payload_bytes: u16,
    /// Ticks between this client's packets. One is every tick.
    ///
    /// A viewer's ghosts are stamped and aged only on the ticks it is served,
    /// so a period at or above the policy's `grace` leaves grace with nothing
    /// to absorb and behaves as a grace of zero. It does not churn the ghost
    /// set: at a period of 8 a client still holds its 256 ghosts, because
    /// everything in the set is stamped on every serve. What it costs is
    /// accuracy, in proportion, on both mean and 99th-percentile error.
    pub send_period: u8,
}

impl Default for ClientLimits {
    fn default() -> ClientLimits {
        ClientLimits {
            payload_bytes: crate::budget::DEFAULT_PAYLOAD_BYTES,
            send_period: 1,
        }
    }
}

/// One viewer's replication state.
///
/// Viewers are partitioned across worker threads by contiguous range, and each
/// worker writes its viewers' ghosts, sequence and subscription every tick. At
/// 96 bytes these do not divide a cache line, so the two workers either side of
/// a chunk boundary write the same one.
///
/// That is left alone deliberately. Padding to 128 was measured and cost 2-3%
/// across the pipeline benchmarks, single-threaded runs included, because it
/// spreads the viewer array over a third more cache lines to walk. The sharing
/// it removes is one line per chunk boundary, so `threads - 1` lines however
/// many viewers there are, while the padding is paid per viewer. The trade only
/// pays if a partition ever gets small enough for that constant to matter.
#[derive(Debug)]
pub(crate) struct Viewer {
    /// The entity this viewer watches from.
    pub avatar: EntityId,
    /// Which cells it draws from. `None` until the first tick that serves it.
    pub sub: Option<Subscription>,
    /// What its client is believed to hold.
    pub ghosts: GhostTable,
    /// How much fits in one of its packets.
    pub budget: PacketBudget,
    /// Ghosts this client has not yet been told to drop. Departures are found
    /// after the tick's records are chosen, so they go out in the next packet.
    pub pending_despawns: Vec<EntityId>,
    pub sequence: u16,
    pub send_period: u8,
    /// Whether it is being served. A slot that is not is free for reuse.
    pub registered: bool,
}

impl Viewer {
    pub(crate) fn reset(
        &mut self,
        avatar: EntityId,
        budget: PacketBudget,
        send_period: u8,
    ) {
        self.avatar = avatar;
        self.sub = None;
        self.ghosts.clear();
        self.pending_despawns.clear();
        self.sequence = 0;
        self.budget = budget;
        self.send_period = send_period.max(1);
        self.registered = true;
    }

    /// Whether this viewer is served on `tick`. The phase comes from the id, so
    /// viewers on the same period spread across ticks rather than bunching.
    #[inline]
    pub(crate) fn due(&self, id: ViewerId, tick: u32) -> bool {
        let period = self.send_period as u32;
        period <= 1 || (tick.wrapping_add(id.raw()) % period) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_transparent() {
        assert_eq!(size_of::<ViewerId>(), size_of::<u32>());
    }

    #[test]
    fn raw_round_trips() {
        assert_eq!(ViewerId::from_raw(12).raw(), 12);
        assert_eq!(ViewerId::from_raw(12).index(), 12usize);
    }

    #[test]
    fn a_default_client_takes_an_mtu_sized_payload_every_tick() {
        let d = ClientLimits::default();
        assert_eq!(d.payload_bytes, 1200);
        assert_eq!(d.send_period, 1);
    }
}
