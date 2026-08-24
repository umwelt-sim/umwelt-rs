//! The tick loop and the storage it owns.
//!
//! [`WorldSimulation`] owns entity positions, liveness, the odometer, the
//! cell-ordered snapshot, and per-viewer replication state. A consumer supplies
//! a [`Game`] and gets a single-tick entry point.
//!
//! One tick is: the game moves entities, the odometer observes how far they
//! went, the snapshot is rebuilt in cell order, and then every due viewer is
//! subscribed, gathered, scored and selected against that snapshot.
//!
//! Packet assembly and transport are not here. A tick stops at a [`Selection`]
//! per viewer, which [`tick_with`](WorldSimulation::tick_with) hands out.

use crate::budget::PacketBudget;
use crate::codec::RecordCodec;
use crate::config::WorldConfig;
use crate::entity::{EntityId, LiveSet};
use crate::fixed::Fixed;
use crate::gather::DiscoveredEntities;
use crate::ghost::GhostTable;
use crate::odometer::Odometer;
use crate::pos::Pos3;
use crate::select::{Policy, Selection, select};
use crate::sim::viewer::{ClientLimits, Viewer, ViewerId};
use crate::snapshot::CellSnapshot;
use crate::subscription::Subscription;

/// Entities a viewer's client knows about: the nearest this many. A
/// placeholder; see the ghost cap sweep in the design document.
pub const DEFAULT_GHOST_CAP: usize = 256;

/// Entities a viewer's gather examines before it stops.
///
/// Matches the ghost cap, because selection keeps the nearest `ghost_cap`
/// candidates and discards the rest unscored, so gathering past it is measured
/// waste. They stay separate parameters of
/// [`with_replication`](WorldSimulation::with_replication) only so they can be
/// swept apart.
pub const DEFAULT_WALK_CAP: usize = DEFAULT_GHOST_CAP;

/// Ticks a ghost survives after leaving the ghost set. A placeholder.
pub const DEFAULT_GRACE: u32 = 3;

/// The consumer's game, called once per tick.
pub trait Game {
    /// Moves entities, spawns and despawns. Everything that is not position is
    /// the consumer's own storage, keyed by [`EntityId`].
    fn step(&mut self, world: &mut Step<'_>);
}

/// What a [`Game`] may do during one tick.
pub struct Step<'a> {
    xs: &'a mut Vec<Fixed>,
    ys: &'a mut Vec<Fixed>,
    zs: &'a mut Vec<Fixed>,
    live: &'a mut LiveSet,
    cfg: &'a WorldConfig,
    tick: u32,
}

impl Step<'_> {
    #[inline(always)]
    pub fn tick(&self) -> u32 {
        self.tick
    }

    #[inline(always)]
    pub fn config(&self) -> &WorldConfig {
        self.cfg
    }

    /// Every slot ever allocated, live or not. Entity id is the index.
    #[inline(always)]
    pub fn slots(&self) -> usize {
        self.xs.len()
    }

    #[inline(always)]
    pub fn live(&self) -> &LiveSet {
        self.live
    }

    /// The position arrays, to be moved in place. Struct of arrays, so there is
    /// no per-tick marshaling pass.
    #[inline(always)]
    pub fn positions_mut(&mut self) -> (&mut [Fixed], &mut [Fixed], &mut [Fixed]) {
        (self.xs.as_mut_slice(), self.ys.as_mut_slice(), self.zs.as_mut_slice())
    }

    /// Appends an entity. Slots are never reused, so the id is new.
    pub fn spawn(&mut self, pos: Pos3) -> EntityId {
        debug_assert!(self.cfg.contains(pos), "spawned outside the region at {pos:?}");
        let id = EntityId::from_raw(self.xs.len() as u32);
        self.xs.push(pos.x);
        self.ys.push(pos.y);
        self.zs.push(pos.z);
        self.live.insert(id);
        id
    }

    /// Removes an entity from the snapshot. The slot is not reclaimed.
    pub fn despawn(&mut self, id: EntityId) {
        self.live.remove(id);
    }
}

