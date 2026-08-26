//! One region, listening for edges.
//!
//! Everything in the world arrives from an edge: entities are spawned where an
//! edge asks, moved where an edge says, and despawned when an edge gives them
//! up. The region allocates the ids and replicates, and does nothing else.
//!
//! ```text
//! cargo run --release --example herd-sim
//! cargo run --release --example herd-sim -- --addr 0.0.0.0:7777 --hz 20 --region 7
//! ```
//!
//! Then point one or more `herd-edge` at it. Runs until interrupted.

#[path = "herd/mod.rs"]
mod herd;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeSink, Inbound, RegionId, RegionServer, SharedSecret};
use umwelt::sim::{ClientLimits, Flow, Game, Handoff, Overrun, Pacing, Step, Wait};
use umwelt::{WorldConfig, WorldSimulation};

/// The region's game. Everything it does is what the edges asked for, which is
/// the point of the exercise: no world logic, only the wire.
struct Applier {
    inbound: Arc<Inbound>,
    /// Slots ever allocated, which is what the snapshot rebuild and the
    /// odometer walk. `WorldSimulation` reports live entities and has no
    /// accessor for this, so the game reads it off `Step` and publishes it.
    /// Under churn it grows without bound, since despawn does not reclaim.
    slots: Arc<AtomicUsize>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
        self.slots.store(step.slots(), Ordering::Relaxed);
    }
}

fn main() {
    let addr: String = herd::arg_or("addr", herd::DEFAULT_ADDR.to_string());
    let secret: String = herd::arg_or("secret", herd::DEFAULT_SECRET.to_string());
    let region = RegionId::from_raw(herd::arg_or("region", 7u32));
    let cfg: WorldConfig = herd::world(herd::arg_or("hz", 20u32));
    // How the loop waits for its deadline. `sleep` is what a deployment wants;
    // `hold` gives up a core to stop the idle penalty distorting a measurement.
    // A tick that is short against its period spends most of the period asleep,
    // and §Idle costs speed measured what that takes back: up to 4x on an idle
    // region. Sweeping viewer count with `sleep` measures that as much as it
    // measures the work.
    let wait = match herd::arg_or("wait", "sleep".to_string()).as_str() {
        "hold" => Wait::Hold,
        "none" => Wait::None,
        "sleep" => Wait::Sleep,
        other => {
            eprintln!("--wait: expected sleep, hold or none, got {other:?}");
            std::process::exit(2);
        }
    };

    let server = RegionServer::bind(
        addr.as_str(),
        region,
        cfg,
        Arc::new(SharedSecret::new(secret.into_bytes())),
    )
    .unwrap_or_else(|e| {
        eprintln!("bind {addr}: {e}");
        std::process::exit(1);
    });

    println!(
        "herd-sim: {region} on {} at {} Hz, {} m region, {} m view radius, wait {wait:?}",
        server.local_addr(),
        cfg.tick_hz(),
        cfg.region_size().floor_meters(),
        cfg.horizontal_view_radius().floor_meters(),
    );
    println!("herd-sim: waiting for edges");

    let edges = Arc::clone(server.edges());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let sink = EdgeSink::new(Arc::clone(&edges));
    let slots = Arc::new(AtomicUsize::new(0));
    let mut sim = WorldSimulation::new(
        cfg,
        Applier { inbound: Arc::clone(&inbound), slots: Arc::clone(&slots) },
    )
    .with_sink(Handoff::new(sink.clone()));

    std::thread::scope(|scope| {
        // Accept loop. Each attached edge gets a thread that only queues what it
        // reads; the tick loop is what applies it.
        scope.spawn(|| {
            let outcome = server.run(|edge| {
                println!("herd-sim: {:?} attached from {}", edge.id(), edge.peer());
                if let Err(e) = inbound.serve(&edge) {
                    println!("herd-sim: {:?} failed: {e}", edge.id());
                }
                println!(
                    "herd-sim: {:?} detached, giving up {} entities",
                    edge.id(),
                    edge.entities().len()
                );
            });
            if let Err(e) = outcome {
                eprintln!("herd-sim: accept loop stopped: {e}");
            }
        });

        // Tick loop. settle runs between ticks, which is the only place a viewer
        // can be registered or dropped.
        let mut reported = Instant::now();
        let mut ticks = 0u32;
        let mut served = 0u64;
        let mut records = 0u64;
        let mut spent = Duration::ZERO;
        let mut worst = Duration::ZERO;

        sim.run(
            Pacing { wait, overrun: Overrun::Dilate, ticks: None },
            |report, sim| {
                inbound.settle(sim, &sink, ClientLimits::default());

                ticks += 1;
                served += report.stats.viewers as u64;
                records += report.stats.records as u64;
                spent += report.took;
                worst = worst.max(report.took);

                if reported.elapsed() >= Duration::from_secs(1) {
                    // Dropped counts payloads the handoff declined to queue
                    // because the I/O thread had not drained the slot. A rising
                    // count means the delivery side is behind, not the tick.
                    println!(
                        "herd-sim: {ticks} ticks | {} edges | {} entities | \
                         {} slots | {} viewers/tick | {records} records | \
                         mean {:.2} ms worst {:.2} ms | \
                         delivered {} dropped {} | undeliverable {} refused {}",
                        edges.len(),
                        sim.entity_count(),
                        slots.load(Ordering::Relaxed),
                        served / ticks.max(1) as u64,
                        spent.as_secs_f64() * 1_000.0 / ticks.max(1) as f64,
                        worst.as_secs_f64() * 1_000.0,
                        sim.sink().delivered(),
                        sim.sink().dropped(),
                        sink.undeliverable(),
                        inbound.refused(),
                    );
                    reported = Instant::now();
                    ticks = 0;
                    served = 0;
                    records = 0;
                    spent = Duration::ZERO;
                    worst = Duration::ZERO;
                }
                Flow::Continue
            },
        );
    });
}
