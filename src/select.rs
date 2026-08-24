//! Scoring and budget selection.
//!
//! A viewer's ghost set is the nearest `ghost_cap` candidates. Within that set,
//! one pass scores each by how far the client's copy has drifted, one sort
//! ranks them, and the packet takes as many as fit.
//!
//! Relevance and staleness do different jobs here. Distance decides what a
//! client knows about, because it changes slowly and so the ghost set holds
//! still. Drift decides what is worth sending, because it resets every time
//! something is sent. Choosing the ghost set by drift instead churns it: the
//! entity just corrected becomes the least stale, falls out, and arrives again
//! as a stranger.

use crate::entity::EntityId;
use crate::fixed::DistSq;
use crate::gather::DiscoveredEntities;
use crate::ghost::GhostTable;
use crate::odometer::Odometer;

/// Bands in a weight table. `DistSq` wraps a `u64`, so `ilog2` cannot exceed 63.
pub const BANDS: usize = 64;

/// Drift an entity the client cannot see is treated as carrying.
///
/// The natural scale is the view radius: a client told nothing about an entity
/// could be wrong about it by anything up to the whole radius. 262,144 raw
/// units is 256 m, the default. Raising it introduces strangers sooner at the
/// cost of refreshing known entities less often.
pub const DEFAULT_UNSEEN_DRIFT: u32 = 262_144;

/// Marks a ranked entry as one the client holds no ghost of. Candidate lists
/// are bounded by the walk cap, so the top bit of an index is free.
const NEW_BIT: u32 = 1 << 31;

/// Relevance weight per squared-distance band. This is the growth curve, held
/// as data so a sweep is a table change rather than a code change.
///
/// Bands are squared distance, so one band is a factor of the square root of
/// two in separation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Weights([u16; BANDS]);

impl Weights {
    /// # Panics
    ///
    /// If any weight is zero. A zero weight stops that band's scores from ever
    /// growing, which starves every entity in it, and starvation being
    /// structurally impossible is the property this design exists for.
    pub fn new(table: [u16; BANDS]) -> Weights {
        assert!(table.iter().all(|&w| w != 0), "a zero weight starves its band");
        Weights(table)
    }

    /// Halves every four bands. A band is `ilog2` of a squared separation, so
    /// it is half a doubling of distance, which makes this proportional to
    /// `d^-0.5` rather than to `d^-1`.
    ///
    /// A placeholder until measurement picks a curve; see open question 2 in
    /// the design document. Note that it is indexed from band 0 while anything
    /// inside a 256 m view radius falls between bands 20 and 35, so only that
    /// upper stretch of the table is ever read.
    pub fn placeholder() -> Weights {
        let mut t = [0u16; BANDS];
        for (b, slot) in t.iter_mut().enumerate() {
            *slot = 1u16 << (12 - (b / 4).min(12));
        }
        Weights::new(t)
    }

    #[inline(always)]
    fn at(&self, band: usize) -> u32 {
        self.0[band] as u32
    }
}

impl Default for Weights {
    fn default() -> Weights {
        Weights::placeholder()
    }
}

/// Per-viewer replication policy, constant across ticks.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// Entities a viewer's client knows about: the nearest this many
    /// candidates. Bounds the ghost table's footprint, which the benchmarks
    /// measure as the dominant per-viewer cost.
    ///
    /// Gathering more candidates than this discards the excess, so a walk cap
    /// above it is wasted work.
    pub ghost_cap: usize,
    /// Ticks a ghost survives after leaving the ghost set. Absorbs a viewer
    /// jittering across the boundary rather than making it a departure and an
    /// arrival.
    pub grace: u32,
    /// See [`DEFAULT_UNSEEN_DRIFT`].
    pub unseen_drift: u32,
    pub weights: Weights,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            ghost_cap: 0,
            grace: 0,
            unseen_drift: DEFAULT_UNSEEN_DRIFT,
            weights: Weights::placeholder(),
        }
    }
}

