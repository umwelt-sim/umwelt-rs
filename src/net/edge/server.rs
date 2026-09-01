//! An edge's front door: game clients on one side, regions on the other.
//!
//! [`EdgeServer`] holds a [`RegionClient`] and a QUIC endpoint and relays
//! between them. It connects to nothing and binds nothing: the caller supplies
//! a connected `async_nats::Client` and a bound `quinn::Endpoint`, so
//! credentials, certificates and the crypto provider stay with the deployment
//! and this crate installs no process-global default.
//!
//! A state packet never enters consumer code. It arrives from a region already
//! assembled, the edge looks up which connection owns the avatar it is
//! addressed to, and the bytes go out on that connection's datagram.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::game::{EdgeGame, TeleportDecision};
use crate::id::{ClientId, EntityHandle, EntityKey, Mint, RegionId};
use crate::net::control::{self, EdgeHeartbeat};
use crate::net::edge::handle::{
    Client, Counters, EdgeHandle, EdgeStats, Entities, Outgoing, Shared, finish_leaving,
    on_presence, span,
};
use crate::net::edge::protocol::{EdgeInfo, Framer, FromClient, ToClient};
use crate::net::error::NetError;
use crate::net::region::client::{Incoming, RegionClient};
use crate::net::region::edges::EdgeName;
use crate::net::region::protocol::{PROTOCOL_VERSION, Spawn};
use crate::net::version::ServerVersion;
use crate::pos::Pos3;

/// How long a region is given to say what world it runs.
///
/// Asked once per region, on the thread that publishes, the first time a client
/// spawns into one. A client cannot read that region's packets until the answer
/// reaches it.
const INFO_TIMEOUT: Duration = Duration::from_secs(5);

/// How often queued positions are published.
///
/// Well under a tick at any rate a region runs, so a move never waits for the
/// tick that would have carried it. One publish per client move would be tens
/// of thousands of tiny messages a second at any real client count.
const MOVE_FLUSH: Duration = Duration::from_millis(5);

/// How long the region reader waits before checking whether it should stop.
const DRAIN_POLL: Duration = Duration::from_millis(200);

/// How often an edge says what it is carrying, until told otherwise.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);

/// How often the heartbeat task wakes to see whether it is due.
const HEARTBEAT_GRANULARITY: Duration = Duration::from_millis(250);

