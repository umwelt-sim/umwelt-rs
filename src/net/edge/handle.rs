//! What an edge holds, and what a consumer does with it.
//!
//! [`EdgeHandle`] is cheap to clone and callable from anywhere: a timer, a
//! task, another client's callback. It is deliberately not a capability object
//! handed to a callback, because nothing an edge does is valid only at one
//! moment, and an edge that could only speak from inside a callback would
//! force a consumer to queue its own work until some unrelated event fired.
//!
//! Relaying needs none of it. A client's own spawns, moves and despawns are
//! carried without this file being asked.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::entity::{EntityId, EntityKind};
use crate::game::EdgeGame;
use crate::id::{ClientId, EntityHandle, EntityKey, Mint, RegionId};
use crate::net::control::EdgeLoad;
use crate::net::edge::protocol::{EdgeInfo, ToClient};
use crate::net::error::NetError;
use crate::net::region::client::RegionClient;
use crate::net::region::protocol::{Presence, Spawn};
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
    /// State packets handed to a client's connection.
    pub relayed: u64,
    /// State packets that reached no client: it had gone, or the datagram was
    /// refused.
    pub undeliverable: u64,
    /// Spawns, moves, despawns and messages read from clients.
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
    pub(crate) handles: HashMap<EntityHandle, EntityKey>,
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
    pub(crate) handle: Option<EntityHandle>,
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
    /// The entity this one replaces, set when a teleport spawns the
    /// destination copy. When this entity's `Presence::Added` arrives, the
    /// handle is remapped from the old key to this one and the old entity is
    /// despawned from its origin region.
    pub(crate) replaces: Option<EntityKey>,
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
    Message(RegionId, EntityId, Vec<u8>),
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
    /// Game state carried during a teleport, keyed by the destination entity's
    /// key. Inserted when the teleport is initiated, consumed when the
    /// destination's `Presence::Added` arrives.
    pub(crate) teleport_state: Mutex<HashMap<EntityKey, Vec<u8>>>,
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
    pub(crate) fn post(
        &self,
        client: ClientId,
        message: ToClient<'_>,
    ) -> Result<(), NetError> {
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
    pub(crate) fn set_positions(
        &self,
        moves: impl IntoIterator<Item = (EntityKey, Pos3)>,
    ) {
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
        handle: Option<EntityHandle>,
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
                replaces: None,
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
        self.tell_region(Outgoing::Spawn(
            region,
            Spawn { position: at, kind, token: key.raw() },
        ));
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

    /// The regions this edge currently holds live entities in.
    ///
    /// Entities marked for removal are excluded: their despawn has already been
    /// sent, and keeping the region alive while waiting for the confirmation
    /// would prevent the silence-based timeout from cleaning up a stopped sim.
    pub(crate) fn regions(&self) -> Vec<RegionId> {
        let mut seen: Vec<RegionId> = self
            .entities()
            .by_key
            .values()
            .filter(|e| !e.doomed)
            .map(|e| e.region)
            .collect();
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

    /// Gives an entity back, wherever it is.
    ///
    /// Stops moving it now rather than when the region reports it gone. The
    /// client that held it is told through
    /// [`EdgeGame::removed`](crate::EdgeGame::removed), whether it asked for
    /// this or not.
    pub fn despawn(&self, entity: EntityKey) -> Result<(), NetError> {
        self.live()?.release(entity);
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

    /// Forwards a game message to the region an entity lives in.
    ///
    /// The region delivers it to [`Game::message`](crate::Game::message)
    /// with the entity's id as the sender. The body is the game's own
    /// bytes, at most 4091 bytes.
    pub fn send_to_region(&self, entity: EntityKey, body: &[u8]) -> Result<(), NetError> {
        let (region, id) = self.entity_id(entity).ok_or(NetError::Unknown("entity"))?;
        self.live()?.tell_region(Outgoing::Message(region, id, body.to_vec()));
        Ok(())
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

    /// Who owns an entity, if anyone does. `None` for a detached one, and for
    /// a key this edge is not holding.
    pub fn client_of(&self, entity: EntityKey) -> Option<ClientId> {
        self.live().ok()?.entities().by_key.get(&entity).and_then(|e| e.client)
    }

    /// Everything a client holds, in no particular order. Empty for a client
    /// that has gone.
    pub fn entities_of(&self, client: ClientId) -> Vec<EntityKey> {
        let Ok(shared) = self.live() else { return Vec::new() };
        shared
            .clients()
            .get(&client)
            .map(|held| held.keys.iter().copied().collect())
            .unwrap_or_default()
    }

    /// This edge's name for what a region calls `id`.
    ///
    /// Ids are unique within a region and no further, so both halves are
    /// needed. `None` before the region has answered, and after it has said the
    /// entity is gone.
    pub fn key_of(&self, region: RegionId, id: EntityId) -> Option<EntityKey> {
        self.live().ok()?.entities().by_id.get(&(region, id)).copied()
    }

    /// Where an entity is and what that region calls it. `None` until the
    /// region has answered.
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

            // Extract everything needed from the entity before releasing
            // the lock on `entities`, so borrow-checker is happy.
            let (client, handle, pending, replaces) = {
                let mut entities = shared.entities();
                let Some(held) = entities.by_key.get_mut(&key) else {
                    // A token this edge never spent, or one whose entity it
                    // has already forgotten. Give the entity back rather than
                    // leaving it in a region nobody is managing.
                    drop(entities);
                    shared.tell_region(Outgoing::Despawn(region, entity));
                    return;
                };
                held.id = Some(entity);
                let out = (
                    held.client,
                    held.handle,
                    held.pending.take(),
                    held.replaces.take(),
                );
                entities.by_id.insert((region, entity), key);
                out
            };

            if let Some(old_key) = replaces {
                complete_teleport(
                    shared, key, entity, region, client, pending, old_key,
                );
            } else {
                // Normal spawn, not a teleport.
                if let Some(to) = pending {
                    shared.queue_move(region, entity, to);
                }
                if let (Some(client), Some(handle)) = (client, handle) {
                    let _ = shared.post(
                        client,
                        ToClient::Spawned { handle, region, entity },
                    );
                }
                shared.with_game(|game| game.spawned(key, client, region, entity));
            }
        }
        Presence::Removed { entity } => {
            let Some(key) = shared.entities().by_id.get(&(region, entity)).copied()
            else {
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

/// The second half of a teleport: the destination region reported the new
/// entity. Remaps the client's handle from the old key to the new one,
/// despawns the origin copy, and tells the client and the edge game.
fn complete_teleport(
    shared: &Arc<Shared>,
    new_key: EntityKey,
    new_entity: EntityId,
    dest: RegionId,
    client: Option<ClientId>,
    pending: Option<Pos3>,
    old_key: EntityKey,
) {
    // Read the old entity's details under the entities lock, then update
    // both keys atomically.
    let (handle, from_region, old_id, old_observed) = {
        let mut entities = shared.entities();
        let Some(old) = entities.by_key.get(&old_key) else {
            // The old entity is already gone — the client disconnected or
            // the entity was despawned while the teleport was in flight.
            // Clean up the new one.
            drop(entities);
            shared.tell_region(Outgoing::Despawn(dest, new_entity));
            shared.forget(new_key);
            return;
        };
        let handle = old.handle;
        let from_region = old.region;
        let old_id = old.id;
        let old_observed = old.kind.observes();

        // Move the handle onto the new entity.
        if let Some(handle) = handle {
            entities
                .by_key
                .get_mut(&new_key)
                .expect("just confirmed in on_presence")
                .handle = Some(handle);
        }
        // Remove the old entity from by_id so the origin's
        // Presence::Removed (which arrives later) finds nothing.
        if let Some(old_id) = old_id {
            entities.by_id.remove(&(from_region, old_id));
        }
        // Remove the old entity from by_key.
        entities.by_key.remove(&old_key);
        drop(entities);
        (handle, from_region, old_id, old_observed)
    };

    // Update the client's handle mapping: handle → new_key, remove old_key.
    if let (Some(client), Some(handle)) = (client, handle) {
        let mut clients = shared.clients();
        if let Some(held) = clients.get_mut(&client) {
            held.handles.insert(handle, new_key);
            held.keys.remove(&old_key);
        }
        drop(clients);
    }

    // Despawn the origin copy from its region.
    if let Some(old_id) = old_id {
        shared
            .moves
            .lock()
            .expect("not poisoned")
            .remove(&(from_region, old_id));
        shared.tell_region(Outgoing::Despawn(from_region, old_id));
    }

    // Flush any pending position to the new entity.
    if let Some(to) = pending {
        shared.queue_move(dest, new_entity, to);
    }

    // Adjust observer count: the old entity is gone without going through
    // `forget`, so we decrement here if it observed.
    if old_observed {
        shared.counters.observers.fetch_sub(1, Ordering::Relaxed);
    }

    // Tell the client: Spawned first (so it has the new entity id), then
    // Teleported.
    if let (Some(client), Some(handle)) = (client, handle) {
        let _ =
            shared.post(client, ToClient::Spawned { handle, region: dest, entity: new_entity });
        let _ = shared.post(client, ToClient::Teleported { handle, region: dest });
    }

    // Notify the edge game.
    let state = shared
        .teleport_state
        .lock()
        .expect("not poisoned")
        .remove(&new_key)
        .unwrap_or_default();
    shared.with_game(|game| {
        game.spawned(new_key, client, dest, new_entity);
        if let Some(client) = client {
            game.teleport_arrived(new_key, client, from_region, dest, &state);
        }
    });
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
