//! An edge's side of the link, over NATS.
//!
//! [`RegionClient`] is the edge's own name plus a connection the caller made.
//! Through it an edge talks to any number of regions, and reaching one it has
//! never heard of costs nothing: its two subscriptions are wildcards taken at
//! construction, so a payload from a region that did not exist then matches a
//! subscription it already holds. See `docs/adr/0001`.
//!
//! It connects to nothing itself. The caller supplies a connected
//! [`async_nats::Client`] and a Tokio [`Handle`], so where and how the edge
//! reaches the broker is the caller's to choose.
//!
//! A `RegionClient` is not an edge server. An edge server will hold one of
//! these and run its own client-facing protocol on the other side of itself.
//! This knows nothing about game clients, fan-out, or relaying.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use futures::StreamExt;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::config::WorldConfig;
use crate::entity::EntityId;
use crate::net::error::NetError;
use crate::net::region::edges::EdgeName;
use crate::net::region::protocol::{
    DespawnEntities, KIND_KEEPALIVE, MAX_DESPAWN_PER_MESSAGE, MAX_MOVES_PER_MESSAGE,
    MAX_SPAWN_PER_MESSAGE, MoveEntities, PROTOCOL_VERSION, Presence, RegionId, ServerInfo,
    ServerVersion, Spawn, SpawnEntities,
};
use crate::net::region::subjects;
use crate::pos::Pos3;

/// What a region says it is, checked before an edge uses it.
///
/// The config here has been rebuilt from the region's parameters and checked
/// against its digest, so an edge holding an `Offer` holds a world it can
/// decode packets against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer {
    pub region: RegionId,
    /// The crate version the region is running. Informational.
    pub server: ServerVersion,
    pub config: WorldConfig,
}

/// What a region sent this edge.
#[derive(Clone, Debug)]
pub enum Incoming {
    /// One observer's packet, named by the entity it belongs to. Decode it with
    /// [`PacketReader`](crate::PacketReader).
    State { region: RegionId, entity: EntityId, packet: bytes::Bytes },
    /// An entity this edge owns appeared or left, whatever caused it.
    Presence { region: RegionId, what: Presence },
}

/// One edge's connection to the region tier.
pub struct RegionClient {
    edge: EdgeName,
    client: async_nats::Client,
    runtime: Handle,
    /// Behind a lock so the link can be shared: one thread publishing while
    /// another receives is the ordinary shape, and only one should receive.
    inbox: Mutex<Receiver<Incoming>>,
    tasks: Vec<JoinHandle<()>>,
}

impl RegionClient {
    /// Subscribes to everything addressed to this edge, on a connection the
    /// caller made.
    ///
    /// Two subscriptions, both wildcards over the region: state and presence.
    /// Neither is ever taken again, whatever regions the edge later deals with.
    pub fn new(
        client: async_nats::Client,
        runtime: Handle,
        edge: EdgeName,
    ) -> Result<RegionClient, NetError> {
        let (send, inbox) = channel();
        let tasks = vec![
            runtime.block_on(Self::read_state(&client, &edge, send.clone()))?,
            runtime.block_on(Self::read_presence(&client, &edge, send))?,
        ];
        Ok(RegionClient { edge, client, runtime, inbox: Mutex::new(inbox), tasks })
    }

    #[inline]
    pub fn name(&self) -> &EdgeName {
        &self.edge
    }

    /// The NATS client, for anything else this edge publishes on the same
    /// connection. The control plane is the reason it is here.
    #[inline]
    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    /// Asks a region what world it runs, and checks the answer.
    ///
    /// Rebuilding the config is also the check that this end decodes the
    /// region's packets the way the region encodes them. How long to wait for
    /// an answer depends on where the broker is, so it is the caller's.
    pub fn info(&self, region: RegionId, within: Duration) -> Result<Offer, NetError> {
        let answer = self.runtime.block_on(async {
            tokio::time::timeout(
                within,
                self.client.request(subjects::info(region), Vec::new().into()),
            )
            .await
        });
        let message = match answer {
            Ok(Ok(message)) => message,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(NetError::Malformed("server info: no answer")),
        };
        let info = ServerInfo::decode(&message.payload)?;
        if info.protocol != PROTOCOL_VERSION {
            return Err(NetError::ProtocolMismatch {
                ours: PROTOCOL_VERSION,
                theirs: info.protocol,
            });
        }
        Ok(Offer { region: info.region, server: info.server, config: info.params.to_config()? })
    }

    // -- commands ---------------------------------------------------------

