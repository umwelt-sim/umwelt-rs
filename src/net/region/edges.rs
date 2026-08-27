//! The edges relaying for one region, and the entities each one manages.
//!
//! An edge claims the entities whose game clients it holds the connections for.
//! [`Edges::edge_for`] answers which edge manages a given entity, which is what
//! a [`PayloadSink`](crate::PayloadSink) needs in order to address a payload. It
//! is called once per served viewer per tick, so it takes a read lock and no
//! more; claiming and releasing take the write lock and happen when a game
//! client arrives or leaves.
//!
//! Edges arrive by name rather than by connection. Under NATS there is nothing
//! to accept, so an edge becomes known the first time it sends a command, and
//! stops being known when it has been silent long enough. See
//! `docs/adr/0001`.

use core::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::entity::EntityId;
use crate::net::error::NetError;

/// No edge manages this entity. Edge ids are dense from zero, so the top of
/// the range is unreachable.
const UNOWNED: u32 = u32::MAX;

/// What an edge calls itself, and the token that names it in a subject.
///
/// Chosen by the edge and stable across restarts of any region, unlike
/// [`EdgeId`], which is one region's dense internal index.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeName(String);

impl EdgeName {
    /// # Errors
    ///
    /// If the name is empty, longer than 64 bytes, or holds a character that
    /// would change how a subject parses. Subjects are dot-separated and NATS
    /// reads `*` and `>` as wildcards, so a name carrying one of those could
    /// address subjects belonging to another edge.
    pub fn new(name: impl Into<String>) -> Result<EdgeName, NetError> {
        let name = name.into();
        if name.is_empty() || name.len() > 64 {
            return Err(NetError::BadEdgeName("length must be 1 to 64 bytes"));
        }
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(NetError::BadEdgeName("only letters, digits, dash and underscore"));
        }
        Ok(EdgeName(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EdgeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for EdgeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One edge's index within one region. Dense and reusable.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EdgeId(u32);

impl EdgeId {
    #[inline]
    pub const fn from_raw(raw: u32) -> EdgeId {
        EdgeId(raw)
    }

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

impl fmt::Debug for EdgeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Ed{}", self.0)
    }
}

/// Why an entity could not be claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// Another edge already manages this entity. Two edges managing one avatar
    /// would send one viewer's packets to two clients.
    AlreadyClaimed { entity: EntityId, by: EdgeId },
    /// That edge is not attached.
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

/// Running totals for one attached edge, cumulative since it was admitted.
#[derive(Debug, Default)]
pub(crate) struct EdgeStats {
    pub(crate) messages: AtomicU64,
    pub(crate) payloads: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) observers: AtomicUsize,
    pub(crate) refused: AtomicU64,
}

/// One edge's state, sampled together under a single lock.
#[derive(Clone, Debug)]
pub struct EdgeView {
    pub id: EdgeId,
    pub name: EdgeName,
    /// How long this edge has been attached.
    pub uptime: Duration,
    /// Since its last command. An edge silent past the region's expiry is
    /// dropped.
    pub silent: Duration,
    pub entities: usize,
    pub observers: usize,
    pub payloads: u64,
    pub bytes: u64,
    pub messages: u64,
    pub refused: u64,
}

#[derive(Debug)]
struct EdgeRecord {
    name: EdgeName,
    since: Instant,
    heard: Instant,
    entities: Vec<EntityId>,
    stats: Arc<EdgeStats>,
}

/// The edges relaying for one region.
#[derive(Debug, Default)]
pub struct Edges {
    /// Indexed by [`EdgeId`]. `None` is a free slot awaiting reuse.
    slots: Mutex<Vec<Option<EdgeRecord>>>,
    /// Indexed by entity slot, holding an edge id raw or [`UNOWNED`]. Separate
    /// from `slots` so a routing lookup never waits on a claim.
    owners: RwLock<Vec<u32>>,
    /// Entities orphaned by an edge going away, waiting for the tick loop to
    /// despawn them and drop their viewers.
    detached: Mutex<Vec<EntityId>>,
    live: AtomicUsize,
    accepted: AtomicU64,
    /// Bumped whenever the set changes. A sink caches one subject per edge and
    /// rebuilds when this moves, rather than taking the set's lock for every
    /// payload it addresses.
    generation: AtomicU64,
}

impl Edges {
    pub fn new() -> Edges {
        Edges::default()
    }

