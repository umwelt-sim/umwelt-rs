//! One edge, relaying for a region.
//!
//! It holds no game client sockets — that is what an edge server will add. What
//! it does is everything on the region side of an edge: ask for a population,
//! move it, give parts of it back as its clients would come and go, and take
//! the replication that comes down.
//!
//! ```text
//! cargo run --release --example herd-edge
//! cargo run --release --example herd-edge -- --observers 2048 --churn 16
//! cargo run --release --example herd-edge -- --observers 64 --unattended 2000
//! ```
//!
//! Four of these at the default 2,048 observers make the 8,192 the pipeline
//! benchmark runs, which is the worst case: every entity a viewer. The third
//! line is the shape a real region is more likely to have, and the difference
//! between them is the whole per-viewer pipeline. Runs until interrupted.

#[path = "herd/mod.rs"]
mod herd;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeName, EntityKind, Incoming, RegionClient, RegionId};
use umwelt::sim::ViewerId;
use umwelt::{EntityId, Fixed, PacketReader, Pos3, RecordCodec};

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

fn main() {
    let url: String = herd::arg_or("nats", herd::DEFAULT_NATS.to_string());
    let region = RegionId::from_raw(herd::arg_or("region", 7u32));
    // Distinct per process, so several edges against one region do not collide
    // on a subject.
    let name = EdgeName::new(herd::arg_or("name", format!("herd-{}", std::process::id())))
        .unwrap_or_else(|e| {
            eprintln!("--name: {e}");
            std::process::exit(2);
        });
    // Entities with a game client behind them. Each costs a viewer.
    let observers: usize = herd::arg_or("observers", 2048usize);
    // Entities with nothing behind them: replicated to whoever can see them,
    // sent nothing themselves, and costing none of the per-viewer pipeline.
    let unattended: usize = herd::arg_or("unattended", 0usize);
    // Observers to hand back and replace each second, standing in for game
    // clients disconnecting and connecting.
    let churn: usize = herd::arg_or("churn", 0usize);

    // This binary owns its connection, so where and how it reaches the broker
    // is set here rather than by the library.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let client = runtime.block_on(herd::connect(&url, herd::arg("creds"))).unwrap_or_else(|e| {
        eprintln!("nats {url}: {e}");
        std::process::exit(1);
    });
    let link = RegionClient::new(client, runtime.handle().clone(), name.clone())
        .unwrap_or_else(|e| {
            eprintln!("subscribing: {e}");
            std::process::exit(1);
        });
    let offer = link.info(region, Duration::from_secs(5)).unwrap_or_else(|e| {
        eprintln!("asking {region} what it runs: {e}");
        std::process::exit(1);
    });

    let cfg = offer.config;
    let codec = RecordCodec::new(&cfg);
    let hz = cfg.tick_hz();
    println!(
        "herd-edge: {name} talking to {} running umwelt {} at {hz} Hz",
        offer.region, offer.server,
    );

    // A column of its own, so several edges do not stack on one spot. The
    // region is 4096 m; a hundred edges would still fit.
    let lane = (std::process::id() % 64) as i32 * 60 + 64;
    let home = |n: usize| Pos3::from_meters(lane, 64 + (n as i32 % 3072), 0);

    let mut asked: Vec<(Pos3, EntityKind)> =
        (0..observers).map(|n| (home(n), EntityKind::Observer)).collect();
    asked.extend((0..unattended).map(|n| (home(observers + n), EntityKind::Unattended)));
    link.spawn(region, &asked).expect("asks for its population");
    println!(
        "herd-edge: asked for {observers} observers and {unattended} unattended \
         in lane x={lane}"
    );

    // An entity the edge holds, and whether a viewer watches it.
    let arrivals: Mutex<Vec<(EntityId, Option<ViewerId>)>> = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);
    let updates = AtomicU64::new(0);
    let records = AtomicU64::new(0);
    let own = AtomicU64::new(0);

    std::thread::scope(|scope| {
        // Receive: replication coming down, and the ids for anything spawned.
        scope.spawn(|| {
            let mut roster: Vec<(EntityId, ViewerId)> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let Some(message) = link.receive() else { return };
                match message {
                    Incoming::Spawned { entities, .. } => {
                        // Only observers have a viewer, and only viewers are
                        // named by a payload coming down.
                        roster.extend(entities.iter().filter_map(|(id, v)| v.map(|v| (*id, v))));
                        arrivals.lock().expect("not poisoned").extend(entities);
                    }
                    Incoming::Updates { viewer, payload, .. } => {
                        updates.fetch_add(1, Ordering::Relaxed);
                        let Some(reader) = PacketReader::new(&codec, &payload) else {
                            continue;
                        };
                        let avatar =
                            roster.iter().find(|(_, v)| *v == viewer).map(|(id, _)| *id);
                        let mut n = 0u64;
                        for (id, _) in reader.updates() {
                            n += 1;
                            if Some(id) == avatar {
                                own.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        records.fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
        });

        // Send: movement every tick, and churn once a second.
        let period = Duration::from_millis(1_000 / hz.max(1) as u64);
        let step = Fixed::from_raw(Fixed::from_meters(WALK_M_PER_SEC).raw() / hz.max(1) as i32);
        // Entity, where it is, which way it is walking, and whether it observes.
        let mut held: Vec<(EntityId, Pos3, i32, bool)> = Vec::new();
        let mut next_home = observers + unattended;
        let mut reported = Instant::now();
        let mut sent = 0u64;
        let mut given_back = 0u64;

        while !stop.load(Ordering::Relaxed) {
            let deadline = Instant::now() + period;

            for (id, viewer) in arrivals.lock().expect("not poisoned").drain(..) {
                let at = held.len();
                held.push((id, home(at), 1, viewer.is_some()));
            }

            for (_, pos, heading, _) in held.iter_mut() {
                let moved = Fixed::from_raw(pos.x.raw() + step.raw() * *heading);
                if (moved.floor_meters() - lane).abs() > RANGE_M {
                    *heading = -*heading;
                } else {
                    pos.x = moved;
                }
            }

            // Everything this edge manages moves, watched or not: an
            // unattended entity is still authoritative and still replicated.
            let moves: Vec<(EntityId, Pos3)> =
                held.iter().map(|(id, pos, _, _)| (*id, *pos)).collect();
            if !moves.is_empty() {
                if link.move_entities(region, &moves).is_err() {
                    break;
                }
                sent += moves.len() as u64;
            }

            if reported.elapsed() >= Duration::from_secs(1) {
                // Churn is clients, so it is observers that come and go. An
                // unattended entity has nobody to disconnect.
                let watched: Vec<usize> = held
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, _, _, observes))| *observes)
                    .map(|(at, _)| at)
                    .collect();
                if churn > 0 && watched.len() >= churn {
                    let leaving_at = &watched[watched.len() - churn..];
                    let leaving: Vec<EntityId> =
                        leaving_at.iter().map(|&at| held[at].0).collect();
                    if link.despawn(region, &leaving).is_err() {
                        break;
                    }
                    for &at in leaving_at.iter().rev() {
                        held.remove(at);
                    }
                    given_back += leaving.len() as u64;

                    // Clients arriving: ask for replacements.
                    let coming: Vec<Pos3> =
                        (0..churn).map(|k| home(next_home + k)).collect();
                    next_home += churn;
                    if link.spawn_observers(region, &coming).is_err() {
                        break;
                    }
                }

                println!(
                    "herd-edge: holding {} ({} observing) | {sent} moves sent | \
                     {} updates | {} records | {} of them its own | \
                     {given_back} handed back",
                    held.len(),
                    held.iter().filter(|(_, _, _, observes)| *observes).count(),
                    updates.swap(0, Ordering::Relaxed),
                    records.swap(0, Ordering::Relaxed),
                    own.swap(0, Ordering::Relaxed),
                );
                reported = Instant::now();
                sent = 0;
            }

            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
}