/// What one tick did, for instrumentation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickStats {
    /// Viewers served. Excludes those not due this tick and those whose avatar
    /// is dead or outside the region.
    pub viewers: u64,
    /// Candidates the gathers produced, before scoring.
    pub candidates: u64,
    /// Records that fit packets.
    pub records: u64,
    /// Records that told a client about an entity for the first time.
    pub new_ghosts: u64,
    /// Ghosts clients were told to drop.
    pub departed: u64,
}

impl TickStats {
    /// Candidates that were gathered and then lost to the budget.
    pub fn dropped_by_budget(&self) -> u64 {
        self.candidates.saturating_sub(self.records)
    }
}

/// One region's simulation.
pub struct WorldSimulation<G: Game> {
    cfg: WorldConfig,
    game: G,

    xs: Vec<Fixed>,
    ys: Vec<Fixed>,
    zs: Vec<Fixed>,
    live: LiveSet,

    odo: Odometer,
    snap: CellSnapshot,

    viewers: Vec<Viewer>,
    free: Vec<ViewerId>,

    /// Per-worker scratch. Single-threaded for now, so there is one of each.
    found: DiscoveredEntities,
    selection: Selection,

    walk_cap: usize,
    policy: Policy,
    codec: RecordCodec,
    tick: u32,
}

impl<G: Game> WorldSimulation<G> {
    pub fn new(cfg: WorldConfig, game: G) -> WorldSimulation<G> {
        WorldSimulation::with_replication(
            cfg,
            game,
            DEFAULT_WALK_CAP,
            Policy {
                ghost_cap: DEFAULT_GHOST_CAP,
                grace: DEFAULT_GRACE,
                // A client told nothing about an entity could be wrong about it
                // by up to the whole view radius.
                unseen_drift: cfg.horizontal_view_radius().raw() as u32,
                weights: crate::select::Weights::placeholder(),
            },
        )
    }

    /// Both parameters are placeholders in `new`, so this exists to sweep them.
    pub fn with_replication(
        cfg: WorldConfig,
        game: G,
        walk_cap: usize,
        policy: Policy,
    ) -> WorldSimulation<G> {
        let codec = RecordCodec::new(&cfg);
        let snap = CellSnapshot::new(&cfg);
        let overshoot = snap.sub_threshold() as usize;
        WorldSimulation {
            cfg,
            game,
            xs: Vec::new(),
            ys: Vec::new(),
            zs: Vec::new(),
            live: LiveSet::new(),
            odo: Odometer::new(),
            snap,
            viewers: Vec::new(),
            free: Vec::new(),
            // Sized so a tick does not grow them. The gather checks its cap at
            // cell boundaries and a cell below the subdivision threshold is
            // walked whole, so the overshoot is bounded by that threshold
            // rather than by a sub-cell.
            found: DiscoveredEntities::with_capacity(walk_cap + overshoot),
            selection: Selection::with_capacity(policy.ghost_cap),
            walk_cap,
            policy,
            codec,
            tick: 0,
        }
    }

    #[inline(always)]
    pub fn config(&self) -> &WorldConfig {
        &self.cfg
    }

    #[inline(always)]
    pub fn tick_count(&self) -> u32 {
        self.tick
    }

    #[inline(always)]
    pub fn game(&self) -> &G {
        &self.game
    }

    #[inline(always)]
    pub fn snapshot(&self) -> &CellSnapshot {
        &self.snap
    }

    #[inline(always)]
    pub fn odometer(&self) -> &Odometer {
        &self.odo
    }

    /// Entities currently alive.
    #[inline(always)]
    pub fn entity_count(&self) -> usize {
        self.live.live()
    }

    /// Viewer slots ever allocated, registered or not.
    #[inline(always)]
    pub fn viewer_slots(&self) -> usize {
        self.viewers.len()
    }

    /// Ghosts one viewer's client currently holds.
    pub fn ghost_count(&self, v: ViewerId) -> usize {
        self.viewers[v.index()].ghosts.len()
    }

