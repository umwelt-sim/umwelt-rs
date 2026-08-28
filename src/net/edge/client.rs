//! A game client's side of the link.
//!
//! [`EdgeClient`] is what a game developer holds. It speaks four commands —
//! spawn, move, despawn, and whatever the game's own protocol carries — and
//! hands back what the edge says. Which of those rides a reliable stream and
//! which rides a datagram is umwelt's decision, not the consumer's: a lost move
//! is superseded within a tick and a lost spawn is not recoverable by anything,
//! and that is a property of the message rather than a choice at the call site.
//!
//! It connects to nothing. The caller supplies a `quinn::Connection`, so the
//! endpoint, its certificates and the crypto provider stay with whoever is
//! deploying. See `docs/adr/0006`.
//!
//! This is the counterpart of [`RegionClient`](crate::net::RegionClient), and
//! the two are as separate as the links they speak for.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::entity::EntityId;
use crate::net::edge::protocol::{Framer, FromClient, ToClient};
use crate::net::error::NetError;
use crate::net::region::protocol::{EntityKind, RegionId};
use crate::pos::Pos3;

/// What an edge sent this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromEdge {
    /// A region allocated an id for the entity this handle asked for. Until
    /// this arrives the handle is the only name for it, which is the point of
    /// having one.
    Spawned { handle: u32, region: RegionId, entity: EntityId },
    /// Gone, whatever caused it — including a despawn this client never asked
    /// for, because a region's own game can despawn anything.
    Removed { handle: u32 },
    /// One packet, to be read with [`PacketReader`](crate::PacketReader).
    State { region: RegionId, packet: bytes::Bytes },
    /// The game's own bytes, which umwelt did not read.
    Message(Vec<u8>),
}

/// One game client's connection to an edge.
pub struct EdgeClient {
    conn: quinn::Connection,
    /// Reliable messages, drained by the writer task. A channel because writing
    /// a QUIC stream is async and sending is not.
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Behind a lock so the client can be shared: one thread sending while
    /// another receives is the ordinary shape, and only one should receive.
    inbox: Mutex<Receiver<FromEdge>>,
    /// Handles are this connection's own names, spent once and never reused, so
    /// a stale one names nothing rather than something else.
    handles: AtomicU32,
    tasks: Vec<JoinHandle<()>>,
}

impl EdgeClient {
    /// Starts talking, on a connection the caller made.
    ///
    /// Opens the one bidirectional stream that carries everything reliable in
    /// both directions. The edge accepts it; nothing else is negotiated.
    pub fn new(conn: quinn::Connection, runtime: Handle) -> Result<EdgeClient, NetError> {
        let (send, recv) = runtime.block_on(conn.open_bi())?;
        let (queue, drain) = tokio::sync::mpsc::unbounded_channel();
        let (post, inbox) = channel();
        let tasks = vec![
            runtime.spawn(write_stream(send, drain)),
            runtime.spawn(read_stream(recv, post.clone())),
            runtime.spawn(read_datagrams(conn.clone(), post)),
        ];
        Ok(EdgeClient {
            conn,
            out: queue,
            inbox: Mutex::new(inbox),
            handles: AtomicU32::new(1),
            tasks,
        })
    }

    // -- commands ---------------------------------------------------------

