//! What an edge holds, and what a consumer does with it.
//!
//! [`EdgeHandle`] is cheap to clone and callable from anywhere: a timer, a
//! task, another client's callback. It is deliberately not a capability object
//! handed to a callback, because nothing an edge does is valid only at one
//! moment, and an edge that could only speak from inside a callback would force
//! a consumer to queue its own work until some unrelated event fired. See
//! `docs/adr/0006`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::id::{ClientId, EntityKey, RegionId};
use crate::entity::EntityId;
use crate::game::EdgeGame;
use crate::net::control::EdgeLoad;
use crate::id::Mint;
use crate::net::edge::protocol::{EdgeInfo, ToClient};
use crate::net::error::NetError;
use crate::net::region::RegionClient;
use crate::net::region::protocol::{EntityKind, Presence, Spawn};
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
    /// Entities with a client behind them, kept current rather than counted on
    /// demand: `stats` is on a path a consumer may call every second, and
    /// scanning every entity for it holds the lock the relay path needs.
    observers: AtomicU32,
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
    /// An index into [`keys`](Self::keys): this connection's own names for
    /// the entities it asked for.
    ///
    /// Kept separate rather than folded in because it is read on the per-move
    /// path — every move a client sends names a handle — and one map keyed by
    /// [`EntityKey`] would make that a scan. The two are maintained together in
    /// `ask` and `forget`, and a test pins that they agree.
    pub(crate) handles: HashMap<u32, EntityKey>,
    /// Everything bound to this client, including entities the consumer asked
    /// for on its behalf, which have no handle of the client's own.
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
    pub(crate) kind: EntityKind,
    /// `None` until the region reports the id it allocated.
    pub(crate) id: Option<EntityId>,
    /// Where it should be, held until the region reports an id to move. At
    /// most one, because a position is latest-only.
    pub(crate) pending: Option<Pos3>,
    /// Given back, and waiting for the region to say it has gone. Nothing is
    /// sent under it in the meantime; an entity given back before the region
    /// answered at all is forgotten outright instead.
    pub(crate) doomed: bool,
}

#[derive(Default)]
pub(crate) struct Entities {
    pub(crate) by_key: HashMap<EntityKey, Entity>,
    pub(crate) by_id: HashMap<(RegionId, EntityId), EntityKey>,
}

