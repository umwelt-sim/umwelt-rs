//! Entity identity.

use core::fmt;

/// An entity's identity within a region.
///
/// A dense index, not a generational handle. A reused id is indistinguishable
/// from the original, so a client holding a ghost of the despawned entity will
/// alias the new one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityId(u32);

impl EntityId {
    #[inline(always)]
    pub const fn from_raw(raw: u32) -> EntityId {
        EntityId(raw)
    }

    #[inline(always)]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The id as an array index.
    #[inline(always)]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_transparent() {
        assert_eq!(size_of::<EntityId>(), size_of::<u32>());
    }

    #[test]
    fn raw_round_trips() {
        assert_eq!(EntityId::from_raw(9001).raw(), 9001);
        assert_eq!(EntityId::from_raw(9001).index(), 9001usize);
    }

    #[test]
    fn ordering_follows_raw_value() {
        assert!(EntityId::from_raw(3) < EntityId::from_raw(4));
    }
}

/// Which entity slots hold a live entity.
///
/// One bit per slot, parallel to the position arrays. A slot's bit is set while
/// an entity occupies it and cleared on despawn. Ids are never moved, so an
/// `EntityId` stays valid for the lifetime of the entity it names.
///
/// Clearing a bit removes the entity from the snapshot. It does not make the
/// slot safe to reuse: a client holding a ghost of the previous occupant would
/// alias the new one. Reuse requires either compaction during a quiet period or
/// a quarantine until every client has acknowledged the despawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveSet {
    words: Vec<u64>,
    slots: usize,
    live: usize,
}

impl LiveSet {
    pub fn new() -> LiveSet {
        LiveSet::default()
    }

    pub fn with_capacity(slots: usize) -> LiveSet {
        LiveSet {
            words: Vec::with_capacity(slots.div_ceil(64)),
            slots: 0,
            live: 0,
        }
    }

    /// Highest slot count seen, live or not. Matches the length of the position
    /// arrays.
    #[inline(always)]
    pub fn slots(&self) -> usize {
        self.slots
    }

    /// How many slots currently hold a live entity.
    #[inline(always)]
    pub fn live(&self) -> usize {
        self.live
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    #[inline(always)]
    pub fn contains(&self, id: EntityId) -> bool {
        let i = id.index();
        if i >= self.slots {
            return false;
        }
        self.words[i >> 6] & (1u64 << (i & 63)) != 0
    }

    /// Marks a slot live, growing to cover it if needed.
    pub fn insert(&mut self, id: EntityId) {
        let i = id.index();
        if i >= self.slots {
            self.slots = i + 1;
            let needed = self.slots.div_ceil(64);
            if self.words.len() < needed {
                self.words.resize(needed, 0);
            }
        }
        let w = &mut self.words[i >> 6];
        let bit = 1u64 << (i & 63);
        if *w & bit == 0 {
            *w |= bit;
            self.live += 1;
        }
    }

    /// Marks a slot dead. The slot is not reclaimed and the id is not reused.
    pub fn remove(&mut self, id: EntityId) {
        let i = id.index();
        if i >= self.slots {
            return;
        }
        let w = &mut self.words[i >> 6];
        let bit = 1u64 << (i & 63);
        if *w & bit != 0 {
            *w &= !bit;
            self.live -= 1;
        }
    }

    pub fn clear(&mut self) {
        self.words.clear();
        self.slots = 0;
        self.live = 0;
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    fn id(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    #[test]
    fn insert_then_contains() {
        let mut s = LiveSet::new();
        s.insert(id(0));
        s.insert(id(63));
        s.insert(id(64));
        s.insert(id(1000));
        for n in [0, 63, 64, 1000] {
            assert!(s.contains(id(n)), "slot {n} should be live");
        }
        assert!(!s.contains(id(1)));
        assert!(!s.contains(id(999)));
        assert_eq!(s.live(), 4);
        assert_eq!(s.slots(), 1001);
    }

    #[test]
    fn remove_clears_one_bit_only() {
        let mut s = LiveSet::new();
        for n in 0..128 {
            s.insert(id(n));
        }
        s.remove(id(64));
        assert!(!s.contains(id(64)));
        assert!(s.contains(id(63)));
        assert!(s.contains(id(65)));
        assert_eq!(s.live(), 127);
    }

    #[test]
    fn slots_does_not_shrink_on_remove() {
        let mut s = LiveSet::new();
        s.insert(id(500));
        s.remove(id(500));
        assert_eq!(s.live(), 0);
        assert_eq!(s.slots(), 501, "an id must stay valid for its entity's lifetime");
    }

    #[test]
    fn repeated_operations_do_not_double_count() {
        let mut s = LiveSet::new();
        s.insert(id(7));
        s.insert(id(7));
        assert_eq!(s.live(), 1);
        s.remove(id(7));
        s.remove(id(7));
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn absent_slots_are_not_live() {
        let s = LiveSet::new();
        assert!(!s.contains(id(0)));
        assert!(!s.contains(id(9999)));
        assert!(s.is_empty());
    }
}
