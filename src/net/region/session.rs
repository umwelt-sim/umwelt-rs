//! What a running region and its attached edges say to each other.
//!
//! The handshake settles who may talk. This is what they say afterward: edges
//! ask for entities, move them, and give them back, and the region sends each
//! viewer's assembled payload to the edge that manages it.
//!
//! # Where each half runs
//!
//! Inbound work cannot all happen in one place, because the two things it needs
//! are available at different moments.
//!
//! - [`Inbound::accept`] runs on the reader task and only queues. It never
//!   touches the simulation, because the simulation is mid-tick as often as
//!   not.
//! - [`Inbound::apply`] runs inside [`Game::step`], which is the only place a
//!   [`Step`] exists, so it is the only place an entity can be spawned,
//!   despawned or moved.
//! - [`Inbound::settle`] runs between ticks, which is the only place
//!   `&mut WorldSimulation` exists, so it is the only place a viewer can be
//!   registered or dropped.
//!
//! The split is required rather than stylistic. Registering a viewer mid-tick
//! would change the set the workers are iterating over, and spawning between
//! ticks would write the position arrays after the snapshot had been built from
//! them.
//!
//! # Who allocates what
//!
//! The region allocates every entity id. An edge asks for entities by position
//! and is told the ids it got, so two edges cannot pick colliding ids because
//! neither picks any. Mapping those ids to game client sockets is the edge's
//! own bookkeeping and no business of this protocol.
//!
//! Ids are unique within one region. An edge relaying for several regions sees
//! the same numbers from each and has to key by `(RegionId, EntityId)`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::runtime::Handle;

use crate::entity::{EntityId, EntityKind};
use crate::game::Game;
use crate::id::RegionId;
use crate::net::control::RegionLoad;
use crate::net::error::NetError;
use crate::net::region::edges::{EdgeId, EdgeStats, Edges};
use crate::net::region::protocol::{
    DespawnEntities, KIND_DESPAWN_ENTITIES, KIND_KEEPALIVE, KIND_MOVE_ENTITIES,
    KIND_SPAWN_ENTITIES, MoveEntities, Presence, Spawn, SpawnEntities,
};
use crate::net::region::subjects;
use crate::pos::Pos3;
use crate::sim::{ClientLimits, PayloadSink, Step, TickSpan, ViewerId, WorldSimulation};

/// No viewer watches this entity. Reserved in the entity-to-viewer map.
const NO_WATCHER: u32 = u32::MAX;

/// No avatar belongs to this viewer. Reserved in the viewer-to-entity map.
///
/// Both ids are dense from zero, so the top of the range is unreachable.
const NO_AVATAR: u32 = u32::MAX;

/// What an edge asked the region to do.
#[derive(Clone, Debug)]
enum Command {
    Spawn { edge: EdgeId, spawns: Vec<Spawn> },
    Move { edge: EdgeId, moves: Vec<(EntityId, Pos3)> },
    Despawn { edge: EdgeId, ids: Vec<EntityId> },
}

/// What one [`Inbound::apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Entities created.
    pub spawned: u32,
    /// Positions written.
    pub moved: u32,
    /// Entities removed at an edge's asking.
    pub despawned: u32,
    /// Despawned because the edge managing them detached.
    pub orphaned: u32,
    /// Commands declined: an entity the sending edge does not manage, a
    /// position outside the region, or an entity that is already gone.
    pub refused: u32,
}

/// What one [`Inbound::settle`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Viewers added.
    pub registered: u32,
    /// Viewers dropped.
    pub unregistered: u32,
    /// Presence messages published, additions and removals together.
    pub reported: u32,
}

/// What a region has been doing, between heartbeats.
///
/// `Inbound` holds it because `Inbound` is the one thing both ends touch: the
/// tick calls [`apply`](Inbound::apply) and [`settle`](Inbound::settle), and
/// [`RegionServer`](crate::net::RegionServer) holds an `Arc` of it. Neither
/// call is optional, so nothing has to be wired up for this to fill in.
#[derive(Debug, Default)]
struct Load {
    /// Overwritten every tick: what the region holds right now.
    tick_count: u32,
    entities: u32,
    id_space: u32,
    /// Accumulated until a heartbeat drains it.
    span: TickSpan,
}

