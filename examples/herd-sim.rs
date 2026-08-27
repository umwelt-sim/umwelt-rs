//! One region, listening for edges.
//!
//! Everything in the world arrives from an edge: entities are spawned where an
//! edge asks, moved where an edge says, and despawned when an edge gives them
//! up. The region allocates the ids and replicates, and does nothing else.
//!
//! ```text
//! cargo run --release --example herd-sim
//! cargo run --release --example herd-sim -- --addr 0.0.0.0:7777 --hz 20 --region 7
//! cargo run --release --example herd-sim -- --plain --wait hold
//! ```
//!
//! Then point one or more `herd-edge` at it. Runs until interrupted.
//!
//! **Output.** A dashboard when stdout is a terminal: the region's own numbers,
//! then a card for each attached edge, redrawn once a second, with cards added
//! and dropped as edges come and go. One line a second when stdout is
//! redirected, because a measurement script parses those lines. `--plain` and
//! `--tui` force either. `--width` sets how many card columns fit, defaulting
//! to 100 columns, since terminal size cannot be read without a dependency.

#[path = "herd/mod.rs"]
mod herd;
#[path = "herd/tui.rs"]
mod tui;

use std::io::{IsTerminal, Write};
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
    // A dashboard when stdout is a terminal, one line a second when it is
    // redirected, since a measurement script parses those lines. `--plain`
    // forces the lines either way.
    let plain = match (herd::arg("plain").is_some(), herd::arg("tui").is_some()) {
        (_, true) => false,
        (true, _) => true,
        _ => !std::io::stdout().is_terminal(),
    };
    let width: usize = herd::arg_or("width", 100usize);

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

    if plain {
        println!(
            "herd-sim: {region} on {} at {} Hz, {} m region, {} m view radius, wait {wait:?}",
            server.local_addr(),
            cfg.tick_hz(),
            cfg.region_size().floor_meters(),
            cfg.horizontal_view_radius().floor_meters(),
        );
        println!("herd-sim: waiting for edges");
    }

    let edges = Arc::clone(server.edges());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let sink = EdgeSink::new(Arc::clone(&edges));
    let wait_name = match wait {
        Wait::Sleep => "sleep",
        Wait::Hold => "hold",
        Wait::None => "none",
    };
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
                // Printing here would tear a dashboard frame, so the cards are
                // the only report when one is running.
                if plain {
                    println!("herd-sim: {:?} attached from {}", edge.id(), edge.peer());
                }
                let failure = inbound.serve(&edge).err();
                if plain {
                    if let Some(e) = failure {
                        println!("herd-sim: {:?} failed: {e}", edge.id());
                    }
                    println!(
                        "herd-sim: {:?} detached, giving up {} entities",
                        edge.id(),
                        edge.entity_count()
                    );
                }
            });
            if let Err(e) = outcome {
                eprintln!("herd-sim: accept loop stopped: {e}");
            }
        });

        // Tick loop. settle runs between ticks, which is the only place a viewer
        // can be registered or dropped.
        let mut reported = Instant::now();
        let started = Instant::now();
        let mut dash = tui::Dashboard::new(width);
        let mut ticks = 0u32;
        let mut served = 0u64;
        let mut records = 0u64;
        let mut spent = Duration::ZERO;
        let mut worst = Duration::ZERO;
        if !plain {
            print!("{}", tui::Dashboard::enter());
        }

        sim.run(
            Pacing { wait, overrun: Overrun::Dilate, ticks: None },
            |report, sim| {
                inbound.settle(sim, &sink, ClientLimits::default());

                ticks += 1;
                served += report.stats.viewers;
                records += report.stats.records;
                spent += report.took;
                worst = worst.max(report.took);

                if reported.elapsed() >= Duration::from_secs(1) {
                    let mean_ms = spent.as_secs_f64() * 1_000.0 / ticks.max(1) as f64;
                    let viewers = served / ticks.max(1) as u64;
                    if plain {
                        // Dropped counts payloads the handoff declined to queue
                        // because the I/O thread had not drained the slot. A rising
                        // count means the delivery side is behind, not the tick.
                        println!(
                            "herd-sim: {ticks} ticks | {} edges | {} entities | \
                             {} slots | {viewers} viewers/tick | {records} records | \
                             mean {mean_ms:.2} ms worst {:.2} ms | \
                             delivered {} dropped {} | undeliverable {} refused {}",
                            edges.len(),
                            sim.entity_count(),
                            slots.load(Ordering::Relaxed),
                            worst.as_secs_f64() * 1_000.0,
                            sim.sink().delivered(),
                            sim.sink().dropped(),
                            sink.undeliverable(),
                            inbound.refused(),
                        );
                    } else {
                        let frame = tui::Frame {
                            region,
                            addr: server.local_addr().to_string(),
                            tick_hz: cfg.tick_hz(),
                            wait: wait_name,
                            uptime: started.elapsed(),
                            entities: sim.entity_count(),
                            slots: slots.load(Ordering::Relaxed),
                            viewers,
                            records,
                            mean_ms,
                            worst_ms: worst.as_secs_f64() * 1_000.0,
                            delivered: sim.sink().delivered(),
                            dropped: sim.sink().dropped(),
                            undeliverable: sink.undeliverable(),
                            refused: inbound.refused(),
                            edges: edges.view(),
                        };
                        let painted = dash.render(&frame);
                        let mut out = std::io::stdout().lock();
                        let _ = out.write_all(painted.as_bytes());
                        let _ = out.flush();
                    }
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
