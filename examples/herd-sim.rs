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

use umwelt::net::{EdgeSink, Edges, Inbound, RegionId, RegionServer};
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
    let url: String = herd::arg_or("nats", herd::DEFAULT_NATS.to_string());
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

    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let server = RegionServer::connect(&url, region, cfg, Arc::clone(&inbound))
        .unwrap_or_else(|e| {
            eprintln!("nats {url}: {e}");
            std::process::exit(1);
        });
    let sink = EdgeSink::new(region, server.client().clone(), server.runtime(), Arc::clone(&edges));

    if plain {
        println!(
            "herd-sim: {region} over {} at {} Hz, {} m region, {} m view radius, wait {wait:?}",
            url,
            cfg.tick_hz(),
            cfg.region_size().floor_meters(),
            cfg.horizontal_view_radius().floor_meters(),
        );
        println!("herd-sim: waiting for edges");
    }

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
                        addr: url.clone(),
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
}