/// An edge server.
///
/// Held for the whole run: dropping it closes the endpoint, stops the tasks and
/// gives back every entity its clients held.
///
/// ```no_run
/// use umwelt::{ClientId, EdgeGame, EdgeServer};
///
/// /// An edge that only says who is here. Relaying needs no code.
/// struct Doorman;
///
/// impl EdgeGame for Doorman {
///     fn connected(&mut self, client: ClientId, from: std::net::SocketAddr) {
///         println!("{client} from {from}");
///     }
/// }
///
/// // The caller connects and binds. Where the broker is, what certificate
/// // this edge presents and which crypto provider is installed are all
/// // decisions that stay with the deployment.
/// let runtime = tokio::runtime::Runtime::new()?;
/// let nats = runtime.block_on(async_nats::connect("nats://127.0.0.1:4222"))?;
/// let quic: quinn::Endpoint = // built from whatever the operator trusts
/// # unimplemented!();
///
/// let edge = EdgeServer::new(nats, runtime.handle().clone(), quic, |_| Doorman)?;
/// println!("{} is up", edge.name());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct EdgeServer {
    shared: Arc<Shared>,
    name: EdgeName,
    stop: Arc<AtomicBool>,
    heartbeat: Arc<AtomicU64>,
    tasks: Vec<JoinHandle<()>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl EdgeServer {
    /// Starts relaying, on a connection and an endpoint the caller made.
    ///
    /// **The edge names itself.** A fresh name per incarnation is a correctness
    /// requirement: reuse one and the edge inherits
    /// entities a region still thinks it owns and cannot enumerate. A
    /// correctness requirement is not the consumer's to remember, so there is
    /// no way to supply one.
    ///
    /// `game` is handed the [`EdgeHandle`] it will send through, which is how a
    /// game that needs to speak unprompted gets something to speak with without
    /// the server and the game each needing the other first.
    pub fn new<G: EdgeGame>(
        nats: async_nats::Client,
        runtime: Handle,
        quic: quinn::Endpoint,
        game: impl FnOnce(EdgeHandle) -> G,
    ) -> Result<EdgeServer, NetError> {
        let name = EdgeName::new(mint_name()).expect("a minted name is well formed");
        let link = RegionClient::new(nats, runtime.clone(), name.clone())?;

        let (outbound, queue) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            link,
            clients: Mutex::new(HashMap::new()),
            entities: Mutex::new(Entities::default()),
            moves: Mutex::new(HashMap::new()),
            worlds: Mutex::new(HashMap::new()),
            told: Mutex::new(HashSet::new()),
            outbound: Mutex::new(outbound),
            game: Mutex::new(Box::new(NoGame)),
            client_ids: Mint::new(),
            entity_keys: Mint::new(),
            counters: Counters::default(),
            teleport_state: Mutex::new(HashMap::new()),
        });
        // The handle exists before the game, and the game before the server, so
        // neither has to be constructed twice.
        let built = game(EdgeHandle { shared: Arc::downgrade(&shared) });
        *shared.game.lock().expect("not poisoned") = Box::new(built);

        let stop = Arc::new(AtomicBool::new(false));
        let heartbeat = Arc::new(AtomicU64::new(DEFAULT_HEARTBEAT.as_nanos() as u64));

        let tasks = vec![
            runtime.spawn(accept(quic, Arc::clone(&shared))),
            runtime.spawn(beat(
                Arc::clone(&shared),
                name.clone(),
                Arc::clone(&heartbeat),
            )),
        ];

        // Both of these are threads rather than tasks, and for the same
        // reason: `RegionClient` receives and publishes by blocking on its
        // runtime, and doing that from inside that runtime is not allowed.
        // Everything else on this side runs in the runtime, so the region side
        // is reached through a channel and a reader thread.
        let reader = std::thread::Builder::new()
            .name(format!("umwelt-edge-in-{name}"))
            .spawn({
                let shared = Arc::clone(&shared);
                let stop = Arc::clone(&stop);
                move || drain_regions(&shared, &stop)
            })
            .map_err(NetError::from)?;
        let writer = std::thread::Builder::new()
            .name(format!("umwelt-edge-out-{name}"))
            .spawn({
                let shared = Arc::clone(&shared);
                let stop = Arc::clone(&stop);
                move || publish_to_regions(&shared, &queue, &stop)
            })
            .map_err(NetError::from)?;

        Ok(EdgeServer {
            shared,
            name,
            stop,
            heartbeat,
            tasks,
            threads: vec![reader, writer],
        })
    }

    /// What this edge calls itself, which it chose.
    #[inline]
    pub fn name(&self) -> &EdgeName {
        &self.name
    }

    /// A handle to send through. Cheap to clone.
    #[inline]
    pub fn handle(&self) -> EdgeHandle {
        EdgeHandle { shared: Arc::downgrade(&self.shared) }
    }

    /// What this edge has done since it started.
    #[inline]
    pub fn stats(&self) -> EdgeStats {
        self.shared.stats()
    }

    /// How often this edge says what it is carrying.
    ///
    /// Thirty seconds until told otherwise, and zero switches heartbeats off.
    /// The library holds the timer; the cadence is the deployment's.
    pub fn set_heartbeat_interval(&self, every: Duration) {
        self.heartbeat.store(every.as_nanos() as u64, Ordering::Relaxed);
    }
}

impl Drop for EdgeServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for task in &self.tasks {
            task.abort();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl core::fmt::Debug for EdgeServer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeServer").field("name", &self.name).finish_non_exhaustive()
    }
}

