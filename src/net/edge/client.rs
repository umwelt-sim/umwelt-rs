//! A game client's side of the link.
//!
//! [`EdgeClient`] is what a game developer holds, and [`ClientHandle`] is what
//! it sends through. Between them they speak four commands — spawn, move,
//! despawn, and whatever the game's own protocol carries — and hand what comes
//! back to a [`ClientGame`].
//!
//! Which command rides a reliable stream and which rides a datagram is umwelt's
//! decision, not the consumer's: a lost move is superseded within a tick and a
//! lost spawn is not recoverable by anything, so it is a property of the message
//! rather than a choice at the call site. Nothing here asks a consumer to poll,
//! to pick a timeout, or to decide what one means.
//!
//! It connects to nothing. The caller supplies a `quinn::Connection`, so the
//! endpoint, its certificates and the crypto provider stay with whoever is
//! deploying, and so does reconnecting. See `docs/adr/0006`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::game::ClientGame;
use crate::net::edge::protocol::{Framer, FromClient, ToClient};
use crate::net::error::NetError;
use crate::net::region::protocol::{EntityKind, RegionId};
use crate::pos::Pos3;

/// What a client holds, shared with every handle to it.
struct Shared {
    conn: quinn::Connection,
    /// Reliable messages, drained by the writer task. A channel because writing
    /// a QUIC stream is async and sending is not.
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Handles are this connection's own names, spent once and never reused, so
    /// a stale one names nothing rather than something else.
    handles: AtomicU32,
    game: Mutex<Box<dyn ClientGame>>,
}

impl Shared {
    /// **Nothing else may be locked here.** A game is free to call back into
    /// [`ClientHandle`].
    fn with_game(&self, f: impl FnOnce(&mut dyn ClientGame)) {
        let mut game = self.game.lock().expect("not poisoned");
        f(game.as_mut());
    }
}

/// One game client's connection to an edge.
///
/// Held for the whole session: dropping it stops reading and closes nothing,
/// which is the caller's to do through [`connection`](Self::connection).
pub struct EdgeClient {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl EdgeClient {
    /// Starts talking, on a connection the caller made.
    ///
    /// Opens the one bidirectional stream that carries everything reliable in
    /// both directions, and starts reading. `game` is handed the
    /// [`ClientHandle`] it will send through, which is how a game that needs to
    /// speak unprompted gets something to speak with without the client and the
    /// game each needing the other first.
    pub fn new<G: ClientGame>(
        conn: quinn::Connection,
        runtime: Handle,
        game: impl FnOnce(ClientHandle) -> G,
    ) -> Result<EdgeClient, NetError> {
        let (send, recv) = runtime.block_on(conn.open_bi())?;
        let (queue, drain) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            conn: conn.clone(),
            out: queue,
            handles: AtomicU32::new(1),
            game: Mutex::new(Box::new(NoGame)),
        });
        let built = game(ClientHandle { shared: Arc::downgrade(&shared) });
        *shared.game.lock().expect("not poisoned") = Box::new(built);

        let tasks = vec![
            runtime.spawn(write_stream(send, drain)),
            runtime.spawn(read_stream(recv, Arc::clone(&shared))),
            runtime.spawn(read_datagrams(conn, Arc::clone(&shared))),
        ];
        Ok(EdgeClient { shared, tasks })
    }

    /// A handle to send through. Cheap to clone.
    #[inline]
    pub fn handle(&self) -> ClientHandle {
        ClientHandle { shared: Arc::downgrade(&self.shared) }
    }

    /// The connection, for a caller that wants to close it or read its stats.
    #[inline]
    pub fn connection(&self) -> &quinn::Connection {
        &self.shared.conn
    }
}

impl Drop for EdgeClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl core::fmt::Debug for EdgeClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeClient")
            .field("edge", &self.shared.conn.remote_address())
            .finish_non_exhaustive()
    }
}

/// The default until the consumer's game replaces it, which happens before any
/// task starts. Exists so `Shared` can be built before the game that needs it.
struct NoGame;
impl ClientGame for NoGame {}

/// What a game client says to its edge.
///
/// Cheap to clone and callable from anywhere. Holds a weak reference, so a game
/// keeping one does not keep the client alive: every call fails cleanly once
/// the [`EdgeClient`] has been dropped.
#[derive(Clone)]
pub struct ClientHandle {
    shared: Weak<Shared>,
}

