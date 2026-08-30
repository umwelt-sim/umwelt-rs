//! Per-viewer ghost records.
//!
//! [`GhostTable`] holds what one client has been told about each entity: the
//! odometer reading at the last send, and the tick the entity was last a
//! candidate. Priority scoring differences the stored mark against the entity's
//! current reading to get how far that client's copy has drifted.
//!
//! An entity with no entry is one the client does not know exists. Being a
//! candidate does not create an entry; only being sent does.

use crate::entity::EntityId;

/// Reserved as the empty-slot marker. No entity may carry this id.
const EMPTY: u32 = u32::MAX;

/// Smallest allocated table, in slots. Power of two.
const MIN_SLOTS: usize = 16;

/// What one client has been told about one entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ghost {
    id: u32,
    /// Odometer reading when this entity was last sent to this client.
    mark: u32,
    /// Tick this entity was last a candidate for this client.
    last_seen: u32,
}

impl Ghost {
    const VACANT: Ghost = Ghost { id: EMPTY, mark: 0, last_seen: 0 };
}

/// Ghost records for one viewer, keyed by entity id.
///
/// Open addressing with linear probing, power-of-two slot count, held at or
/// below half full. Computed: 12 bytes per slot, so 24 bytes per ghost.
///
/// The hash is fixed rather than seeded, so a replay reconstructs identical
/// probe orders. Entity ids are server-assigned.
///
/// One of these belongs to each viewer, and viewers are partitioned across
/// threads. Whatever owns them is responsible for keeping two viewers' tables
/// off one cache line.
#[derive(Debug, Clone, Default)]
pub struct GhostTable {
    slots: Vec<Ghost>,
    len: usize,
}

