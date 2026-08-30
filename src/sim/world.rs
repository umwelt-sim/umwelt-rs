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
//! Each served viewer's [`Selection`] is then assembled into a payload and
//! handed to the [`PayloadSink`] the consumer attached, after
//! [`tick_with`](WorldSimulation::tick_with) shows both to its observer.
//! Viewers are partitioned across worker threads.
//!
//! Not here: the clock that drives the tick, events, and any transport past the
//! sink.

use std::time::Duration;

use crate::budget::PacketBudget;
use crate::codec::RecordCodec;
use crate::config::WorldConfig;
use crate::entity::{EntityId, LiveIter, LiveSet};
use crate::fixed::Fixed;
use crate::game::Game;
use crate::gather::DiscoveredEntities;
use crate::ghost::GhostTable;
use crate::odometer::Odometer;
use crate::packet::{DESPAWN_BYTES, PacketWriter};
use crate::pos::Pos3;
use crate::select::{Policy, Selection, select};
use crate::sim::sink::{NullSink, PayloadSink};
use crate::sim::viewer::{ClientLimits, Viewer, ViewerId};
use crate::snapshot::CellSnapshot;
use crate::subscription::Subscription;

/// Entities a viewer's client knows about: the nearest this many.
///
/// Below about 160 a client's packet does not fill, since only a ghost that
/// moved consumes a slot. Above 256 every ghost is refreshed less often and
/// error grows in every distance band. Quality is flat between the two, and
/// this sits at the top of that range. The sweep behind those figures is
/// §Quality harness in the design document.
pub const DEFAULT_GHOST_CAP: usize = 256;

/// Entities a viewer's gather examines before it stops.
///
/// Matches the ghost cap, because selection keeps the nearest `ghost_cap`
/// candidates and discards the rest unscored, so gathering past it is measured
/// waste. They stay separate parameters of
/// [`with_replication`](WorldSimulation::with_replication) only so they can be
/// swept apart.
pub const DEFAULT_WALK_CAP: usize = DEFAULT_GHOST_CAP;

/// Ticks a ghost survives after leaving the ghost set.
///
/// One tick absorbs a rank flapping across the edge of the set without keeping
/// anything longer, and gives the least client-side error of any value swept.
/// Larger values trade error for churn at a worsening rate. The sweep is
/// §Quality harness in the design document.
///
/// A ghost is only aged when its viewer is served, so a grace below a client's
/// [`send_period`](ClientLimits::send_period) behaves as zero.
pub const DEFAULT_GRACE: u32 = 1;

/// What a [`Game`] may do during one tick.
pub struct Step<'a> {
    xs: &'a mut Vec<Fixed>,
    ys: &'a mut Vec<Fixed>,
    zs: &'a mut Vec<Fixed>,
    live: &'a mut LiveSet,
    /// Ids despawned this tick, whoever asked. Recorded because a despawn the
    /// game performs is otherwise invisible to anything outside the game, and
    /// something has to tell the edge that owned it.
    despawned: &'a mut Vec<EntityId>,
    cfg: &'a WorldConfig,
    tick: u32,
}

impl Step<'_> {
    /// Which tick is running.
    #[inline]
    pub fn tick(&self) -> u32 {
        self.tick
    }

    /// The world this runs in.
    #[inline]
    pub fn config(&self) -> &WorldConfig {
        self.cfg
    }

    /// How many entities are alive.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.live.live()
    }

    /// Whether that entity is alive. A despawned id is not.
    #[inline]
    pub fn contains(&self, id: EntityId) -> bool {
        self.live.contains(id)
    }

    /// Every live entity, in ascending id order.
    ///
    /// Holds the whole `Step` borrowed for as long as the iterator lives, so
    /// collect the ids first if the loop body has to move anything.
    #[inline]
    pub fn entities(&self) -> LiveIter<'_> {
        self.live.iter()
    }

    /// Where an entity is, or `None` if it is not alive.
    #[inline]
    pub fn position(&self, id: EntityId) -> Option<Pos3> {
        if !self.live.contains(id) {
            return None;
        }
        let at = id.index();
        Some(Pos3::new(self.xs[at], self.ys[at], self.zs[at]))
    }

    /// Moves an entity. An id that is not alive is ignored.
    pub fn move_to(&mut self, id: EntityId, pos: Pos3) {
        debug_assert!(self.cfg.contains(pos), "moved outside the region to {pos:?}");
        if !self.live.contains(id) {
            return;
        }
        let at = id.index();
        self.xs[at] = pos.x;
        self.ys[at] = pos.y;
        self.zs[at] = pos.z;
    }

    /// Offsets an entity, saturating at the bounds of [`Fixed`] on each axis.
    /// An id that is not alive is ignored.
    pub fn translate(&mut self, id: EntityId, dx: Fixed, dy: Fixed, dz: Fixed) {
        let Some(at) = self.position(id) else { return };
        self.move_to(
            id,
            Pos3::new(
                at.x.saturating_add(dx),
                at.y.saturating_add(dy),
                at.z.saturating_add(dz),
            ),
        );
    }

    /// Every position, to be moved in place. Struct of arrays, so a sweep over
    /// the whole world costs no marshaling pass.
    ///
    /// This is the bulk path, and the slices are indexed by
    /// [`EntityId::index`]. They cover despawned entities too, so a sweep that
    /// must skip those needs [`contains`](Self::contains). Prefer
    /// [`move_to`](Self::move_to) and [`translate`](Self::translate) for
    /// anything that names its entity: they bounds-check the destination and
    /// never touch a despawned one.
    #[inline]
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
        if self.live.contains(id) {
            self.live.remove(id);
            self.despawned.push(id);
        }
    }
}

