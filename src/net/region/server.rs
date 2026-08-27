//! A region's side of the link, over NATS.
//!
//! [`RegionServer`] answers requests for the region's world parameters, reads
//! the commands its edges send, and drops an edge that has gone quiet. It holds
//! no world state and runs no tick.
//!
//! It owns a Tokio runtime on threads of its own. The tick loop never touches
//! it: [`Handoff`](crate::Handoff) already moves payloads off the tick thread,
//! and that thread is the one that publishes. See `docs/adr/0001`.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::runtime::{Handle, Runtime};
use tokio::task::JoinHandle;

use crate::config::WorldConfig;
use crate::net::error::NetError;
use crate::net::region::edges::Edges;
use crate::net::region::protocol::{
    PROTOCOL_VERSION, RegionId, ServerInfo, ServerVersion, WorldParams,
};
use crate::net::region::session::Inbound;
use crate::net::region::subjects;

/// How long an edge may say nothing before the region drops it and despawns
/// what it managed.
///
/// A closed socket used to carry this. An edge under load sends moves every
/// tick, so this only decides how long an idle edge survives.
pub const EDGE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often silence is checked.
const SWEEP: Duration = Duration::from_secs(1);

/// A region's front door.
pub struct RegionServer {
    region: RegionId,
    config: WorldConfig,
    client: async_nats::Client,
    runtime: Runtime,
    edges: Arc<Edges>,
    tasks: Vec<JoinHandle<()>>,
}

impl RegionServer {
    /// Connects to NATS and starts serving.
    ///
    /// Three things run from here on: replies to `umwelt.{region}.info`,
    /// commands read off `umwelt.{region}.edge.*.command`, and a sweep that
    /// drops edges silent past [`EDGE_TIMEOUT`].
    pub fn connect(
        url: &str,
        region: RegionId,
        config: WorldConfig,
        inbound: Arc<Inbound>,
    ) -> Result<RegionServer, NetError> {
        let runtime = Runtime::new()?;
        let client = runtime.block_on(async_nats::connect(url))?;
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
                    edges.expire(EDGE_TIMEOUT);
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

    /// The NATS client, for a sink that publishes payloads.
    #[inline]
    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    /// A handle onto this server's runtime, so a synchronous thread can drive
    /// a publish without a runtime of its own.
    #[inline]
    pub fn runtime(&self) -> Handle {
        self.runtime.handle().clone()
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
