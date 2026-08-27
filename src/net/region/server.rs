//! A region's side of the link, over NATS.
//!
//! [`RegionServer`] answers requests for the region's world parameters, reads
//! the commands its edges send, and drops an edge that has gone quiet. It holds
//! no world state and runs no tick.
//!
//! It connects to nothing. The caller supplies a connected
//! [`async_nats::Client`] and a Tokio [`Handle`] to drive it, so the broker
//! address, credentials, TLS, cluster membership and reconnect policy are the
//! caller's to choose. The tick loop never touches either:
//! [`Handoff`](crate::Handoff) already moves payloads off the tick thread, and
//! that thread is the one that publishes. See `docs/adr/0001`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::config::WorldConfig;
use crate::net::control::{self, Heartbeat, RegionLoad};
use crate::net::error::NetError;
use crate::net::region::edges::Edges;
use crate::net::region::protocol::{
    PROTOCOL_VERSION, RegionId, ServerInfo, ServerVersion, WorldParams,
};
use crate::net::region::session::Inbound;
use crate::net::region::subjects;

/// How often silence is checked against the caller's timeout.
const SWEEP: Duration = Duration::from_secs(1);

/// A region's front door.
pub struct RegionServer {
    region: RegionId,
    config: WorldConfig,
    client: async_nats::Client,
    runtime: Handle,
    edges: Arc<Edges>,
    tasks: Vec<JoinHandle<()>>,
}

impl RegionServer {
    /// Starts serving on a connection the caller made.
    ///
    /// Three things run from here on: replies to `umwelt.{region}.info`,
    /// commands read off `umwelt.{region}.edge.*.command`, and a sweep that
    /// drops edges silent for longer than `edge_timeout`.
    ///
    /// `edge_timeout` decides how long an edge that says nothing survives
    /// before the region drops it and despawns what it managed. A closed socket
    /// used to carry that. An edge under load sends moves every tick, so this
    /// only decides how long an idle one lasts, and how long a dead one keeps
    /// its entities alive.
    pub fn new(
        client: async_nats::Client,
        runtime: Handle,
        region: RegionId,
        config: WorldConfig,
        inbound: Arc<Inbound>,
        edge_timeout: Duration,
    ) -> Result<RegionServer, NetError> {
        let edges = Arc::clone(inbound.edges());

        let mut tasks = Vec::new();
        tasks.push(runtime.block_on(Self::serve_info(&client, region, config))?);
        tasks.push(runtime.block_on(Self::serve_commands(
            &client,
            region,
            Arc::clone(&edges),
            Arc::clone(&inbound),
        ))?);
        tasks.push(runtime.spawn({
            let edges = Arc::clone(&edges);
            async move {
                let mut every = tokio::time::interval(SWEEP);
                loop {
                    every.tick().await;
                    edges.expire(edge_timeout);
                }
            }
        }));

        Ok(RegionServer { region, config, client, runtime, edges, tasks })
    }

    #[inline]
    pub fn region(&self) -> RegionId {
        self.region
    }

    #[inline]
    pub fn config(&self) -> &WorldConfig {
        &self.config
    }

    /// The edges relaying for this region.
    #[inline]
    pub fn edges(&self) -> &Arc<Edges> {
        &self.edges
    }

    /// Publishes one heartbeat.
    ///
    /// Called by the consumer, as often as it wants. The library holds no timer
    /// and defines no cadence; see `docs/adr/0002`. The consumer supplies what
    /// only its loop knows, and this fills in the region, the versions, the
    /// world digest and the edge count.
    pub fn heartbeat(&self, load: RegionLoad) -> Result<(), NetError> {
        let beat = Heartbeat {
            region: self.region,
            protocol: PROTOCOL_VERSION,
            server: ServerVersion::CURRENT,
            protocol_hash: self.config.protocol_hash(),
            edges: self.edges.len() as u32,
            load,
        };
        let mut body = Vec::with_capacity(Heartbeat::BYTES);
        beat.encode(&mut body);
        self.runtime
            .block_on(self.client.publish(control::subject(self.region), body.into()))?;
        Ok(())
    }

    /// The NATS client, for a sink that publishes payloads.
    #[inline]
    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    async fn serve_info(
        client: &async_nats::Client,
        region: RegionId,
        config: WorldConfig,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut requests = client.subscribe(subjects::info(region)).await?;
        let client = client.clone();
        Ok(tokio::spawn(async move {
            let info = ServerInfo {
                protocol: PROTOCOL_VERSION,
                server: ServerVersion::CURRENT,
                region,
                params: WorldParams::from_config(&config),
            };
            let mut body = Vec::new();
            info.encode(&mut body);
            while let Some(request) = requests.next().await {
                let Some(to) = request.reply else { continue };
                let _ = client.publish(to, body.clone().into()).await;
            }
        }))
    }

    async fn serve_commands(
        client: &async_nats::Client,
        region: RegionId,
        edges: Arc<Edges>,
        inbound: Arc<Inbound>,
    ) -> Result<JoinHandle<()>, NetError> {
        let mut commands = client.subscribe(subjects::commands_to(region)).await?;
        Ok(tokio::spawn(async move {
            while let Some(message) = commands.next().await {
                // The sender is in the subject, so a message cannot claim to be
                // from an edge other than the one that published it.
                let Ok(name) = subjects::sender(&message.subject) else { continue };
                let edge = edges.admit(&name);
                inbound.accept(edge, &message.payload);
            }
        }))
    }
}

impl Drop for RegionServer {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl core::fmt::Debug for RegionServer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegionServer")
            .field("region", &self.region)
            .field("edges", &self.edges.len())
            .finish_non_exhaustive()
    }
}