/// What one tick did, for instrumentation.
///
/// Deliberately not `PartialEq`: `sink_nanos` is a wall-clock measurement, so
/// two runs of identical work do not produce equal stats and comparing them
/// would look meaningful while never being true.
#[derive(Clone, Copy, Debug, Default)]
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
    /// Despawn records actually written, which lag departures by a tick.
    pub despawns_sent: u64,
    /// Viewers whose subscription box moved this tick.
    ///
    /// A box is the cells within `cell_radius` of the viewer's own cell, so it
    /// only moves when the viewer crosses a cell boundary. Against
    /// [`viewers`](Self::viewers) it is the share of served viewers that
    /// changed which cells they subscribe to, which is what a cross-region or
    /// edge-side subscription protocol would have to carry.
    pub subs_changed: u64,
    /// Payload bytes assembled.
    pub bytes: u64,
    /// Nanoseconds spent inside [`PayloadSink::send`], summed across workers.
    ///
    /// Wall clock, so it is the one field here that does not reproduce.
    ///
    /// Summed, not wall clock: divide by
    /// [`thread_count`](WorldSimulation::thread_count) for what it cost the
    /// tick. A sink is the one part of a tick the library does not control, and
    /// safe Rust cannot preempt one that blocks, so the most that can be done
    /// is to say plainly how much of the tick it took.
    pub sink_nanos: u64,
}

impl TickStats {
    /// Candidates that were gathered and then lost to the budget.
    pub fn dropped_by_budget(&self) -> u64 {
        self.candidates.saturating_sub(self.records)
    }

    fn merge(&mut self, o: TickStats) {
        self.viewers += o.viewers;
        self.candidates += o.candidates;
        self.records += o.records;
        self.new_ghosts += o.new_ghosts;
        self.departed += o.departed;
        self.despawns_sent += o.despawns_sent;
        self.subs_changed += o.subs_changed;
        self.bytes += o.bytes;
        self.sink_nanos += o.sink_nanos;
    }
}

/// One worker thread's scratch.
///
/// `DiscoveredEntities` and `Selection` are each 128-byte aligned, so adjacent
/// workers cannot share a cache line however these are stored.
#[derive(Debug)]
struct Scratch {
    found: DiscoveredEntities,
    selection: Selection,
    writer: PacketWriter,
    /// Despawns drained from a viewer's queue for this packet.
    despawns: Vec<EntityId>,
    stats: TickStats,
}

/// Everything a worker reads and nothing it writes, so it is shared across
/// threads by reference.
struct Frame<'a, S: PayloadSink> {
    sink: &'a S,
    tick: u32,
    cfg: &'a WorldConfig,
    snap: &'a CellSnapshot,
    odo: &'a Odometer,
    live: &'a LiveSet,
    xs: &'a [Fixed],
    ys: &'a [Fixed],
    zs: &'a [Fixed],
    policy: &'a Policy,
    walk_cap: usize,
}

