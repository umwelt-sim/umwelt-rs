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
use std::time::{Duration, Instant};

use crate::config::WorldConfig;
use crate::entity::{EntityId, EntityKind};
use crate::fixed::Fixed;
use crate::game::{EdgeGame, TeleportDecision};
use crate::id::{ClientId, EntityHandle, EntityKey, Mint, RegionId};
use crate::map::WorldMap;
use crate::net::control::EdgeLoad;
use crate::net::edge::protocol::{EdgeInfo, ToClient};
use crate::net::edge::seam;
use crate::net::error::NetError;
use crate::net::region::client::RegionClient;
use crate::net::region::protocol::{Presence, Spawn};
use crate::pos::{Pos3, WorldPos};

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
    /// Regions on the world map this edge read at startup. Zero is a
    /// deployment of islands, or an edge that found no map.
    pub placed_regions: u32,
    /// World positions no placed region covers, dropped. Counted apart from
    /// `refused` because a client walking into the edge of the world is not
    /// making a mistake.
    pub off_map: u64,
    /// Shadows this edge keeps right now: second viewers across seams for its
    /// observers whose view reaches one (`docs/adr/0010`).
    pub shadows: u32,
    /// Boundary collisions answered by crossing the entity into the placed
    /// region beyond, by the teleport this edge already has (`docs/adr/0010`).
    pub crossings: u64,
    /// Boundary collisions dropped: nothing placed beyond that side, or the
    /// game denied the crossing. The region had already stopped the entity at
    /// its box.
    pub walls: u64,
    /// Teleports in flight right now, asked for or crossings.
    pub transitions: u32,
    /// Teleports given up because the destination never confirmed the spawn
    /// within [`TELEPORT_TIMEOUT`].
    pub teleport_timeouts: u64,
}

/// How long a teleport waits for the destination to confirm the spawn before
/// it is given up: the destination copy is forgotten, what was held for the
/// entity meanwhile goes to the region it is still in, and its client is told
/// the teleport failed.
///
/// `docs/adr/0003` measured the whole sequence at 10.5 to 22.9 ms with both
/// regions on one machine, so only a destination that is down or unreachable
/// reaches this. `docs/adr/0008` left the bound open. Two seconds is a guess,
/// and not yet a setting.
pub const TELEPORT_TIMEOUT: Duration = Duration::from_secs(2);

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
    off_map: AtomicU64,
    shadows: AtomicU32,
    crossings: AtomicU64,
    walls: AtomicU64,
    teleport_timeouts: AtomicU64,
}

/// What a region runs, as the edge keeps it: what a client is told, and the
/// config the edge's own seam arithmetic reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RegionWorld {
    pub(crate) info: EdgeInfo,
    pub(crate) config: WorldConfig,
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
    /// The shadows this observer keeps across seams, one per neighbor its
    /// view reaches. Empty for anything that is not an observer near a seam.
    pub(crate) shadows: Vec<EntityKey>,
    /// For a shadow, the observer it stands in for. Its packets go to that
    /// owner's client under that owner's handle, and it is never reported to
    /// the consumer's game.
    pub(crate) shadow_of: Option<EntityKey>,
    /// The name every region writes for it: the key it was first minted
    /// under, kept across a teleport (`docs/adr/0010`).
    pub(crate) name: u64,
    /// The last thing its client said to it, kept so a crossing can say it
    /// again to the region the entity lands in.
    ///
    /// A region that has just been handed an entity has heard nothing about
    /// it. Whatever standing instruction the client gave — the one that had
    /// the entity walking into the boundary in the first place — was sent to
    /// the region it left. Every such message passes through this edge, so
    /// the edge is what can say it again, and the client never learns that
    /// anything was repeated.
    pub(crate) standing: Option<Vec<u8>>,
    /// Set while a teleport of this entity is in flight, until the
    /// destination confirms or the wait is given up.
    pub(crate) transition: Option<Transition>,
}

