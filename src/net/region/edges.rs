//! The edges connected to one region, and the entities each one manages.
//!
//! A region simulation does not think about sockets one at a time. It has a set
//! of edges relaying for it, that set changes as edges come and go, and the
//! things that need to address one — a payload sink choosing where to write, an
//! operator asking who is attached — need something to ask. [`Edges`] is that
//! something.
//!
//! [`Edge`] is one connected edge. Holding one keeps it attached; dropping it
//! closes the link, frees the id, and releases every entity it managed.
//!
//! # Why an edge owns entities
//!
//! A game client connects to an edge, and the edge registers a viewer with the
//! region for that client's avatar. The region then produces a payload per
//! viewer per tick and hands each to a
//! [`PayloadSink`](crate::PayloadSink) — which has to send it to the edge
//! holding that client's connection, and to no other. Which edge that is, is a
//! fact only this set knows.
//!
//! So each edge claims the entities it manages, and [`Edges::edge_for`]
//! answers the routing question. A sink resolves a served
//! [`ViewerId`](crate::ViewerId) to its avatar with
//! [`WorldSimulation::avatar_of`](crate::WorldSimulation::avatar_of), then
//! that avatar to an edge with `edge_for`.
//!
//! **The lookup is on the sink's path, which is on the tick's path.** Every
//! served viewer costs one, from every worker thread at once, so `edge_for`
//! reads an atomic under a shared lock and takes no exclusive lock at all.
//! Claiming and releasing do take one, and they happen when a game client
//! arrives or leaves rather than per tick. None of this is measured; §Payloads
//! leave through a sink is why the shape was chosen that way regardless.
//!
//! Ids are dense and reusable, the same bargain [`ViewerId`](crate::ViewerId)
//! makes and for the same reason: a recycled [`EdgeId`] names a different
//! connection with no state carried over, so there is nothing for a stale
//! reference to alias.

use core::fmt;
use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::entity::EntityId;
use crate::net::error::NetError;

/// No edge manages this entity.
///
/// Edge ids are dense from zero, so the top of the range is unreachable and
/// free to reserve. [`GhostTable`](crate::GhostTable) reserves the same value
/// for the same reason.
const UNOWNED: u32 = u32::MAX;

/// One connected edge's identity within one region server.
///
/// Dense and reusable. Not a network address, and not stable across
/// reconnection: an edge that drops and comes back is a new `EdgeId`.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeId(u32);

impl EdgeId {
    #[inline(always)]
    pub const fn from_raw(raw: u32) -> EdgeId {
        EdgeId(raw)
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

impl fmt::Debug for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed{}", self.0)
    }
}

/// Why an entity could not be claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// Another edge already manages this entity.
    ///
    /// Two edges managing one avatar would mean two clients being sent the same
    /// viewer's packets, so this is a bug to surface rather than a race to
    /// resolve by taking the last writer.
    AlreadyClaimed { entity: EntityId, by: EdgeId },
    /// That edge is not attached. A stale [`EdgeId`] naming a slot that has
    /// since been freed or reused reaches this.
    NoSuchEdge(EdgeId),
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaimError::AlreadyClaimed { entity, by } => {
                write!(f, "{entity:?} is already managed by {by:?}")
            }
            ClaimError::NoSuchEdge(id) => write!(f, "{id:?} is not attached"),
        }
    }
}

impl std::error::Error for ClaimError {}

/// What one attached edge is, from the region's side.
#[derive(Debug)]
struct EdgeRecord {
    peer: SocketAddr,
    /// Entities whose clients this edge holds the connections for.
    entities: Vec<EntityId>,
}

/// The edges relaying for one region, and what each of them manages.
///
/// Shared between the accept loop, the thread holding each edge, and anything
/// asking who is attached, so every method takes `&self`.
#[derive(Debug, Default)]
pub struct Edges {
    /// Indexed by [`EdgeId`]. `None` is a free slot awaiting reuse.
    slots: Mutex<Vec<Option<EdgeRecord>>>,
    /// Indexed by entity slot, holding an edge id raw or [`UNOWNED`]. Separate
    /// from `slots` so a routing lookup never waits on a claim.
    owners: RwLock<Vec<AtomicU32>>,
    live: AtomicUsize,
    accepted: AtomicU64,
}