/// The default until the consumer's game replaces it, which happens before any
/// task starts. Exists so `Shared` can be built before the game that needs it.
struct NoGame;
impl EdgeGame for NoGame {}

/// A name no other incarnation will use.
///
/// Nothing scopes on the name, so anything short and unlikely to repeat does.
/// It is drawn from the process id and the clock so that two edges started in
/// the same second on the same host still differ.
fn mint_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u128(
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
    );
    format!("edge-{:012x}", h.finish() & 0xffff_ffff_ffff)
}

// -- clients ---------------------------------------------------------------

async fn accept(quic: quinn::Endpoint, shared: Arc<Shared>) {
    while let Some(incoming) = quic.accept().await {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let Ok(conn) = incoming.await else { return };
            serve_client(conn, shared).await;
        });
    }
}

async fn serve_client(conn: quinn::Connection, shared: Arc<Shared>) {
    let from = conn.remote_address();
    let (out, queue) = tokio::sync::mpsc::unbounded_channel();
    let client = ClientId::from_raw(shared.client_ids.next());
    shared.clients().insert(
        client,
        Client {
            conn: conn.clone(),
            out,
            handles: HashMap::new(),
            keys: HashSet::new(),
            leaving: false,
        },
    );
    shared.with_game(|game| game.connected(client, from));

    // Datagrams are read from the moment there is a connection, before the
    // stream exists. Opening a QUIC stream is lazy — the peer sees it only when
    // the first bytes arrive — so a client whose first message is a move would
    // otherwise be waiting on a stream it has not written to yet. Anything the
    // edge says back queues on the channel until the writer starts.
    let datagrams =
        tokio::spawn(read_datagrams(conn.clone(), Arc::clone(&shared), client));

    // One bidirectional stream carries everything reliable, both ways. The
    // client opens it; a client that never does can still move what it never
    // asked for, which is nothing.
    let writer = match conn.accept_bi().await {
        Ok((send, recv)) => {
            let writer = tokio::spawn(write_stream(send, queue));
            read_stream(recv, &shared, client).await;
            Some(writer)
        }
        // The connection went before a stream arrived.
        Err(_) => None,
    };

    datagrams.abort();
    if let Some(writer) = writer {
        writer.abort();
    }
    sweep(&shared, client);
}

async fn write_stream(
    mut send: quinn::SendStream,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(framed) = queue.recv().await {
        if send.write_all(&framed).await.is_err() {
            return;
        }
    }
}

async fn read_stream(
    mut recv: quinn::RecvStream,
    shared: &Arc<Shared>,
    client: ClientId,
) {
    let mut framer = Framer::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = match recv.read(&mut buf).await {
            Ok(Some(n)) => n,
            // Clean end, or the connection went. Either way there is no more.
            Ok(None) | Err(_) => return,
        };
        framer.push(&buf[..read]);
        loop {
            match framer.take() {
                Ok(Some(body)) => match FromClient::decode(&body) {
                    Ok(message) => on_client(shared, client, message),
                    // One bad message says nothing about the next, so it is
                    // counted and dropped rather than closing the connection.
                    Err(_) => shared.count_refused(),
                },
                Ok(None) => break,
                // The stream cannot be resynchronized after a length this end
                // will not allocate for, so the connection goes.
                Err(_) => {
                    let clients = shared.clients();
                    if let Some(held) = clients.get(&client) {
                        held.conn.close(1u32.into(), b"frame length");
                    }
                    return;
                }
            }
        }
    }
}

async fn read_datagrams(conn: quinn::Connection, shared: Arc<Shared>, client: ClientId) {
    while let Ok(datagram) = conn.read_datagram().await {
        match FromClient::decode(&datagram) {
            Ok(message) => on_client(&shared, client, message),
            Err(_) => shared.count_refused(),
        }
    }
}