/// Subscribes, gathers, scores, selects and assembles for one viewer. Returns
/// whether it was served, leaving the payload in `w.writer`.
fn serve<S: PayloadSink>(
    f: &Frame<'_, S>,
    id: ViewerId,
    v: &mut Viewer,
    w: &mut Scratch,
    on_viewer: &(impl Fn(Outbound<'_>) + Sync),
) -> bool {
    if !v.registered || !v.due(id, f.tick) {
        return false;
    }
    let avatar = v.avatar;
    if !f.live.contains(avatar) {
        return false;
    }
    let i = avatar.index();
    let at = Pos3::new(f.xs[i], f.ys[i], f.zs[i]);
    if !f.cfg.contains(at) {
        return false;
    }

    let sub = Subscription::at_center(f.cfg, f.cfg.cell_of(at.horizontal()));
    // A box only moves when its viewer crosses a cell boundary, so this counts
    // crossings rather than motion.
    if v.sub != Some(sub) {
        w.stats.subs_changed += 1;
    }
    v.sub = Some(sub);

    w.found.clear();
    f.snap.gather_into_capped(at, sub, f.walk_cap, &mut w.found);

    // No event queue exists yet, so nothing is held back for one.
    let state = v.budget.state_bytes_available(0);
    // Despawns go first but take at most half the payload, so a viewer whose
    // ghost set turned over cannot spend a whole packet forgetting things.
    let room = (state / 2) / DESPAWN_BYTES;
    let taking = v.pending_despawns.len().min(room);
    w.despawns.clear();
    w.despawns.extend(v.pending_despawns.drain(..taking));
    let slots = state.saturating_sub(taking * DESPAWN_BYTES) / v.budget.record_bytes();

    let Scratch { found, selection, writer, despawns, stats } = w;
    select(f.tick, found, f.odo, f.policy, slots, &mut v.ghosts, selection);

    v.sequence = v.sequence.wrapping_add(1);
    let cands = found.as_slice();
    let snap = f.snap;
    writer.build(
        f.tick,
        v.sequence,
        despawns,
        selection.records().iter().map(|r| {
            let e = cands[r.index()];
            (e.id, snap.pos_at(e.snapshot_index as usize))
        }),
    );

    stats.bytes += writer.payload().len() as u64;

    // Observers see the payload before it changes hands, since after the
    // handoff the writer holds a spare and not this payload.
    on_viewer(Outbound {
        viewer: id,
        bytes: writer.payload(),
        selection,
        candidates: found,
    });

    let handoff = std::time::Instant::now();
    f.sink.send(id, writer.payload());
    stats.sink_nanos += handoff.elapsed().as_nanos() as u64;

    // Departures are found by the eviction inside `select`, after this tick's
    // records were chosen, so they ride the next packet.
    v.pending_despawns.extend_from_slice(selection.departed());

    stats.viewers += 1;
    stats.candidates += found.len() as u64;
    stats.records += selection.records().len() as u64;
    stats.new_ghosts += selection.records().iter().filter(|r| r.is_new()).count() as u64;
    stats.departed += selection.departed().len() as u64;
    stats.despawns_sent += despawns.len() as u64;
    true
}

/// One viewer's finished work for a tick.
pub struct Outbound<'a> {
    /// Whose work this was.
    pub viewer: ViewerId,
    /// The assembled payload. Scratch that the next viewer overwrites.
    pub bytes: &'a [u8],
    /// What was chosen, ranked. The payload was assembled from it.
    pub selection: &'a Selection,
    /// Everything the gather found, chosen or not.
    pub candidates: &'a DiscoveredEntities,
}

/// What the ticks since somebody last looked cost.
///
/// Only the pacing loop knows how late a tick was and whether a deadline was
/// skipped, and only the simulation knows how many viewers it served. So the
/// simulation accumulates both, and whatever publishes a heartbeat drains it.
///
/// Not public: the only thing that can fill one is the pacing loop, and the
/// only thing that drains one is the heartbeat. A consumer handed the type
/// could do neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TickSpan {
    pub ticks: u32,
    /// Viewers served, summed over the span rather than averaged, so a reader
    /// can divide by whatever window it cares about.
    pub viewers: u64,
    /// Time inside those ticks, summed.
    pub spent: Duration,
    pub worst: Duration,
    /// Ticks that started after their deadline.
    pub late: u32,
    /// Deadlines skipped under [`Overrun::Drop`](crate::Overrun).
    pub dropped: u32,
}

impl TickSpan {
    /// Folds another span into this one. Worst takes the larger; everything
    /// else adds.
    pub(crate) fn merge(&mut self, o: TickSpan) {
        self.ticks += o.ticks;
        self.viewers += o.viewers;
        self.spent += o.spent;
        self.worst = self.worst.max(o.worst);
        self.late += o.late;
        self.dropped += o.dropped;
    }

    /// Mean time inside a tick over the span, or zero if it holds no ticks.
    pub(crate) fn mean(&self) -> Duration {
        self.spent.checked_div(self.ticks).unwrap_or_default()
    }
}

/// One region's simulation.
///
/// Holds the entities, runs the tick, and does every viewer's replication work
/// against the snapshot each tick rebuilds. A consumer supplies a [`Game`] and
/// calls [`tick`](Self::tick), or hands the whole loop over to
/// [`run`](Self::run).
///
/// ```
/// use umwelt::{ClientLimits, EntityId, Fixed, Game, Pos3, Step};
/// use umwelt::{WorldConfig, WorldSimulation};
///
/// /// Fills the region on its first tick, then drifts everything north.
/// #[derive(Default)]
/// struct Crowd {
///     arrived: bool,
///     watcher: Option<EntityId>,
/// }
///
/// impl Game for Crowd {
///     fn step(&mut self, world: &mut Step<'_>) {
///         if !self.arrived {
///             self.arrived = true;
///             self.watcher = Some(world.spawn(Pos3::from_meters(2048, 2048, 0)));
///             for n in 0..64 {
///                 world.spawn(Pos3::from_meters(2048 + n, 2048, 0));
///             }
///             return;
///         }
///         // A tenth of a meter a tick, in place, with no marshaling pass.
///         let step = Fixed::from_raw(102);
///         let (_, ys, _) = world.positions_mut();
///         for y in ys {
///             *y = y.saturating_add(step);
///         }
///     }
/// }
///
/// let mut sim = WorldSimulation::new(WorldConfig::default(), Crowd::default());
/// sim.tick();
///
/// // A viewer is an entity somebody is looking out of. Entities exist whether
/// // or not anyone watches them; only a viewer costs replication work.
/// let watcher = sim.game().watcher.expect("spawned on the first tick");
/// sim.register_viewer(watcher, ClientLimits::default());
///
/// let stats = sim.tick();
/// assert_eq!(stats.viewers, 1);
/// assert!(stats.records > 0, "the crowd is inside the view radius");
/// ```
pub struct WorldSimulation<G: Game, S: PayloadSink = NullSink> {
    cfg: WorldConfig,
    game: G,
    sink: S,

    xs: Vec<Fixed>,
    ys: Vec<Fixed>,
    zs: Vec<Fixed>,
    live: LiveSet,
    /// Cleared at the start of every tick and filled by [`Step::despawn`].
    despawned: Vec<EntityId>,

    odo: Odometer,
    snap: CellSnapshot,

    viewers: Vec<Viewer>,
    free: Vec<ViewerId>,

