//! What an edge holds, and what a consumer does with it.
//!
//! [`EdgeHandle`] is cheap to clone and callable from anywhere: a timer, a
//! task, another client's callback. It is deliberately not a capability object
//! handed to a callback, because nothing an edge does is valid only at one
//! moment, and an edge that could only speak from inside a callback would force
//! a consumer to queue its own work until some unrelated event fired. See
//! `docs/adr/0006`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::entity::EntityId;
use crate::game::EdgeGame;
use crate::net::control::EdgeLoad;
use crate::net::edge::ids::{ClientId, EntityKey, Mint};
use crate::net::edge::protocol::{Framer, ToClient};
use crate::net::error::NetError;
use crate::net::region::RegionClient;
use crate::net::region::protocol::{EntityKind, Presence, RegionId, Spawn};
use crate::pos::Pos3;

/// What this edge has done since it started.
///
/// Cumulative rather than per-span, so reading it takes nothing away from the
/// heartbeat, which publishes differences.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeStats {
    /// Game clients connected right now.
    pub clients: u32,
    /// Entities this edge manages, across every region.
    pub entities: u32,
    /// How many of those have a client behind them.
    pub observers: u32,
    pub relayed: u64,
    /// State packets that reached no client: it had gone, or the datagram was
    /// refused.
    pub undeliverable: u64,
    pub commands: u64,
    /// Commands this edge declined, for an unknown handle or one belonging to
    /// another connection.
    pub refused: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Counters {
    relayed: AtomicU64,
    undeliverable: AtomicU64,
    commands: AtomicU64,
    refused: AtomicU64,
}

/// One connected game client.
pub(crate) struct Client {
    pub(crate) conn: quinn::Connection,
    /// Reliable messages, drained by this connection's writer task. A channel
    /// because writing a QUIC stream is async and sending is not.
    pub(crate) out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// This connection's own names for the entities it asked for.
    pub(crate) handles: HashMap<u32, EntityKey>,
    /// Everything bound to it, including entities the consumer asked for on
    /// its behalf.
    pub(crate) keys: HashSet<EntityKey>,
    /// Its entities are being swept. `disconnected` fires when the last one
    /// is gone, so nothing about it is delivered after that call.
    pub(crate) leaving: bool,
}

/// One entity this edge manages.
pub(crate) struct Entity {
    pub(crate) client: Option<ClientId>,
    /// The asking client's own name for it, if a client asked.
    pub(crate) handle: Option<u32>,
    pub(crate) region: RegionId,
    pub(crate) observes: bool,
    /// `None` until the region reports the id it allocated.
    pub(crate) id: Option<EntityId>,
    /// Where it should be, held until the region reports an id to move. At
    /// most one, because a position is latest-only.
    pub(crate) pending: Option<Pos3>,
    /// Given back before the region confirmed it. Despawned on arrival.
    pub(crate) doomed: bool,
}

#[derive(Default)]
pub(crate) struct Entities {
    pub(crate) by_key: HashMap<EntityKey, Entity>,
    pub(crate) by_id: HashMap<(RegionId, EntityId), EntityKey>,
}

/// Something to say to a region.
///
/// Queued rather than published where it is decided, because `RegionClient`
/// publishes by blocking on its runtime and most of an edge runs *inside* that
/// runtime. One thread owns the region side and everything reaches it through
/// here.
pub(crate) enum Outgoing {
    Spawn(RegionId, Spawn),
    Despawn(RegionId, EntityId),
}

/// Everything an edge server holds, shared with every handle to it.
pub(crate) struct Shared {
    pub(crate) link: RegionClient,
    pub(crate) clients: Mutex<HashMap<ClientId, Client>>,
    pub(crate) entities: Mutex<Entities>,
    /// Positions waiting to be published, latest per entity, flushed by a
    /// timer. One publish per client move would be tens of thousands of tiny
    /// messages a second at any real client count.
    pub(crate) moves: Mutex<HashMap<(RegionId, EntityId), Pos3>>,
    /// Drained by the thread that owns the region side. A `Sender` is `Send`
    /// but not `Sync`, so it is behind a lock like everything else here.
    pub(crate) outbound: Mutex<Sender<Outgoing>>,
    pub(crate) game: Mutex<Box<dyn EdgeGame>>,
    pub(crate) client_ids: Mint,
    pub(crate) entity_keys: Mint,
    pub(crate) counters: Counters,
}

impl Shared {
    /// Hands something to the region thread. Failure means that thread has
    /// stopped, which means the server is going away.
    pub(crate) fn tell_region(&self, what: Outgoing) {
        let _ = self.outbound.lock().expect("not poisoned").send(what);
    }