    // -- the set ----------------------------------------------------------

    /// Changes to the set, counted. A caller holding anything derived from
    /// membership rebuilds when this moves.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Edges attached right now.
    #[inline]
    pub fn len(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Edges admitted since this region started, including those since gone.
    #[inline]
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }

    /// The id this region uses for an edge, admitting it if it is new.
    ///
    /// Called when a command arrives, which is the only way a region learns an
    /// edge exists.
    pub fn admit(&self, name: &EdgeName) -> EdgeId {
        let mut slots = self.slots.lock().expect("not poisoned");
        if let Some(at) = slots.iter().position(|h| h.as_ref().is_some_and(|r| &r.name == name)) {
            let rec = slots[at].as_mut().expect("just matched");
            rec.heard = Instant::now();
            return EdgeId::from_raw(at as u32);
        }
        let at = match slots.iter().position(|held| held.is_none()) {
            Some(free) => free,
            None => {
                slots.push(None);
                slots.len() - 1
            }
        };
        let now = Instant::now();
        slots[at] = Some(EdgeRecord {
            name: name.clone(),
            since: now,
            heard: now,
            entities: Vec::new(),
            stats: Arc::new(EdgeStats::default()),
        });
        self.live.fetch_add(1, Ordering::Relaxed);
        self.accepted.fetch_add(1, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Release);
        EdgeId::from_raw(at as u32)
    }

    /// Drops every edge silent for longer than `after`, orphaning what it
    /// managed.
    ///
    /// A closed socket used to say an edge had gone. Nothing says so now, so
    /// silence does. An edge holding entities sends a keepalive when it has
    /// nothing else to say, and one under load sends moves every tick.
    ///
    /// Returns how many were dropped.
    pub fn expire(&self, after: Duration) -> usize {
        let stale: Vec<EdgeId> = {
            let slots = self.slots.lock().expect("not poisoned");
            slots
                .iter()
                .enumerate()
                .filter_map(|(i, held)| {
                    let rec = held.as_ref()?;
                    (rec.heard.elapsed() > after).then(|| EdgeId::from_raw(i as u32))
                })
                .collect()
        };
        for id in &stale {
            self.detach(*id);
        }
        stale.len()
    }

    pub fn name(&self, id: EdgeId) -> Option<EdgeName> {
        let slots = self.slots.lock().expect("not poisoned");
        slots.get(id.index()).and_then(|h| h.as_ref()).map(|r| r.name.clone())
    }

    pub(crate) fn stats(&self, edge: EdgeId) -> Option<Arc<EdgeStats>> {
        let slots = self.slots.lock().expect("not poisoned");
        slots.get(edge.index()).and_then(|h| h.as_ref()).map(|r| Arc::clone(&r.stats))
    }