    /// Asks for an entity, in the region this game put this client in.
    ///
    /// Returns the handle to name it by from here on, valid at once: a move
    /// sent under it before the region answers is held by the edge and sent
    /// when the id arrives. The id itself comes back as [`FromEdge::Spawned`].
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
        let handle = self.handles.fetch_add(1, Ordering::Relaxed);
        self.reliable(&FromClient::Spawn { handle, region, position: at, kind })?;
        Ok(handle)
    }

    /// Sends a new absolute position.
    ///
    /// Latest-only, so it rides a datagram: a lost one is superseded by the
    /// next, and waiting for a retransmission of a position two ticks stale
    /// helps nobody.
    pub fn move_entity(&self, handle: u32, to: Pos3) -> Result<(), NetError> {
        self.datagram(&FromClient::Move { handle, position: to })
    }

    /// Several at once. Each is its own datagram, since each has to fit one.
    pub fn move_entities(&self, moves: &[(u32, Pos3)]) -> Result<(), NetError> {
        for &(handle, to) in moves {
            self.move_entity(handle, to)?;
        }
        Ok(())
    }

    /// Gives an entity back. The edge confirms with [`FromEdge::Removed`].
    pub fn despawn(&self, handle: u32) -> Result<(), NetError> {
        self.reliable(&FromClient::Despawn { handle })
    }

    /// The game's own bytes, reliable and ordered. umwelt does not read them.
    pub fn send(&self, body: &[u8]) -> Result<(), NetError> {
        self.reliable(&FromClient::Message(body.to_vec()))
    }

    /// The game's own bytes on a datagram, for anything latest-only.
    pub fn send_datagram(&self, body: &[u8]) -> Result<(), NetError> {
        self.datagram(&FromClient::Message(body.to_vec()))
    }

    // -- what comes back --------------------------------------------------

    /// Takes the next message, waiting for one. `None` once the link closes.
    pub fn receive(&self) -> Option<FromEdge> {
        self.inbox.lock().expect("not poisoned").recv().ok()
    }

    /// Takes the next message, waiting no longer than `within`.
    pub fn receive_timeout(&self, within: Duration) -> Option<FromEdge> {
        self.inbox.lock().expect("not poisoned").recv_timeout(within).ok()
    }

    /// Takes the next message if one is already here.
    pub fn try_receive(&self) -> Option<FromEdge> {
        self.inbox.lock().expect("not poisoned").try_recv().ok()
    }

    /// The connection, for a caller that wants to close it or read its stats.
    #[inline]
    pub fn connection(&self) -> &quinn::Connection {
        &self.conn
    }

    fn reliable(&self, message: &FromClient) -> Result<(), NetError> {
        let mut body = Vec::new();
        message.encode(&mut body);
        let mut framed = Vec::with_capacity(body.len() + 4);
        Framer::frame(&body, &mut framed);
        self.out.send(framed).map_err(|_| NetError::Unknown("connection"))
    }

    fn datagram(&self, message: &FromClient) -> Result<(), NetError> {
        let mut body = Vec::new();
        message.encode(&mut body);
        self.conn.send_datagram(body.into())?;
        Ok(())
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
            .field("edge", &self.conn.remote_address())
            .finish_non_exhaustive()
    }
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

async fn read_stream(mut recv: quinn::RecvStream, out: Sender<FromEdge>) {
    let mut framer = Framer::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = match recv.read(&mut buf).await {
            Ok(Some(read)) => read,
            Ok(None) | Err(_) => return,
        };
        framer.push(&buf[..read]);
        loop {
            match framer.take() {
                // One bad message says nothing about the next.
                Ok(Some(body)) => {
                    if let Some(one) = decode(&body)
                        && out.send(one).is_err()
                    {
                        return;
                    }
                }
                Ok(None) => break,
                // A length this end will not allocate for cannot be
                // resynchronized past.
                Err(_) => return,
            }
        }
    }
}

async fn read_datagrams(conn: quinn::Connection, out: Sender<FromEdge>) {
    while let Ok(datagram) = conn.read_datagram().await {
        // Sliced rather than copied: a state packet is the whole point of this
        // path and it arrives refcounted.
        let carried = match ToClient::decode(&datagram) {
            Ok(ToClient::State { region, packet }) => {
                let at = datagram.len() - packet.len();
                FromEdge::State { region, packet: datagram.slice(at..) }
            }
            Ok(other) => match owned(other) {
                Some(one) => one,
                None => continue,
            },
            Err(_) => continue,
        };
        if out.send(carried).is_err() {
            return;
        }
    }
}

fn decode(body: &[u8]) -> Option<FromEdge> {
    owned(ToClient::decode(body).ok()?)
}

fn owned(message: ToClient<'_>) -> Option<FromEdge> {
    Some(match message {
        ToClient::Spawned { handle, region, entity } => {
            FromEdge::Spawned { handle, region, entity }
        }
        ToClient::Removed { handle } => FromEdge::Removed { handle },
        ToClient::Message(body) => FromEdge::Message(body.to_vec()),
        ToClient::State { region, packet } => {
            FromEdge::State { region, packet: bytes::Bytes::copy_from_slice(packet) }
        }
    })
}
