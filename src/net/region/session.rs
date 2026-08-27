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
//! - [`Inbound::serve`] runs on the edge's own thread and only queues. It never
//!   touches the simulation, because the simulation is mid-tick as often as not.
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

use std::cmp::Ordering as Ordering2;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::entity::EntityId;
use crate::net::error::NetError;
use crate::net::region::edges::{Edge, EdgeId, EdgeStats, Edges};
use crate::net::region::protocol::{
    DespawnEntities, EntitiesSpawned, EntityKind, KIND_DESPAWN_ENTITIES, KIND_ENTITIES_SPAWNED,
    KIND_MOVE_ENTITIES, KIND_POSITION_UPDATES, KIND_QUIT, KIND_SPAWN_ENTITIES,
    MAX_SPAWN_PER_MESSAGE, MoveEntities, SpawnEntities,
};
use crate::net::region::wire::read_frame;
use crate::pos::Pos3;
use crate::sim::{ClientLimits, Game, PayloadSink, Step, ViewerId, WorldSimulation};

/// No viewer watches this entity. Reserved in the entity-to-viewer map.
const NO_WATCHER: u32 = u32::MAX;

/// No avatar belongs to this viewer. Reserved in the viewer-to-entity map.
///
/// Both ids are dense from zero, so the top of the range is unreachable.
const NO_AVATAR: u32 = u32::MAX;

/// What an edge asked the region to do.
#[derive(Clone, Debug)]
enum Command {
    Spawn { edge: EdgeId, spawns: Vec<(Pos3, EntityKind)> },
    Move { edge: EdgeId, moves: Vec<(EntityId, Pos3)> },
    Despawn { edge: EdgeId, ids: Vec<EntityId> },
}

/// What one [`Inbound::apply`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub spawned: u32,
    pub moved: u32,
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
    pub registered: u32,
    pub unregistered: u32,
    /// [`EntitiesSpawned`] messages sent back.
    pub replies: u32,
}

/// Commands from every edge, and the bookkeeping that outlives one tick.
#[derive(Debug)]
pub struct Inbound {
    edges: Arc<Edges>,
    queue: Mutex<Vec<Command>>,
    /// Spawned during the last apply, waiting to be answered for. An observer
    /// also gets a viewer; an unattended entity is only reported back.
    fresh: Mutex<Vec<(EdgeId, EntityId, EntityKind)>>,
    /// Despawned during the last apply, waiting for its viewer to be dropped.
    /// `None` for an entity orphaned by its edge detaching, since that edge's
    /// counters have already gone with it.
    gone: Mutex<Vec<(Option<EdgeId>, EntityId)>>,
    /// Entity index to viewer raw. The direction the simulation does not hold,
    /// and the one tearing an entity down needs.
    watchers: Mutex<Vec<u32>>,
    received: AtomicU64,
    refused: AtomicU64,
}