    pub(crate) fn clients(&self) -> MutexGuard<'_, HashMap<ClientId, Client>> {
        self.clients.lock().expect("not poisoned")
    }

    pub(crate) fn entities(&self) -> MutexGuard<'_, Entities> {
        self.entities.lock().expect("not poisoned")
    }

    /// Calls into the consumer's game.
    ///
    /// **Nothing else may be locked here.** A game is free to call back into
    /// [`EdgeHandle`], which takes these locks itself.
    pub(crate) fn with_game(&self, f: impl FnOnce(&mut dyn EdgeGame)) {
        let mut game = self.game.lock().expect("not poisoned");
        f(game.as_mut());
    }

    pub(crate) fn count_command(&self) {
        self.counters.commands.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_refused(&self) {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_relayed(&self, ok: bool) {
        let counter =
            if ok { &self.counters.relayed } else { &self.counters.undeliverable };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Puts one message on a client's connection.
    ///
    /// Latest-only messages ride a datagram and everything else rides the
    /// reliable stream, which is what [`ToClient::is_latest_only`] decides.
    pub(crate) fn post(&self, client: ClientId, message: ToClient<'_>) -> Result<(), NetError> {
        let mut body = Vec::new();
        message.encode(&mut body);
        let clients = self.clients();
        let held = clients.get(&client).ok_or(NetError::Unknown("client"))?;
        if message.is_latest_only() {
            // A packet at the region's full budget plus this header can exceed
            // what the path will carry, and quinn refuses rather than
            // fragmenting. That shows up as `undeliverable` rather than as a
            // silent truncation.
            held.conn.send_datagram(body.into())?;
        } else {
            let mut framed = Vec::with_capacity(body.len() + 4);
            Framer::frame(&body, &mut framed);
            held.out.send(framed).map_err(|_| NetError::Unknown("client"))?;
        }
        Ok(())
    }

    /// Where an entity should be. `false` if this edge does not hold the key.
    ///
    /// Held rather than sent if the region has not yet said which id it is. At
    /// most one is held, because a position is latest-only.
    pub(crate) fn set_position(&self, key: EntityKey, to: Pos3) -> bool {
        let mut entities = self.entities();
        let Some(held) = entities.by_key.get_mut(&key) else { return false };
        // Already given back. Whoever is still moving it has not been told yet,
        // so this is not their mistake — but a region refuses a move for an
        // entity it has despawned, and there is no reason to send one.
        if held.doomed {
            return true;
        }
        let region = held.region;
        match held.id {
            Some(id) => {
                drop(entities);
                self.queue_move(region, id, to);
            }
            None => held.pending = Some(to),
        }
        true
    }

    /// Queues a position, latest wins.
    pub(crate) fn queue_move(&self, region: RegionId, id: EntityId, to: Pos3) {
        self.moves.lock().expect("not poisoned").insert((region, id), to);
    }

    /// Publishes everything queued since the last flush.
    pub(crate) fn flush_moves(&self) {
        let batch = std::mem::take(&mut *self.moves.lock().expect("not poisoned"));
        if batch.is_empty() {
            return;
        }
        let mut by_region: HashMap<RegionId, Vec<(EntityId, Pos3)>> = HashMap::new();
        for ((region, id), to) in batch {
            by_region.entry(region).or_default().push((id, to));
        }
        for (region, moves) in by_region {
            // Nothing to do about a publish that fails. Positions are
            // latest-only, so the next flush carries a newer one.
            let _ = self.link.move_entities(region, &moves);
        }
    }

    /// Asks a region for an entity and records what was asked for.
    pub(crate) fn ask(
        &self,
        client: Option<ClientId>,
        handle: Option<u32>,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        let key = EntityKey::from_raw(self.entity_keys.next());
        self.entities().by_key.insert(
            key,
            Entity {
                client,
                handle,
                region,
                observes: kind.observes(),
                id: None,
                pending: None,
                doomed: false,
            },
        );
        if let Some(client) = client {
            let mut clients = self.clients();
            if let Some(held) = clients.get_mut(&client) {
                held.keys.insert(key);
                if let Some(handle) = handle {
                    held.handles.insert(handle, key);
                }
            }
        }
        // The key is the correlation token: unique to this edge, never reused,
        // and echoed back by the region without being looked inside.
        self.tell_region(Outgoing::Spawn(region, Spawn { position: at, kind, token: key.raw() }));
        Ok(key)
    }

    /// Gives an entity back, and stops moving it now rather than when the
    /// region reports it gone. A move already in flight for an entity just
    /// despawned is refused, and there is no reason to send another.
    ///
    /// An entity the region has not answered for yet is simply forgotten. Its
    /// arrival then carries a token this edge no longer holds, and the path
    /// that already handles an unrecognised token gives it straight back.
    pub(crate) fn release(&self, key: EntityKey) {
        let mut entities = self.entities();
        let Some(entity) = entities.by_key.get_mut(&key) else { return };
        entity.pending = None;
        let region = entity.region;
        let Some(id) = entity.id else {
            drop(entities);
            self.forget(key);
            return;
        };
        entity.doomed = true;
        drop(entities);
        self.moves.lock().expect("not poisoned").remove(&(region, id));
        self.tell_region(Outgoing::Despawn(region, id));
    }

    /// Drops every record of an entity. Its client is told separately.
    pub(crate) fn forget(&self, key: EntityKey) -> Option<Entity> {
        let mut entities = self.entities();
        let entity = entities.by_key.remove(&key)?;
        if let Some(id) = entity.id {
            entities.by_id.remove(&(entity.region, id));
        }
        drop(entities);
        if let Some(client) = entity.client {
            let mut clients = self.clients();
            if let Some(held) = clients.get_mut(&client) {
                held.keys.remove(&key);
                if let Some(handle) = entity.handle {
                    held.handles.remove(&handle);
                }
            }
        }
        Some(entity)
    }

    pub(crate) fn stats(&self) -> EdgeStats {
        let clients = self.clients().len() as u32;
        let entities = self.entities();
        EdgeStats {
            clients,
            entities: entities.by_key.len() as u32,
            observers: entities.by_key.values().filter(|e| e.observes).count() as u32,
            relayed: self.counters.relayed.load(Ordering::Relaxed),
            undeliverable: self.counters.undeliverable.load(Ordering::Relaxed),
            commands: self.counters.commands.load(Ordering::Relaxed),
            refused: self.counters.refused.load(Ordering::Relaxed),
        }
    }

    /// The regions this edge currently holds entities in.
    pub(crate) fn regions(&self) -> Vec<RegionId> {
        let mut seen: Vec<RegionId> =
            self.entities().by_key.values().map(|e| e.region).collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }
}

/// Turns cumulative counters into what one heartbeat span carries.
pub(crate) fn span(now: EdgeStats, before: EdgeStats) -> EdgeLoad {
    EdgeLoad {
        clients: now.clients,
        entities: now.entities,
        observers: now.observers,
        relayed: now.relayed.saturating_sub(before.relayed),
        undeliverable: now.undeliverable.saturating_sub(before.undeliverable),
        commands: now.commands.saturating_sub(before.commands),
        refused: now.refused.saturating_sub(before.refused),
    }
}

/// An edge's own side of itself: entities, clients, and what to say to them.
///
/// Cheap to clone, and every method takes `&self`. Clone it into a timer or a
/// task and call it from there.
#[derive(Clone)]
pub struct EdgeHandle {
    pub(crate) shared: Arc<Shared>,
}

impl EdgeHandle {
    // -- entities ---------------------------------------------------------

    /// Asks a region for an entity on this client's behalf.
    ///
    /// Returns immediately with a key that is valid at once: a move sent under
    /// it before the region answers is held and sent when the id arrives. The
    /// id itself reaches [`EdgeGame::spawned`](crate::EdgeGame::spawned).
    ///
    /// The entity is despawned when the client disconnects.
    pub fn spawn(
        &self,
        client: ClientId,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        self.shared.ask(Some(client), None, region, at, kind)
    }

    /// Several at once. Chunked to the message cap here rather than by the
    /// caller.
    pub fn spawn_many(
        &self,
        client: ClientId,
        region: RegionId,
        wanted: &[(Pos3, EntityKind)],
    ) -> Result<Vec<EntityKey>, NetError> {
        wanted.iter().map(|&(at, kind)| self.spawn(client, region, at, kind)).collect()
    }

    /// An entity with no client behind it, which lives until this edge does.
    ///
    /// Not swept when a client disconnects, which is what calling this rather
    /// than [`spawn`](Self::spawn) asked for.
    pub fn spawn_detached(
        &self,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        self.shared.ask(None, None, region, at, kind)
    }

    /// Sends a new absolute position.
    ///
    /// A key whose entity has already gone is dropped and counted rather than
    /// refused: removal arrives unprompted, so this is a race and not a
    /// mistake.
    pub fn move_entity(&self, entity: EntityKey, to: Pos3) -> Result<(), NetError> {
        if !self.shared.set_position(entity, to) {
            self.shared.count_refused();
        }
        Ok(())
    }

    /// Several at once, however many regions the batch spans.
    pub fn move_entities(&self, moves: &[(EntityKey, Pos3)]) -> Result<(), NetError> {
        for &(entity, to) in moves {
            self.move_entity(entity, to)?;
        }
        Ok(())
    }

    pub fn despawn(&self, entity: EntityKey) -> Result<(), NetError> {
        self.shared.release(entity);
        Ok(())
    }

    pub fn despawn_many(&self, entities: &[EntityKey]) -> Result<(), NetError> {
        for &entity in entities {
            self.shared.release(entity);
        }
        Ok(())
    }

    // -- clients ----------------------------------------------------------

    /// Reliable and ordered.
    pub fn send(&self, client: ClientId, body: &[u8]) -> Result<(), NetError> {
        self.shared.post(client, ToClient::Message(body))
    }

    /// An unreliable datagram, for anything latest-only.
    pub fn send_datagram(&self, client: ClientId, body: &[u8]) -> Result<(), NetError> {
        let mut framed = Vec::with_capacity(body.len() + 1);
        ToClient::Message(body).encode(&mut framed);
        let clients = self.shared.clients();
        let held = clients.get(&client).ok_or(NetError::Unknown("client"))?;
        held.conn.send_datagram(framed.into())?;
        Ok(())
    }

    /// To whoever owns this entity, if anyone does.
    pub fn send_to_entity(&self, entity: EntityKey, body: &[u8]) -> Result<(), NetError> {
        let client = self.client_of(entity).ok_or(NetError::Unknown("entity"))?;
        self.send(client, body)
    }

    /// Closes a client's connection. Its entities are swept as if it had gone
    /// on its own.
    pub fn disconnect(&self, client: ClientId) {
        let clients = self.shared.clients();
        if let Some(held) = clients.get(&client) {
            held.conn.close(0u32.into(), b"closed by the edge");
        }
    }

    // -- the mapping, maintained for you ----------------------------------

    pub fn client_of(&self, entity: EntityKey) -> Option<ClientId> {
        self.shared.entities().by_key.get(&entity).and_then(|e| e.client)
    }

    pub fn entities_of(&self, client: ClientId) -> Vec<EntityKey> {
        self.shared
            .clients()
            .get(&client)
            .map(|held| held.keys.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn key_of(&self, region: RegionId, id: EntityId) -> Option<EntityKey> {
        self.shared.entities().by_id.get(&(region, id)).copied()
    }

    pub fn entity_id(&self, entity: EntityKey) -> Option<(RegionId, EntityId)> {
        let entities = self.shared.entities();
        let held = entities.by_key.get(&entity)?;
        Some((held.region, held.id?))
    }

    /// What this edge has done since it started.
    pub fn stats(&self) -> EdgeStats {
        self.shared.stats()
    }

}

impl core::fmt::Debug for EdgeHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeHandle").finish_non_exhaustive()
    }
}

/// What a region reported, applied to what this edge is holding.
///
/// Free of the tasks that call it so the state machine can be read, and tested,
/// in one place.
pub(crate) fn on_presence(shared: &Arc<Shared>, region: RegionId, what: Presence) {
    match what {
        Presence::Added { entity, token } => {
            let key = EntityKey::from_raw(token);
            let mut entities = shared.entities();
            let Some(held) = entities.by_key.get_mut(&key) else {
                // A token this edge never spent, or one whose entity it has
                // already forgotten. Give the entity back rather than leaving
                // it in a region nobody is managing.
                drop(entities);
                shared.tell_region(Outgoing::Despawn(region, entity));
                return;
            };
            held.id = Some(entity);
            let (client, handle, pending) = (held.client, held.handle, held.pending.take());
            entities.by_id.insert((region, entity), key);
            drop(entities);

            if let Some(to) = pending {
                shared.queue_move(region, entity, to);
            }
            if let (Some(client), Some(handle)) = (client, handle) {
                let _ = shared.post(client, ToClient::Spawned { handle, region, entity });
            }
            shared.with_game(|game| game.spawned(key, client, region, entity));
        }
        Presence::Removed { entity } => {
            let Some(key) = shared.entities().by_id.get(&(region, entity)).copied() else {
                return;
            };
            let Some(held) = shared.forget(key) else { return };
            if let (Some(client), Some(handle)) = (held.client, held.handle) {
                let _ = shared.post(client, ToClient::Removed { handle });
            }
            shared.with_game(|game| game.removed(key, held.client));
            if let Some(client) = held.client {
                finish_leaving(shared, client);
            }
        }
    }
}

/// Fires `disconnected` once the last of a leaving client's entities is gone.
///
/// Ordered this way so that when `disconnected` arrives the client owns
/// nothing and nothing further about it will be delivered, which is what makes
/// the callback need no guard against a developer cleaning up out of habit.
pub(crate) fn finish_leaving(shared: &Arc<Shared>, client: ClientId) {
    let done = {
        let clients = shared.clients();
        match clients.get(&client) {
            Some(held) => held.leaving && held.keys.is_empty(),
            None => false,
        }
    };
    if !done {
        return;
    }
    shared.clients().remove(&client);
    shared.with_game(|game| game.disconnected(client));
}