/// Which way a message travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transport {
    /// Reliable and ordered.
    Stream,
    /// Unreliable, unordered, one message per packet.
    Datagram,
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
    /// What world each region runs, asked for the first time an entity is
    /// spawned into one, and which clients have been told. A client cannot
    /// decode a packet without it, and cannot be told at connect time because
    /// an edge has no home region.
    pub(crate) worlds: Mutex<HashMap<RegionId, EdgeInfo>>,
    pub(crate) told: Mutex<HashSet<(ClientId, RegionId)>>,
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

    /// Puts one message on a client's connection, on the transport its kind
    /// calls for: latest-only messages ride a datagram, everything else the
    /// reliable stream.
    pub(crate) fn post(&self, client: ClientId, message: ToClient<'_>) -> Result<(), NetError> {
        let by = if message.is_latest_only() {
            Transport::Datagram
        } else {
            Transport::Stream
        };
        self.post_by(client, message, by)
    }

    /// Puts one message on a named transport.
    ///
    /// Separate from [`post`](Self::post) only because a consumer's own
    /// messages can go either way, and the kind alone cannot say which.
    pub(crate) fn post_by(
        &self,
        client: ClientId,
        message: ToClient<'_>,
        by: Transport,
    ) -> Result<(), NetError> {
        // Framed in place: the length prefix is reserved and filled in, rather
        // than encoding once and copying into a second buffer to prefix it.
        let mut framed = vec![0u8; 4];
        message.encode_onto(&mut framed);
        let body = &framed[4..];
        let clients = self.clients();
        let held = clients.get(&client).ok_or(NetError::Unknown("client"))?;
        if by == Transport::Datagram {
            // Dropped rather than queued when the connection has no room. State
            // is latest-only, so a packet waiting behind staler ones is worth
            // less than the one after it, and filling a send buffer with them
            // only delays what a client actually wants. `Handoff` makes the
            // same call on the region side, for the same data.
            // Two ways not to fit, given the same answer rather than one
            // anticipated and one discovered: no room left in the send buffer,
            // and a packet larger than the path will carry at all. quinn
            // refuses the second rather than fragmenting.
            if held.conn.max_datagram_size().is_none_or(|room| room < body.len())
                || held.conn.datagram_send_buffer_space() < body.len()
            {
                return Err(NetError::Congested);
            }
            held.conn.send_datagram(framed.split_off(4).into())?;
        } else {
            let len = (framed.len() - 4) as u32;
            framed[..4].copy_from_slice(&len.to_le_bytes());
            held.out.send(framed).map_err(|_| NetError::Unknown("client"))?;
        }
        Ok(())
    }

    /// Where entities should be, for those this edge is still holding.
    ///
    /// A position is held rather than queued if the region has not yet said
    /// which id it is; at most one is held, because a position is latest-only.
    /// A key this edge is not holding is skipped: removal arrives unprompted,
    /// so acting on an entity that has just gone is a race.
    ///
    /// Takes each lock once for the whole batch. A client with any real number
    /// of entities moves them all at once, so taking them per entity made the
    /// number of acquisitions the number of entities.
    pub(crate) fn set_positions(&self, moves: impl IntoIterator<Item = (EntityKey, Pos3)>) {
        let mut queued: Vec<((RegionId, EntityId), Pos3)> = Vec::new();
        {
            let mut entities = self.entities();
            for (key, to) in moves {
                let Some(held) = entities.by_key.get_mut(&key) else { continue };
                if held.doomed {
                    continue;
                }
                match held.id {
                    Some(id) => queued.push(((held.region, id), to)),
                    None => held.pending = Some(to),
                }
            }
        }
        if queued.is_empty() {
            return;
        }
        let mut waiting = self.moves.lock().expect("not poisoned");
        waiting.extend(queued);
    }

    /// One entity's position. Use [`set_positions`](Self::set_positions) for
    /// more than one.
    pub(crate) fn set_position(&self, key: EntityKey, to: Pos3) {
        self.set_positions([(key, to)]);
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
                kind,
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
                debug_assert!(
                    held.handles.values().all(|k| held.keys.contains(k)),
                    "the handle index named an entity this client does not hold"
                );
            }
        }
        if kind.observes() {
            self.counters.observers.fetch_add(1, Ordering::Relaxed);
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
    /// that already handles an unrecognized token gives it straight back.
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
        if entity.kind.observes() {
            self.counters.observers.fetch_sub(1, Ordering::Relaxed);
        }
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
                debug_assert!(
                    held.handles.values().all(|k| held.keys.contains(k)),
                    "the handle index outlived the entity it named"
                );
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
            observers: self.counters.observers.load(Ordering::Relaxed),
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
///
/// Holds a weak reference, so a game keeping one does not keep the server
/// alive — the server owns the game, and an owning handle would make a cycle
/// neither could break. Every call fails cleanly once the
/// [`EdgeServer`](crate::net::EdgeServer) has been dropped.
#[derive(Clone)]
pub struct EdgeHandle {
    pub(crate) shared: Weak<Shared>,
}

impl EdgeHandle {
    fn live(&self) -> Result<Arc<Shared>, NetError> {
        self.shared.upgrade().ok_or(NetError::Unknown("edge"))
    }
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
        self.live()?.ask(Some(client), None, region, at, kind)
    }

    /// Several at once.
    ///
    /// Each is queued for the region thread, which groups a batch by region
    /// and hands it to `RegionClient`; the message cap is applied there, not
    /// by the caller and not here.
    pub fn spawn_many(
        &self,
        client: ClientId,
        region: RegionId,
        wanted: &[(Pos3, EntityKind)],
    ) -> Result<Vec<EntityKey>, NetError> {
        let shared = self.live()?;
        wanted
            .iter()
            .map(|&(at, kind)| shared.ask(Some(client), None, region, at, kind))
            .collect()
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
        self.live()?.ask(None, None, region, at, kind)
    }

    /// Sends a new absolute position.
    ///
    /// A key whose entity has already gone is dropped, silently. Removal
    /// arrives unprompted — a region's game can despawn anything, and an entity
    /// can die between a caller reading its keys and sending the batch — so
    /// this is a race rather than a mistake, and `refused` counts mistakes.
    pub fn move_entity(&self, entity: EntityKey, to: Pos3) -> Result<(), NetError> {
        self.live()?.set_position(entity, to);
        Ok(())
    }

    /// Several at once, however many regions the batch spans.
    pub fn move_entities(&self, moves: &[(EntityKey, Pos3)]) -> Result<(), NetError> {
        self.live()?.set_positions(moves.iter().copied());
        Ok(())
    }

    pub fn despawn(&self, entity: EntityKey) -> Result<(), NetError> {
        self.live()?.release(entity);
        Ok(())
    }

    pub fn despawn_many(&self, entities: &[EntityKey]) -> Result<(), NetError> {
        let shared = self.live()?;
        for &entity in entities {
            shared.release(entity);
        }
        Ok(())
    }

    // -- clients ----------------------------------------------------------

    /// Reliable and ordered.
    pub fn send(&self, client: ClientId, body: &[u8]) -> Result<(), NetError> {
        self.live()?.post(client, ToClient::Message(body))
    }

    /// An unreliable datagram, for anything latest-only.
    ///
    /// Dropped rather than queued when the connection has no room, on the same
    /// terms as the state umwelt sends: a datagram waiting behind staler ones
    /// is worth less than the one after it.
    pub fn send_datagram(&self, client: ClientId, body: &[u8]) -> Result<(), NetError> {
        self.live()?.post_by(client, ToClient::Message(body), Transport::Datagram)
    }

    /// To whoever owns this entity, if anyone does.
    pub fn send_to_entity(&self, entity: EntityKey, body: &[u8]) -> Result<(), NetError> {
        let client = self.client_of(entity).ok_or(NetError::Unknown("entity"))?;
        self.send(client, body)
    }

    /// Closes a client's connection. Its entities are swept as if it had gone
    /// on its own.
    pub fn disconnect(&self, client: ClientId) {
        let Ok(shared) = self.live() else { return };
        let clients = shared.clients();
        if let Some(held) = clients.get(&client) {
            held.conn.close(0u32.into(), b"closed by the edge");
        }
    }

    // -- the mapping, maintained for you ----------------------------------

    pub fn client_of(&self, entity: EntityKey) -> Option<ClientId> {
        self.live().ok()?.entities().by_key.get(&entity).and_then(|e| e.client)
    }

    pub fn entities_of(&self, client: ClientId) -> Vec<EntityKey> {
        let Ok(shared) = self.live() else { return Vec::new() };
        shared
            .clients()
            .get(&client)
            .map(|held| held.keys.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn key_of(&self, region: RegionId, id: EntityId) -> Option<EntityKey> {
        self.live().ok()?.entities().by_id.get(&(region, id)).copied()
    }

    pub fn entity_id(&self, entity: EntityKey) -> Option<(RegionId, EntityId)> {
        let shared = self.live().ok()?;
        let entities = shared.entities();
        let held = entities.by_key.get(&entity)?;
        Some((held.region, held.id?))
    }

    /// What this edge has done since it started.
    pub fn stats(&self) -> EdgeStats {
        self.live().map(|shared| shared.stats()).unwrap_or_default()
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
    // Nothing will name this client again and its id is never reused, so every
    // record of it goes. Left behind, `told` gains an entry per client per
    // region for the life of the process.
    shared.told.lock().expect("not poisoned").retain(|(held, _)| *held != client);
    shared.with_game(|game| game.disconnected(client));
}