/// A teleport in flight, kept on the entity leaving (`docs/adr/0008`, and a
/// crossing in `docs/adr/0010`).
///
/// What arrives for the entity meanwhile is held here and forwarded after the
/// remap, so a move or a message sent during the transition reaches the
/// destination rather than a copy about to be given back.
pub(crate) struct Transition {
    /// The destination copy, whose `Presence::Added` completes this.
    pub(crate) to: EntityKey,
    pub(crate) dest: RegionId,
    /// The latest position asked for meanwhile, in world coordinates, since
    /// which frame it belongs to is not known until the remap.
    pub(crate) held_move: Option<WorldPos>,
    /// Entity messages sent meanwhile, in arrival order.
    pub(crate) held_messages: Vec<Vec<u8>>,
}

/// One teleport in flight and when it is given up, for the sweep.
pub(crate) struct Transit {
    pub(crate) from: EntityKey,
    pub(crate) to: EntityKey,
    pub(crate) deadline: Instant,
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
///
/// # Locking
///
/// Several independent mutexes rather than one, because the relay path reads
/// `entities` while client tasks write `clients` and neither should wait on
/// the other. The rule that keeps that safe is **hold one at a time**: take a
/// lock, get what is needed out of it, release it, then take the next. Paths
/// that need two do it in sequence, with an explicit `drop` where scoping does
/// not already end the borrow.
///
/// The one exception is [`tell_the_world`](super::server), which holds
/// `entities` and `told` together to decide which clients still need a
/// region's parameters. Nothing takes those two the other way round.
///
/// [`with_game`](Self::with_game) is the same rule at its sharpest: a game is
/// free to call back in through [`EdgeHandle`], so nothing at all may be held
/// while consumer code runs.
///
/// This is a convention rather than something the type system checks, so a new
/// path that holds two has to be read for it. Breaking it deadlocks the edge
/// rather than corrupting anything.
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
    pub(crate) worlds: Mutex<HashMap<RegionId, RegionWorld>>,
    pub(crate) told: Mutex<HashSet<(ClientId, RegionId)>>,
    pub(crate) game: Mutex<Box<dyn EdgeGame>>,
    pub(crate) client_ids: Mint,
    pub(crate) entity_keys: Mint,
    pub(crate) counters: Counters,
    /// Which region sits where, read once when this edge started
    /// (`docs/adr/0010`). Empty when the broker held no map.
    pub(crate) map: WorldMap,
    /// The region size every placed region shares, learned from the first
    /// placed region to answer `info`. `None` until one has, during which a
    /// world position cannot be placed and is refused.
    pub(crate) region_size: Mutex<Option<Fixed>>,
    /// Game state carried during a teleport, keyed by the destination entity's
    /// key. Inserted when the teleport is initiated, consumed when the
    /// destination's `Presence::Added` arrives.
    pub(crate) teleport_state: Mutex<HashMap<EntityKey, Vec<u8>>>,
    /// Teleports in flight, oldest first, swept for ones past their deadline.
    pub(crate) transits: Mutex<Vec<Transit>>,
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
    /// [`EdgeHandle`], which takes these locks itself, and none of them is
    /// reentrant. See the locking rule on [`Shared`].
    pub(crate) fn with_game(&self, f: impl FnOnce(&mut dyn EdgeGame)) {
        let mut game = self.game.lock().expect("not poisoned");
        f(game.as_mut());
    }

