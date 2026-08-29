//! Per-entity accumulated displacement.
//!
//! [`Odometer`] holds, for each entity slot, the sum of how far that entity has
//! moved between consecutive calls to [`Odometer::accumulate`]. It is the
//! staleness input to priority scoring: the difference between an entity's
//! reading now and its reading when a client was last told about it bounds how
//! far that client's copy of it has fallen behind.
//!
//! The value carries no direction and no rate. It is a sum of per-call
//! displacements, so calling at a different cadence yields a different number.
//! Velocity belongs to the consumer and does not appear here.

use crate::entity::{EntityId, LiveSet};
use crate::fixed::Fixed;

/// Accumulated displacement per entity slot.
///
/// Readings are in raw [`Fixed`] units and wrap. Difference two of them with
/// [`u32::wrapping_sub`]. A true difference of 2^32 units or more aliases;
/// computed, that is 4,194,304 m of path, about 29 hours at 40 m/s.
#[derive(Debug, Clone, Default)]
pub struct Odometer {
    total: Vec<u32>,
    prev_x: Vec<Fixed>,
    prev_y: Vec<Fixed>,
    prev_z: Vec<Fixed>,
}

impl Odometer {
    /// Empty.
    pub fn new() -> Odometer {
        Odometer::default()
    }

    /// Empty, with room for `slots` entities before it grows.
    pub fn with_capacity(slots: usize) -> Odometer {
        Odometer {
            total: Vec::with_capacity(slots),
            prev_x: Vec::with_capacity(slots),
            prev_y: Vec::with_capacity(slots),
            prev_z: Vec::with_capacity(slots),
        }
    }

    /// How many slots carry a reading.
    #[inline]
    pub fn slots(&self) -> usize {
        self.total.len()
    }

    /// The running total for one entity.
    ///
    /// # Panics
    ///
    /// If `id` names a slot this odometer has never covered.
    #[inline]
    pub fn reading(&self, id: EntityId) -> u32 {
        self.total[id.index()]
    }

    /// Every reading, indexed by entity id.
    #[inline]
    pub fn as_slice(&self) -> &[u32] {
        &self.total
    }

    /// Adds each live entity's displacement since the previous call.
    ///
    /// The three slices are parallel; entity id is the index into them, matching
    /// [`CellSnapshot::update`](crate::snapshot::CellSnapshot::update). Slots
    /// absent from `live` are left untouched, so a despawned entity's reading
    /// freezes.
    ///
    /// A slot seen for the first time is seeded from its current position and
    /// contributes nothing on that call.
    ///
    /// Displacement is `|dx| + |dy| + |dz|`, which needs no square root and
    /// over-estimates the Euclidean step by up to a factor of 1.73 depending on
    /// direction of travel. Nothing distinguishes a step from a teleport; both
    /// are measured, since both leave a client's copy equally wrong.
    ///
    /// Positions do not change within a tick, so a second call in the same tick
    /// adds nothing.
    ///
    /// Reusing an entity slot for a different entity is not supported: the new
    /// occupant's first call would accumulate its separation from the previous
    /// occupant's last position.
    ///
    /// Allocates only while slot capacity is growing.
    ///
    /// # Panics
    ///
    /// If the three slices differ in length, or if `live` covers fewer slots
    /// than the arrays hold.
    pub fn accumulate(
        &mut self,
        xs: &[Fixed],
        ys: &[Fixed],
        zs: &[Fixed],
        live: &LiveSet,
    ) {
        assert_eq!(xs.len(), ys.len(), "position arrays must be parallel");
        assert_eq!(xs.len(), zs.len(), "position arrays must be parallel");
        assert!(
            live.slots() >= xs.len(),
            "live set covers {} slots, position arrays hold {}",
            live.slots(),
            xs.len()
        );

        let slots = xs.len();
        if slots > self.total.len() {
            let old = self.total.len();
            self.total.resize(slots, 0);
            self.prev_x.resize(slots, Fixed::ZERO);
            self.prev_y.resize(slots, Fixed::ZERO);
            self.prev_z.resize(slots, Fixed::ZERO);
            // No prior observation, so the first call contributes nothing.
            self.prev_x[old..].copy_from_slice(&xs[old..]);
            self.prev_y[old..].copy_from_slice(&ys[old..]);
            self.prev_z[old..].copy_from_slice(&zs[old..]);
        }

        for id in live.iter() {
            let i = id.index();
            if i >= slots {
                break;
            }
            let step = xs[i]
                .raw()
                .abs_diff(self.prev_x[i].raw())
                .saturating_add(ys[i].raw().abs_diff(self.prev_y[i].raw()))
                .saturating_add(zs[i].raw().abs_diff(self.prev_z[i].raw()));
            self.total[i] = self.total[i].wrapping_add(step);
            self.prev_x[i] = xs[i];
            self.prev_y[i] = ys[i];
            self.prev_z[i] = zs[i];
        }
    }

