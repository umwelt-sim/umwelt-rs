//! An edge's side of the link, over NATS.
//!
//! [`RegionLink`] is one NATS connection and the edge's own name. Through it an
//! edge talks to any number of regions, and reaching one it has never heard of
//! costs nothing: its two subscriptions are wildcards taken at startup, so a
//! payload from a region that did not exist then matches a subscription it
//! already holds. See `docs/adr/0001`.
//!
//! A `RegionLink` is not an edge server. An edge server will hold one of these
//! and run its own client-facing protocol on the other side of itself. This
//! knows nothing about game clients, fan-out, or relaying.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

use futures::StreamExt;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::config::WorldConfig;
use crate::entity::EntityId;
use crate::net::error::NetError;
use crate::net::region::edges::EdgeName;
use crate::net::region::protocol::{
    DespawnEntities, EntitiesSpawned, EntityKind, KIND_KEEPALIVE, MAX_DESPAWN_PER_MESSAGE,
    MAX_MOVES_PER_MESSAGE, MAX_SPAWN_PER_MESSAGE, MoveEntities, PROTOCOL_VERSION, RegionId,
    ServerInfo, ServerVersion, SpawnEntities,
};
use crate::net::region::subjects;
use crate::pos::Pos3;
use crate::sim::ViewerId;

/// How long an edge waits for a region to answer a request for its parameters.
pub const INFO_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// The entities a spawn asked for, and the viewer watching each. The region
    /// allocated the ids.
    Spawned { region: RegionId, entities: Vec<(EntityId, Option<ViewerId>)> },
    /// One viewer's payload. Decode it with
    /// [`PacketReader`](crate::PacketReader).
    Updates { region: RegionId, viewer: ViewerId, payload: bytes::Bytes },
}

/// One edge's connection to the region tier.
pub struct RegionLink {
    edge: EdgeName,
    client: async_nats::Client,
    runtime: Runtime,
    /// Behind a lock so the link can be shared: one thread publishing while
    /// another receives is the ordinary shape, and only one should receive.
    inbox: Mutex<Receiver<Incoming>>,
    tasks: Vec<JoinHandle<()>>,
}

impl RegionLink {
    /// Connects to NATS and subscribes to everything addressed to this edge.
    ///
    /// Two subscriptions, both wildcards over the region: payloads and replies.
    /// Neither is ever taken again, whatever regions the edge later deals with.
    pub fn connect(url: &str, edge: EdgeName) -> Result<RegionLink, NetError> {
        let runtime = Runtime::new()?;
        let client = runtime.block_on(async_nats::connect(url))?;
        let (send, inbox) = channel();

        let mut tasks = Vec::new();
        tasks.push(runtime.block_on(Self::read_payloads(&client, &edge, send.clone()))?);
        tasks.push(runtime.block_on(Self::read_replies(&client, &edge, send))?);

        Ok(RegionLink { edge, client, runtime, inbox: Mutex::new(inbox), tasks })
    }

    #[inline]
    pub fn name(&self) -> &EdgeName {
        &self.edge
    }

    /// Asks a region what world it runs, and checks the answer.
    ///
    /// Rebuilding the config is also the check that this end decodes the
    /// region's packets the way the region encodes them.
    pub fn info(&self, region: RegionId) -> Result<Offer, NetError> {
        let answer = self.runtime.block_on(async {
            tokio::time::timeout(
                INFO_TIMEOUT,
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
    /// Each carries an [`EntityKind`], because whether a viewer is registered
    /// for it is the difference between an entity that costs 12 bytes of
    /// snapshot and one that costs the whole per-viewer pipeline every tick.
    pub fn spawn(
        &self,
        region: RegionId,
        spawns: &[(Pos3, EntityKind)],
    ) -> Result<(), NetError> {
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
    /// A region drops an edge silent past
    /// [`EDGE_TIMEOUT`](crate::net::EDGE_TIMEOUT) and despawns what it managed.
    /// An edge sending moves every tick never needs this; an idle one does.
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

    /// Takes the next message if one is already here.
    pub fn try_receive(&self) -> Option<Incoming> {
        match self.inbox.lock().expect("not poisoned").try_recv() {
            Ok(message) => Some(message),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
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

    async fn read_payloads(
        client: &async_nats::Client,
        edge: &EdgeName,
        out: Sender<Incoming>,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut payloads = client.subscribe(subjects::to_edge(edge, "payload")).await?;
        Ok(tokio::spawn(async move {
            while let Some(message) = payloads.next().await {
                let Ok(region) = subjects::origin(&message.subject) else { continue };
                if message.payload.len() < 4 {
                    continue;
                }
                let viewer = ViewerId::from_raw(u32::from_le_bytes([
                    message.payload[0],
                    message.payload[1],
                    message.payload[2],
                    message.payload[3],
                ]));
                let payload = message.payload.slice(4..);
                if out.send(Incoming::Updates { region, viewer, payload }).is_err() {
                    return;
                }
            }
        }))
    }

    async fn read_replies(
        client: &async_nats::Client,
        edge: &EdgeName,
        out: Sender<Incoming>,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut replies = client.subscribe(subjects::to_edge(edge, "reply")).await?;
        Ok(tokio::spawn(async move {
            while let Some(message) = replies.next().await {
                let Ok(region) = subjects::origin(&message.subject) else { continue };
                let Some((_kind, body)) = message.payload.split_first() else { continue };
                let Ok(spawned) = EntitiesSpawned::decode(body) else { continue };
                let entities = spawned.entities;
                if out.send(Incoming::Spawned { region, entities }).is_err() {
                    return;
                }
            }
        }))
    }
}

impl Drop for RegionLink {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl core::fmt::Debug for RegionLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegionLink").field("edge", &self.edge).finish_non_exhaustive()
    }
}
