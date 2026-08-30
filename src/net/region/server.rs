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
//! that thread is the one that publishes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::config::WorldConfig;
use crate::id::RegionId;
use crate::net::control::{self, Heartbeat};
use crate::net::error::NetError;
use crate::net::region::edges::Edges;
use crate::net::region::protocol::{PROTOCOL_VERSION, ServerInfo, WorldParams};
use crate::net::region::session::Inbound;
use crate::net::region::subjects;
use crate::net::version::ServerVersion;

/// How often silence is checked against the caller's timeout.
const SWEEP: Duration = Duration::from_secs(1);

/// How often a region publishes what it is carrying, until told otherwise.
///
/// A control plane runs at human timescales, so this is far slower than
/// anything else here.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);

/// How often the heartbeat task wakes to see whether it is due.
///
/// Coarse on purpose: the interval it is checking against is measured in tens
/// of seconds, and a quarter second of slop against that is not worth a timer
/// that has to be rebuilt every time the interval changes.
const HEARTBEAT_GRANULARITY: Duration = Duration::from_millis(250);

/// A region's front door.
pub struct RegionServer {
    region: RegionId,
    config: WorldConfig,
    client: async_nats::Client,
    edges: Arc<Edges>,
    /// Nanoseconds between heartbeats, read by the task each time it wakes.
    /// Zero switches them off.
    heartbeat: Arc<AtomicU64>,
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

        let heartbeat = Arc::new(AtomicU64::new(DEFAULT_HEARTBEAT.as_nanos() as u64));
        tasks.push(runtime.spawn(Self::beat(
            client.clone(),
            region,
            config,
            Arc::clone(&edges),
            Arc::clone(&inbound),
            Arc::clone(&heartbeat),
        )));

        Ok(RegionServer { region, config, client, edges, heartbeat, tasks })
    }

    /// Which region this serves.
    #[inline]
    pub fn region(&self) -> RegionId {
        self.region
    }

    /// The world it answers with.
    #[inline]
    pub fn config(&self) -> &WorldConfig {
        &self.config
    }

    /// The edges relaying for this region.
    #[inline]
    pub fn edges(&self) -> &Arc<Edges> {
        &self.edges
    }

    /// How often this region says what it is carrying.
    ///
    /// Thirty seconds until told otherwise, and zero switches heartbeats off.
    /// The library holds the timer; how much resolution an operator wants and
    /// how much traffic that is worth stay the deployment's.
    ///
    /// Silence now means a region that has stopped, so switching heartbeats off
    /// is a deliberate act rather than the default it used to be.
    pub fn set_heartbeat_interval(&self, every: Duration) {
        self.heartbeat.store(every.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Publishes what the region is carrying, as often as it is asked to.
    ///
    /// The numbers come off `Inbound`, which the tick fills in as a side effect
    /// of `settle`. Nothing is wired up by the consumer, and nothing here
    /// touches the tick thread.
    async fn beat(
        client: async_nats::Client,
        region: RegionId,
        config: WorldConfig,
        edges: Arc<Edges>,
        inbound: Arc<Inbound>,
        interval: Arc<AtomicU64>,
    ) {
        let subject: async_nats::Subject = control::subject(region).into();
        let mut wake = tokio::time::interval(HEARTBEAT_GRANULARITY);
        let mut since = Duration::ZERO;
        loop {
            wake.tick().await;
            let every = Duration::from_nanos(interval.load(Ordering::Relaxed));
            if every.is_zero() {
                // Switched off. The span keeps accumulating, so switching them
                // back on reports the whole interval rather than a slice.
                continue;
            }
            since += HEARTBEAT_GRANULARITY;
            if since < every {
                continue;
            }
            since = Duration::ZERO;
            let beat = Heartbeat {
                region,
                protocol: PROTOCOL_VERSION,
                server: ServerVersion::CURRENT,
                protocol_hash: config.protocol_hash(),
                edges: edges.len() as u32,
                load: inbound.take_load(),
            };
            let mut body = Vec::with_capacity(Heartbeat::BYTES);
            beat.encode(&mut body);
            // Nothing to do about a publish that fails: there is no consumer to
            // tell, and the next beat carries the span this one would have.
            let _ = client.publish(subject.clone(), body.into()).await;
        }
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