    /// One per worker thread.
    workers: Vec<Scratch>,
    threads: usize,

    walk_cap: usize,
    policy: Policy,
    codec: RecordCodec,
    tick: u32,
    /// Accumulated by the pacing loop, drained by whoever reports load.
    span: TickSpan,
}

impl<G: Game> WorldSimulation<G, NullSink> {
    /// Discards payloads. Attach one with
    /// [`with_sink`](WorldSimulation::with_sink).
    pub fn new(cfg: WorldConfig, game: G) -> WorldSimulation<G, NullSink> {
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
                weights: crate::select::Weights::inverse_distance(),
            },
        )
    }

    /// Both parameters are placeholders in `new`, so this exists to sweep them.
    pub fn with_replication(
        cfg: WorldConfig,
        game: G,
        walk_cap: usize,
        policy: Policy,
    ) -> WorldSimulation<G, NullSink> {
        let codec = RecordCodec::new(&cfg);
        let snap = CellSnapshot::new(&cfg);
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let mut sim = WorldSimulation {
            cfg,
            game,
            sink: NullSink,
            xs: Vec::new(),
            ys: Vec::new(),
            zs: Vec::new(),
            live: LiveSet::new(),
            despawned: Vec::new(),
            odo: Odometer::new(),
            snap,
            viewers: Vec::new(),
            free: Vec::new(),
            workers: Vec::new(),
            threads: 1,
            walk_cap,
            policy,
            codec,
            tick: 0,
            span: TickSpan::default(),
        };
        sim.set_thread_count(threads);
        sim
    }
}

impl<G: Game, S: PayloadSink> WorldSimulation<G, S> {
    /// Sends finished payloads to `sink` instead of wherever they were going.
    ///
    /// Consuming rather than a setter so the sink's type is inferred and no
    /// other constructor has to name it:
    ///
    /// ```ignore
    /// let sim = WorldSimulation::new(cfg,
    /// game).with_sink(EdgeSink::connect(addr)?);
    /// ```
    pub fn with_sink<T: PayloadSink>(self, sink: T) -> WorldSimulation<G, T> {
        WorldSimulation {
            cfg: self.cfg,
            game: self.game,
            sink,
            xs: self.xs,
            ys: self.ys,
            zs: self.zs,
            live: self.live,
            despawned: self.despawned,
            odo: self.odo,
            snap: self.snap,
            viewers: self.viewers,
            free: self.free,
            workers: self.workers,
            threads: self.threads,
            walk_cap: self.walk_cap,
            policy: self.policy,
            codec: self.codec,
            tick: self.tick,
            span: self.span,
        }
    }

    /// Where payloads are going.
    #[inline]
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Worker threads used for per-viewer replication.
    ///
    /// Defaults to [`std::thread::available_parallelism`]. It is configurable
    /// because the right number is not obvious: hyperthreading or efficiency
    /// cores help an evenly spread region and hurt a crowded one.
    #[inline]
    pub fn thread_count(&self) -> usize {
        self.threads
    }

    /// Allocates scratch for `n` workers. Clamped to at least one.
    ///
    /// Each worker's buffers are sized so a tick does not grow them. The gather
    /// checks its cap at cell boundaries and a cell below the subdivision
    /// threshold is walked whole, so the overshoot is bounded by that threshold
    /// rather than by a sub-cell.
    pub fn set_thread_count(&mut self, n: usize) {
        self.threads = n.max(1);
        let overshoot = self.snap.sub_threshold() as usize;
        let cap = self.walk_cap + overshoot;
        let ghosts = self.policy.ghost_cap;
        let codec = self.codec.clone();
        let payload = crate::budget::DEFAULT_PAYLOAD_BYTES as usize;
        self.workers.resize_with(self.threads, || Scratch {
            found: DiscoveredEntities::with_capacity(cap),
            selection: Selection::with_capacity(ghosts),
            writer: PacketWriter::new(codec.clone(), payload),
            despawns: Vec::with_capacity(ghosts),
            stats: TickStats::default(),
        });
    }

    /// The world this simulation runs in.
    #[inline]
    pub fn config(&self) -> &WorldConfig {
        &self.cfg
    }

    /// Ticks run so far.
    #[inline]
    pub fn tick_count(&self) -> u32 {
        self.tick
    }

    /// The consumer's game.
    #[inline]
    pub fn game(&self) -> &G {
        &self.game
    }

    /// The cell-ordered view the last tick was served from.
    #[doc(hidden)]
    #[inline]
    pub fn snapshot(&self) -> &CellSnapshot {
        &self.snap
    }

    /// How far each entity has moved since it was last sent.
    #[doc(hidden)]
    #[inline]
    pub fn odometer(&self) -> &Odometer {
        &self.odo
    }

    /// Entities despawned during the last tick, whoever asked for it.
    ///
    /// Cleared at the start of each tick, so this holds the last tick's only. A
    /// despawn the consumer's game performs is otherwise invisible to anything
    /// outside the game, and this is how a caller learns of one in time to act
    /// between ticks.
    #[inline]
    pub fn despawned(&self) -> &[EntityId] {
        &self.despawned
    }