impl Edges {
    pub fn new() -> Edges {
        Edges::default()
    }

    // -- the set ----------------------------------------------------------

    /// Edges attached right now.
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Edges admitted since this region server was bound, including those that
    /// have since gone.
    #[inline(always)]
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// Where one edge is connected from, or `None` if that id is not currently
    /// in use.
    pub fn peer(&self, id: EdgeId) -> Option<SocketAddr> {
        let slots = self.slots.lock().expect("not poisoned");
        slots.get(id.index()).and_then(|held| held.as_ref()).map(|rec| rec.peer)
    }

    /// Every edge attached right now, for an operator view.
    ///
    /// Allocates and takes the lock, so this is a reporting path rather than
    /// something to call per tick.
    pub fn connected(&self) -> Vec<(EdgeId, SocketAddr)> {
        let slots = self.slots.lock().expect("not poisoned");
        slots
            .iter()
            .enumerate()
            .filter_map(|(i, held)| {
                held.as_ref().map(|rec| (EdgeId::from_raw(i as u32), rec.peer))
            })
            .collect()
    }

    // -- who manages what -------------------------------------------------

    /// Records that `edge` manages `entity`.
    ///
    /// Claiming an entity the same edge already manages is accepted and changes
    /// nothing, so a caller resending a claim does not have to check first.
    pub fn claim(&self, edge: EdgeId, entity: EntityId) -> Result<(), ClaimError> {
        let mut slots = self.slots.lock().expect("not poisoned");
        let rec = slots
            .get_mut(edge.index())
            .and_then(|held| held.as_mut())
            .ok_or(ClaimError::NoSuchEdge(edge))?;

        let mut owners = self.owners.write().expect("not poisoned");
        if owners.len() <= entity.index() {
            owners.resize_with(entity.index() + 1, || AtomicU32::new(UNOWNED));
        }
        let held_by = owners[entity.index()].load(Ordering::Relaxed);
        if held_by == edge.raw() {
            return Ok(());
        }
        if held_by != UNOWNED {
            return Err(ClaimError::AlreadyClaimed { entity, by: EdgeId::from_raw(held_by) });
        }
        owners[entity.index()].store(edge.raw(), Ordering::Relaxed);
        rec.entities.push(entity);
        Ok(())
    }

    /// Gives up an entity, whichever edge held it.
    ///
    /// Returns the edge that had it, or `None` if nobody did. Called when a
    /// game client disconnects, or when the entity despawns.
    pub fn release(&self, entity: EntityId) -> Option<EdgeId> {
        let mut slots = self.slots.lock().expect("not poisoned");
        let owners = self.owners.read().expect("not poisoned");
        let held_by = owners.get(entity.index())?.swap(UNOWNED, Ordering::Relaxed);
        drop(owners);
        if held_by == UNOWNED {
            return None;
        }
        let edge = EdgeId::from_raw(held_by);
        if let Some(Some(rec)) = slots.get_mut(edge.index()) {
            rec.entities.retain(|held| *held != entity);
        }
        Some(edge)
    }

    /// Which edge manages this entity, if any.
    ///
    /// **This is the routing question**, and the reason the mapping exists: a
    /// payload built for a viewer goes to the edge holding that viewer's client
    /// connection. Takes a shared lock and one relaxed load, and never waits on
    /// a claim.
    #[inline]
    pub fn edge_for(&self, entity: EntityId) -> Option<EdgeId> {
        let owners = self.owners.read().expect("not poisoned");
        let held_by = owners.get(entity.index())?.load(Ordering::Relaxed);
        (held_by != UNOWNED).then(|| EdgeId::from_raw(held_by))
    }