    /// Every attached edge with its counters, sampled at one moment.
    pub fn view(&self) -> Vec<EdgeView> {
        let slots = self.slots.lock().expect("not poisoned");
        slots
            .iter()
            .enumerate()
            .filter_map(|(i, held)| {
                let rec = held.as_ref()?;
                Some(EdgeView {
                    id: EdgeId::from_raw(i as u32),
                    name: rec.name.clone(),
                    uptime: rec.since.elapsed(),
                    silent: rec.heard.elapsed(),
                    entities: rec.entities.len(),
                    observers: rec.stats.observers.load(Ordering::Relaxed),
                    payloads: rec.stats.payloads.load(Ordering::Relaxed),
                    bytes: rec.stats.bytes.load(Ordering::Relaxed),
                    messages: rec.stats.messages.load(Ordering::Relaxed),
                    refused: rec.stats.refused.load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    // -- who manages what -------------------------------------------------

    /// Records that `edge` manages `entity`. Repeating a claim the same edge
    /// already holds changes nothing.
    pub fn claim(&self, edge: EdgeId, entity: EntityId) -> Result<(), ClaimError> {
        let mut slots = self.slots.lock().expect("not poisoned");
        let rec = slots
            .get_mut(edge.index())
            .and_then(|held| held.as_mut())
            .ok_or(ClaimError::NoSuchEdge(edge))?;

        let mut owners = self.owners.write().expect("not poisoned");
        if owners.len() <= entity.index() {
            owners.resize(entity.index() + 1, UNOWNED);
        }
        let held_by = owners[entity.index()];
        if held_by == edge.raw() {
            return Ok(());
        }
        if held_by != UNOWNED {
            return Err(ClaimError::AlreadyClaimed { entity, by: EdgeId::from_raw(held_by) });
        }
        owners[entity.index()] = edge.raw();
        rec.entities.push(entity);
        Ok(())
    }

    /// Gives up an entity, whichever edge held it. Returns the edge that had
    /// it, or `None` if nobody did.
    pub fn release(&self, entity: EntityId) -> Option<EdgeId> {
        let mut slots = self.slots.lock().expect("not poisoned");
        let mut owners = self.owners.write().expect("not poisoned");
        let held_by = std::mem::replace(owners.get_mut(entity.index())?, UNOWNED);
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

    /// Which edge manages this entity, if any. A payload built for a viewer is
    /// addressed to the edge holding that viewer's client connection, and this
    /// is that lookup. Takes a read lock only.
    #[inline]
    pub fn edge_for(&self, entity: EntityId) -> Option<EdgeId> {
        let owners = self.owners.read().expect("not poisoned");
        let held_by = *owners.get(entity.index())?;
        (held_by != UNOWNED).then(|| EdgeId::from_raw(held_by))
    }

    /// How many entities one edge manages.
    pub fn entity_count(&self, edge: EdgeId) -> usize {
        let slots = self.slots.lock().expect("not poisoned");
        slots.get(edge.index()).and_then(|h| h.as_ref()).map(|r| r.entities.len()).unwrap_or(0)
    }

    /// Entities orphaned by an edge going away, taken once. The tick loop
    /// despawns them: an entity whose edge has gone has no client behind it.
    pub fn take_detached(&self) -> Vec<EntityId> {
        std::mem::take(&mut *self.detached.lock().expect("not poisoned"))
    }

    fn detach(&self, id: EdgeId) {
        let mut slots = self.slots.lock().expect("not poisoned");
        let Some(Some(rec)) = slots.get_mut(id.index()).map(|held| held.take()) else {
            return;
        };
        let mut owners = self.owners.write().expect("not poisoned");
        for entity in &rec.entities {
            // Only clear what this edge still held: a claim may have moved on
            // after a release this list has not seen.
            if let Some(slot) = owners.get_mut(entity.index())
                && *slot == id.raw()
            {
                *slot = UNOWNED;
            }
        }
        drop(owners);
        if !rec.entities.is_empty() {
            self.detached.lock().expect("not poisoned").extend_from_slice(&rec.entities);
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    fn name(s: &str) -> EdgeName {
        EdgeName::new(s).expect("valid")
    }

    #[test]
    fn an_edge_name_refuses_what_would_break_a_subject() {
        assert!(EdgeName::new("edge-1").is_ok());
        assert!(EdgeName::new("edge_1").is_ok());
        // A dot would split into another subject token, and the wildcards would
        // reach subjects belonging to other edges.
        assert!(EdgeName::new("edge.1").is_err());
        assert!(EdgeName::new("*").is_err());
        assert!(EdgeName::new(">").is_err());
        assert!(EdgeName::new("").is_err());
        assert!(EdgeName::new("x".repeat(65)).is_err());
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
        assert!(edges.view().is_empty());
        assert_eq!(edges.edge_for(ent(0)), None);
    }

    #[test]
    fn an_edge_is_admitted_once_however_often_it_speaks() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        assert_eq!(edges.admit(&name("alpha")), a, "the same name is the same edge");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges.accepted(), 1);

        let b = edges.admit(&name("beta"));
        assert_ne!(a, b);
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn silence_past_the_expiry_drops_an_edge() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        edges.claim(a, ent(10)).expect("unclaimed");

        assert_eq!(edges.expire(Duration::from_secs(60)), 0, "not silent long enough");
        assert_eq!(edges.len(), 1);

        assert_eq!(edges.expire(Duration::ZERO), 1);
        assert!(edges.is_empty());
        assert_eq!(edges.edge_for(ent(10)), None, "its entities stop routing");
        assert_eq!(edges.take_detached(), vec![ent(10)], "and go to the tick loop");
    }

    #[test]
    fn speaking_again_resets_the_silence() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        std::thread::sleep(Duration::from_millis(20));
        edges.admit(&name("alpha")); // a command arriving
        assert_eq!(edges.expire(Duration::from_millis(15)), 0);
        assert_eq!(edges.len(), 1);
        let _ = a;
    }

    #[test]
    fn a_freed_id_is_reused_and_inherits_nothing() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.expire(Duration::ZERO);

        let reused = edges.admit(&name("gamma"));
        assert_eq!(reused, a, "the same slot came back");
        assert_eq!(edges.entity_count(reused), 0, "with none of the old claims");
        assert_eq!(edges.edge_for(ent(10)), None);
    }

    #[test]
    fn routing_finds_the_edge_that_manages_an_entity() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        let b = edges.admit(&name("beta"));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(b, ent(20)).expect("unclaimed");

        assert_eq!(edges.edge_for(ent(10)), Some(a));
        assert_eq!(edges.edge_for(ent(20)), Some(b));
        assert_eq!(edges.edge_for(ent(30)), None, "an unmanaged entity routes nowhere");
    }

    #[test]
    fn two_edges_cannot_manage_one_entity() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        let b = edges.admit(&name("beta"));
        edges.claim(a, ent(10)).expect("unclaimed");

        assert_eq!(
            edges.claim(b, ent(10)),
            Err(ClaimError::AlreadyClaimed { entity: ent(10), by: a })
        );
        assert_eq!(edges.edge_for(ent(10)), Some(a), "the first claim stands");
        assert_eq!(edges.entity_count(b), 0);
    }

    #[test]
    fn reclaiming_an_entity_this_edge_already_has_changes_nothing() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(10)).expect("the same edge may repeat itself");
        assert_eq!(edges.entity_count(a), 1, "and it is not counted twice");
    }