/// One scored member of a viewer's ghost set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranked {
    score: u32,
    at: u32,
}

impl Ranked {
    /// Index into the [`DiscoveredEntities`] this selection was made from.
    /// Means nothing against any other list.
    #[inline(always)]
    pub fn index(&self) -> usize {
        (self.at & !NEW_BIT) as usize
    }

    /// Whether the client holds no ghost of this entity yet, so the record is
    /// an introduction rather than an update.
    #[inline(always)]
    pub fn is_new(&self) -> bool {
        self.at & NEW_BIT != 0
    }

    #[inline(always)]
    pub fn score(&self) -> u32 {
        self.score
    }
}

/// What one viewer gets this tick.
///
/// Held per worker thread and reused across viewers. 128-byte alignment for the
/// same reason [`DiscoveredEntities`] has it.
#[repr(align(128))]
#[derive(Debug, Clone, Default)]
pub struct Selection {
    ranked: Vec<Ranked>,
    departed: Vec<EntityId>,
    sent: usize,
}

impl Selection {
    pub fn new() -> Selection {
        Selection::default()
    }

    pub fn with_capacity(candidates: usize) -> Selection {
        Selection {
            ranked: Vec::with_capacity(candidates),
            departed: Vec::with_capacity(candidates),
            sent: 0,
        }
    }

    /// The viewer's ghost set, ranked. Highest score first.
    #[inline(always)]
    pub fn ranked(&self) -> &[Ranked] {
        &self.ranked
    }

    /// The records that fit this tick's packet.
    ///
    /// Never includes an update that scored zero, since an entity whose client
    /// copy is already correct has nothing to send.
    #[inline(always)]
    pub fn records(&self) -> &[Ranked] {
        &self.ranked[..self.sent]
    }

    /// Ghosts the client should drop.
    #[inline(always)]
    pub fn departed(&self) -> &[EntityId] {
        &self.departed
    }

    /// Empties the buffers without releasing their allocations.
    pub fn clear(&mut self) {
        self.ranked.clear();
        self.departed.clear();
        self.sent = 0;
    }
}

/// Scores a viewer's ghost set, ranks it, and commits the result to `ghosts`.
///
/// `slots` is how many records fit this tick's packet.
///
/// The ghost set is the leading `ghost_cap` of `candidates`. The gather walks
/// outward from the viewer, so that is the nearest `ghost_cap` entities, and it
/// holds still from tick to tick.
///
/// Every member of the ghost set is stamped, so only an entity that has left
/// the set ages out and departs. An update that scored zero consumes no slot.
///
/// Ties break on candidate index, which the gather's walk order fixes, so a
/// replay ranks identically.
///
/// Allocates only while the buffers are growing.
pub fn select(
    tick: u32,
    candidates: &DiscoveredEntities,
    odometer: &Odometer,
    policy: &Policy,
    slots: usize,
    ghosts: &mut GhostTable,
    out: &mut Selection,
) {
    out.clear();
    let cands = candidates.as_slice();
    let n = cands.len().min(policy.ghost_cap);

    for (k, e) in cands[..n].iter().enumerate() {
        let (drift, flag) = match ghosts.mark(e.id) {
            Some(mark) => (odometer.reading(e.id).wrapping_sub(mark), 0),
            None => (policy.unseen_drift, NEW_BIT),
        };
        out.ranked.push(Ranked {
            score: score_of(drift, e.dist_sq, &policy.weights),
            at: k as u32 | flag,
        });
    }

    out.ranked.sort_unstable_by(|a, b| b.score.cmp(&a.score).then(a.at.cmp(&b.at)));

    // Scores descend, so the entries worth sending are a prefix.
    let ceiling = out.ranked.len().min(slots);
    out.sent = out.ranked[..ceiling].partition_point(|r| r.score > 0);

    for r in &out.ranked[..out.sent] {
        let id = cands[r.index()].id;
        ghosts.sent(id, odometer.reading(id), tick);
    }
    for r in &out.ranked[out.sent..] {
        ghosts.seen(cands[r.index()].id, tick);
    }

    ghosts.evict(tick, policy.grace, &mut out.departed);
}

