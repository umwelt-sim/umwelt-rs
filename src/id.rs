//! Identifying participants in a world simulation at a logical level. 
//! These IDs are transport agnostic and provide newtype identities
//! for regions, connections, edge-held entities, and game-held entities.
//!
//! It might seem excessive to have newtypes for IDs as understood by
//! each simulation, edge, and game client. However, since all of these 
//! IDs are just numbers, the type system does some work here in avoiding
//! confusion at call sites.
//! 
//! [`EntityId`](crate::EntityId) is the exception that can be found at
//! the root of the crate.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

/// Which region a simulation owns.
///
/// Typically assigned by a control plane but can also be explicitly set
/// in tests and rigid configuration scenarios.
/// [`RegionServer::new`](crate::net::RegionServer::new).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RegionId(u32);

impl RegionId {
    /// From the raw value, which is how one crosses a wire.
    #[inline]
    pub const fn from_raw(raw: u32) -> RegionId {
        RegionId(raw)
    }

    /// The raw value.
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
/// Whatever a game knows about *who* is behind a connection is the responsibility
/// of the game and not Umwelt.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(u64);

impl ClientId {
    /// From the raw value. Minting one is the edge's; this is for decoding.
    #[inline]
    pub const fn from_raw(raw: u64) -> ClientId {
        ClientId(raw)
    }

    /// The raw value.
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
/// valid before the region has answered. It doubles as the token an edge sends
/// with a spawn, which the region echoes back without looking inside, so an
/// edge needs no separate token space.
///
/// **Never reused**, for the same reason an [`EntityId`](crate::EntityId) is
/// not: a stale key must resolve to nothing rather than to a different entity.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityKey(u64);

impl EntityKey {
    /// From the raw value, which is what a region echoes back as a token.
    #[inline]
    pub const fn from_raw(raw: u64) -> EntityKey {
        EntityKey(raw)
    }

    /// The raw value.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The name of an entity the region's own game spawned, which no edge
    /// minted a key for: the region in the high half, the entity's id in the
    /// low, and the top bit clear. A key an edge mints carries the top bit
    /// set, so the two never meet. A region's id has to fit 31 bits.
    #[inline]
    pub const fn of_region(region: RegionId, id: crate::EntityId) -> EntityKey {
        debug_assert!(region.raw() >> 31 == 0, "a region id must fit 31 bits");
        EntityKey(((region.raw() as u64) << 32) | id.raw() as u64)
    }

    /// Whether an edge minted this key, as opposed to a region composing it.
    #[inline]
    pub const fn minted_by_an_edge(self) -> bool {
        self.0 >> 63 == 1
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

/// A game client's own name for one of its entities.
///
/// Minted by [`ClientHandle`](crate::net::ClientHandle) when a game asks for an
/// entity, and usable before any region has answered: a move sent under one is
/// held at the edge until the id arrives.
///
/// One connection's own numbering. **Never reused**, so a stale one names
/// nothing rather than a different entity, and it means nothing to any other
/// connection. A game holding entities in two regions at once tells them apart
/// by this, since [`EntityId`](crate::EntityId) is only unique within a region.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityHandle(u32);

impl EntityHandle {
    /// From the raw value.
    #[inline]
    pub const fn from_raw(raw: u32) -> EntityHandle {
        EntityHandle(raw)
    }

    /// The raw value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for EntityHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "H{}", self.0)
    }
}

impl fmt::Display for EntityHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "handle {}", self.0)
    }
}

/// Hands out ids that are never reused.
///
/// Starts at one, so zero is never a live id and a zeroed field is visibly not
/// one.
#[derive(Debug)]
pub(crate) struct Mint(AtomicU64);

impl Mint {
    /// The bit every seeded id carries, so a seeded id never equals a name a
    /// region composes from its own id and an entity's.
    const TOP: u64 = 1 << 63;

    pub(crate) const fn new() -> Mint {
        Mint(AtomicU64::new(1))
    }

    /// A mint whose ids carry `prefix` in the high half with the top bit set,
    /// counting from one in the low half. Two mints with different prefixes
    /// never hand out the same id, which is what lets every edge mint entity
    /// keys without asking anyone. Only the low 31 bits of `prefix` are used.
    ///
    /// The low half runs into the prefix after 2^32 ids. An edge does not
    /// mint that many in one incarnation.
    pub(crate) const fn seeded(prefix: u32) -> Mint {
        Mint(AtomicU64::new(Mint::TOP | (((prefix & 0x7fff_ffff) as u64) << 32) | 1))
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
        assert_eq!(size_of::<EntityHandle>(), size_of::<u32>());
        assert_eq!(size_of::<RegionId>(), size_of::<u32>());
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
    fn a_seeded_mint_carries_its_prefix_under_the_top_bit() {
        let m = Mint::seeded(0x1234);
        let first = m.next();
        assert_eq!(first >> 63, 1, "the top bit is set");
        assert_eq!((first >> 32) & 0x7fff_ffff, 0x1234, "the prefix is in the high half");
        assert_eq!(first & 0xffff_ffff, 1, "counting starts at one");
        assert_eq!(m.next(), first + 1);
    }

    #[test]
    fn a_prefix_wider_than_31_bits_is_cut_to_31() {
        let a = Mint::seeded(0x8000_0001).next();
        let b = Mint::seeded(0x0000_0001).next();
        assert_eq!(a, b, "bit 31 of the prefix is not the top bit");
    }

    #[test]
    fn two_seeds_never_meet() {
        let a = Mint::seeded(1);
        let b = Mint::seeded(2);
        let from_a: std::collections::HashSet<u64> = (0..1000).map(|_| a.next()).collect();
        assert!((0..1000).map(|_| b.next()).all(|k| !from_a.contains(&k)));
    }

    #[test]
    fn a_composed_name_never_meets_a_minted_key() {
        let name = EntityKey::of_region(RegionId::from_raw(7), crate::EntityId::from_raw(3));
        assert!(!name.minted_by_an_edge());
        assert_eq!(name.raw(), (7u64 << 32) | 3);
        let widest = EntityKey::of_region(
            RegionId::from_raw(0x7fff_ffff),
            crate::EntityId::from_raw(u32::MAX),
        );
        assert!(!widest.minted_by_an_edge());
        let m = Mint::seeded(7);
        for _ in 0..1000 {
            let key = EntityKey::from_raw(m.next());
            assert!(key.minted_by_an_edge());
            assert_ne!(key, name);
        }
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