    #[test]
    fn claiming_through_a_stale_edge_id_is_refused() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        edges.expire(Duration::ZERO);
        assert_eq!(edges.claim(a, ent(10)), Err(ClaimError::NoSuchEdge(a)));
        assert_eq!(edges.edge_for(ent(10)), None);
    }

    #[test]
    fn releasing_an_entity_stops_it_routing() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.claim(a, ent(11)).expect("unclaimed");

        assert_eq!(edges.release(ent(10)), Some(a));
        assert_eq!(edges.edge_for(ent(10)), None);
        assert_eq!(edges.entity_count(a), 1, "the rest are untouched");
        assert_eq!(edges.release(ent(10)), None, "releasing twice reports nobody");
        assert_eq!(edges.release(ent(99)), None, "so does releasing what nobody held");
    }

    #[test]
    fn a_released_entity_can_move_to_another_edge() {
        let edges = Edges::new();
        let a = edges.admit(&name("alpha"));
        let b = edges.admit(&name("beta"));
        edges.claim(a, ent(10)).expect("unclaimed");
        edges.release(ent(10));
        edges.claim(b, ent(10)).expect("free again");
        assert_eq!(edges.edge_for(ent(10)), Some(b));
        assert_eq!(edges.entity_count(a), 0);
    }

    #[test]
    fn the_set_takes_traffic_from_several_threads() {
        let edges = Edges::new();
        let ids: Vec<EdgeId> =
            (0..8).map(|n| edges.admit(&name(&format!("edge{n}")))).collect();
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
        for id in &ids {
            assert_eq!(edges.entity_count(*id), 100);
        }
        assert_eq!(edges.len(), 8);
    }
}