    /// Entities currently alive.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.live.live()
    }

    /// Every entity slot ever allocated, live or not.
    ///
    /// Despawn clears a liveness bit and does not reclaim the slot, so this
    /// climbs with churn while [`entity_count`](Self::entity_count) does not.
    /// See §Slot growth under churn.
    #[inline]
    pub fn slots(&self) -> usize {
        self.xs.len()
    }

    /// Records one tick against the span. Called by the pacing loop.
    pub(crate) fn record_tick(
        &mut self,
        took: Duration,
        late: bool,
        dropped: u32,
        viewers: u64,
    ) {
        self.span.ticks += 1;
        self.span.viewers += viewers;
        self.span.spent += took;
        self.span.worst = self.span.worst.max(took);
        self.span.late += u32::from(late);
        self.span.dropped += dropped;
    }

    /// Takes the span and starts a new one.
    ///
    /// Not public: whatever publishes a heartbeat drains this, and a second
    /// caller would silently take half the numbers away from the first.
    pub(crate) fn take_span(&mut self) -> TickSpan {
        std::mem::take(&mut self.span)
    }

    /// Where one entity is, or `None` if it is not live.
    ///
    /// Positions are stored by entity id, so this is a direct index. It answers
    /// "where is entity N" for the simulation's own arrays; the snapshot, which
    /// is ordered by cell, still cannot.
    #[inline]
    pub fn position(&self, id: EntityId) -> Option<Pos3> {
        if !self.live.contains(id) {
            return None;
        }
        let i = id.index();
        Some(Pos3::new(self.xs[i], self.ys[i], self.zs[i]))
    }

    /// Viewer slots ever allocated, registered or not.
    #[inline]
    pub fn viewer_slots(&self) -> usize {
        self.viewers.len()
    }

    /// The entity a viewer controls, or `None` if the id is not registered.
    ///
    /// Exists so a caller never has to reconstruct the mapping itself. Doing
    /// that means passing a `ViewerId` through a raw `u32` into an `EntityId`,
    /// which the two newtypes are there to prevent.
    pub fn avatar_of(&self, v: ViewerId) -> Option<EntityId> {
        let viewer = self.viewers.get(v.index())?;
        viewer.registered.then_some(viewer.avatar)
    }

    /// Ghosts one viewer's client currently holds.
    pub fn ghost_count(&self, v: ViewerId) -> usize {
        self.viewers[v.index()].ghosts.len()
    }

    /// Registers a logical client against the entity it controls.
    ///
    /// Nothing here opens a connection. The edge maps the returned id to a
    /// socket; the simulation never sees one.
    pub fn register_viewer(
        &mut self,
        avatar: EntityId,
        limits: ClientLimits,
    ) -> ViewerId {
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
            pending_despawns: Vec::new(),
            sequence: 0,
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
        viewer.pending_despawns.clear();
        // Retains the table's allocation for whoever takes the slot next.
        viewer.ghosts.clear();
        self.free.push(v);
    }

    /// Advances one tick, discarding the per-viewer selections.
    pub fn tick(&mut self) -> TickStats {
        self.tick_with(&|_| {})
    }

    /// Advances one tick, handing each served viewer's selection to `on_viewer`
    /// before the buffers are reused.
    ///
    /// The [`Selection`] and [`DiscoveredEntities`] passed in are scratch that
    /// the next viewer overwrites, so anything worth keeping must be copied.
    /// Packet assembly attaches here.
    ///
    /// Viewers are partitioned across [`thread_count`](Self::thread_count)
    /// workers, so `on_viewer` is called from several threads at once and must
    /// be `Sync`.
    #[doc(hidden)]
    pub fn tick_with(&mut self, on_viewer: &(impl Fn(Outbound<'_>) + Sync)) -> TickStats {
        self.tick = self.tick.wrapping_add(1);

        {
            self.despawned.clear();
            let mut step = Step {
                xs: &mut self.xs,
                ys: &mut self.ys,
                zs: &mut self.zs,
                live: &mut self.live,
                despawned: &mut self.despawned,
                cfg: &self.cfg,
                tick: self.tick,
            };
            self.game.step(&mut step);
        }

        self.odo.accumulate(&self.xs, &self.ys, &self.zs, &self.live);
        self.snap.update(&self.xs, &self.ys, &self.zs, &self.live);

        let frame = Frame {
            sink: &self.sink,
            tick: self.tick,
            cfg: &self.cfg,
            snap: &self.snap,
            odo: &self.odo,
            live: &self.live,
            xs: &self.xs,
            ys: &self.ys,
            zs: &self.zs,
            policy: &self.policy,
            walk_cap: self.walk_cap,
        };

        let viewers = &mut self.viewers;
        let workers = &mut self.workers;
        for w in workers.iter_mut() {
            w.stats = TickStats::default();
        }
        if viewers.is_empty() {
            return TickStats::default();
        }

        // One worker is the common case in tests and the cheap case for a small
        // region, and scoped threads cost a spawn each per tick.
        let threads = self.threads.min(viewers.len());
        if threads <= 1 {
            let w = &mut workers[0];
            for (k, v) in viewers.iter_mut().enumerate() {
                serve(&frame, ViewerId::from_raw(k as u32), v, w, on_viewer);
            }
            return w.stats;
        }

        let chunk = viewers.len().div_ceil(threads);
        std::thread::scope(|scope| {
            for (c, (vs, w)) in
                viewers.chunks_mut(chunk).zip(workers.iter_mut()).enumerate()
            {
                let frame = &frame;
                let base = c * chunk;
                scope.spawn(move || {
                    for (k, v) in vs.iter_mut().enumerate() {
                        serve(
                            frame,
                            ViewerId::from_raw((base + k) as u32),
                            v,
                            w,
                            on_viewer,
                        );
                    }
                });
            }
        });

        let mut stats = TickStats::default();
        for w in workers.iter() {
            stats.merge(w.stats);
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::sim::sink::RecordingSink;

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
    fn populate<G: Game, S: PayloadSink>(
        sim: &mut WorldSimulation<G, S>,
        n: usize,
    ) -> Vec<EntityId> {
        let mut ids = Vec::with_capacity(n);
        let mut step = Step {
            xs: &mut sim.xs,
            ys: &mut sim.ys,
            zs: &mut sim.zs,
            live: &mut sim.live,
            despawned: &mut sim.despawned,
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
        assert!(stats.candidates > 0, "the viewer should see its neighbors");
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
        let hit = AtomicBool::new(false);
        let watch = |o: Outbound<'_>| {
            if o.selection
                .records()
                .iter()
                .any(|r| o.candidates.as_slice()[r.index()].id == still)
            {
                hit.store(true, Ordering::Relaxed);
            }
        };

        for _ in 0..10 {
            s.tick_with(&watch);
        }
        assert!(s.ghost_count(v) > 0);
        assert!(
            hit.load(Ordering::Relaxed),
            "the still entity must become a ghost, or the rest proves nothing"
        );

        for tick in 0..15 {
            hit.store(false, Ordering::Relaxed);
            s.tick_with(&watch);
            assert!(
                !hit.load(Ordering::Relaxed),
                "tick {tick}: a still entity must not take a slot once its copy is right"
            );
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
            despawned: &mut s.despawned,
            cfg: &s.cfg,
            tick: 0,
        };
        step.spawn(Pos3::from_meters(2050, 2050, 0));

        let stats = s.tick();
        assert_eq!(stats.records, 1, "a newcomer is a status change");
        assert_eq!(stats.new_ghosts, 1);
    }

    #[test]
    fn a_payload_carries_exactly_what_was_selected() {
        use crate::packet::TickObservation;
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 400);
        s.set_thread_count(1);
        s.register_viewer(ids[0], ClientLimits::default());
        for _ in 0..8 {
            s.tick();
        }

        let codec = RecordCodec::new(&WorldConfig::default());
        let checked = AtomicBool::new(false);
        s.tick_with(&|o: Outbound<'_>| {
            let r = TickObservation::new(&codec, o.bytes).expect("well formed payload");
            let want: Vec<EntityId> = o
                .selection
                .records()
                .iter()
                .map(|k| o.candidates.as_slice()[k.index()].id)
                .collect();
            let got: Vec<EntityId> = r.updates().map(|(id, _)| id).collect();
            assert_eq!(got, want, "a payload must carry the selection, in order");
            assert_eq!(r.header().updates as usize, want.len());
            assert!(!want.is_empty());
            checked.store(true, Ordering::Relaxed);
        });
        assert!(checked.load(Ordering::Relaxed), "the viewer must have been served");
    }

    #[test]
    fn a_payload_never_exceeds_the_declared_payload_size() {
        let mut s = sim(Walk::new(2));
        let ids = populate(&mut s, 800);
        for id in ids.iter().take(30) {
            s.register_viewer(*id, ClientLimits::default());
        }
        for _ in 0..12 {
            let over = AtomicBool::new(false);
            s.tick_with(&|o: Outbound<'_>| {
                if o.bytes.len() > crate::budget::DEFAULT_PAYLOAD_BYTES as usize {
                    over.store(true, Ordering::Relaxed);
                }
            });
            assert!(!over.load(Ordering::Relaxed), "a payload overran its budget");
        }
    }

    #[test]
    fn quantized_positions_survive_the_round_trip() {
        use crate::packet::TickObservation;
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 60);
        s.set_thread_count(1);
        s.register_viewer(ids[0], ClientLimits::default());
        for _ in 0..4 {
            s.tick();
        }

        let codec = RecordCodec::new(&WorldConfig::default());
        let seen: Mutex<Vec<(EntityId, Pos3)>> = Mutex::new(Vec::new());
        s.tick_with(&|o: Outbound<'_>| {
            let r = TickObservation::new(&codec, o.bytes).expect("well formed");
            *seen.lock().unwrap() = r.updates().collect();
        });
        let seen = seen.into_inner().unwrap();
        assert!(!seen.is_empty());
        for (id, pos) in seen {
            assert_eq!(Some(pos), s.position(id), "the wire is lossless at this config");
        }
    }

    #[test]
    fn a_sink_receives_a_payload_per_served_viewer() {
        use crate::packet::TickObservation;
        let mut s = sim(Walk::new(1)).with_sink(RecordingSink::new());
        let ids = populate(&mut s, 200);
        let a = s.register_viewer(ids[0], ClientLimits::default());
        let b = s.register_viewer(ids[1], ClientLimits::default());

        let stats = s.tick();
        assert_eq!(stats.viewers, 2);
        assert_eq!(s.sink().sends(), 2, "one payload per served viewer");

        let codec = RecordCodec::new(&WorldConfig::default());
        for v in [a, b] {
            let bytes = s.sink().latest(v).expect("served");
            let r = TickObservation::new(&codec, &bytes).expect("well formed");
            assert_eq!(r.header().tick, 1);
            assert_eq!(r.header().sequence, 1, "sequence starts at one per client");
            assert!(r.updates().count() > 0);
        }
    }

    #[test]
    fn a_sink_hears_nothing_for_a_viewer_that_is_not_due() {
        let mut s = sim(Walk::new(1)).with_sink(RecordingSink::new());
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits { payload_bytes: 1200, send_period: 4 });
        let mut served = 0;
        for _ in 0..4 {
            served += s.tick().viewers;
        }
        assert_eq!(served, 1);
        assert_eq!(s.sink().sends(), 1, "a sink is only handed what was assembled");
    }

    #[test]
    fn a_sequence_number_advances_once_per_payload() {
        use crate::packet::PacketHeader;
        let mut s = sim(Walk::new(1)).with_sink(RecordingSink::new());
        let ids = populate(&mut s, 60);
        let v = s.register_viewer(ids[0], ClientLimits::default());
        for tick in 1..=6u32 {
            s.tick();
            let bytes = s.sink().latest(v).expect("served");
            let h = PacketHeader::decode(&bytes).expect("well formed");
            assert_eq!(h.sequence, tick as u16);
            assert_eq!(h.tick, tick);
        }
    }

    #[test]
    fn a_panicking_sink_is_not_swallowed() {
        struct Boom;
        impl crate::sim::sink::PayloadSink for Boom {
            fn send(&self, _v: ViewerId, _p: &[u8]) {
                panic!("sink exploded");
            }
        }
        let mut s = sim(Walk::new(1)).with_sink(Boom);
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits::default());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.tick()));
        assert!(r.is_err(), "a panicking sink must surface, not be swallowed");
    }

    #[test]
    fn a_slow_sink_delays_the_tick_but_is_attributed() {
        struct Slow;
        impl crate::sim::sink::PayloadSink for Slow {
            fn send(&self, _v: ViewerId, _p: &[u8]) {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
        let mut s = sim(Walk::new(1)).with_sink(Slow);
        let ids = populate(&mut s, 40);
        for id in ids.iter().take(20) {
            s.register_viewer(*id, ClientLimits::default());
        }
        let t = std::time::Instant::now();
        let stats = s.tick();
        let took = t.elapsed();
        assert_eq!(stats.viewers, 20);
        assert!(
            took.as_micros() >= 200,
            "the tick absorbs the sink's cost, since it cannot be preempted: {took:?}"
        );
        // Nothing can stop it, but nobody has to guess where the time went.
        assert!(
            stats.sink_nanos >= 20 * 200_000,
            "the sink's cost must be attributable, got {} ns across 20 sends",
            stats.sink_nanos
        );
    }

    #[test]
    fn a_quick_sink_barely_registers() {
        let mut s = sim(Walk::new(1)).with_sink(RecordingSink::new());
        let ids = populate(&mut s, 200);
        for id in ids.iter().take(20) {
            s.register_viewer(*id, ClientLimits::default());
        }
        s.tick();
        let stats = s.tick();
        assert_eq!(stats.viewers, 20);
        assert!(stats.sink_nanos > 0, "a sink that does anything is measurable");
        assert!(
            stats.sink_nanos < 20 * 200_000,
            "a sink that copies a payload is nothing like one that sleeps"
        );
    }

    #[test]
    fn the_default_sink_is_free_to_hold() {
        assert_eq!(size_of::<NullSink>(), 0);
    }

    #[test]
    fn a_viewer_reports_the_entity_it_controls() {
        let mut s = sim(Walk::still());
        let ids = populate(&mut s, 10);
        let v = s.register_viewer(ids[4], ClientLimits::default());
        assert_eq!(s.avatar_of(v), Some(ids[4]));
        s.unregister_viewer(v);
        assert_eq!(s.avatar_of(v), None, "an unregistered viewer controls nothing");
        assert_eq!(s.avatar_of(ViewerId::from_raw(99)), None);
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
    fn thread_count_does_not_change_the_outcome() {
        // Replication is read-only against the snapshot and each viewer owns
        // its own ghosts, so partitioning viewers across threads must not
        // change a single record.
        let run = |threads: usize| {
            let mut s = sim(Walk::new(1));
            let ids = populate(&mut s, 400);
            s.set_thread_count(threads);
            for id in ids.iter().take(50) {
                s.register_viewer(*id, ClientLimits::default());
            }
            // Everything but the timing, which is wall clock and reproduces
            // for nobody.
            let counts = |t: TickStats| {
                [
                    t.viewers,
                    t.candidates,
                    t.records,
                    t.new_ghosts,
                    t.departed,
                    t.despawns_sent,
                    t.bytes,
                ]
            };
            let stats: Vec<[u64; 7]> = (0..12).map(|_| counts(s.tick())).collect();
            let ghosts: Vec<usize> =
                (0..50).map(|i| s.ghost_count(ViewerId::from_raw(i))).collect();
            (stats, ghosts)
        };
        let one = run(1);
        assert_eq!(one, run(2));
        assert_eq!(one, run(3), "a partition that does not divide evenly");
        assert_eq!(one, run(8));
        assert_eq!(one, run(64), "more workers than a chunk can use");
    }

    #[test]
    fn a_thread_count_below_one_is_clamped() {
        let mut s = sim(Walk::still());
        s.set_thread_count(0);
        assert_eq!(s.thread_count(), 1);
        assert_eq!(s.workers.len(), 1);
        s.set_thread_count(6);
        assert_eq!(s.workers.len(), 6);
    }

    #[test]
    fn a_subscription_changes_only_when_its_viewer_crosses_a_cell() {
        // A one meter step, against a cell that is many meters across.
        let mut s = sim(Walk::new(1));
        let ids = populate(&mut s, 60);
        s.register_viewer(ids[0], ClientLimits::default());

        let first = s.tick();
        assert_eq!(first.viewers, 1, "the viewer is served");
        assert_eq!(first.subs_changed, 1, "its first subscription is a change from none");

        let cell = s.config().cell_size().floor_meters();
        let mut crossings = 0;
        for _ in 0..cell {
            crossings += s.tick().subs_changed;
        }
        assert!(
            crossings > 0 && crossings <= 2,
            "walking {cell} m at a meter a tick leaves a {cell} m cell once, not {crossings} times"
        );
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
        let found = s.workers[0].found.capacity();
        let snap = s.snapshot().len();
        let ghosts: Vec<usize> = (0..20).map(|i| s.viewers[i].ghosts.slots()).collect();

        for _ in 0..50 {
            s.tick();
        }
        assert_eq!(
            s.workers[0].found.capacity(),
            found,
            "the gather buffer must not grow"
        );
        assert_eq!(s.snapshot().len(), snap);
        let after: Vec<usize> = (0..20).map(|i| s.viewers[i].ghosts.slots()).collect();
        assert_eq!(after, ghosts, "ghost tables must reach a steady size");
    }

    // -- what a game may do to one entity -----------------------------------

    fn step_over(sim: &mut WorldSimulation<Walk>) -> Step<'_> {
        Step {
            xs: &mut sim.xs,
            ys: &mut sim.ys,
            zs: &mut sim.zs,
            live: &mut sim.live,
            despawned: &mut sim.despawned,
            cfg: &sim.cfg,
            tick: 0,
        }
    }

    #[test]
    fn a_position_reads_back_what_was_spawned() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let at = Pos3::from_meters(2048, 2049, 3);
        let id = step.spawn(at);
        assert_eq!(step.position(id), Some(at));
        assert!(step.contains(id));
    }

    #[test]
    fn a_despawned_entity_has_no_position() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let id = step.spawn(Pos3::from_meters(2048, 2048, 0));
        step.despawn(id);
        assert_eq!(step.position(id), None);
        assert!(!step.contains(id));
    }

    #[test]
    fn a_never_allocated_id_has_no_position() {
        let mut s = sim(Walk::still());
        let step = step_over(&mut s);
        assert_eq!(step.position(EntityId::from_raw(9)), None);
    }

    #[test]
    fn move_to_puts_an_entity_where_it_was_told() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let id = step.spawn(Pos3::from_meters(2048, 2048, 0));
        let to = Pos3::from_meters(2100, 2000, 12);
        step.move_to(id, to);
        assert_eq!(step.position(id), Some(to));
    }

    #[test]
    fn move_to_leaves_a_despawned_entity_alone() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let gone = step.spawn(Pos3::from_meters(2048, 2048, 0));
        let neighbour = step.spawn(Pos3::from_meters(2049, 2048, 0));
        step.despawn(gone);
        step.move_to(gone, Pos3::from_meters(2100, 2100, 0));
        assert_eq!(step.position(gone), None);
        assert_eq!(step.position(neighbour), Some(Pos3::from_meters(2049, 2048, 0)));
    }

    #[test]
    fn translate_offsets_from_where_the_entity_is() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let id = step.spawn(Pos3::from_meters(2048, 2048, 0));
        step.translate(
            id,
            Fixed::from_millis(0, 250),
            Fixed::from_meters(-1),
            Fixed::ZERO,
        );
        assert_eq!(
            step.position(id),
            Some(Pos3::new(
                Fixed::from_millis(2048, 250),
                Fixed::from_meters(2047),
                Fixed::ZERO
            ))
        );
    }

    #[test]
    fn translate_accumulates_across_calls() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let id = step.spawn(Pos3::from_meters(2048, 2048, 0));
        for _ in 0..4 {
            step.translate(id, Fixed::from_millis(0, 250), Fixed::ZERO, Fixed::ZERO);
        }
        assert_eq!(step.position(id).map(|at| at.x), Some(Fixed::from_meters(2049)));
    }

    #[test]
    fn translate_leaves_a_despawned_entity_alone() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let id = step.spawn(Pos3::from_meters(2048, 2048, 0));
        step.despawn(id);
        step.translate(id, Fixed::from_meters(1), Fixed::ZERO, Fixed::ZERO);
        assert_eq!(step.position(id), None);
    }

    #[test]
    fn entities_lists_the_live_ids_in_ascending_order() {
        let mut s = sim(Walk::still());
        let mut step = step_over(&mut s);
        let ids: Vec<EntityId> =
            (0..5).map(|k| step.spawn(Pos3::from_meters(2048 + k, 2048, 0))).collect();
        step.despawn(ids[1]);
        step.despawn(ids[3]);
        assert_eq!(step.entities().collect::<Vec<_>>(), vec![ids[0], ids[2], ids[4]]);
        assert_eq!(step.entity_count(), 3);
    }
}