/// One command from one client.
///
/// The ownership check here is per connection — did *this* client ask for that
/// entity — which is a different question from the region's per-edge check.
/// Both are consistency checks rather than authorization.
fn on_client(shared: &Arc<Shared>, client: ClientId, message: FromClient) {
    shared.count_command();
    match message {
        // The client names the region, because an edge has none. It reaches
        // every region through one wildcard subscription and has no way to know
        // which regions exist, let alone where a player belongs — that is the
        // game's, and the game is what told this client where it is.
        FromClient::Spawn { handle, region, position, kind } => {
            let taken = shared
                .clients()
                .get(&client)
                .is_some_and(|held| held.handles.contains_key(&handle));
            // A handle is this connection's only name for an entity, so reusing
            // one would leave two entities it cannot tell apart.
            if taken {
                shared.count_refused();
                return;
            }
            // A position outside the region is refused there, not here: an edge
            // that checked would have to hold every region's world.
            let _ = shared.ask(Some(client), Some(handle), region, position, kind);
        }
        // A move for a handle this connection is not holding is dropped
        // silently rather than counted. A region's game can despawn anything,
        // so a client can still be moving something it has not yet been told is
        // gone — a race, and `refused` is for mistakes. The one case here that
        // was a mistake, a first move overtaking the spawn that named it, is
        // gone: a client sends its first move for a handle on the ordered
        // stream, where it cannot pass anything.
        FromClient::Move { handle, position } => {
            if let Some(key) = key_of(shared, client, handle) {
                shared.set_position(key, position);
            }
        }
        FromClient::Moves(moves) => {
            // Resolved in one pass under one lock, rather than taking it again
            // for every move in the batch.
            let resolved: Vec<(EntityKey, Pos3)> = {
                let clients = shared.clients();
                let Some(held) = clients.get(&client) else { return };
                moves
                    .iter()
                    .filter_map(|&(handle, to)| {
                        Some((held.handles.get(&handle).copied()?, to))
                    })
                    .collect()
            };
            for (key, to) in resolved {
                shared.set_position(key, to);
            }
        }
        FromClient::Despawn { handle } => match key_of(shared, client, handle) {
            Some(key) => shared.release(key),
            None => shared.count_refused(),
        },
        FromClient::Message(body) => {
            shared.with_game(|game| game.message_received(client, &body))
        }
        FromClient::EntityMessage { handle, body } => {
            let Some(key) = key_of(shared, client, handle) else {
                shared.count_refused();
                return;
            };
            let resolved = shared
                .entities()
                .by_key
                .get(&key)
                .and_then(|e| Some((e.region, e.id?)));
            if let Some((region, id)) = resolved {
                shared.tell_region(Outgoing::Message(region, id, body));
            }
        }
        FromClient::Teleport { handle, region: dest, position } => {
            // Look up the entity this handle names. Each lock is taken and
            // released separately — holding both at once risks deadlocking
            // against the region reader, which takes them in the other order.
            let Some(key) = key_of(shared, client, handle) else {
                shared.count_refused();
                return;
            };
            let (kind, from) = {
                let entities = shared.entities();
                let Some(entity) = entities.by_key.get(&key) else {
                    shared.count_refused();
                    return;
                };
                (entity.kind, entity.region)
            };
            if from == dest {
                // Same region — a move, not a teleport.
                shared.count_refused();
                return;
            }
            // Ask the consumer's game whether to allow it.
            let mut decision = TeleportDecision::Allow;
            shared.with_game(|game| {
                decision = game.teleporting(key, client, from, dest, position);
            });
            match decision {
                TeleportDecision::Deny => {
                    let _ = shared.post(
                        client,
                        ToClient::TeleportFailed { handle, region: dest },
                    );
                }
                TeleportDecision::Allow | TeleportDecision::Carry(_) => {
                    // Spawn in the destination. The new entity replaces the
                    // old one: when the destination confirms, the handle is
                    // remapped and the origin copy is despawned.
                    let new_key = match shared.ask(
                        Some(client), None, dest, position, kind,
                    ) {
                        Ok(k) => k,
                        Err(_) => {
                            let _ = shared.post(
                                client,
                                ToClient::TeleportFailed { handle, region: dest },
                            );
                            return;
                        }
                    };
                    // Link the new entity to the old one.
                    shared
                        .entities()
                        .by_key
                        .get_mut(&new_key)
                        .expect("just created")
                        .replaces = Some(key);
                    // Stash game state for delivery on arrival.
                    if let TeleportDecision::Carry(state) = decision {
                        shared
                            .teleport_state
                            .lock()
                            .expect("not poisoned")
                            .insert(new_key, state);
                    }
                }
            }
        }
    }
}