    /// Asks a region to create entities and record this edge as managing them.
    ///
    /// Each carries an [`EntityKind`](crate::net::EntityKind), because whether a
    /// viewer is registered
    /// for it is the difference between an entity that costs 12 bytes of snapshot
    /// and one that costs the whole per-viewer pipeline every tick.
    pub fn spawn(&self, region: RegionId, spawns: &[Spawn]) -> Result<(), NetError> {
        self.send_all(region, spawns.chunks(MAX_SPAWN_PER_MESSAGE).map(|chunk| {
            let mut body = Vec::new();
            SpawnEntities { spawns: chunk.to_vec() }.encode(&mut body);
            body
        }))
    }

    /// [`spawn`](Self::spawn) where every entity has a client behind it.
    pub fn spawn_observers(
        &self,
        region: RegionId,
        positions: &[Pos3],
    ) -> Result<(), NetError> {
        self.spawn(region, &SpawnEntities::observers(positions).spawns)
    }

    /// Sends new absolute positions for entities this edge manages.
    pub fn move_entities(
        &self,
        region: RegionId,
        moves: &[(EntityId, Pos3)],
    ) -> Result<(), NetError> {
        self.send_all(region, moves.chunks(MAX_MOVES_PER_MESSAGE).map(|chunk| {
            let mut body = Vec::new();
            MoveEntities { moves: chunk.to_vec() }.encode(&mut body);
            body
        }))
    }

    /// Gives entities back, because the game clients behind them have gone.
    pub fn despawn(&self, region: RegionId, ids: &[EntityId]) -> Result<(), NetError> {
        self.send_all(region, ids.chunks(MAX_DESPAWN_PER_MESSAGE).map(|chunk| {
            let mut body = Vec::new();
            DespawnEntities { ids: chunk.to_vec() }.encode(&mut body);
            body
        }))
    }

    /// Says nothing except that this edge is still here.
    ///
    /// A region drops an edge silent past the timeout it was built with, and
    /// despawns what that edge managed. An edge sending moves every tick never
    /// needs this; an idle one does.
    pub fn keepalive(&self, region: RegionId) -> Result<(), NetError> {
        self.send_all(region, std::iter::once(vec![KIND_KEEPALIVE]))
    }

    // -- what comes back --------------------------------------------------

    /// Takes the next message, waiting for one.
    ///
    /// `None` once the link is closed.
    pub fn receive(&self) -> Option<Incoming> {
        self.inbox.lock().expect("not poisoned").recv().ok()
    }

    /// Takes the next message, waiting no longer than `within`.
    ///
    /// `None` on timeout or once the link is closed. A caller that must not
    /// block forever on a message that may never arrive wants this rather than
    /// [`receive`](Self::receive).
    pub fn receive_timeout(&self, within: Duration) -> Option<Incoming> {
        self.inbox.lock().expect("not poisoned").recv_timeout(within).ok()
    }

    /// Takes the next message if one is already here.
    pub fn try_receive(&self) -> Option<Incoming> {
        self.inbox.lock().expect("not poisoned").try_recv().ok()
    }

    fn send_all(
        &self,
        region: RegionId,
        bodies: impl Iterator<Item = Vec<u8>>,
    ) -> Result<(), NetError> {
        let subject: async_nats::Subject = subjects::command(region, &self.edge).into();
        self.runtime.block_on(async {
            for body in bodies {
                self.client.publish(subject.clone(), body.into()).await?;
            }
            // One flush for the whole message, however many the cap split it
            // into.
            self.client.flush().await?;
            Ok(())
        })
    }

    async fn read_state(
        client: &async_nats::Client,
        edge: &EdgeName,
        out: Sender<Incoming>,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut packets = client.subscribe(subjects::to_edge(edge, "state")).await?;
        Ok(tokio::spawn(async move {
            while let Some(message) = packets.next().await {
                let Ok(region) = subjects::origin(&message.subject) else { continue };
                if message.payload.len() < 4 {
                    continue;
                }
                let entity = EntityId::from_raw(u32::from_le_bytes([
                    message.payload[0],
                    message.payload[1],
                    message.payload[2],
                    message.payload[3],
                ]));
                let packet = message.payload.slice(4..);
                if out.send(Incoming::State { region, entity, packet }).is_err() {
                    return;
                }
            }
        }))
    }

    async fn read_presence(
        client: &async_nats::Client,
        edge: &EdgeName,
        out: Sender<Incoming>,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut changes = client.subscribe(subjects::to_edge(edge, "presence")).await?;
        Ok(tokio::spawn(async move {
            while let Some(message) = changes.next().await {
                let Ok(region) = subjects::origin(&message.subject) else { continue };
                let Ok(what) = Presence::decode(&message.payload) else { continue };
                if out.send(Incoming::Presence { region, what }).is_err() {
                    return;
                }
            }
        }))
    }
}

impl Drop for RegionClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl core::fmt::Debug for RegionClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegionClient").field("edge", &self.edge).finish_non_exhaustive()
    }
}