    /// Registers a logical client against the entity it controls.
    ///
    /// Nothing here opens a connection. The edge maps the returned id to a
    /// socket; the simulation never sees one.
    pub fn register_viewer(&mut self, avatar: EntityId, limits: ClientLimits) -> ViewerId {
        let budget = PacketBudget::new(&self.codec, limits.payload_bytes);
        if let Some(id) = self.free.pop() {
            self.viewers[id.index()].reset(avatar, budget, limits.send_period);
            return id;
        }
        let id = ViewerId::from_raw(self.viewers.len() as u32);
        self.viewers.push(Viewer {
            avatar,
            sub: None,
            ghosts: GhostTable::with_capacity(self.policy.ghost_cap),
            budget,
            send_period: limits.send_period.max(1),
            registered: true,
        });
        id
    }

    /// Drops a client. The id may be handed to a later client, whose ghost set
    /// starts empty.
    pub fn unregister_viewer(&mut self, v: ViewerId) {
        let viewer = &mut self.viewers[v.index()];
        if !viewer.registered {
            return;
        }
        viewer.registered = false;
        viewer.sub = None;
        // Retains the table's allocation for whoever takes the slot next.
        viewer.ghosts.clear();
        self.free.push(v);
    }

    /// Advances one tick, discarding the per-viewer selections.
    pub fn tick(&mut self) -> TickStats {
        self.tick_with(|_, _, _| {})
    }

    /// Advances one tick, handing each served viewer's selection to `on_viewer`
    /// before the buffers are reused.
    ///
    /// The [`Selection`] and [`DiscoveredEntities`] passed in are scratch that
    /// the next viewer overwrites, so anything worth keeping must be copied.
    /// Packet assembly attaches here.
    pub fn tick_with(
        &mut self,
        mut on_viewer: impl FnMut(ViewerId, &Selection, &DiscoveredEntities),
    ) -> TickStats {
        self.tick = self.tick.wrapping_add(1);

        {
            let mut step = Step {
                xs: &mut self.xs,
                ys: &mut self.ys,
                zs: &mut self.zs,
                live: &mut self.live,
                cfg: &self.cfg,
                tick: self.tick,
            };
            self.game.step(&mut step);
        }

        self.odo.accumulate(&self.xs, &self.ys, &self.zs, &self.live);
        self.snap.update(&self.xs, &self.ys, &self.zs, &self.live);

        let mut stats = TickStats::default();
        for i in 0..self.viewers.len() {
            let id = ViewerId::from_raw(i as u32);
            if !self.serve(id, &mut stats) {
                continue;
            }
            on_viewer(id, &self.selection, &self.found);
        }
        stats
    }