    /// The entities one edge manages.
    ///
    /// Allocates and takes the lock. A reporting and handoff path, not a
    /// per-tick one.
    pub fn entities(&self, edge: EdgeId) -> Vec<EntityId> {
        let slots = self.slots.lock().expect("not poisoned");
        slots
            .get(edge.index())
            .and_then(|held| held.as_ref())
            .map(|rec| rec.entities.clone())
            .unwrap_or_default()
    }

    /// How many entities one edge manages, without building the list.
    pub fn entity_count(&self, edge: EdgeId) -> usize {
        let slots = self.slots.lock().expect("not poisoned");
        slots
            .get(edge.index())
            .and_then(|held| held.as_ref())
            .map(|rec| rec.entities.len())
            .unwrap_or(0)
    }

    // -- attachment -------------------------------------------------------

    /// Takes the lowest free id, so the set stays dense.
    fn register(&self, peer: SocketAddr) -> EdgeId {
        let mut slots = self.slots.lock().expect("not poisoned");
        let at = match slots.iter().position(|held| held.is_none()) {
            Some(free) => free,
            None => {
                slots.push(None);
                slots.len() - 1
            }
        };
        slots[at] = Some(EdgeRecord { peer, entities: Vec::new() });
        self.live.fetch_add(1, Ordering::Relaxed);
        self.accepted.fetch_add(1, Ordering::Relaxed);
        EdgeId::from_raw(at as u32)
    }