fn key_of(
    shared: &Arc<Shared>,
    client: ClientId,
    handle: EntityHandle,
) -> Option<EntityKey> {
    shared.clients().get(&client)?.handles.get(&handle).copied()
}

/// Gives back everything a client held, then reports it gone.
///
/// `disconnected` is not fired here: it fires when the last entity's removal
/// comes back, so that by the time it arrives the client owns nothing.
fn sweep(shared: &Arc<Shared>, client: ClientId) {
    let keys: Vec<EntityKey> = {
        let mut clients = shared.clients();
        let Some(held) = clients.get_mut(&client) else { return };
        if held.leaving {
            return;
        }
        held.leaving = true;
        held.keys.iter().copied().collect()
    };
    for key in keys {
        shared.release(key);
    }
    // A client that held nothing has nothing to wait for.
    finish_leaving(shared, client);
}

// -- regions ---------------------------------------------------------------

/// Reads everything the regions send, on a thread of its own.
fn drain_regions(shared: &Arc<Shared>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let Some(message) = shared.link.receive_timeout(DRAIN_POLL) else { continue };
        match message {
            Incoming::State { region, entity, packet } => {
                relay(shared, region, entity, &packet);
            }
            Incoming::Presence { region, what } => on_presence(shared, region, what),
        }
    }
}