/// Commands from every edge, and the bookkeeping that outlives one tick.
#[derive(Debug)]
pub struct Inbound {
    edges: Arc<Edges>,
    queue: Mutex<Vec<Command>>,
    /// Spawned during the last apply, waiting to be answered for. An observer
    /// also gets a viewer; an unattended entity is only reported back.
    fresh: Mutex<Vec<(EdgeId, EntityId, EntityKind, u64)>>,
    /// Despawned during the last apply, waiting for its viewer to be dropped.
    /// `None` for an entity orphaned by its edge detaching, since that edge's
    /// counters have already gone with it.
    gone: Mutex<Vec<(Option<EdgeId>, EntityId)>>,
    /// Entity index to viewer raw. The direction the simulation does not hold,
    /// and the one tearing an entity down needs.
    watchers: Mutex<Vec<u32>>,
    received: AtomicU64,
    refused: AtomicU64,
    load: Mutex<Load>,
}

impl Inbound {
    /// Holding nothing, for the region these edges attach to.
    pub fn new(edges: Arc<Edges>) -> Inbound {
        Inbound {
            edges,
            queue: Mutex::new(Vec::new()),
            fresh: Mutex::new(Vec::new()),
            gone: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
            received: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            load: Mutex::new(Load::default()),
        }
    }

    /// Takes the load accumulated since the last call.
    ///
    /// Not public: a heartbeat drains this, and a second caller would silently
    /// take half the numbers away from the first.
    pub(crate) fn take_load(&self) -> RegionLoad {
        let mut load = self.load.lock().expect("not poisoned");
        let span = std::mem::take(&mut load.span);
        RegionLoad {
            tick_count: load.tick_count,
            entities: load.entities,
            id_space: load.id_space,
            viewers: span.viewers.checked_div(u64::from(span.ticks)).unwrap_or_default()
                as u32,
            mean_tick: span.mean(),
            worst_tick: span.worst,
            late: span.late,
            dropped: span.dropped,
        }
    }

    /// Messages queued since this region started.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Commands declined, summed across every apply.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// The edges relaying for this region.
    #[inline]
    pub fn edges(&self) -> &Arc<Edges> {
        &self.edges
    }

