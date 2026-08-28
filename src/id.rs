//! Who and what, named.
//!
//! Nothing here is about networking. These name a region, a connection and an
//! entity an edge is holding, and they would name the same things in a program
//! that never opened a socket. They live outside `net` so that the traits a
//! consumer implements can be written without reaching into it.
//!
//! [`EntityId`](crate::EntityId) is the exception that stays where it is: it is
//! an identifier *and* the index of a slot in the simulation's position arrays,
//! so it belongs beside the set that says which slots are live.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

/// Which region a simulation owns.
///
/// Assigned by the control plane, which is not built. Until then a consumer
/// picks one and passes it to
/// [`RegionServer::new`](crate::net::RegionServer::new).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RegionId(u32);

impl RegionId {
    #[inline]
    pub const fn from_raw(raw: u32) -> RegionId {
        RegionId(raw)
    }

    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "R{}", self.0)
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "region {}", self.0)
    }
}

/// One live game client connection.
///
/// Not an account, not a player, not a session that survives a reconnect —
/// reconnecting produces a new one. Whatever a game knows about *who* is behind
/// a connection is its own, keyed by this.
///
/// **Never reused.** A recycled [`EdgeId`](crate::net::EdgeId) is safe because
/// nothing outside a region holds one across the gap. A `ClientId` is held
/// freely by the consumer, in its own tables and timers, and a recycled one
/// would send one player's packets to another. So a stale one resolves to
/// nothing rather than to somebody else.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(u64);

impl ClientId {
    #[inline]
    pub const fn from_raw(raw: u64) -> ClientId {
        ClientId(raw)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "C{}", self.0)
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "client {}", self.0)
    }
}

/// One entity this edge manages, wherever it is.
///
/// The edge's own name for an entity, minted when it asks a region for one and
/// valid before the region has answered. It doubles as the correlation token on
/// [`SpawnEntities`](crate::net::SpawnEntities), which a region echoes without
/// looking inside, so an edge needs no separate token space.
///
/// **Never reused**, for the same reason an [`EntityId`](crate::EntityId) is
/// not: a stale key must resolve to nothing rather than to a different entity.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityKey(u64);

impl EntityKey {
    #[inline]
    pub const fn from_raw(raw: u64) -> EntityKey {
        EntityKey(raw)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for EntityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "K{}", self.0)
    }
}

impl fmt::Display for EntityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entity key {}", self.0)
    }
}

/// Hands out ids that are never reused.
///
/// Starts at one, so zero is never a live id and a zeroed field is visibly not
/// one.
#[derive(Debug)]
pub(crate) struct Mint(AtomicU64);

impl Mint {
    pub(crate) const fn new() -> Mint {
        Mint(AtomicU64::new(1))
    }

    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_transparent() {
        assert_eq!(size_of::<ClientId>(), size_of::<u64>());
        assert_eq!(size_of::<EntityKey>(), size_of::<u64>());
    }

    #[test]
    fn a_mint_never_repeats_and_never_hands_out_zero() {
        let m = Mint::new();
        let first = m.next();
        assert_ne!(first, 0);
        assert_eq!(m.next(), first + 1);
        assert_eq!(m.next(), first + 2);
    }

    #[test]
    fn a_mint_is_safe_to_share() {
        let m = Mint::new();
        let seen = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let mine: Vec<u64> = (0..100).map(|_| m.next()).collect();
                    seen.lock().expect("not poisoned").extend(mine);
                });
            }
        });
        let mut all = seen.into_inner().expect("not poisoned");
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(all.len(), before, "a mint handed out the same id twice");
    }
}