    /// Subscribes, gathers, scores and selects for one viewer. Returns whether
    /// it was served.
    fn serve(&mut self, id: ViewerId, stats: &mut TickStats) -> bool {
        let i = id.index();
        if !self.viewers[i].registered || !self.viewers[i].due(id, self.tick) {
            return false;
        }

        let avatar = self.viewers[i].avatar;
        if !self.live.contains(avatar) {
            return false;
        }
        let at = Pos3::new(self.xs[avatar.index()], self.ys[avatar.index()], self.zs[avatar.index()]);
        if !self.cfg.contains(at) {
            return false;
        }

        let sub = Subscription::at_center(&self.cfg, self.cfg.cell_of(at.horizontal()));
        self.viewers[i].sub = Some(sub);

        self.found.clear();
        self.snap.gather_into_capped(at, sub, self.walk_cap, &mut self.found);

        // No event queue exists yet, so nothing is held back for one.
        let slots = self.viewers[i].budget.slots(0);
        select(
            self.tick,
            &self.found,
            &self.odo,
            &self.policy,
            slots,
            &mut self.viewers[i].ghosts,
            &mut self.selection,
        );

        stats.viewers += 1;
        stats.candidates += self.found.len() as u64;
        stats.records += self.selection.records().len() as u64;
        stats.new_ghosts += self.selection.records().iter().filter(|r| r.is_new()).count() as u64;
        stats.departed += self.selection.departed().len() as u64;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entity walks along x, wrapping inside the region so nothing ever
    /// leaves it.
    struct Walk {
        per_tick: i32,
        frozen: Vec<EntityId>,
    }

    impl Walk {
        fn new(per_tick: i32) -> Walk {
            Walk { per_tick, frozen: Vec::new() }
        }
        fn still() -> Walk {
            Walk { per_tick: 0, frozen: Vec::new() }
        }
    }

    impl Game for Walk {
        fn step(&mut self, w: &mut Step<'_>) {
            if self.per_tick == 0 {
                return;
            }
            let extent = w.config().region_size().raw();
            let d = Fixed::from_meters(self.per_tick).raw();
            let frozen = self.frozen.clone();
            let (xs, _, _) = w.positions_mut();
            for (i, x) in xs.iter_mut().enumerate() {
                if frozen.contains(&EntityId::from_raw(i as u32)) {
                    continue;
                }
                *x = Fixed::from_raw((x.raw() + d) % extent);
            }
        }
    }

    /// `n` entities on a grid around the middle of the region, close enough
    /// together that a viewer among them sees many of them.
    fn populate<G: Game>(sim: &mut WorldSimulation<G>, n: usize) -> Vec<EntityId> {
        let mut ids = Vec::with_capacity(n);
        let mut step = Step {
            xs: &mut sim.xs,
            ys: &mut sim.ys,
            zs: &mut sim.zs,
            live: &mut sim.live,
            cfg: &sim.cfg,
            tick: 0,
        };
        let side = (n as f64).sqrt().ceil() as i32;
        for k in 0..n as i32 {
            let x = 2048 + (k % side) * 4;
            let y = 2048 + (k / side) * 4;
            ids.push(step.spawn(Pos3::from_meters(x, y, 0)));
        }
        ids
    }

    fn sim(game: Walk) -> WorldSimulation<Walk> {
        WorldSimulation::new(WorldConfig::default(), game)
    }

    // -- the tick -----------------------------------------------------------

    #[test]
    fn a_tick_publishes_a_snapshot_of_the_live_entities() {
        let mut s = sim(Walk::new(1));
        populate(&mut s, 40);
        s.tick();
        assert_eq!(s.tick_count(), 1);
        assert_eq!(s.snapshot().len(), 40);
        assert_eq!(s.entity_count(), 40);
    }

    #[test]
    fn a_registered_viewer_receives_records() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits::default());

