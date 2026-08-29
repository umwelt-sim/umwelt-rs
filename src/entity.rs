//! Entities are opaque objects tracked by the simulator. All identities
//! are guaranteed to be unique within a region, where a region is owned
//! by a simulator process.

use core::fmt;

/// An entity's identity within a region.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityId(u32);

impl EntityId {
    /// From the raw value a region allocated.
    #[inline]
    pub const fn from_raw(raw: u32) -> EntityId {
        EntityId(raw)
    }

    /// The raw value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The id as an array index.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "E{}", self.0)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entity {}", self.0)
    }
}

/// What is behind an entity, which decides whether it observes.
///
/// An entity has a position and can be seen by whoever is near it. A viewer
/// receives: only an observer is sent what it can see, and only an observer
/// costs a subscription, a gather, a score, a selection and a packet every tick
/// it is served, plus a table of what its client already holds. Measured
/// at a constant 8,192 entities, a viewer costs about 1.6 µs a tick against
/// 0.4 ms of work paid per entity regardless of who observes.
///
/// Static scenery has no kind here, because it is never spawned. A rock that
/// never moves is already in the client's content package, and holding it in a
/// region would cost snapshot bytes and a gather-walk visit every tick to
/// replicate a position the client has. A region holds state that is
/// authoritative and changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum EntityKind {
    /// Nothing is behind it. Simulated and replicated to whoever can see it,
    /// observes nothing itself, and no viewer is registered. Projectiles,
    /// wildlife, NPCs, a vehicle with no driver.
    #[default]
    Unattended = 0,
    /// A game client is behind it. The region registers a viewer watching it,
    /// so it is sent a budgeted approximation of what it can see.
    Observer = 1,
}

impl EntityKind {
    /// Whether a viewer is registered for it.
    #[inline]
    pub const fn observes(self) -> bool {
        matches!(self, EntityKind::Observer)
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntityKind::Unattended => write!(f, "unattended"),
            EntityKind::Observer => write!(f, "observer"),
        }
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
    /// Empty.
    pub fn new() -> LiveSet {
        LiveSet::default()
    }

    /// Empty, with room for `slots` before it grows.
    pub fn with_capacity(slots: usize) -> LiveSet {
        LiveSet { words: Vec::with_capacity(slots.div_ceil(64)), slots: 0, live: 0 }
    }

    /// Highest slot count seen, live or not. Matches the length of the position
    /// arrays.
    #[inline]
    pub fn slots(&self) -> usize {
        self.slots
    }

    /// How many slots currently hold a live entity.
    #[inline]
    pub fn live(&self) -> usize {
        self.live
    }

    /// Whether nothing is alive.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Live slots in ascending order, skipping dead ones 64 at a time.
    ///
    /// Testing each slot in turn costs the same whether it holds an entity or
    /// not, so a region that has despawned far more than it holds pays for
    /// every slot it ever allocated. A word that is entirely dead is one
    /// comparison rather than 64. See §Slot growth under churn.
    #[inline]
    pub fn iter(&self) -> LiveIter<'_> {
        LiveIter {
            words: &self.words,
            at: 0,
            current: self.words.first().copied().unwrap_or(0),
        }
    }

    /// Whether that slot holds a live entity.
    #[inline]
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

    /// Marks every slot dead, keeping the allocation.
    pub fn clear(&mut self) {
        self.words.clear();
        self.slots = 0;
        self.live = 0;
    }
}

/// Ascending live slots from a [`LiveSet`], produced by [`LiveSet::iter`].
///
/// Holds the current word rather than re-reading it, and clears the lowest set
/// bit per step, so a dense word costs one instruction per live slot and an
/// empty one costs a single test.
#[derive(Clone, Debug)]
pub struct LiveIter<'a> {
    words: &'a [u64],
    /// Index of the word `current` came from.
    at: usize,
    /// Bits of that word not yet yielded.
    current: u64,
}

impl Iterator for LiveIter<'_> {
    type Item = EntityId;

    #[inline]
    fn next(&mut self) -> Option<EntityId> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some(EntityId::from_raw(((self.at << 6) + bit) as u32));
            }
            self.at += 1;
            self.current = *self.words.get(self.at)?;
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    fn set(of: &[u32]) -> LiveSet {
        let mut live = LiveSet::new();
        for &n in of {
            live.insert(EntityId::from_raw(n));
        }
        live
    }

    fn walked(live: &LiveSet) -> Vec<u32> {
        live.iter().map(|id| id.raw()).collect()
    }

    #[test]
    fn iter_yields_nothing_when_nothing_is_live() {
        assert!(walked(&LiveSet::new()).is_empty());
    }

    #[test]
    fn iter_yields_live_slots_in_ascending_order() {
        assert_eq!(walked(&set(&[5, 0, 63, 64, 200])), vec![0, 5, 63, 64, 200]);
    }

    #[test]
    fn iter_skips_a_long_run_of_dead_slots() {
        // One live slot either side of 10,000 dead ones.
        let mut live = set(&[0, 10_001]);
        assert_eq!(walked(&live), vec![0, 10_001]);
        live.remove(EntityId::from_raw(0));
        assert_eq!(walked(&live), vec![10_001]);
    }

    #[test]
    fn iter_follows_removals() {
        let mut live = set(&[1, 2, 3, 64, 65]);
        live.remove(EntityId::from_raw(2));
        live.remove(EntityId::from_raw(64));
        assert_eq!(walked(&live), vec![1, 3, 65]);
    }

    #[test]
    fn iter_yields_a_full_word() {
        let live = set(&(0..64).collect::<Vec<u32>>());
        assert_eq!(walked(&live), (0..64).collect::<Vec<u32>>());
    }

    #[test]
    fn iter_agrees_with_contains_across_every_slot() {
        // The property the two walks in `snapshot` and `odometer` rely on.
        let mut live = LiveSet::new();
        for n in 0..1_000u32 {
            if n % 7 == 0 || n % 11 == 0 {
                live.insert(EntityId::from_raw(n));
            }
        }
        live.insert(EntityId::from_raw(4_000));
        for n in 0..500u32 {
            if n % 3 == 0 {
                live.remove(EntityId::from_raw(n));
            }
        }
        let by_contains: Vec<u32> = (0..live.slots() as u32)
            .filter(|&n| live.contains(EntityId::from_raw(n)))
            .collect();
        assert_eq!(walked(&live), by_contains);
        assert_eq!(walked(&live).len(), live.live());
    }

    #[test]
    fn iter_counts_what_live_reports() {
        let mut live = set(&(0..300).collect::<Vec<u32>>());
        for n in (0..300).step_by(2) {
            live.remove(EntityId::from_raw(n));
        }
        assert_eq!(walked(&live).len(), live.live());
    }

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