    /// Forgets every entity.
    pub fn clear(&mut self) {
        self.total.clear();
        self.prev_x.clear();
        self.prev_y.clear();
        self.prev_z.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Position arrays and a live set, as `WorldSimulation` will hold them.
    struct World {
        xs: Vec<Fixed>,
        ys: Vec<Fixed>,
        zs: Vec<Fixed>,
        live: LiveSet,
    }

    impl World {
        fn new() -> World {
            World { xs: Vec::new(), ys: Vec::new(), zs: Vec::new(), live: LiveSet::new() }
        }

        fn spawn(&mut self, x: i32, y: i32, z: i32) -> EntityId {
            let id = EntityId::from_raw(self.xs.len() as u32);
            self.xs.push(Fixed::from_meters(x));
            self.ys.push(Fixed::from_meters(y));
            self.zs.push(Fixed::from_meters(z));
            self.live.insert(id);
            id
        }

        fn move_to(&mut self, id: EntityId, x: i32, y: i32, z: i32) {
            let i = id.index();
            self.xs[i] = Fixed::from_meters(x);
            self.ys[i] = Fixed::from_meters(y);
            self.zs[i] = Fixed::from_meters(z);
        }

        fn accumulate(&self, odo: &mut Odometer) {
            odo.accumulate(&self.xs, &self.ys, &self.zs, &self.live);
        }
    }

    /// A reading equivalent to `m` meters of path.
    fn meters(m: i32) -> u32 {
        Fixed::from_meters(m).raw() as u32
    }

    #[test]
    fn a_stationary_entity_accumulates_nothing() {
        let mut w = World::new();
        let a = w.spawn(100, 200, 5);
        let mut odo = Odometer::new();
        for _ in 0..50 {
            w.accumulate(&mut odo);
        }
        assert_eq!(odo.reading(a), 0, "an idle entity must never look stale");
    }

    #[test]
    fn a_new_slot_starts_at_zero() {
        let mut w = World::new();
        let a = w.spawn(1000, 1000, 0);
        let b = w.spawn(2000, 2000, 300);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        assert_eq!(odo.reading(a), 0);
        assert_eq!(odo.reading(b), 0);
    }

    #[test]
    fn a_straight_line_accumulates_its_path_length() {
        let mut w = World::new();
        let a = w.spawn(0, 0, 0);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        for m in 1..=10 {
            w.move_to(a, m, 0, 0);
            w.accumulate(&mut odo);
        }
        assert_eq!(odo.reading(a), meters(10));
    }

    #[test]
    fn displacement_is_manhattan_and_includes_height() {
        let mut w = World::new();
        let a = w.spawn(0, 0, 0);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        // A 3-4-12 step is 13 m of separation and 19 m by the approximation
        // in use. The cell grid is horizontal; this is not.
        w.move_to(a, 3, 4, 12);
        w.accumulate(&mut odo);
        assert_eq!(odo.reading(a), meters(19));
    }

    #[test]
    fn a_despawned_entity_stops_accumulating() {
        let mut w = World::new();
        let a = w.spawn(0, 0, 0);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        w.move_to(a, 5, 0, 0);
        w.accumulate(&mut odo);
        let frozen = odo.reading(a);

        w.live.remove(a);
        w.move_to(a, 900, 0, 0);
        w.accumulate(&mut odo);
        assert_eq!(odo.reading(a), frozen, "a dead slot must not accumulate");
    }

    #[test]
    fn growth_preserves_existing_readings() {
        let mut w = World::new();
        let a = w.spawn(0, 0, 0);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        w.move_to(a, 6, 0, 0);
        w.accumulate(&mut odo);
        assert_eq!(odo.reading(a), meters(6));

        let b = w.spawn(500, 500, 0);
        w.accumulate(&mut odo);
        assert_eq!(odo.reading(a), meters(6), "an existing reading must survive growth");
        assert_eq!(odo.reading(b), 0, "a new slot has no prior observation");
    }

    #[test]
    fn a_difference_survives_the_total_wrapping() {
        let mut w = World::new();
        let a = w.spawn(0, 0, 0);
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);

        let step_m = 1_000;
        let per_step = meters(step_m) as u64;
        let to_wrap = (u32::MAX as u64 / per_step) + 1;

        let mut k = 0u64;
        while k < to_wrap {
            w.move_to(a, if k % 2 == 0 { step_m } else { 0 }, 0, 0);
            w.accumulate(&mut odo);
            k += 1;
        }
        assert!(
            k * per_step > u32::MAX as u64,
            "the total must have wrapped or this proves nothing"
        );

        let mark = odo.reading(a);
        for _ in 0..7 {
            w.move_to(a, if k % 2 == 0 { step_m } else { 0 }, 0, 0);
            w.accumulate(&mut odo);
            k += 1;
        }
        assert_eq!(
            odo.reading(a).wrapping_sub(mark),
            (7 * per_step) as u32,
            "a difference below 2^32 stays exact after the total wraps"
        );
    }

    #[test]
    fn repeated_calls_stop_allocating() {
        let mut w = World::new();
        for i in 0..64 {
            w.spawn(i, i * 2, 0);
        }
        let mut odo = Odometer::new();
        w.accumulate(&mut odo);
        let ptr = odo.as_slice().as_ptr();
        for _ in 0..100 {
            w.accumulate(&mut odo);
        }
        assert_eq!(odo.as_slice().as_ptr(), ptr, "steady state must not reallocate");
    }
}