impl Inbound {
    pub fn new(edges: Arc<Edges>) -> Inbound {
        Inbound {
            edges,
            queue: Mutex::new(Vec::new()),
            fresh: Mutex::new(Vec::new()),
            gone: Mutex::new(Vec::new()),
            watchers: Mutex::new(Vec::new()),
            received: AtomicU64::new(0),
            refused: AtomicU64::new(0),
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

    /// Reads from one edge until it closes, queueing what it sends.
    ///
    /// Blocks, so this is what [`RegionServer::run`](crate::net::RegionServer::run)
    /// hands each edge's thread. Nothing here touches the simulation: a command
    /// is queued and applied on the next tick.
    pub fn serve(&self, edge: &Edge) -> Result<(), NetError> {
        let id = edge.id();
        let stats = self.edges.stats(id);
        let mut sock = edge.stream();
        let mut body = Vec::new();
        loop {
            let kind = match read_frame(&mut sock, &mut body) {
                Ok(kind) => kind,
                Err(NetError::Closed) => return Ok(()),
                Err(e) => return Err(e),
            };
            let command = match kind {
                KIND_SPAWN_ENTITIES => {
                    Command::Spawn { edge: id, spawns: SpawnEntities::decode(&body)?.spawns }
                }
                KIND_MOVE_ENTITIES => {
                    Command::Move { edge: id, moves: MoveEntities::decode(&body)?.moves }
                }
                KIND_DESPAWN_ENTITIES => {
                    Command::Despawn { edge: id, ids: DespawnEntities::decode(&body)?.ids }
                }
                KIND_QUIT => return Ok(()),
                other => {
                    return Err(NetError::Unexpected { expected: "a session message", got: other });
                }
            };
            self.queue.lock().expect("not poisoned").push(command);
            self.received.fetch_add(1, Ordering::Relaxed);
            if let Some(stats) = &stats {
                stats.messages.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Applies what edges sent. Call from inside [`Game::step`].
    ///
    /// Every command naming an entity is checked against who manages it, so an
    /// edge cannot move or despawn another edge's entity. That check is what
    /// stands between the edges, since entity ids are region-wide and an edge
    /// can name any of them.
    pub fn apply(&self, step: &mut Step<'_>) -> Applied {
        let cfg = *step.config();
        let commands = std::mem::take(&mut *self.queue.lock().expect("not poisoned"));
        let mut out = Applied::default();
        // Collected here and handed over once. Locking per entity would take a
        // mutex several hundred times for one spawn message.
        let mut fresh: Vec<(EdgeId, EntityId, EntityKind)> = Vec::new();
        let mut gone: Vec<(Option<EdgeId>, EntityId)> = Vec::new();

        // Orphans first, so a stale command later in this same batch cannot
        // move an entity whose edge has already gone.
        for id in self.edges.take_detached() {
            if step.live().contains(id) {
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
                    for (pos, kind) in spawns {
                        if !cfg.contains(pos) {
                            refuse(&mut out, &stats);
                            continue;
                        }
                        let id = step.spawn(pos);
                        if self.edges.claim(edge, id).is_err() {
                            // The edge detached between sending and now.
                            step.despawn(id);
                            refuse(&mut out, &stats);
                            continue;
                        }
                        fresh.push((edge, id, kind));
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
                            || !step.live().contains(id)
                        {
                            refuse(&mut out, &stats);
                            continue;
                        }
                        writes.push((id.index(), pos));
                    }
                }
            }
        }

        let (xs, ys, zs) = step.positions_mut();
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
        //
        // Both loops group their work by edge, so each edge's counter is
        // touched once rather than once per entity.
        let mut per_edge: Vec<Pending> = Vec::new();

        for (edge, id) in gone {
            let Some(viewer) = self.take_watcher(id) else { continue };
            sim.unregister_viewer(viewer);
            sink.unbind(viewer);
            if let Some(edge) = edge {
                let at = group(&mut per_edge, edge);
                per_edge[at].1 -= 1;
            }
            out.unregistered += 1;
        }

        for (edge, id, kind) in fresh {
            // Only an observer costs a viewer. An unattended entity is
            // replicated to whoever can see it and receives nothing itself, so
            // it never enters the per-viewer pipeline at all.
            let viewer = kind.observes().then(|| {
                let viewer = sim.register_viewer(id, limits);
                sink.bind(viewer, id);
                self.set_watcher(id, viewer);
                out.registered += 1;
                viewer
            });
            let at = group(&mut per_edge, edge);
            per_edge[at].1 += i32::from(viewer.is_some());
            per_edge[at].2.push((id, viewer));
        }

        let replies = per_edge;
        let mut body = Vec::new();
        for (edge, observers, list) in replies {
            if let Some(stats) = self.edges.stats(edge) {
                match observers.cmp(&0) {
                    Ordering2::Greater => {
                        stats.observers.fetch_add(observers as usize, Ordering::Relaxed);
                    }
                    Ordering2::Less => {
                        stats.observers.fetch_sub(observers.unsigned_abs() as usize, Ordering::Relaxed);
                    }
                    Ordering2::Equal => {}
                }
            }
            for chunk in list.chunks(MAX_SPAWN_PER_MESSAGE) {
                EntitiesSpawned { entities: chunk.to_vec() }.encode(&mut body);
                // An edge that went away mid-tick is already being torn down.
                if self.edges.send(edge, KIND_ENTITIES_SPAWNED, &body).is_ok() {
                    out.replies += 1;
                }
            }
        }
        out
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

/// One edge's share of a settle: how its observer count moved, and the
/// entities to report back with the viewer registered for each.
type Pending = (EdgeId, i32, Vec<(EntityId, Option<ViewerId>)>);

/// Index of `edge`'s row, appending one if it has none yet. Regions hold a
/// handful of edges, so a scan beats a map.
fn group(rows: &mut Vec<Pending>, edge: EdgeId) -> usize {
    match rows.iter().position(|(at, _, _)| *at == edge) {
        Some(at) => at,
        None => {
            rows.push((edge, 0, Vec::new()));
            rows.len() - 1
        }
    }
}

/// Counts one declined command, on the run total and on the edge that sent it.
fn refuse(out: &mut Applied, stats: &Option<Arc<EdgeStats>>) {
    out.refused += 1;
    if let Some(stats) = stats {
        stats.refused.fetch_add(1, Ordering::Relaxed);
    }
}

/// Sends each viewer's payload to the edge that manages its avatar.
///
/// Cheap to clone: one lives in the [`WorldSimulation`] and one stays with the
/// tick loop, which is what binds a viewer when it registers.
///
/// **Wrap this in [`Handoff`](crate::Handoff).** `send` writes to a socket, and
/// [`PayloadSink`] is called from inside the tick, so an edge that stops reading
/// would otherwise stall the region. `Handoff` turns the tick's side of it into
/// a memory copy and drops rather than queues, which is right for state nobody
/// wants stale.
#[derive(Clone, Debug)]
pub struct EdgeSink {
    shared: Arc<SinkShared>,
}

#[derive(Debug)]
struct SinkShared {
    edges: Arc<Edges>,
    /// Viewer index to avatar entity raw. The routing question is asked about
    /// an entity, and a payload arrives named by its viewer.
    avatars: RwLock<Vec<AtomicU32>>,
    sent: AtomicU64,
    undeliverable: AtomicU64,
    failed: AtomicU64,
}

impl EdgeSink {
    pub fn new(edges: Arc<Edges>) -> EdgeSink {
        EdgeSink {
            shared: Arc::new(SinkShared {
                edges,
                avatars: RwLock::new(Vec::new()),
                sent: AtomicU64::new(0),
                undeliverable: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
        }
    }

    /// Records which entity a viewer watches, so its payloads can be routed.
    pub fn bind(&self, viewer: ViewerId, avatar: EntityId) {
        let mut avatars = self.shared.avatars.write().expect("not poisoned");
        if avatars.len() <= viewer.index() {
            avatars.resize_with(viewer.index() + 1, || AtomicU32::new(NO_AVATAR));
        }
        avatars[viewer.index()].store(avatar.raw(), Ordering::Relaxed);
    }

    /// Forgets a viewer, so a recycled id cannot route to the previous avatar.
    pub fn unbind(&self, viewer: ViewerId) {
        let avatars = self.shared.avatars.read().expect("not poisoned");
        if let Some(slot) = avatars.get(viewer.index()) {
            slot.store(NO_AVATAR, Ordering::Relaxed);
        }
    }

    /// Payloads written to an edge.
    pub fn sent(&self) -> u64 {
        self.shared.sent.load(Ordering::Relaxed)
    }

    /// Payloads for a viewer with no avatar bound, or an avatar no edge
    /// manages. A viewer whose edge detached mid-tick lands here.
    pub fn undeliverable(&self) -> u64 {
        self.shared.undeliverable.load(Ordering::Relaxed)
    }

    /// Payloads whose write to the edge failed.
    pub fn failed(&self) -> u64 {
        self.shared.failed.load(Ordering::Relaxed)
    }

    fn avatar_of(&self, viewer: ViewerId) -> Option<EntityId> {
        let avatars = self.shared.avatars.read().expect("not poisoned");
        match avatars.get(viewer.index()).map(|slot| slot.load(Ordering::Relaxed)) {
            Some(raw) if raw != NO_AVATAR => Some(EntityId::from_raw(raw)),
            _ => None,
        }
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
        // Written straight into the edge's buffer: no payload-sized copy, and
        // no syscall until `flush`. Both mattered; see §The smoke test.
        let named = viewer.raw().to_le_bytes();
        match self.shared.edges.send_parts(edge, KIND_POSITION_UPDATES, &[&named, payload]) {
            Ok(()) => self.shared.sent.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.shared.failed.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Pushes the batch to the edges. One syscall per edge with work waiting,
    /// rather than one per payload.
    fn flush(&self) {
        self.shared.edges.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::region::protocol::RegionId;
    use crate::net::region::server::RegionServer;
    use crate::net::region::{AllowAll, RegionClient};
    use crate::{Fixed, WorldConfig};

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    /// A region server with one edge attached, and the client end of it.
    fn attached() -> (RegionServer, Edge, RegionClient) {
        let server = RegionServer::bind(
            "127.0.0.1:0",
            RegionId::from_raw(1),
            WorldConfig::default(),
            Arc::new(AllowAll),
        )
        .expect("binds");
        let addr = server.local_addr();
        let (edge, client) = std::thread::scope(|scope| {
            let taking = scope.spawn(|| server.accept().expect("attaches"));
            let client = RegionClient::connect(addr, b"").expect("handshake completes");
            (taking.join().expect("thread"), client)
        });
        (server, edge, client)
    }

    #[test]
    fn a_payload_routes_to_the_edge_managing_its_avatar() {
        let (server, edge, _client) = attached();
        let sink = EdgeSink::new(Arc::clone(server.edges()));

        edge.claim(ent(10)).expect("unclaimed");
        sink.bind(ViewerId::from_raw(0), ent(10));

        sink.send(ViewerId::from_raw(0), b"a payload");
        assert_eq!(sink.sent(), 1);
        assert_eq!(sink.undeliverable(), 0);
    }

    #[test]
    fn a_payload_for_an_unbound_viewer_goes_nowhere() {
        let (server, _edge, _client) = attached();
        let sink = EdgeSink::new(Arc::clone(server.edges()));
        sink.send(ViewerId::from_raw(3), b"a payload");
        assert_eq!(sink.sent(), 0);
        assert_eq!(sink.undeliverable(), 1);
    }

    #[test]
    fn a_payload_whose_avatar_no_edge_manages_goes_nowhere() {
        let (server, _edge, _client) = attached();
        let sink = EdgeSink::new(Arc::clone(server.edges()));
        // Bound, but never claimed by any edge.
        sink.bind(ViewerId::from_raw(0), ent(10));
        sink.send(ViewerId::from_raw(0), b"a payload");
        assert_eq!(sink.undeliverable(), 1);
    }

    #[test]
    fn unbinding_stops_a_recycled_viewer_routing_to_the_old_avatar() {
        let (server, edge, _client) = attached();
        let sink = EdgeSink::new(Arc::clone(server.edges()));
        edge.claim(ent(10)).expect("unclaimed");

        sink.bind(ViewerId::from_raw(0), ent(10));
        sink.unbind(ViewerId::from_raw(0));
        sink.send(ViewerId::from_raw(0), b"a payload");

        assert_eq!(sink.sent(), 0);
        assert_eq!(sink.undeliverable(), 1, "a dropped viewer routes nowhere");
    }

    #[test]
    fn a_detached_edge_takes_its_routes_with_it() {
        let (server, edge, _client) = attached();
        let sink = EdgeSink::new(Arc::clone(server.edges()));
        edge.claim(ent(10)).expect("unclaimed");
        sink.bind(ViewerId::from_raw(0), ent(10));

        drop(edge);
        sink.send(ViewerId::from_raw(0), b"a payload");
        assert_eq!(sink.undeliverable(), 1);
    }

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
        let sent = Pos3::new(Fixed::from_millis(7, 500), Fixed::ZERO, Fixed::from_raw(1));
        let m = MoveEntities { moves: vec![(ent(1), sent)] };
        let mut body = Vec::new();
        m.encode(&mut body);
        let back = MoveEntities::decode(&body).expect("well formed");
        assert_eq!(back.moves[0].1, sent);
    }
}