/// `drift x weight(band)`, saturating.
#[inline(always)]
fn score_of(drift: u32, dist_sq: DistSq, w: &Weights) -> u32 {
    drift.saturating_mul(w.at(band_of(dist_sq)))
}

/// `| 1` guards `ilog2` against a viewer scoring its own entity at zero
/// separation, which the gather does not exclude.
#[inline(always)]
fn band_of(dist_sq: DistSq) -> usize {
    (dist_sq.raw() | 1).ilog2() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::LiveSet;
    use crate::fixed::Fixed;
    use crate::gather::DiscoveredEntity;

    /// Positions and an odometer, driven as `WorldSimulation` will drive them.
    struct World {
        xs: Vec<Fixed>,
        ys: Vec<Fixed>,
        zs: Vec<Fixed>,
        live: LiveSet,
        odo: Odometer,
    }

    impl World {
        fn new(n: usize) -> World {
            let mut live = LiveSet::with_capacity(n);
            for i in 0..n {
                live.insert(EntityId::from_raw(i as u32));
            }
            let mut w = World {
                xs: vec![Fixed::ZERO; n],
                ys: vec![Fixed::ZERO; n],
                zs: vec![Fixed::ZERO; n],
                live,
                odo: Odometer::with_capacity(n),
            };
            w.tick();
            w
        }

        fn walk(&mut self, id: EntityId, m: i32) {
            self.xs[id.index()] += Fixed::from_meters(m);
        }

        fn walk_all_but(&mut self, skip: u32, m: i32) {
            for i in 0..self.xs.len() {
                if i as u32 != skip {
                    self.xs[i] += Fixed::from_meters(m);
                }
            }
        }

        fn tick(&mut self) {
            self.odo.accumulate(&self.xs, &self.ys, &self.zs, &self.live);
        }
    }

    /// Candidates as `(entity id, separation in metres)`, in walk order.
    fn candidates(items: &[(u32, i32)]) -> DiscoveredEntities {
        let mut d = DiscoveredEntities::new();
        for (k, &(id, m)) in items.iter().enumerate() {
            d.push(DiscoveredEntity::new(
                EntityId::from_raw(id),
                k as u32,
                DistSq::from_radius(Fixed::from_meters(m)),
            ));
        }
        d
    }

    /// One candidate per entity, entity `i` at `1 + i` metres, so entity 0 is
    /// nearest and wins the first slot.
    fn ladder(n: usize) -> DiscoveredEntities {
        let items: Vec<(u32, i32)> = (0..n).map(|i| (i as u32, 1 + i as i32)).collect();
        candidates(&items)
    }

    fn policy(ghost_cap: usize, grace: u32) -> Policy {
        Policy { ghost_cap, grace, ..Policy::default() }
    }

    fn id_at(c: &DiscoveredEntities, r: &Ranked) -> u32 {
        c.as_slice()[r.index()].id.raw()
    }

    // -- the two properties the whole design exists for ---------------------

    #[test]
    fn an_idle_entity_stops_being_sent() {
        let n = 200;
        let mut w = World::new(n);
        let cands = ladder(n);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        for tick in 1..=10u32 {
            w.walk_all_but(0, 1);
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        }
        assert!(ghosts.mark(EntityId::from_raw(0)).is_some(), "entity 0 must be a ghost");

        for tick in 11..=30u32 {
            w.walk_all_but(0, 1);
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
            assert!(
                !sel.records().iter().any(|r| id_at(&cands, r) == 0),
                "tick {tick}: an entity that has not moved must not consume a slot"
            );
        }
    }

    #[test]
    fn a_stationary_crowd_sends_nothing_after_warmup() {
        let n = 50;
        let mut w = World::new(n);
        let cands = ladder(n);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(sel.records().len(), n, "every entity is new on the first tick");

        for tick in 2..=20u32 {
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
            assert!(sel.records().is_empty(), "tick {tick}: nothing moved, so nothing to say");
            assert_eq!(sel.ranked().len(), n, "the ghosts are still correct, not gone");
        }
    }

    // -- ordering -----------------------------------------------------------

    #[test]
    fn a_near_stranger_outranks_a_slightly_stale_neighbour() {
        let mut w = World::new(2);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        let known = candidates(&[(0, 5)]);
        select(1, &known, &w.odo, &p, 98, &mut ghosts, &mut sel);
        w.walk(EntityId::from_raw(0), 1);
        w.tick();

        let both = candidates(&[(0, 5), (1, 5)]);
        select(2, &both, &w.odo, &p, 98, &mut ghosts, &mut sel);
        let first = sel.ranked()[0];
        assert!(first.is_new(), "an unseen neighbour beats a barely stale one");
        assert_eq!(id_at(&both, &first), 1);
    }

    #[test]
    fn a_distant_stranger_loses_to_a_badly_stale_neighbour() {
        // The half of the fix that stops the ghost set churning: an entity the
        // client has never seen no longer outranks everything unconditionally.
        let mut w = World::new(2);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        let known = candidates(&[(0, 5)]);
        select(1, &known, &w.odo, &p, 98, &mut ghosts, &mut sel);
        w.walk(EntityId::from_raw(0), 200);
        w.tick();

        let both = candidates(&[(0, 5), (1, 250)]);
        select(2, &both, &w.odo, &p, 98, &mut ghosts, &mut sel);
        let first = sel.ranked()[0];
        assert!(!first.is_new(), "a badly stale neighbour beats a distant stranger");
        assert_eq!(id_at(&both, &first), 0);
    }

    #[test]
    fn new_entities_are_ordered_nearest_first() {
        let w = World::new(3);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        // Presented far-to-near, so walk order cannot be what sorts them.
        let cands = candidates(&[(0, 200), (1, 40), (2, 5)]);
        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        let order: Vec<u32> = sel.ranked().iter().map(|r| id_at(&cands, r)).collect();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn at_equal_drift_the_nearer_entity_wins() {
        let mut w = World::new(2);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        let cands = candidates(&[(0, 200), (1, 5)]);
        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        w.walk(EntityId::from_raw(0), 7);
        w.walk(EntityId::from_raw(1), 7);
        w.tick();

        select(2, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(id_at(&cands, &sel.ranked()[0]), 1, "same drift, so distance decides");
    }

    #[test]
    fn an_unseen_entity_scores_like_a_ghost_that_drifted_the_view_radius() {
        let w = Weights::placeholder();
        let near = DistSq::from_radius(Fixed::from_meters(5));
        assert_eq!(
            score_of(DEFAULT_UNSEEN_DRIFT, near, &w),
            score_of(Fixed::from_meters(256).raw() as u32, near, &w),
            "the unseen default is the view radius, on the same scale as drift"
        );
    }

    // -- the caps -----------------------------------------------------------

    #[test]
    fn the_ghost_set_is_capped() {
        let w = World::new(300);
        let cands = ladder(300);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(64, 0);

        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(sel.ranked().len(), 64);
        assert_eq!(ghosts.len(), 64, "grace of zero makes the cap exact");
    }

    #[test]
    fn a_ghost_that_leaves_the_set_departs_after_grace() {
        let mut w = World::new(3);
        // Entity 2 is nearest at first, then falls behind the other two.
        let inside = candidates(&[(2, 5), (0, 6), (1, 7)]);
        let outside = candidates(&[(0, 6), (1, 7), (2, 8)]);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(2, 2);

        select(1, &inside, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert!(ghosts.mark(EntityId::from_raw(2)).is_some(), "entity 2 starts as a ghost");

        let mut gone = false;
        for tick in 2..=10u32 {
            w.tick();
            select(tick, &outside, &w.odo, &p, 98, &mut ghosts, &mut sel);
            gone |= sel.departed().contains(&EntityId::from_raw(2));
        }
        assert!(gone, "a ghost that leaves the set must be reported as departed");
    }

    #[test]
    fn a_binding_ghost_cap_does_not_churn() {
        // The failure the whole-pipeline benchmark found: with more candidates
        // than ghost slots, the set thrashed and every packet was first
        // sightings. Settled, departures and introductions must both stop.
        let n = 600;
        let mut w = World::new(n);
        let cands = ladder(n);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(256, 3);

        for tick in 1..=40u32 {
            for i in 0..n {
                w.walk(EntityId::from_raw(i as u32), 1);
            }
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        }
        assert_eq!(ghosts.len(), 256, "the ghost set is the nearest 256 and stays so");

        for tick in 41..=60u32 {
            for i in 0..n {
                w.walk(EntityId::from_raw(i as u32), 1);
            }
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
            assert!(sel.departed().is_empty(), "tick {tick}: a stable set must not depart");
            assert!(
                !sel.records().iter().any(|r| r.is_new()),
                "tick {tick}: nothing should still be arriving"
            );
        }
    }

    #[test]
    fn everything_is_sent_when_candidates_fit_the_budget() {
        let mut w = World::new(95);
        let cands = ladder(95);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(sel.records().len(), 95, "no selection pressure at 95 against 98");

        for i in 0..95 {
            w.walk(EntityId::from_raw(i), 3);
        }
        w.tick();
        select(2, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(sel.records().len(), 95, "everyone moved and everyone fits");
    }

    #[test]
    fn nothing_exceeds_the_packet() {
        let mut w = World::new(300);
        let cands = ladder(300);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);

        for tick in 1..=10u32 {
            for i in 0..300 {
                w.walk(EntityId::from_raw(i), 2);
            }
            w.tick();
            select(tick, &cands, &w.odo, &p, 37, &mut ghosts, &mut sel);
            assert!(sel.records().len() <= 37, "tick {tick} overran the packet");
        }
    }

    // -- mechanics ----------------------------------------------------------

    #[test]
    fn ranking_is_deterministic() {
        let mut w = World::new(120);
        let cands = ladder(120);
        let p = policy(1024, 1000);

        let run = |w: &World| {
            let mut ghosts = GhostTable::new();
            let mut sel = Selection::new();
            select(1, &cands, &w.odo, &p, 40, &mut ghosts, &mut sel);
            sel.ranked().to_vec()
        };
        assert_eq!(run(&w), run(&w), "identical input must rank identically");

        for i in 0..120 {
            w.walk(EntityId::from_raw(i), (i % 5) as i32);
        }
        w.tick();
        assert_eq!(run(&w), run(&w), "including ties, which are broken on walk order");
    }

    #[test]
    fn a_viewer_scoring_its_own_entity_does_not_panic() {
        let w = World::new(2);
        let cands = candidates(&[(0, 0), (1, 10)]);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::new();
        let p = policy(1024, 1000);
        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        assert_eq!(sel.ranked().len(), 2);
    }

    #[test]
    fn reuse_across_viewers_does_not_reallocate() {
        let mut w = World::new(300);
        let cands = ladder(300);
        let mut ghosts = GhostTable::new();
        let mut sel = Selection::with_capacity(512);
        let p = policy(256, 3);

        for i in 0..300 {
            w.walk(EntityId::from_raw(i), 1);
        }
        w.tick();
        select(1, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        let ptr = sel.ranked().as_ptr();

        for tick in 2..=100u32 {
            for i in 0..300 {
                w.walk(EntityId::from_raw(i), 1);
            }
            w.tick();
            select(tick, &cands, &w.odo, &p, 98, &mut ghosts, &mut sel);
        }
        assert_eq!(sel.ranked().as_ptr(), ptr, "steady state must not reallocate");
    }

    #[test]
    fn a_zero_weight_is_refused() {
        let mut t = [1u16; BANDS];
        t[7] = 0;
        assert!(std::panic::catch_unwind(|| Weights::new(t)).is_err());
    }
}