/// Publishes everything the rest of the edge decided to say, on a thread of
/// its own.
///
/// Also where queued positions are flushed, so one thread owns the whole region
/// side and a burst of spawns becomes one message per region rather than one
/// message each.
fn publish_to_regions(
    shared: &Arc<Shared>,
    queue: &std::sync::mpsc::Receiver<Outgoing>,
    stop: &AtomicBool,
) {
    use std::sync::mpsc::RecvTimeoutError;
    while !stop.load(Ordering::Relaxed) {
        let first = match queue.recv_timeout(MOVE_FLUSH) {
            Ok(one) => Some(one),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut spawns: HashMap<RegionId, Vec<Spawn>> = HashMap::new();
        let mut gone: HashMap<RegionId, Vec<crate::EntityId>> = HashMap::new();
        let mut messages: Vec<(RegionId, crate::EntityId, Vec<u8>)> = Vec::new();
        for one in first.into_iter().chain(queue.try_iter()) {
            match one {
                Outgoing::Spawn(region, spawn) => {
                    spawns.entry(region).or_default().push(spawn)
                }
                Outgoing::Despawn(region, id) => gone.entry(region).or_default().push(id),
                Outgoing::Message(region, entity, body) => {
                    messages.push((region, entity, body))
                }
            }
        }
        // Spawns first: a despawn in the same batch can only name an entity
        // from an earlier one, so nothing here depends on the other order.
        for (region, batch) in spawns {
            tell_the_world(shared, region, &batch);
            let _ = shared.link.spawn(region, &batch);
        }
        for (region, batch) in gone {
            let _ = shared.link.despawn(region, &batch);
        }
        for (region, entity, body) in messages {
            let _ = shared.link.game_message(region, entity, &body);
        }
        shared.flush_moves();
    }
}

/// Tells whoever asked what world the region runs, once each.
///
/// On the publishing thread because asking blocks on the runtime, and before
/// the spawn itself so the answer is on the client's stream ahead of anything
/// encoded against it.
fn tell_the_world(shared: &Arc<Shared>, region: RegionId, batch: &[Spawn]) {
    let owners: Vec<ClientId> = {
        let entities = shared.entities();
        let told = shared.told.lock().expect("not poisoned");
        batch
            .iter()
            .filter_map(|spawn| entities.by_key.get(&EntityKey::from_raw(spawn.token)))
            .filter_map(|held| held.client)
            .filter(|client| !told.contains(&(*client, region)))
            .collect()
    };
    if owners.is_empty() {
        return;
    }

    let info = {
        let known = shared.worlds.lock().expect("not poisoned").get(&region).copied();
        match known {
            Some(info) => info,
            // Not cached on failure: a region that is not up yet may be by the
            // next spawn.
            None => match shared.link.info(region, INFO_TIMEOUT) {
                Ok(offer) => {
                    // Only the two extents cross to a client. What the region
                    // told this edge — versions, view radius, speed cap, tick
                    // rate, digest — is this link's business and stops here.
                    let info = EdgeInfo {
                        region,
                        region_size_m: offer.config.region_size().floor_meters(),
                        vertical_extent_m: offer.config.vertical_extent().floor_meters(),
                    };
                    shared.worlds.lock().expect("not poisoned").insert(region, info);
                    info
                }
                Err(_) => return,
            },
        }
    };

    for client in owners {
        if shared.post(client, ToClient::Region(info)).is_ok() {
            shared.told.lock().expect("not poisoned").insert((client, region));
        }
    }
}

/// One packet, from the region that built it to the client that owns its
/// avatar. The bytes are not decoded on the way through.
fn relay(shared: &Arc<Shared>, region: RegionId, entity: crate::EntityId, packet: &[u8]) {
    // The edge knows which avatar a packet was built for and which of that
    // client's handles it is, so the client is told the handle rather than the
    // region and the id. Neither of those is a game's business.
    let owner = {
        let entities = shared.entities();
        entities
            .by_id
            .get(&(region, entity))
            .and_then(|key| entities.by_key.get(key))
            .and_then(|held| Some((held.client?, held.handle?)))
    };
    let Some((client, handle)) = owner else {
        shared.count_relayed(false);
        return;
    };
    let sent = shared.post(client, ToClient::State { handle, packet }).is_ok();
    shared.count_relayed(sent);
}

// -- timers ----------------------------------------------------------------

async fn beat(shared: Arc<Shared>, name: EdgeName, interval: Arc<AtomicU64>) {
    let subject: async_nats::Subject = control::edge_subject(&name).into();
    let client = shared.link.client().clone();
    let mut wake = tokio::time::interval(HEARTBEAT_GRANULARITY);
    let mut since = Duration::ZERO;
    let mut before = EdgeStats::default();
    loop {
        wake.tick().await;
        let every = Duration::from_nanos(interval.load(Ordering::Relaxed));
        if every.is_zero() {
            continue;
        }
        since += HEARTBEAT_GRANULARITY;
        if since < every {
            continue;
        }
        since = Duration::ZERO;
        let now = shared.stats();
        let heartbeat = EdgeHeartbeat {
            edge: name.clone(),
            protocol: PROTOCOL_VERSION,
            server: ServerVersion::CURRENT,
            regions: shared.regions(),
            load: span(now, before),
        };
        before = now;
        let mut body = Vec::with_capacity(EdgeHeartbeat::FIXED_BYTES + 64);
        heartbeat.encode(&mut body);
        // Nothing to do about a publish that fails: there is no consumer to
        // tell, and the next beat carries the span this one would have.
        let _ = client.publish(subject.clone(), body.into()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_name_is_a_valid_subject_token() {
        for _ in 0..100 {
            let name = mint_name();
            EdgeName::new(&name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        }
    }

    #[test]
    fn two_edges_started_together_do_not_share_a_name() {
        let names: HashSet<String> = (0..64).map(|_| mint_name()).collect();
        assert_eq!(names.len(), 64, "a name repeated within one process");
    }
}