    /// Detaches an edge and releases everything it managed.
    ///
    /// The entities are not despawned. They stop being routable, which is the
    /// truth: there is no longer a connection to send their clients anything
    /// over.
    fn deregister(&self, id: EdgeId) {
        let mut slots = self.slots.lock().expect("not poisoned");
        let Some(Some(rec)) = slots.get_mut(id.index()).map(|held| held.take()) else {
            return;
        };
        let owners = self.owners.read().expect("not poisoned");
        for entity in &rec.entities {
            if let Some(slot) = owners.get(entity.index()) {
                // Only clear what this edge still held: a claim may have moved
                // on after a release this list has not seen.
                let _ = slot.compare_exchange(
                    id.raw(),
                    UNOWNED,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
    }

    /// Admits a connected peer to the set.
    ///
    /// Called by [`RegionServer`](crate::net::RegionServer) once a handshake
    /// has completed, which is the only point an edge exists.
    pub(crate) fn admit(self: &Arc<Edges>, stream: TcpStream, peer: SocketAddr) -> Edge {
        let id = self.register(peer);
        Edge { id, peer, stream, edges: Arc::clone(self) }
    }
}

/// One attached edge, past the handshake.
///
/// Holding one keeps the edge attached and counted in [`Edges::len`]. Dropping
/// it closes the link, frees the id, and releases every entity it managed.
///
/// There is nothing to carry yet. The state payloads
/// [`PacketWriter`](crate::PacketWriter) assembles do not travel this link, and
/// the sink that would route them here is not built.
#[derive(Debug)]
pub struct Edge {
    id: EdgeId,
    peer: SocketAddr,
    stream: TcpStream,
    edges: Arc<Edges>,
}

impl Edge {
    #[inline(always)]
    pub fn id(&self) -> EdgeId {
        self.id
    }

    #[inline(always)]
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    #[inline(always)]
    pub fn stream(&self) -> &TcpStream {
        &self.stream
    }

    /// The set this edge belongs to.
    #[inline(always)]
    pub fn edges(&self) -> &Arc<Edges> {
        &self.edges
    }

    /// Takes over an entity, meaning this edge holds its client's connection.
    pub fn claim(&self, entity: EntityId) -> Result<(), ClaimError> {
        self.edges.claim(self.id, entity)
    }

    /// Gives up an entity. Returns whether this edge was the one holding it.
    pub fn release(&self, entity: EntityId) -> bool {
        self.edges.release(entity) == Some(self.id)
    }

    /// The entities this edge manages.
    pub fn entities(&self) -> Vec<EntityId> {
        self.edges.entities(self.id)
    }

    /// Holds the link open until the edge closes it.
    ///
    /// Reads and discards, since nothing speaks on this link yet. This is what
    /// an edge does in the meantime, and it is the placeholder the payload path
    /// replaces.
    pub fn wait_for_close(&mut self) -> Result<(), NetError> {
        let mut scratch = [0u8; 256];
        loop {
            match self.stream.read(&mut scratch) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(NetError::Io(e)),
            }
        }
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        self.edges.deregister(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    #[test]
    fn an_edge_id_round_trips() {
        assert_eq!(EdgeId::from_raw(9001).raw(), 9001);
        assert_eq!(EdgeId::from_raw(9001).index(), 9001usize);
        assert_eq!(size_of::<EdgeId>(), size_of::<u32>());
        assert_eq!(format!("{:?}", EdgeId::from_raw(3)), "Ed3");
    }

    #[test]
    fn a_new_set_holds_nothing() {
        let edges = Edges::new();
        assert!(edges.is_empty());
        assert_eq!(edges.accepted(), 0);
        assert!(edges.connected().is_empty());
        assert_eq!(edges.peer(EdgeId::from_raw(0)), None);
        assert_eq!(edges.edge_for(ent(0)), None);
    }

    #[test]
    fn ids_are_handed_out_densely() {
        let edges = Edges::new();
        for n in 0..4u32 {
            assert_eq!(edges.register(addr(9000 + n as u16)), EdgeId::from_raw(n));
        }
        assert_eq!(edges.len(), 4);
        assert_eq!(edges.accepted(), 4);
    }

    #[test]
    fn a_freed_id_is_reused() {
        let edges = Edges::new();
        for n in 0..3u16 {
            edges.register(addr(9000 + n));
        }
        edges.deregister(EdgeId::from_raw(1));
        assert_eq!(edges.len(), 2);
        assert_eq!(
            edges.register(addr(9999)),
            EdgeId::from_raw(1),
            "the lowest free slot is taken first"
        );
        assert_eq!(edges.accepted(), 4, "reuse is still an admission");
    }

    #[test]
    fn deregistering_twice_counts_once() {
        let edges = Edges::new();
        let id = edges.register(addr(9000));
        edges.deregister(id);
        edges.deregister(id);
        assert_eq!(edges.len(), 0, "the count must not go negative");
    }

    #[test]
    fn the_set_reports_who_is_attached() {
        let edges = Edges::new();
        edges.register(addr(9000));
        let second = edges.register(addr(9001));
        edges.register(addr(9002));
        edges.deregister(second);

        assert_eq!(edges.peer(EdgeId::from_raw(0)), Some(addr(9000)));
        assert_eq!(edges.peer(second), None, "a freed id names nobody");

        let attached = edges.connected();
        assert_eq!(attached.len(), 2);
        assert_eq!(attached[0], (EdgeId::from_raw(0), addr(9000)));
        assert_eq!(attached[1], (EdgeId::from_raw(2), addr(9002)));
    }

    #[test]
    fn an_edge_manages_the_entities_it_claims() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        let b = edges.register(addr(9001));

        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(11)).expect("unclaimed");
        edges.claim(b, ent(20)).expect("unclaimed");

        assert_eq!(edges.entities(a), vec![ent(10), ent(11)]);
        assert_eq!(edges.entities(b), vec![ent(20)]);
        assert_eq!(edges.entity_count(a), 2);
    }

    #[test]
    fn routing_finds_the_edge_that_manages_an_entity() {
        // The lookup a sink makes for every payload it is handed.
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        let b = edges.register(addr(9001));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(b, ent(20)).expect("unclaimed");

        assert_eq!(edges.edge_for(ent(10)), Some(a));
        assert_eq!(edges.edge_for(ent(20)), Some(b));
        assert_eq!(edges.edge_for(ent(30)), None, "an unmanaged entity routes nowhere");
    }

    #[test]
    fn two_edges_cannot_manage_one_entity() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        let b = edges.register(addr(9001));
        edges.claim(a, ent(10)).expect("unclaimed");

        assert_eq!(
            edges.claim(b, ent(10)),
            Err(ClaimError::AlreadyClaimed { entity: ent(10), by: a })
        );
        assert_eq!(edges.edge_for(ent(10)), Some(a), "the first claim stands");
        assert!(edges.entities(b).is_empty());
    }

    #[test]
    fn reclaiming_an_entity_this_edge_already_has_changes_nothing() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(10)).expect("the same edge may repeat itself");
        assert_eq!(edges.entities(a), vec![ent(10)], "and it is not listed twice");
    }

    #[test]
    fn claiming_through_a_stale_edge_id_is_refused() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        edges.deregister(a);
        assert_eq!(edges.claim(a, ent(10)), Err(ClaimError::NoSuchEdge(a)));
        assert_eq!(edges.edge_for(ent(10)), None);
    }

    #[test]
    fn releasing_an_entity_stops_it_routing() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(11)).expect("unclaimed");

        assert_eq!(edges.release(ent(10)), Some(a));
        assert_eq!(edges.edge_for(ent(10)), None);
        assert_eq!(edges.entities(a), vec![ent(11)], "the rest are untouched");

        assert_eq!(edges.release(ent(10)), None, "releasing twice reports nobody");
        assert_eq!(edges.release(ent(99)), None, "so does releasing what nobody held");
    }

    #[test]
    fn a_released_entity_can_move_to_another_edge() {
        // A game client reconnecting through a different edge.
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        let b = edges.register(addr(9001));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.release(ent(10));
        edges.claim(b, ent(10)).expect("free again");
        assert_eq!(edges.edge_for(ent(10)), Some(b));
        assert!(edges.entities(a).is_empty());
    }

    #[test]
    fn detaching_an_edge_releases_everything_it_managed() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        let b = edges.register(addr(9001));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(11)).expect("unclaimed");
        edges.claim(b, ent(20)).expect("unclaimed");

        edges.deregister(a);
        assert_eq!(edges.edge_for(ent(10)), None);
        assert_eq!(edges.edge_for(ent(11)), None);
        assert_eq!(edges.edge_for(ent(20)), Some(b), "another edge is unaffected");
        assert!(edges.entities(a).is_empty());
    }

    #[test]
    fn a_reused_id_does_not_inherit_the_previous_edges_entities() {
        let edges = Edges::new();
        let a = edges.register(addr(9000));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.deregister(a);

        let reused = edges.register(addr(9001));
        assert_eq!(reused, a, "the same slot came back");
        assert!(edges.entities(reused).is_empty(), "with none of the old claims");
        assert_eq!(edges.edge_for(ent(10)), None);
    }

    #[test]
    fn routing_survives_claims_from_several_threads() {
        let edges = Edges::new();
        let ids: Vec<EdgeId> = (0..8).map(|n| edges.register(addr(9000 + n))).collect();

        std::thread::scope(|scope| {
            for (t, id) in ids.iter().enumerate() {
                let edges = &edges;
                scope.spawn(move || {
                    for n in 0..100u32 {
                        let entity = ent(t as u32 * 100 + n);
                        edges.claim(*id, entity).expect("each thread owns its range");
                        assert_eq!(edges.edge_for(entity), Some(*id));
                    }
                });
            }
        });

        for (t, id) in ids.iter().enumerate() {
            assert_eq!(edges.entity_count(*id), 100);
            assert_eq!(edges.edge_for(ent(t as u32 * 100)), Some(*id));
        }
    }

    #[test]
    fn the_set_takes_registrations_from_several_threads() {
        let edges = Edges::new();
        std::thread::scope(|scope| {
            for t in 0..8u16 {
                let edges = &edges;
                scope.spawn(move || {
                    for n in 0..50u16 {
                        let id = edges.register(addr(9000 + t * 50 + n));
                        edges.deregister(id);
                    }
                });
            }
        });
        assert_eq!(edges.len(), 0);
        assert_eq!(edges.accepted(), 400);
        assert!(edges.connected().is_empty());
    }
}