    pub(crate) fn count_command(&self) {
        self.counters.commands.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_off_map(&self) {
        self.counters.off_map.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_wall(&self) {
        self.counters.walls.fetch_add(1, Ordering::Relaxed);
    }

    /// The placed region a world position falls in and the position inside
    /// it. `None` off the map, or before any placed region has said how big a
    /// region is.
    pub(crate) fn locate(&self, at: WorldPos) -> Option<(RegionId, Pos3)> {
        let size = (*self.region_size.lock().expect("not poisoned"))?;
        self.map.locate(at, size)
    }

    /// A world position in one region's frame, inside its box or not. A region
    /// off the map is its own frame.
    pub(crate) fn to_frame(&self, region: RegionId, at: WorldPos) -> Option<Pos3> {
        match self.map.placement_of(region) {
            Some(square) => {
                let size = (*self.region_size.lock().expect("not poisoned"))?;
                at.to_local(square, size)
            }
            None => at.to_unplaced(),
        }
    }

    /// World positions for entities this edge holds, each translated into the
    /// frame of the region its entity is in. A position that does not fit
    /// that frame is dropped and counted.
    pub(crate) fn set_world_positions(
        &self,
        moves: impl IntoIterator<Item = (EntityKey, WorldPos)>,
    ) {
        let mut local: Vec<(EntityKey, Pos3)> = Vec::new();
        let mut dropped = 0u64;
        {
            let mut entities = self.entities();
            for (key, to) in moves {
                let Some(held) = entities.by_key.get_mut(&key) else { continue };
                // In transit, which frame this belongs to is not known until
                // the remap: held in world coordinates, latest only.
                if let Some(transition) = held.transition.as_mut() {
                    transition.held_move = Some(to);
                    continue;
                }
                match self.to_frame(held.region, to) {
                    Some(at) => local.push((key, at)),
                    None => dropped += 1,
                }
            }
        }
        if dropped > 0 {
            self.counters.off_map.fetch_add(dropped, Ordering::Relaxed);
        }
        self.set_positions(local);
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
        // What the connection needs is taken under the lock and the lock is
        // then dropped. A `quinn::Connection` is a handle, so cloning it is a
        // refcount, and sending used to happen with the whole client map held:
        // one client's datagram blocked every other client's traffic behind it.
        let (conn, out) = {
            let clients = self.clients();
            let held = clients.get(&client).ok_or(NetError::Unknown("client"))?;
            (held.conn.clone(), held.out.clone())
        };

        if by == Transport::Datagram {
            // A datagram carries exactly one message, so it needs no length in
            // front of it and is encoded straight into the buffer that goes out.
            let mut body = Vec::new();
            message.encode_onto(&mut body);
            // Dropped rather than queued when the connection has no room. State
            // is latest-only, so a packet waiting behind staler ones is worth
            // less than the one after it, and filling a send buffer with them
            // only delays what a client actually wants. `Handoff` makes the
            // same call on the region side, for the same data.
            // Two ways not to fit, given the same answer rather than one
            // anticipated and one discovered: no room left in the send buffer,
            // and a packet larger than the path will carry at all. quinn
            // refuses the second rather than fragmenting.
            if conn.max_datagram_size().is_none_or(|room| room < body.len())
                || conn.datagram_send_buffer_space() < body.len()
            {
                return Err(NetError::Congested);
            }
            conn.send_datagram(body.into())?;
        } else {
            // A stream is a byte sequence, so this one does need the prefix.
            // Reserved, then filled in, rather than encoding and copying into
            // a second buffer to put a length in front of it.
            let mut framed = vec![0u8; 4];
            message.encode_onto(&mut framed);
            let len = (framed.len() - 4) as u32;
            framed[..4].copy_from_slice(&len.to_le_bytes());
            out.send(framed).map_err(|_| NetError::Unknown("client"))?;
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
        self.ask_for(client, handle, region, at, kind, None, None)
    }

    /// An entity that keeps a name it already has: the destination copy of a
    /// teleport, so a client holding the name from before holds it after.
    pub(crate) fn ask_named(
        &self,
        client: Option<ClientId>,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
        name: u64,
    ) -> Result<EntityKey, NetError> {
        self.ask_for(client, None, region, at, kind, None, Some(name))
    }

    /// A shadow for `owner` in `region`, at a point inside that region's box.
    /// Owned by the owner's client so it is swept with it, named by no handle
    /// so the client is never told of it, and never reported to the game.
    pub(crate) fn ask_shadow(
        &self,
        owner: EntityKey,
        client: Option<ClientId>,
        region: RegionId,
        at: Pos3,
    ) -> Result<EntityKey, NetError> {
        let key = self.ask_for(
            client,
            None,
            region,
            at,
            EntityKind::shadow(),
            Some(owner),
            None,
        )?;
        self.counters.shadows.fetch_add(1, Ordering::Relaxed);
        Ok(key)
    }

    // Every argument is one fact about the spawn being asked for; a struct
    // would name the same seven once more.
    #[allow(clippy::too_many_arguments)]
    fn ask_for(
        &self,
        client: Option<ClientId>,
        handle: Option<EntityHandle>,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
        shadow_of: Option<EntityKey>,
        name: Option<u64>,
    ) -> Result<EntityKey, NetError> {
        let key = EntityKey::from_raw(self.entity_keys.next());
        let name = name.unwrap_or(key.raw());
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
                shadows: Vec::new(),
                shadow_of,
                name,
                transition: None,
                standing: None,
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
        if kind.observes() && !kind.is_shadow() {
            self.counters.observers.fetch_add(1, Ordering::Relaxed);
        }
        // The key is the correlation token: unique to this edge, never reused,
        // and echoed back by the region without being looked inside.
        self.tell_region(Outgoing::Spawn(
            region,
            Spawn { position: at, kind, token: key.raw(), name },
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
        if entity.kind.is_shadow() {
            self.counters.shadows.fetch_sub(1, Ordering::Relaxed);
        } else if entity.kind.observes() {
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
        let transitions = self.transits.lock().expect("not poisoned").len() as u32;
        let entities = self.entities();
        EdgeStats {
            clients,
            entities: entities.by_key.len() as u32,
            observers: self.counters.observers.load(Ordering::Relaxed),
            relayed: self.counters.relayed.load(Ordering::Relaxed),
            undeliverable: self.counters.undeliverable.load(Ordering::Relaxed),
            commands: self.counters.commands.load(Ordering::Relaxed),
            refused: self.counters.refused.load(Ordering::Relaxed),
            placed_regions: self.map.len() as u32,
            off_map: self.counters.off_map.load(Ordering::Relaxed),
            shadows: self.counters.shadows.load(Ordering::Relaxed),
            crossings: self.counters.crossings.load(Ordering::Relaxed),
            walls: self.counters.walls.load(Ordering::Relaxed),
            transitions,
            teleport_timeouts: self.counters.teleport_timeouts.load(Ordering::Relaxed),
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
        at: WorldPos,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        let shared = self.live()?;
        let (region, local) = shared.locate(at).ok_or(NetError::OffMap)?;
        shared.ask(Some(client), None, region, local, kind)
    }

    /// As [`spawn`](Self::spawn), into a named region, which is the one way
    /// into a region off the map. `at` is in that region's frame: the map's
    /// if it is placed, its own if not.
    pub fn spawn_into(
        &self,
        client: ClientId,
        region: RegionId,
        at: WorldPos,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        let shared = self.live()?;
        let local = shared.to_frame(region, at).ok_or(NetError::OffMap)?;
        shared.ask(Some(client), None, region, local, kind)
    }

    /// An entity with no client behind it, which lives until this edge does.
    ///
    /// Not swept when a client disconnects, which is what calling this rather
    /// than [`spawn`](Self::spawn) asked for.
    pub fn spawn_detached(
        &self,
        at: WorldPos,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        let shared = self.live()?;
        let (region, local) = shared.locate(at).ok_or(NetError::OffMap)?;
        shared.ask(None, None, region, local, kind)
    }

    /// As [`spawn_detached`](Self::spawn_detached), into a named region.
    pub fn spawn_detached_into(
        &self,
        region: RegionId,
        at: WorldPos,
        kind: EntityKind,
    ) -> Result<EntityKey, NetError> {
        let shared = self.live()?;
        let local = shared.to_frame(region, at).ok_or(NetError::OffMap)?;
        shared.ask(None, None, region, local, kind)
    }

    /// Sends a new absolute position.
    ///
    /// A key whose entity has already gone is dropped, silently. Removal
    /// arrives unprompted — a region's game can despawn anything, and an entity
    /// can die between a caller reading its keys and sending the batch — so
    /// this is a race rather than a mistake, and `refused` counts mistakes.
    pub fn move_entity(&self, entity: EntityKey, to: WorldPos) -> Result<(), NetError> {
        self.live()?.set_world_positions([(entity, to)]);
        Ok(())
    }

    /// Several at once, however many regions the batch spans. Each position is
    /// translated into the frame of the region its entity is in.
    pub fn move_entities(&self, moves: &[(EntityKey, WorldPos)]) -> Result<(), NetError> {
        self.live()?.set_world_positions(moves.iter().copied());
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
    /// The region delivers it to
    /// [`Game::message_received`](crate::Game::message_received)
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
        let keys: Vec<EntityKey> = shared
            .clients()
            .get(&client)
            .map(|held| held.keys.iter().copied().collect())
            .unwrap_or_default();
        // The shadows the edge keeps for this client's observers are swept
        // with them but were never asked for, so they are not listed.
        let entities = shared.entities();
        keys.into_iter()
            .filter(|key| entities.by_key.get(key).is_none_or(|e| e.shadow_of.is_none()))
            .collect()
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
            let (client, handle, pending, replaces, is_shadow, name) = {
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
                    held.shadow_of.is_some(),
                    held.name,
                );
                entities.by_id.insert((region, entity), key);
                out
            };

            if let Some(old_key) = replaces {
                complete_teleport(shared, key, entity, region, client, pending, old_key);
            } else {
                // Normal spawn, not a teleport.
                if let Some(to) = pending {
                    shared.queue_move(region, entity, to);
                }
                if let (Some(client), Some(handle)) = (client, handle) {
                    let name = EntityKey::from_raw(name);
                    let _ =
                        shared.post(client, ToClient::Spawned { handle, region, name });
                }
                // A shadow is the library's, and the game is not told of it.
                if !is_shadow {
                    shared.with_game(|game| game.spawned(key, client, region, entity));
                }
            }
        }
        // An observer's view reaches its region's box. Which neighbors are
        // there is the map's to say, and a shadow is kept in each so the
        // client is served the far side too. Nothing for a region off the map.
        Presence::ViewCollision { entity, position } => {
            let (owner, client, existing): (
                EntityKey,
                Option<ClientId>,
                Vec<(EntityKey, RegionId)>,
            ) = {
                let entities = shared.entities();
                let Some(&key) = entities.by_id.get(&(region, entity)) else { return };
                let Some(held) = entities.by_key.get(&key) else { return };
                if held.shadow_of.is_some() {
                    return;
                }
                let existing = held
                    .shadows
                    .iter()
                    .filter_map(|k| entities.by_key.get(k).map(|s| (*k, s.region)))
                    .collect();
                (key, held.client, existing)
            };
            let Some(square) = shared.map.placement_of(region) else { return };
            let Some(cfg) = shared
                .worlds
                .lock()
                .expect("not poisoned")
                .get(&region)
                .map(|w| w.config)
            else {
                return;
            };
            let here = WorldPos::from_local(position, square, cfg.region_size());
            let mut keep: Vec<EntityKey> = Vec::new();
            for beyond in seam::squares_in_view(&cfg, square, position) {
                let Some(neighbor) = shared.map.region_at(beyond) else { continue };
                let Some(point) = seam::shadow_point(&cfg, here, beyond) else {
                    continue;
                };
                match existing.iter().find(|(_, r)| *r == neighbor) {
                    Some(&(shadow, _)) => {
                        shared.set_positions([(shadow, point)]);
                        keep.push(shadow);
                    }
                    None => {
                        if let Ok(shadow) =
                            shared.ask_shadow(owner, client, neighbor, point)
                        {
                            keep.push(shadow);
                        }
                    }
                }
            }
            for (shadow, _) in existing {
                if !keep.contains(&shadow) {
                    shared.release(shadow);
                }
            }
            if let Some(held) = shared.entities().by_key.get_mut(&owner) {
                held.shadows = keep;
            }
        }
        Presence::ViewCleared { entity } => {
            let shadows: Vec<EntityKey> = {
                let mut entities = shared.entities();
                let Some(&key) = entities.by_id.get(&(region, entity)) else { return };
                let Some(held) = entities.by_key.get_mut(&key) else { return };
                std::mem::take(&mut held.shadows)
            };
            for shadow in shadows {
                shared.release(shadow);
            }
        }
        // The entity itself reached its region's box. Whether a placed region
        // lies beyond is the map's to say: if one does, the edge crosses the
        // entity into it by the teleport it already has; if none does, the
        // wall is a wall, and the region has already stopped the entity at it
        // (`docs/adr/0010`, "Crossing"). A shadow never crosses: it stands
        // where its owner's view reaches, and its owner is what crosses.
        Presence::BoundaryCollision { entity, target } => {
            let key = {
                let entities = shared.entities();
                let Some(&key) = entities.by_id.get(&(region, entity)) else { return };
                let Some(held) = entities.by_key.get(&key) else { return };
                if held.shadow_of.is_some() || held.doomed || held.transition.is_some() {
                    return;
                }
                key
            };
            let placed = shared.map.placement_of(region).and_then(|square| {
                let size = (*shared.region_size.lock().expect("not poisoned"))?;
                Some(WorldPos::from_local(target, square, size))
            });
            let Some(beyond) = placed else {
                shared.count_wall();
                return;
            };
            match shared.locate(beyond) {
                Some((dest, local)) if dest != region => {
                    match begin_teleport(shared, key, dest, local, beyond) {
                        Some(_) => {
                            shared.counters.crossings.fetch_add(1, Ordering::Relaxed);
                        }
                        None => shared.count_wall(),
                    }
                }
                _ => shared.count_wall(),
            }
        }
        Presence::Removed { entity } => {
            let Some(key) = shared.entities().by_id.get(&(region, entity)).copied()
            else {
                return;
            };
            let Some(held) = shared.forget(key) else { return };
            // A shadow going takes itself off its owner's list, and that is
            // all: the client never knew it and the game is not told.
            if let Some(owner) = held.shadow_of {
                if let Some(o) = shared.entities().by_key.get_mut(&owner) {
                    o.shadows.retain(|k| *k != key);
                }
                return;
            }
            // An owner going takes its shadows with it.
            for shadow in held.shadows {
                shared.release(shadow);
            }
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
    let (handle, from_region, old_id, old_observed, name, transition, standing) = {
        let mut entities = shared.entities();
        let Some(old) = entities.by_key.get(&old_key) else {
            // The old entity is already gone — the client disconnected or
            // the entity was despawned while the teleport was in flight.
            // Clean up the new one.
            drop(entities);
            shared.tell_region(Outgoing::Despawn(dest, new_entity));
            shared.forget(new_key);
            shared.transits.lock().expect("not poisoned").retain(|t| t.to != new_key);
            return;
        };
        let handle = old.handle;
        let from_region = old.region;
        let old_id = old.id;
        let old_observed = old.kind.observes();
        let name = old.name;

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
        // Remove the old entity from by_key. Its shadows stood in for a view
        // that no longer exists; the destination reports its own.
        let (old_shadows, transition, standing) = entities
            .by_key
            .remove(&old_key)
            .map(|e| (e.shadows, e.transition, e.standing))
            .unwrap_or_default();
        drop(entities);
        for shadow in old_shadows {
            shared.release(shadow);
        }
        (handle, from_region, old_id, old_observed, name, transition, standing)
    };
    shared.transits.lock().expect("not poisoned").retain(|t| t.to != new_key);

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
        shared.moves.lock().expect("not poisoned").remove(&(from_region, old_id));
        shared.tell_region(Outgoing::Despawn(from_region, old_id));
    }

    // Flush any pending position to the new entity.
    if let Some(to) = pending {
        shared.queue_move(dest, new_entity, to);
    }
    // The last thing the client said to this entity, said again to the
    // region it has landed in. That region has heard nothing about it, and
    // the instruction still stands: it is what had the entity walking into
    // the boundary that started this. Ahead of anything held during the
    // transition, which is newer and supersedes it.
    if let Some(body) = standing.clone() {
        shared.tell_region(Outgoing::Message(dest, new_entity, body));
    }
    // What arrived for the entity in transit: the latest move, if it falls
    // in the destination, and every entity message in order. A move that
    // does not fit the destination is dropped rather than clamped to its
    // box, which for a crossing would be the seam it just came through.
    if let Some(transition) = transition {
        if let Some(local) = transition.held_move.and_then(|w| shared.to_frame(dest, w)) {
            shared.queue_move(dest, new_entity, local);
        }
        for body in transition.held_messages {
            shared.tell_region(Outgoing::Message(dest, new_entity, body));
        }
    }
    // The arriving entity carries it on, so the next crossing repeats it too.
    if let Some(body) = standing
        && let Some(new) = shared.entities().by_key.get_mut(&new_key)
    {
        new.standing = Some(body);
    }

    // Adjust observer count: the old entity is gone without going through
    // `forget`, so we decrement here if it observed.
    if old_observed {
        shared.counters.observers.fetch_sub(1, Ordering::Relaxed);
    }

    // Tell the client: Spawned first, with the name it has held all along and
    // the region it is now in, then Teleported.
    if let (Some(client), Some(handle)) = (client, handle) {
        let _ = shared.post(
            client,
            ToClient::Spawned { handle, region: dest, name: EntityKey::from_raw(name) },
        );
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
        game.teleport_arrived(new_key, client, from_region, dest, &state);
    });
}

/// Starts a teleport: the destination is asked for a copy under the entity's
/// name, and until it answers the entity leaving holds what arrives for it.
/// `at` is the destination position in world coordinates, for the game.
///
/// `None` is a refusal: the entity is unknown, given back, already in transit
/// or bound for the region it is in; the game denied it; or the destination
/// could not be asked. What to tell a client is the caller's business, since
/// a crossing the client never asked for reports nothing.
pub(crate) fn begin_teleport(
    shared: &Arc<Shared>,
    key: EntityKey,
    dest: RegionId,
    position: Pos3,
    at: WorldPos,
) -> Option<EntityKey> {
    let (client, kind, from, name) = {
        let entities = shared.entities();
        let held = entities.by_key.get(&key)?;
        if held.doomed || held.transition.is_some() || held.region == dest {
            return None;
        }
        (held.client, held.kind, held.region, held.name)
    };
    let mut decision = TeleportDecision::Allow;
    shared.with_game(|game| decision = game.teleporting(key, client, from, dest, at));
    let carried = match decision {
        TeleportDecision::Deny => return None,
        TeleportDecision::Allow => None,
        TeleportDecision::Carry(state) => Some(state),
    };
    // The same name at the destination, so a client holding it from the
    // origin holds one entity across the move.
    let to = shared.ask_named(client, dest, position, kind, name).ok()?;
    {
        let mut entities = shared.entities();
        // Gone while the game was deciding: the copy is given straight back.
        let Some(old) = entities.by_key.get_mut(&key) else {
            drop(entities);
            shared.release(to);
            return None;
        };
        old.transition =
            Some(Transition { to, dest, held_move: None, held_messages: Vec::new() });
        if let Some(new) = entities.by_key.get_mut(&to) {
            new.replaces = Some(key);
        }
    }
    if let Some(state) = carried {
        shared.teleport_state.lock().expect("not poisoned").insert(to, state);
    }
    shared.transits.lock().expect("not poisoned").push(Transit {
        from: key,
        to,
        deadline: Instant::now() + TELEPORT_TIMEOUT,
    });
    Some(to)
}

/// Gives up on every teleport whose destination has not confirmed the spawn
/// by its deadline. Run from the thread that publishes to regions.
pub(crate) fn expire_transitions(shared: &Arc<Shared>, now: Instant) {
    let due: Vec<Transit> = {
        let mut transits = shared.transits.lock().expect("not poisoned");
        if transits.iter().all(|t| t.deadline > now) {
            return;
        }
        let (due, waiting) = transits.drain(..).partition(|t| t.deadline <= now);
        *transits = waiting;
        due
    };
    for Transit { from, to, .. } in due {
        let taken = {
            let mut entities = shared.entities();
            // Confirmed after all, and the remap is under way: leave it be.
            if entities.by_key.get(&to).is_some_and(|new| new.id.is_some()) {
                continue;
            }
            match entities.by_key.get_mut(&from) {
                Some(old) if old.transition.as_ref().is_some_and(|t| t.to == to) => {
                    old.transition.take().map(|t| (t, old.client, old.handle, old.region))
                }
                _ => None,
            }
        };
        let Some((transition, client, handle, region)) = taken else { continue };
        shared.teleport_state.lock().expect("not poisoned").remove(&to);
        shared.forget(to);
        // What was held goes to where the entity still is.
        if let Some(world) = transition.held_move {
            shared.set_world_positions([(from, world)]);
        }
        let id = shared.entities().by_key.get(&from).and_then(|e| e.id);
        if let Some(id) = id {
            for body in transition.held_messages {
                shared.tell_region(Outgoing::Message(region, id, body));
            }
        }
        shared.counters.teleport_timeouts.fetch_add(1, Ordering::Relaxed);
        if let (Some(client), Some(handle)) = (client, handle) {
            let _ = shared.post(
                client,
                ToClient::TeleportFailed { handle, region: transition.dest },
            );
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