    /// Queues one command that arrived from `edge`.
    ///
    /// Called from the NATS reader, which is not the tick thread, so nothing
    /// here touches the simulation. A command is queued and applied on the
    /// next tick. A message that does not decode is counted and dropped: there
    /// is no connection to tear down, and one bad message says nothing about
    /// the next.
    pub fn accept(&self, edge: EdgeId, message: &[u8]) {
        let Some((&kind, body)) = message.split_first() else {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let decoded = match kind {
            KIND_SPAWN_ENTITIES => SpawnEntities::decode(body)
                .map(|m| Command::Spawn { edge, spawns: m.spawns }),
            KIND_MOVE_ENTITIES => {
                MoveEntities::decode(body).map(|m| Command::Move { edge, moves: m.moves })
            }
            KIND_DESPAWN_ENTITIES => DespawnEntities::decode(body)
                .map(|m| Command::Despawn { edge, ids: m.ids }),
            // Says only that the edge is still there, which admitting it
            // already recorded.
            KIND_KEEPALIVE => return,
            _ => Err(NetError::Unexpected { expected: "a command", got: kind }),
        };
        match decoded {
            Ok(command) => {
                self.queue.lock().expect("not poisoned").push(command);
                self.received.fetch_add(1, Ordering::Relaxed);
                if let Some(stats) = self.edges.stats(edge) {
                    stats.messages.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                self.refused.fetch_add(1, Ordering::Relaxed);
                if let Some(stats) = self.edges.stats(edge) {
                    stats.refused.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Applies what edges sent. Call from inside [`Game::step`].
    ///
    /// Every command naming an entity is checked against who manages it, and a
    /// command for an entity the sender does not manage is counted and
    /// dropped. That is a consistency check rather than an authorization one:
    /// it keeps an edge's own stale ids, left over from a despawn or a
    /// migration, from moving somebody else's entity. Who may publish what is
    /// the broker's, and nothing here knows or asks.
    pub fn apply(&self, step: &mut Step<'_>) -> Applied {
        let cfg = *step.config();
        let commands = std::mem::take(&mut *self.queue.lock().expect("not poisoned"));
        let mut out = Applied::default();
        // Collected here and handed over once. Locking per entity would take a
        // mutex several hundred times for one spawn message.
        let mut fresh: Vec<(EdgeId, EntityId, EntityKind, u64)> = Vec::new();
        let mut gone: Vec<(Option<EdgeId>, EntityId)> = Vec::new();

        // Orphans first, so a stale command later in this same batch cannot
        // move an entity whose edge has already gone.
        for id in self.edges.take_detached() {
            if step.contains(id) {
                step.despawn(id);
                gone.push((None, id));
                out.orphaned += 1;
            }
        }

        // Positions are written last: `positions_mut` borrows the whole `Step`,
        // so nothing else on it is reachable while the slices are held.
        let mut writes: Vec<(usize, Pos3)> = Vec::new();

        for command in commands {
            match command {
                Command::Spawn { edge, spawns } => {
                    let stats = self.edges.stats(edge);
                    for want in spawns {
                        if !cfg.contains(want.position) {
                            refuse(&mut out, &stats);
                            continue;
                        }
                        let id = step.spawn(want.position, want.kind.tag());
                        if self.edges.claim(edge, id).is_err() {
                            // The edge went away between sending and now.
                            step.despawn(id);
                            refuse(&mut out, &stats);
                            continue;
                        }
                        fresh.push((edge, id, want.kind, want.token));
                        out.spawned += 1;
                    }
                }
                Command::Despawn { edge, ids } => {
                    let stats = self.edges.stats(edge);
                    for id in ids {
                        if self.edges.edge_for(id) != Some(edge) {
                            refuse(&mut out, &stats);
                            continue;
                        }
                        // Released first, so the reconciliation in `settle`
                        // sees no owner and does not report this twice.
                        self.edges.release(id);
                        step.despawn(id);
                        gone.push((Some(edge), id));
                        out.despawned += 1;
                    }
                }
                Command::Move { edge, moves } => {
                    let stats = self.edges.stats(edge);
                    for (id, pos) in moves {
                        if self.edges.edge_for(id) != Some(edge)
                            || !cfg.contains(pos)
                            || !step.contains(id)
                        {
                            refuse(&mut out, &stats);
                            continue;
                        }
                        writes.push((id.index(), pos));
                    }
                }
            }
        }

        let (xs, ys, zs, _) = step.positions_mut();
        for (at, pos) in writes {
            xs[at] = pos.x;
            ys[at] = pos.y;
            zs[at] = pos.z;
            out.moved += 1;
        }

        if !fresh.is_empty() {
            self.fresh.lock().expect("not poisoned").append(&mut fresh);
        }
        if !gone.is_empty() {
            self.gone.lock().expect("not poisoned").append(&mut gone);
        }
        self.refused.fetch_add(out.refused as u64, Ordering::Relaxed);
        out
    }

    /// Registers a viewer for everything spawned, drops the viewers of
    /// everything despawned, and answers the edges that asked.
    ///
    /// Call between ticks, where `&mut WorldSimulation` is available.
    pub fn settle<G: Game, S: PayloadSink>(
        &self,
        sim: &mut WorldSimulation<G, S>,
        sink: &EdgeSink,
        limits: ClientLimits,
    ) -> Settled {
        let fresh = std::mem::take(&mut *self.fresh.lock().expect("not poisoned"));
        let gone = std::mem::take(&mut *self.gone.lock().expect("not poisoned"));
        let mut out = Settled::default();

        // Teardown first. Viewer ids are reusable, so registering before
        // dropping could hand out an id this batch then clears.
        for (edge, id) in gone {
            self.retire(sim, sink, edge, id, &mut out);
        }

        // A despawn the game performed reaches nothing else: `apply` knows only
        // the ones it did itself. Left alone the ownership record would outlive
        // the entity, its viewer would never be dropped, and the edge would go
        // on sending moves that are refused for as long as it ran.
        //
        // An entity `apply` despawned is released before it is despawned, so it
        // has no owner here and is not reported twice.
        for id in sim.despawned().to_vec() {
            let Some(edge) = self.edges.edge_for(id) else { continue };
            self.edges.release(id);
            self.retire(sim, sink, Some(edge), id, &mut out);
        }

        for (edge, id, kind, token) in fresh {
            // Only an observer costs a viewer. An unattended entity is
            // replicated to whoever can see it and receives nothing itself, so
            // it never enters the per-viewer pipeline at all.
            if kind.observes() {
                let viewer = sim.register_viewer(id, limits);
                sink.bind(viewer, id);
                self.set_watcher(id, viewer);
                if let Some(stats) = self.edges.stats(edge) {
                    stats.observers.fetch_add(1, Ordering::Relaxed);
                }
                out.registered += 1;
            }
            let added = Presence::Added { entity: id, token };
            if sink.presence(edge, added).is_ok() {
                out.reported += 1;
            }
        }

        // The heartbeat's numbers, taken where the tick and the server meet.
        // This is the only call that holds the simulation and is also reachable
        // from `RegionServer`, so it is where the two are joined.
        {
            let mut load = self.load.lock().expect("not poisoned");
            load.tick_count = sim.tick_count();
            load.entities = sim.entity_count() as u32;
            load.id_space = sim.id_space() as u32;
            load.span.merge(sim.take_span());
        }

        out
    }

    /// Drops one entity's viewer and tells its edge the entity is gone.
    ///
    /// `edge` is `None` for an orphan, whose edge has already detached and so
    /// has nothing left to tell and no counters left to adjust.
    fn retire<G: Game, S: PayloadSink>(
        &self,
        sim: &mut WorldSimulation<G, S>,
        sink: &EdgeSink,
        edge: Option<EdgeId>,
        id: EntityId,
        out: &mut Settled,
    ) {
        if let Some(viewer) = self.take_watcher(id) {
            sim.unregister_viewer(viewer);
            sink.unbind(viewer);
            if let Some(stats) = edge.and_then(|e| self.edges.stats(e)) {
                stats.observers.fetch_sub(1, Ordering::Relaxed);
            }
            out.unregistered += 1;
        }
        if let Some(edge) = edge
            && sink.presence(edge, Presence::Removed { entity: id }).is_ok()
        {
            out.reported += 1;
        }
    }

    fn set_watcher(&self, entity: EntityId, viewer: ViewerId) {
        let mut watchers = self.watchers.lock().expect("not poisoned");
        if watchers.len() <= entity.index() {
            watchers.resize(entity.index() + 1, NO_WATCHER);
        }
        watchers[entity.index()] = viewer.raw();
    }

    fn take_watcher(&self, entity: EntityId) -> Option<ViewerId> {
        let mut watchers = self.watchers.lock().expect("not poisoned");
        let slot = watchers.get_mut(entity.index())?;
        let raw = std::mem::replace(slot, NO_WATCHER);
        (raw != NO_WATCHER).then(|| ViewerId::from_raw(raw))
    }
}

/// Counts one declined command, on the run total and on the edge that sent it.
fn refuse(out: &mut Applied, stats: &Option<Arc<EdgeStats>>) {
    out.refused += 1;
    if let Some(stats) = stats {
        stats.refused.fetch_add(1, Ordering::Relaxed);
    }
}

/// Publishes each viewer's payload to the edge that manages its avatar.
///
/// Cheap to clone: one lives in the [`WorldSimulation`] and one stays with the
/// tick loop, which is what binds a viewer when it registers.
///
/// **Wrap this in [`Handoff`](crate::Handoff).** `send` publishes, and
/// [`PayloadSink`] is called from inside the tick. `Handoff` turns the tick's
/// side of it into a memory copy and drops rather than queues, which is right
/// for state nobody wants stale.
#[derive(Clone)]
pub struct EdgeSink {
    shared: Arc<SinkShared>,
}

struct SinkShared {
    region: RegionId,
    client: async_nats::Client,
    runtime: Handle,
    edges: Arc<Edges>,
    /// Viewer index to avatar entity raw. A payload arrives named by its
    /// viewer, and the routing question is asked about an entity.
    avatars: RwLock<Vec<AtomicU32>>,
    /// Two subjects per edge, rebuilt when the set changes. Formatting one per
    /// payload would allocate half a million times a second; taking the set's
    /// lock instead would serialize on it.
    subjects: RwLock<Cached>,
    sent: AtomicU64,
    undeliverable: AtomicU64,
    failed: AtomicU64,
}

#[derive(Default)]
struct Cached {
    generation: u64,
    state: Vec<Option<async_nats::Subject>>,
    presence: Vec<Option<async_nats::Subject>>,
}

impl EdgeSink {
    /// A sink that publishes each payload to the edge managing its viewer.
    ///
    /// Attaches to a [`WorldSimulation`] through
    /// [`with_sink`](crate::WorldSimulation::with_sink), usually wrapped in a
    /// [`Handoff`](crate::Handoff) so the tick never waits on a publish.
    pub fn new(
        region: RegionId,
        client: async_nats::Client,
        runtime: Handle,
        edges: Arc<Edges>,
    ) -> EdgeSink {
        EdgeSink {
            shared: Arc::new(SinkShared {
                region,
                client,
                runtime,
                edges,
                avatars: RwLock::new(Vec::new()),
                subjects: RwLock::new(Cached {
                    generation: u64::MAX,
                    ..Cached::default()
                }),
                sent: AtomicU64::new(0),
                undeliverable: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
        }
    }

    /// Records which entity a viewer watches, so its payloads can be addressed.
    pub fn bind(&self, viewer: ViewerId, avatar: EntityId) {
        let mut avatars = self.shared.avatars.write().expect("not poisoned");
        if avatars.len() <= viewer.index() {
            avatars.resize_with(viewer.index() + 1, || AtomicU32::new(NO_AVATAR));
        }
        avatars[viewer.index()].store(avatar.raw(), Ordering::Relaxed);
    }

    /// Forgets a viewer, so a recycled id cannot address the previous avatar.
    pub fn unbind(&self, viewer: ViewerId) {
        let avatars = self.shared.avatars.read().expect("not poisoned");
        if let Some(slot) = avatars.get(viewer.index()) {
            slot.store(NO_AVATAR, Ordering::Relaxed);
        }
    }

    /// Payloads published.
    pub fn sent(&self) -> u64 {
        self.shared.sent.load(Ordering::Relaxed)
    }

    /// Payloads for a viewer with no avatar bound, or an avatar no edge
    /// manages. A viewer whose edge went away mid-tick lands here.
    pub fn undeliverable(&self) -> u64 {
        self.shared.undeliverable.load(Ordering::Relaxed)
    }

    /// Payloads whose publish failed.
    pub fn failed(&self) -> u64 {
        self.shared.failed.load(Ordering::Relaxed)
    }

    /// Reports one entity appearing or leaving, on the owning edge's presence
    /// subject.
    pub(crate) fn presence(&self, edge: EdgeId, what: Presence) -> Result<(), NetError> {
        let Some(subject) = self.subject(edge, false) else {
            return Err(NetError::BadSubject);
        };
        let mut body = Vec::with_capacity(Presence::BYTES);
        what.encode(&mut body);
        self.shared.runtime.block_on(self.shared.client.publish(subject, body.into()))?;
        Ok(())
    }

    fn avatar_of(&self, viewer: ViewerId) -> Option<EntityId> {
        let avatars = self.shared.avatars.read().expect("not poisoned");
        match avatars.get(viewer.index()).map(|slot| slot.load(Ordering::Relaxed)) {
            Some(raw) if raw != NO_AVATAR => Some(EntityId::from_raw(raw)),
            _ => None,
        }
    }

    /// One edge's subject, rebuilding the table if the set has changed since it
    /// was built. `state` picks which of the two.
    fn subject(&self, edge: EdgeId, state: bool) -> Option<async_nats::Subject> {
        let generation = self.shared.edges.generation();
        {
            let cached = self.shared.subjects.read().expect("not poisoned");
            if cached.generation == generation {
                let table = if state { &cached.state } else { &cached.presence };
                return table.get(edge.index()).cloned().flatten();
            }
        }
        let mut cached = self.shared.subjects.write().expect("not poisoned");
        cached.state.clear();
        cached.presence.clear();
        for view in self.shared.edges.view() {
            let at = view.id.index();
            if cached.state.len() <= at {
                cached.state.resize(at + 1, None);
                cached.presence.resize(at + 1, None);
            }
            cached.state[at] =
                Some(subjects::state(self.shared.region, &view.name).into());
            cached.presence[at] =
                Some(subjects::presence(self.shared.region, &view.name).into());
        }
        cached.generation = generation;
        let table = if state { &cached.state } else { &cached.presence };
        table.get(edge.index()).cloned().flatten()
    }
}

impl core::fmt::Debug for EdgeSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeSink")
            .field("region", &self.shared.region)
            .field("sent", &self.sent())
            .finish_non_exhaustive()
    }
}

impl PayloadSink for EdgeSink {
    fn send(&self, viewer: ViewerId, payload: &[u8]) {
        let Some(avatar) = self.avatar_of(viewer) else {
            self.shared.undeliverable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(edge) = self.shared.edges.edge_for(avatar) else {
            self.shared.undeliverable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let Some(subject) = self.subject(edge, true) else {
            self.shared.undeliverable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // Addressed by the avatar, so an edge holds one map rather than two.
        // ViewerId stays inside the region.
        let mut body = Vec::with_capacity(4 + payload.len());
        body.extend_from_slice(&avatar.raw().to_le_bytes());
        body.extend_from_slice(payload);
        let published = self
            .shared
            .runtime
            .block_on(self.shared.client.publish(subject, body.into()));
        match published {
            Ok(()) => self.shared.sent.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.shared.failed.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Pushes the batch to the broker. The NATS client buffers, so this is
    /// where the write happens rather than once per payload.
    fn flush(&self) {
        let _ = self.shared.runtime.block_on(self.shared.client.flush());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Fixed;

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    // Routing and publishing need a broker, so they are covered in
    // tests/region_edge.rs. What is left here runs without one.

    #[test]
    fn the_watcher_map_is_the_direction_the_simulation_lacks() {
        // Dropping a viewer when its entity despawns needs entity to viewer,
        // and `WorldSimulation` only answers the other way.
        let inbound = Inbound::new(Arc::new(Edges::new()));
        assert_eq!(inbound.take_watcher(ent(4)), None);

        inbound.set_watcher(ent(4), ViewerId::from_raw(9));
        assert_eq!(inbound.take_watcher(ent(4)), Some(ViewerId::from_raw(9)));
        assert_eq!(inbound.take_watcher(ent(4)), None, "taken once");
    }

    #[test]
    fn a_position_survives_the_wire_exactly() {
        // What apply writes into the arrays has to be what the edge sent, or a
        // stationary entity would drift a little every tick.
        let sent =
            Pos3::new(Fixed::from_millimeters(7, 500), Fixed::ZERO, Fixed::from_raw(1));
        let mut body = Vec::new();
        MoveEntities { moves: vec![(ent(1), sent)] }.encode(&mut body);
        let back = MoveEntities::decode(&body[1..]).expect("well formed");
        assert_eq!(back.moves[0].1, sent);
    }

    #[test]
    fn a_command_that_does_not_decode_is_counted_and_dropped() {
        let edges = Arc::new(Edges::new());
        let edge = edges.admit(&crate::net::EdgeName::new("alpha").expect("valid"));
        let inbound = Inbound::new(Arc::clone(&edges));

        inbound.accept(edge, &[]);
        inbound.accept(edge, &[200, 1, 2, 3]);
        inbound.accept(edge, &[KIND_MOVE_ENTITIES, 0xFF]);
        assert_eq!(inbound.refused(), 3);
        assert_eq!(inbound.received(), 0, "nothing was queued");
    }

    #[test]
    fn a_keepalive_says_nothing_but_is_not_a_fault() {
        let edges = Arc::new(Edges::new());
        let edge = edges.admit(&crate::net::EdgeName::new("alpha").expect("valid"));
        let inbound = Inbound::new(Arc::clone(&edges));
        inbound.accept(edge, &[KIND_KEEPALIVE]);
        assert_eq!(inbound.refused(), 0);
        assert_eq!(inbound.received(), 0);
    }

    #[test]
    fn a_well_formed_command_is_queued() {
        let edges = Arc::new(Edges::new());
        let edge = edges.admit(&crate::net::EdgeName::new("alpha").expect("valid"));
        let inbound = Inbound::new(Arc::clone(&edges));

        let mut body = Vec::new();
        MoveEntities { moves: vec![(ent(1), Pos3::from_meters(1, 2, 3))] }
            .encode(&mut body);
        inbound.accept(edge, &body);
        assert_eq!(inbound.received(), 1);
        assert_eq!(inbound.refused(), 0);
    }
}