impl ClientHandle {
    /// Asks for an entity, in the region this game put this client in.
    ///
    /// Returns the handle to name it by from here on, valid at once: a move
    /// sent under it before the region answers is held by the edge and sent
    /// when the id arrives. The id itself reaches
    /// [`ClientGame::spawned`](crate::ClientGame::spawned).
    ///
    /// The region is named because an edge has none — it reaches every region
    /// through one wildcard subscription and has no business deciding where a
    /// player belongs. That is the game's, kept out of band; see
    /// `docs/adr/0003`.
    pub fn spawn(
        &self,
        region: RegionId,
        at: Pos3,
        kind: EntityKind,
    ) -> Result<u32, NetError> {
        let shared = self.live()?;
        let handle = shared.handles.fetch_add(1, Ordering::Relaxed);
        reliable(&shared, &FromClient::Spawn { handle, region, position: at, kind })?;
        Ok(handle)
    }

    /// Sends a new absolute position.
    ///
    /// Latest-only, so it rides a datagram: a lost one is superseded by the
    /// next, and waiting for a retransmission of a position two ticks stale
    /// helps nobody.
    pub fn move_entity(&self, handle: u32, to: Pos3) -> Result<(), NetError> {
        datagram(&*self.live()?, &FromClient::Move { handle, position: to })
    }

    /// Several at once. Each is its own datagram, since each has to fit one.
    pub fn move_entities(&self, moves: &[(u32, Pos3)]) -> Result<(), NetError> {
        let shared = self.live()?;
        for &(handle, to) in moves {
            datagram(&shared, &FromClient::Move { handle, position: to })?;
        }
        Ok(())
    }

    /// Gives an entity back. The edge confirms with
    /// [`ClientGame::removed`](crate::ClientGame::removed).
    pub fn despawn(&self, handle: u32) -> Result<(), NetError> {
        reliable(&*self.live()?, &FromClient::Despawn { handle })
    }

    /// The game's own bytes, reliable and ordered. umwelt does not read them.
    pub fn send(&self, body: &[u8]) -> Result<(), NetError> {
        reliable(&*self.live()?, &FromClient::Message(body.to_vec()))
    }

    /// The game's own bytes on a datagram, for anything latest-only.
    pub fn send_datagram(&self, body: &[u8]) -> Result<(), NetError> {
        datagram(&*self.live()?, &FromClient::Message(body.to_vec()))
    }

    fn live(&self) -> Result<Arc<Shared>, NetError> {
        self.shared.upgrade().ok_or(NetError::Unknown("connection"))
    }
}

impl core::fmt::Debug for ClientHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClientHandle").finish_non_exhaustive()
    }
}

fn reliable(shared: &Shared, message: &FromClient) -> Result<(), NetError> {
    let mut body = Vec::new();
    message.encode(&mut body);
    let mut framed = Vec::with_capacity(body.len() + 4);
    Framer::frame(&body, &mut framed);
    shared.out.send(framed).map_err(|_| NetError::Unknown("connection"))
}

fn datagram(shared: &Shared, message: &FromClient) -> Result<(), NetError> {
    let mut body = Vec::new();
    message.encode(&mut body);
    shared.conn.send_datagram(body.into())?;
    Ok(())
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

async fn read_stream(mut recv: quinn::RecvStream, shared: Arc<Shared>) {
    let mut framer = Framer::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = match recv.read(&mut buf).await {
            Ok(Some(read)) => read,
            // Clean end, or the connection went. The datagram task reports it.
            Ok(None) | Err(_) => return,
        };
        framer.push(&buf[..read]);
        loop {
            match framer.take() {
                // One bad message says nothing about the next.
                Ok(Some(body)) => deliver(&shared, &body),
                Ok(None) => break,
                // A length this end will not allocate for cannot be
                // resynchronized past.
                Err(_) => return,
            }
        }
    }
}

async fn read_datagrams(conn: quinn::Connection, shared: Arc<Shared>) {
    while let Ok(datagram) = conn.read_datagram().await {
        deliver(&shared, &datagram);
    }
    // Datagram reading ends when the connection does, which makes this the one
    // place that knows, and the only call a consumer gets about it.
    shared.with_game(|game| game.disconnected());
}

fn deliver(shared: &Shared, body: &[u8]) {
    let Ok(message) = ToClient::decode(body) else { return };
    shared.with_game(|game| match message {
        ToClient::Spawned { handle, region, entity } => game.spawned(handle, region, entity),
        ToClient::Removed { handle } => game.removed(handle),
        ToClient::State { region, packet } => game.state(region, packet),
        ToClient::Message(body) => game.message(body),
    });
}