/// Fibonacci hash. The high bits of a multiplicative hash are the mixed ones,
/// so they are shifted down rather than the low bits masked. Basically asks
/// where a given ID wants to live
#[inline(always)]
fn home(id: u32, bits: u32) -> usize {
    (id.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize
}

impl GhostTable {
    /// Empty. One viewer holds one of these.
    pub fn new() -> GhostTable {
        GhostTable::default()
    }

    /// Sized to hold `ghosts` without growing.
    pub fn with_capacity(ghosts: usize) -> GhostTable {
        let mut t = GhostTable::new();
        if ghosts > 0 {
            t.rehash((ghosts * 2).next_power_of_two().max(MIN_SLOTS));
        }
        t
    }

    /// How many ghosts this client holds.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this client is believed to hold nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Ghosts this table holds before it grows.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len() / 2
    }

    /// Allocated slots. Twice [`capacity`](Self::capacity).
    #[inline]
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// The mark for `id`, or `None` if the client holds no ghost of it.
    #[inline]
    pub fn mark(&self, id: EntityId) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        match self.find(id.raw()) {
            Ok(i) => Some(self.slots[i].mark),
            Err(_) => None,
        }
    }

    /// Records that `id` is a candidate on `tick`, and returns its mark if the
    /// client holds a ghost of it.
    ///
    /// `None` means the client does not know this entity exists. This does not
    /// create a ghost: a candidate that never wins a slot never becomes one.
    #[inline]
    pub fn seen(&mut self, id: EntityId, tick: u32) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        match self.find(id.raw()) {
            Ok(i) => {
                self.slots[i].last_seen = tick;
                Some(self.slots[i].mark)
            }
            Err(_) => None,
        }
    }

    /// Records that `id` was sent on `tick` at odometer reading `mark`,
    /// creating the ghost if the client did not have one.
    ///
    /// The mark advances on send rather than on acknowledgment, so a lost
    /// packet leaves this client's copy of a since-idle entity permanently
    /// wrong. Unresolved; see §Open questions in the design document.
    ///
    /// Allocates only while growing.
    ///
    /// # Panics
    ///
    /// If `id` is the reserved empty marker.
    pub fn sent(&mut self, id: EntityId, mark: u32, tick: u32) {
        assert!(id.raw() != EMPTY, "entity id {EMPTY} is reserved");
        self.reserve_one();
        match self.find(id.raw()) {
            Ok(i) => {
                self.slots[i].mark = mark;
                self.slots[i].last_seen = tick;
            }
            Err(i) => {
                self.slots[i] = Ghost { id: id.raw(), mark, last_seen: tick };
                self.len += 1;
            }
        }
    }

    /// Drops every ghost not a candidate within `grace` ticks of `tick`,
    /// appending each dropped id to `departed`.
    ///
    /// Nothing leaves this table unreported, so the caller can tell the client
    /// what it no longer holds. Cost is proportional to slots rather than to
    /// ghosts.
    ///
    /// One forward pass suffices. Backward-shift deletion moves an entry either
    /// to a slot at or after the removal point, which the cursor has yet to
    /// reach, or, once the probe scan has wrapped the end of the table, between
    /// two slots the cursor already passed and kept. `tick` and `grace` are
    /// fixed for the call, so an entry the cursor kept once it would keep again.
    ///
    /// Does not clear `departed` and does not shrink the allocation.
    pub fn evict(&mut self, tick: u32, grace: u32, departed: &mut Vec<EntityId>) {
        if self.slots.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.slots.len() {
            let g = self.slots[i];
            if g.id != EMPTY && tick.wrapping_sub(g.last_seen) > grace {
                departed.push(EntityId::from_raw(g.id));
                self.remove_at(i);
                // remove_at may have shifted an entry into slot i.
                continue;
            }
            i += 1;
        }
    }

    /// Forgets everything, as if the client had just arrived.
    pub fn clear(&mut self) {
        self.slots.fill(Ghost::VACANT);
        self.len = 0;
    }

    /// `Ok` at the entry, `Err` at the empty slot it would occupy.
    ///
    /// Terminates because the table is never more than half full.
    #[inline(always)]
    fn find(&self, id: u32) -> Result<usize, usize> {
        let bits = self.slots.len().trailing_zeros();
        let mask = self.slots.len() - 1;
        let mut i = home(id, bits);
        loop {
            let e = self.slots[i].id;
            if e == id {
                return Ok(i);
            }
            if e == EMPTY {
                return Err(i);
            }
            i = (i + 1) & mask;
        }
    }

    /// Knuth's backward-shift deletion, so no tombstone is left behind.
    fn remove_at(&mut self, at: usize) {
        let bits = self.slots.len().trailing_zeros();
        let mask = self.slots.len() - 1;
        let mut hole = at;
        self.slots[hole] = Ghost::VACANT;
        self.len -= 1;

        let mut j = (hole + 1) & mask;
        loop {
            let id = self.slots[j].id;
            if id == EMPTY {
                return;
            }
            // Entry j may fill the hole only if the hole lies on j's probe
            // path, which is the cyclic range from its home to j.
            let h = home(id, bits);
            if (j.wrapping_sub(h) & mask) >= (j.wrapping_sub(hole) & mask) {
                self.slots[hole] = self.slots[j];
                self.slots[j] = Ghost::VACANT;
                hole = j;
            }
            j = (j + 1) & mask;
        }
    }

    fn reserve_one(&mut self) {
        let needed = (self.len + 1) * 2;
        if self.slots.len() >= needed {
            return;
        }
        let mut cap = self.slots.len().max(MIN_SLOTS);
        while cap < needed {
            cap *= 2;
        }
        self.rehash(cap);
    }

    fn rehash(&mut self, cap: usize) {
        debug_assert!(cap.is_power_of_two());
        let old = core::mem::replace(&mut self.slots, vec![Ghost::VACANT; cap]);
        let bits = cap.trailing_zeros();
        let mask = cap - 1;
        for g in old.iter().filter(|g| g.id != EMPTY) {
            let mut i = home(g.id, bits);
            while self.slots[i].id != EMPTY {
                i = (i + 1) & mask;
            }
            self.slots[i] = *g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn id(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    /// `n` distinct entity ids, scattered so they do not arrive in table order.
    fn scatter(n: usize, seed: u64) -> Vec<u32> {
        let mut s = seed | 1;
        let mut seen = HashSet::new();
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let e = (s % 200_000) as u32;
            if seen.insert(e) {
                out.push(e);
            }
        }
        out
    }

    #[test]
    fn a_ghost_record_is_twelve_bytes() {
        assert_eq!(size_of::<Ghost>(), 12, "id, mark, and tick with no padding");
    }

    #[test]
    fn an_unknown_entity_has_no_mark() {
        let mut t = GhostTable::new();
        assert_eq!(t.mark(id(7)), None);
        assert_eq!(t.seen(id(7), 1), None);
        assert!(t.is_empty());
    }

    #[test]
    fn being_a_candidate_does_not_create_a_ghost() {
        let mut t = GhostTable::with_capacity(64);
        for tick in 0..10 {
            assert_eq!(t.seen(id(7), tick), None);
        }
        assert_eq!(t.len(), 0, "only a send creates a ghost");
    }

    #[test]
    fn a_sent_entity_reports_its_mark() {
        let mut t = GhostTable::new();
        t.sent(id(7), 4096, 1);
        assert_eq!(t.mark(id(7)), Some(4096));
        assert_eq!(t.seen(id(7), 2), Some(4096));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn sending_again_moves_the_mark_without_adding_a_ghost() {
        let mut t = GhostTable::new();
        t.sent(id(7), 4096, 1);
        t.sent(id(7), 9000, 2);
        assert_eq!(t.mark(id(7)), Some(9000));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn growth_preserves_every_ghost() {
        let mut t = GhostTable::new();
        let ids = scatter(1000, 0x51);
        for (n, &e) in ids.iter().enumerate() {
            t.sent(id(e), n as u32, 1);
        }
        assert_eq!(t.len(), ids.len());
        for (n, &e) in ids.iter().enumerate() {
            assert_eq!(t.mark(id(e)), Some(n as u32), "ghost {e} lost across growth");
        }
    }

    #[test]
    fn the_table_stays_at_or_below_half_full() {
        let mut t = GhostTable::new();
        for (n, &e) in scatter(500, 0x52).iter().enumerate() {
            t.sent(id(e), 0, 1);
            assert!(t.slots() >= (n + 1) * 2, "{} slots for {} ghosts", t.slots(), n + 1);
        }
    }

    #[test]
    fn eviction_reports_what_it_drops() {
        let mut t = GhostTable::new();
        t.sent(id(1), 0, 10);
        t.sent(id(2), 0, 10);
        t.sent(id(3), 0, 20);

        let mut gone = Vec::new();
        t.evict(20, 5, &mut gone);
        gone.sort_unstable();
        assert_eq!(gone, vec![id(1), id(2)]);
        assert_eq!(t.len(), 1);
        assert_eq!(t.mark(id(3)), Some(0));
        assert_eq!(t.mark(id(1)), None);
    }

    #[test]
    fn a_candidate_survives_eviction() {
        let mut t = GhostTable::new();
        t.sent(id(1), 0, 0);
        for tick in 1..100 {
            t.seen(id(1), tick);
        }
        let mut gone = Vec::new();
        t.evict(99, 5, &mut gone);
        assert!(gone.is_empty(), "a ghost seen every tick must not depart");
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn eviction_leaves_every_survivor_findable() {
        // Catches broken backward-shift deletion, and settles whether one
        // forward pass suffices. A small table makes a probe chain wrapping the
        // end of the table proportionally more likely, so both sizes run.
        //
        // Measured with temporary instrumentation: these 128 configurations
        // wrap the probe scan 24 times, so the case the single-pass argument
        // turns on is exercised rather than assumed.
        for &n in &[30usize, 600] {
            for seed in 0..64u64 {
                let ids = scatter(n, seed);
                let mut t = GhostTable::new();
                for (k, &e) in ids.iter().enumerate() {
                    // Two thirds go stale.
                    let tick = if k % 3 == 0 { 100 } else { 0 };
                    t.sent(id(e), k as u32, tick);
                }
                let live = ids.iter().enumerate().filter(|(k, _)| k % 3 == 0).count();

                let mut gone = Vec::new();
                t.evict(100, 5, &mut gone);

                assert_eq!(t.len(), live, "n {n} seed {seed}: wrong survivor count");
                assert_eq!(
                    gone.len(),
                    n - live,
                    "n {n} seed {seed}: wrong departure count"
                );
                for (k, &e) in ids.iter().enumerate() {
                    if k % 3 == 0 {
                        assert_eq!(
                            t.mark(id(e)),
                            Some(k as u32),
                            "n {n} seed {seed}: lost {e}"
                        );
                    } else {
                        assert_eq!(t.mark(id(e)), None, "n {n} seed {seed}: kept {e}");
                    }
                }
            }
        }
    }

    #[test]
    fn eviction_does_not_shrink_the_allocation() {
        let mut t = GhostTable::new();
        for &e in scatter(300, 0x53).iter() {
            t.sent(id(e), 0, 0);
        }
        let slots = t.slots();
        let mut gone = Vec::new();
        t.evict(100, 5, &mut gone);
        assert_eq!(t.len(), 0);
        assert_eq!(t.slots(), slots, "capacity is a high-water mark");
    }

    #[test]
    fn a_steady_candidate_set_stops_allocating() {
        let mut t = GhostTable::with_capacity(128);
        let ids = scatter(95, 0x54);
        for &e in &ids {
            t.sent(id(e), 0, 0);
        }
        let slots = t.slots();
        for tick in 1..500 {
            for &e in &ids {
                let m = t.seen(id(e), tick).expect("ghost present");
                t.sent(id(e), m + 1, tick);
            }
        }
        assert_eq!(t.slots(), slots, "steady state must not grow");
    }

    #[test]
    fn ghosts_can_be_re_added_after_departing() {
        let mut t = GhostTable::new();
        t.sent(id(42), 111, 0);
        let mut gone = Vec::new();
        t.evict(100, 5, &mut gone);
        assert_eq!(gone, vec![id(42)]);
        t.sent(id(42), 222, 100);
        assert_eq!(t.mark(id(42)), Some(222));
        assert_eq!(t.len(), 1);
    }
}
