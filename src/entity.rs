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

    /// The id as an array index. This array index is only safe in certain situations,
    /// like indexing within a region
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

/// What an entity is and whether it observes, plus a game-defined tag that
/// travels in every observation record.
///
/// The role decides whether a viewer is registered: an observer gets one, an
/// unattended entity does not. The tag is a `u16` the game defines and umwelt
/// does not interpret — a client maps it to an asset, a model, a sprite index,
/// or whatever its rendering needs.
///
/// Measured at a constant 8,192 entities, a viewer costs about 1.6 µs a tick
/// against 0.4 ms of work paid per entity regardless of who observes.
///
/// Static scenery has no kind here, because it is never spawned. A rock that
/// never moves is already in the client's content package and doesn't need to
/// be transmitted or managed in the server's simulation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntityKind {
    role: u8,
    tag: u16,
}

/// The observer/unattended distinction, without the tag.
const ROLE_UNATTENDED: u8 = 0;
const ROLE_OBSERVER: u8 = 1;

impl EntityKind {
    /// An unattended entity carrying a game-defined tag. No viewer is
    /// registered. Projectiles, wildlife, NPCs, resource nodes.
    #[inline]
    pub const fn unattended(tag: u16) -> EntityKind {
        EntityKind { role: ROLE_UNATTENDED, tag }
    }

    /// An observer carrying a game-defined tag. A viewer is registered, so it
    /// receives a budgeted approximation of what it can see.
    #[inline]
    pub const fn observer(tag: u16) -> EntityKind {
        EntityKind { role: ROLE_OBSERVER, tag }
    }

    /// Whether a viewer is registered for it.
    #[inline]
    pub const fn observes(self) -> bool {
        self.role == ROLE_OBSERVER
    }

    /// The game-defined tag. Umwelt does not interpret it.
    #[inline]
    pub const fn tag(self) -> u16 {
        self.tag
    }

    /// The role byte: 0 for unattended, 1 for observer.
    #[inline]
    pub(crate) const fn role(self) -> u8 {
        self.role
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.role {
            ROLE_UNATTENDED => write!(f, "unattended({})", self.tag),
            ROLE_OBSERVER => write!(f, "observer({})", self.tag),
            _ => write!(f, "unknown({})", self.tag),
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

/// Which entities are alive.
///
/// One bit per entity id, parallel to the position arrays. A bit is set while
/// an entity occupies it and cleared on despawn. Ids are never moved, so an
/// `EntityId` stays valid for the lifetime of the entity it names.
///
/// Clearing a bit removes the entity from the snapshot. It does not make the
/// id safe to reuse: a client holding a ghost of the previous occupant would
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

    /// Empty, with room for `entities` before it grows.
    pub fn with_capacity(entities: usize) -> LiveSet {
        LiveSet { words: Vec::with_capacity(entities.div_ceil(64)), slots: 0, live: 0 }
    }

    /// Total entity ids allocated, alive or not. Matches the length of the
    /// position arrays.
    #[inline]
    pub fn id_space(&self) -> usize {
        self.slots
    }

    /// How many entities are currently alive.
    #[inline]
    pub fn live(&self) -> usize {
        self.live
    }

    /// Whether nothing is alive.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Live entities in ascending id order, skipping dead ones 64 at a time.
    ///
    /// Testing each id in turn costs the same whether it holds an entity or
    /// not, so a region that has despawned far more than it holds pays for
    /// every id it ever allocated. A word that is entirely dead is one
    /// comparison rather than 64.
    #[inline]
    pub fn iter(&self) -> LiveIter<'_> {
        LiveIter {
            words: &self.words,
            at: 0,
            current: self.words.first().copied().unwrap_or(0),
        }
    }

    /// Whether that entity is alive.
    #[inline]
    pub fn contains(&self, id: EntityId) -> bool {
        let i = id.index();
        if i >= self.slots {
            return false;
        }
        self.words[i >> 6] & (1u64 << (i & 63)) != 0
    }

    /// Marks an entity live, growing to cover it if needed.
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

    /// Marks an entity dead. The id is not reclaimed or reused.
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

    /// Marks every entity dead, keeping the allocation.
    pub fn clear(&mut self) {
        self.words.clear();
        self.slots = 0;
        self.live = 0;
    }
}

/// Ascending live entities from a [`LiveSet`], produced by [`LiveSet::iter`].
///
/// Holds the current word rather than re-reading it, and clears the lowest set
/// Which entities are alive, readable while their positions are held.
///
/// Handed back by [`Step::positions_mut`](crate::Step::positions_mut). Those
/// slices borrow the whole `Step`, and a sweep over them has to skip whatever
/// is no longer there, so the test comes back alongside the slices rather than
/// having to be recorded first.
#[derive(Clone, Copy, Debug)]
pub struct Live<'a> {
    set: &'a LiveSet,
}

impl<'a> Live<'a> {
    pub(crate) fn new(set: &'a LiveSet) -> Live<'a> {
        Live { set }
    }

    /// Whether that entity is alive. A despawned id is not.
    #[inline]
    pub fn contains(&self, id: EntityId) -> bool {
        self.set.contains(id)
    }

    /// Every live entity, in ascending id order.
    #[inline]
    pub fn iter(&self) -> LiveIter<'a> {
        self.set.iter()
    }

    /// How many entities are alive.
    #[inline]
    pub fn count(&self) -> usize {
        self.set.live()
    }
}

/// bit per step, so a dense word costs one instruction per live entity and an
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
    fn iter_yields_live_entities_in_ascending_order() {
        assert_eq!(walked(&set(&[5, 0, 63, 64, 200])), vec![0, 5, 63, 64, 200]);
    }

    #[test]
    fn iter_skips_a_long_run_of_dead_ids() {
        // One live entity either side of 10,000 dead ones.
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
    fn iter_agrees_with_contains_across_every_id() {
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
        let by_contains: Vec<u32> = (0..live.id_space() as u32)
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
            assert!(s.contains(id(n)), "entity {n} should be live");
        }
        assert!(!s.contains(id(1)));
        assert!(!s.contains(id(999)));
        assert_eq!(s.live(), 4);
        assert_eq!(s.id_space(), 1001);
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
    fn id_space_does_not_shrink_on_remove() {
        let mut s = LiveSet::new();
        s.insert(id(500));
        s.remove(id(500));
        assert_eq!(s.live(), 0);
        assert_eq!(s.id_space(), 501, "an id must stay valid for its entity's lifetime");
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
    fn absent_ids_are_not_live() {
        let s = LiveSet::new();
        assert!(!s.contains(id(0)));
        assert!(!s.contains(id(9999)));
        assert!(s.is_empty());
    }
}