        let stats = s.tick();
        assert_eq!(stats.viewers, 1);
        assert!(stats.candidates > 0, "the viewer should see its neighbours");
        assert!(stats.records > 0);
        assert_eq!(stats.records, stats.new_ghosts, "every record is a first sighting");
    }

    #[test]
    fn an_unregistered_viewer_is_skipped() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 20);
        let v = s.register_viewer(ids[0], ClientLimits::default());
        assert_eq!(s.tick().viewers, 1);
        s.unregister_viewer(v);
        assert_eq!(s.tick().viewers, 0);
        assert_eq!(s.ghost_count(v), 0, "unregistering drops the ghost set");
    }

    #[test]
    fn a_viewer_whose_avatar_despawns_is_skipped() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 20);
        s.register_viewer(ids[0], ClientLimits::default());
        assert_eq!(s.tick().viewers, 1);
        s.live.remove(ids[0]);
        assert_eq!(s.tick().viewers, 0);
    }

    // -- the property the whole phase exists for ----------------------------

    #[test]
    fn a_still_world_stops_sending() {
        let mut s = sim(Walk::still());
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits::default());

        let first = s.tick();
        assert!(first.records > 0, "everything is new on the first tick");

        // Whatever did not fit the first packet is still new, so give the
        // backlog a few ticks to drain.
        for _ in 0..5 {
            s.tick();
        }
        for _ in 0..20 {
            let stats = s.tick();
            assert_eq!(stats.viewers, 1, "the viewer is still being served");
            assert_eq!(stats.records, 0, "nothing moved, so there is nothing to say");
        }
    }

    #[test]
    fn a_moving_world_keeps_sending() {
        let mut s = sim(Walk::new(2));
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits::default());
        for _ in 0..6 {
            s.tick();
        }
        let stats = s.tick();
        assert!(stats.records > 0, "moving entities must keep earning slots");
    }

    #[test]
    fn one_still_entity_in_a_moving_world_stops_costing_slots() {
        let mut game = Walk::new(2);
        let mut s = sim(Walk::still());
        let ids = populate(&mut s, 60);
        // Entity 1 never moves; entity 0 is the viewer.
        game.frozen.push(ids[1]);
        s.game = game;
        let v = s.register_viewer(ids[0], ClientLimits::default());

        let still = ids[1];
        let mut ever_sent = false;
        for _ in 0..10 {
            s.tick_with(|_, sel, found| {
                ever_sent |=
                    sel.records().iter().any(|r| found.as_slice()[r.index()].id == still);
            });
        }
        assert!(s.ghost_count(v) > 0);
        assert!(ever_sent, "the still entity must become a ghost, or the rest proves nothing");

        for _ in 0..15 {
            let mut seen = false;
            s.tick_with(|_, sel, found| {
                seen = sel.records().iter().any(|r| found.as_slice()[r.index()].id == still);
            });
            assert!(!seen, "a still entity must not take a slot once its client copy is right");
        }
    }

    // -- cadence and instrumentation ----------------------------------------

    #[test]
    fn send_period_spreads_viewers_across_ticks() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 40);
        for id in ids.iter().take(8) {
            s.register_viewer(*id, ClientLimits { payload_bytes: 1200, send_period: 4 });
        }
        let mut served = 0u64;
        let mut per_tick = Vec::new();
        for _ in 0..4 {
            let stats = s.tick();
            per_tick.push(stats.viewers);
            served += stats.viewers;
        }
        assert_eq!(served, 8, "every viewer is served exactly once per period");
        assert!(per_tick.iter().all(|&v| v == 2), "load should spread, got {per_tick:?}");
    }

    #[test]
    fn stats_account_for_what_the_budget_dropped() {
        let mut s = sim(Walk::new(2));
        let ids = populate(&mut s, 400);
        s.register_viewer(ids[0], ClientLimits::default());
        let stats = s.tick();
        assert!(stats.candidates > stats.records, "400 entities do not fit one packet");
        assert_eq!(stats.dropped_by_budget(), stats.candidates - stats.records);
    }

    #[test]
    fn spawns_reach_replication_on_the_next_tick() {
        let mut s = sim(Walk::still());
        let ids = populate(&mut s, 10);
        s.register_viewer(ids[0], ClientLimits::default());
        for _ in 0..5 {
            s.tick();
        }
        assert_eq!(s.tick().records, 0, "settled");

        let mut step = Step {
            xs: &mut s.xs,
            ys: &mut s.ys,
            zs: &mut s.zs,
            live: &mut s.live,
            cfg: &s.cfg,
            tick: 0,
        };
        step.spawn(Pos3::from_meters(2050, 2050, 0));

        let stats = s.tick();
        assert_eq!(stats.records, 1, "a newcomer is a status change");
        assert_eq!(stats.new_ghosts, 1);
    }

    #[test]
    fn viewer_ids_are_reused_with_an_empty_ghost_set() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 30);
        let a = s.register_viewer(ids[0], ClientLimits::default());
        for _ in 0..4 {
            s.tick();
        }
        assert!(s.ghost_count(a) > 0);
        s.unregister_viewer(a);

        let b = s.register_viewer(ids[1], ClientLimits::default());
        assert_eq!(a, b, "the slot is handed on");
        assert_eq!(s.ghost_count(b), 0, "a new client knows nothing");
        assert_eq!(s.viewer_slots(), 1);
    }

    #[test]
    fn ticks_stop_allocating_once_the_world_is_steady() {
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 600);
        for id in ids.iter().take(20) {
            s.register_viewer(*id, ClientLimits::default());
        }
        for _ in 0..20 {
            s.tick();
        }
        let found = s.found.capacity();
        let snap = s.snapshot().len();
        let ghosts: Vec<usize> =
            (0..20).map(|i| s.viewers[i].ghosts.slots()).collect();

        for _ in 0..50 {
            s.tick();
        }
        assert_eq!(s.found.capacity(), found, "the gather buffer must not grow");
        assert_eq!(s.snapshot().len(), snap);
        let after: Vec<usize> = (0..20).map(|i| s.viewers[i].ghosts.slots()).collect();
        assert_eq!(after, ghosts, "ghost tables must reach a steady size");
    }
}
