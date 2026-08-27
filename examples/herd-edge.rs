//! One edge, relaying for a region.
//!
//! It holds no game client sockets — that is what an edge server will add. What
//! it does is everything on the region side of an edge: ask for a population,
//! move it, give parts of it back as its clients would come and go, walk some
//! of it into another region, and take the replication that comes down.
//!
//! ```text
//! cargo run --release --example herd-edge
//! cargo run --release --example herd-edge -- --observers 2048 --churn 16
//! cargo run --release --example herd-edge -- --observers 64 --unattended 2000
//! cargo run --release --example herd-edge -- --observers 256 --to 8 --migrate 8
//! ```
//!
//! Four of these at the default 2,048 observers make the 8,192 the pipeline
//! benchmark runs, which is the worst case: every entity a viewer. The third
//! line is the shape a real region is more likely to have, and the difference
//! between them is the whole per-viewer pipeline. Runs until interrupted.
//!
//! The fourth line needs a second `herd-sim --region 8` running as well. It
//! walks eight observers a second into whichever of the two regions is not
//! holding them, by the sequence in `docs/adr/0003`. That sequence is the whole
//! of what migration is: no message the protocol does not already have.

#[path = "herd/mod.rs"]
mod herd;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{EdgeName, EntityKind, Incoming, Presence, RegionClient, RegionId, Spawn};
use umwelt::{EntityId, Fixed, PacketReader, Pos3, RecordCodec};

/// Meters per second a walker covers. Well under the world's 40 m/s cap.
const WALK_M_PER_SEC: i32 = 2;

/// How far either side of home an entity walks before turning around.
const RANGE_M: i32 = 32;

/// One entity this edge manages, and which region is holding it.
///
/// An edge dealing with more than one region keys by `(RegionId, EntityId)`:
/// ids are unique within a region and the same numbers come back from each.
struct Held {
    region: RegionId,
    id: EntityId,
    at: Pos3,
    /// Which way along x it is walking.
    heading: i32,
    /// Whether it has a game client behind it, and so costs a viewer.
    observes: bool,
}

fn main() {
    let url: String = herd::arg_or("nats", herd::DEFAULT_NATS.to_string());
    let region = RegionId::from_raw(herd::arg_or("region", 7u32));
    // Distinct per process, so several edges against one region do not collide
    // on a subject.
    // Fresh every start. An edge that reuses a name inherits entities the
    // region still thinks it owns and cannot enumerate them; see docs/adr/0004.
    let name = EdgeName::new(herd::arg_or("name", format!("herd-{}", herd::shortcode())))
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
    // Where entities walk to, and back from. A second region, which has to be
    // running: nothing here knows which regions exist, and a spawn sent to one
    // that is not there is never answered.
    let to: Option<RegionId> = herd::arg("to").map(|raw| {
        RegionId::from_raw(raw.parse().unwrap_or_else(|_| {
            eprintln!("--to: cannot read {raw:?}");
            std::process::exit(2);
        }))
    });
    // Observers to walk into the other region each second.
    let migrate: usize = herd::arg_or("migrate", 0usize);
    if migrate > 0 && to.is_none() {
        eprintln!("--migrate needs --to, which says where they are going");
        std::process::exit(2);
    }

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

    // The destination is reached through the subscriptions this edge already
    // holds, so this asks only whether it is there and whether it runs a world
    // whose packets decode the same way. See docs/adr/0001.
    if let Some(other) = to {
        let away = link.info(other, Duration::from_secs(5)).unwrap_or_else(|e| {
            eprintln!("asking {other} what it runs: {e}");
            std::process::exit(1);
        });
        if away.config != cfg {
            eprintln!("{other} runs a different world from {region}");
            std::process::exit(1);
        }
        println!(
            "herd-edge: walking {migrate} observers a second between \
             {region} and {other}"
        );
    }

    // A column of its own, so several edges do not stack on one spot. The
    // region is 4096 m; a hundred edges would still fit.
    let lane = (std::process::id() % 64) as i32 * 60 + 64;
    let home = |n: usize| Pos3::from_meters(lane, 64 + (n as i32 % 3072), 0);

    // The token is this edge's own handle for whatever asked. A real edge uses
    // the handle it holds for the game client; herd has no clients, so it
    // counts. Each token is spent once and never reused, so what was asked for
    // can be looked up by it when the region reports the arrival.
    let mut wanted: Vec<(Pos3, EntityKind)> = (0..observers)
        .map(|n| (home(n), EntityKind::Observer))
        .chain((0..unattended).map(|n| (home(observers + n), EntityKind::Unattended)))
        .collect();
    let asked: Vec<Spawn> = wanted
        .iter()
        .enumerate()
        .map(|(token, &(position, kind))| Spawn { position, kind, token: token as u64 })
        .collect();
    link.spawn(region, &asked).expect("asks for its population");
    println!(
        "herd-edge: asked for {observers} observers and {unattended} unattended \
         in lane x={lane}"
    );

    // Entities the regions reported, with the token each was asked for under,
    // and ones they reported gone. Both carry the region, since this edge deals
    // with more than one and the same ids come back from each.
    let arrivals: Mutex<Vec<(RegionId, EntityId, u64)>> = Mutex::new(Vec::new());
    let departures: Mutex<Vec<(RegionId, EntityId)>> = Mutex::new(Vec::new());
    let stop = AtomicBool::new(false);
    let updates = AtomicU64::new(0);
    let records = AtomicU64::new(0);
    let own = AtomicU64::new(0);

    std::thread::scope(|scope| {
        // Receive: replication coming down, and the ids for anything spawned.
        scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                let Some(message) = link.receive() else { return };
                match message {
                    Incoming::Presence { region, what: Presence::Added { entity, token } } => {
                        arrivals.lock().expect("not poisoned").push((region, entity, token));
                    }
                    Incoming::Presence { region, what: Presence::Removed { entity } } => {
                        // Reported whatever caused it, including a despawn this
                        // edge never asked for.
                        departures.lock().expect("not poisoned").push((region, entity));
                    }
                    Incoming::State { entity, packet, .. } => {
                        updates.fetch_add(1, Ordering::Relaxed);
                        let Some(reader) = PacketReader::new(&codec, &packet) else {
                            continue;
                        };
                        let mut n = 0u64;
                        for (id, _) in reader.updates() {
                            n += 1;
                            if id == entity {
                                own.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        records.fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
        });

        // Send: movement every tick, and churn and migration once a second.
        let period = Duration::from_millis(1_000 / hz.max(1) as u64);
        let step = Fixed::from_raw(Fixed::from_meters(WALK_M_PER_SEC).raw() / hz.max(1) as i32);
        let mut held: Vec<Held> = Vec::new();
        // Entities asked for in a destination whose origin copy is still there.
        // The wait belongs here rather than in the library: events arrive on the
        // same channel as payloads, so waiting inside a `migrate` call would
        // mean eating messages meant for this loop. See docs/adr/0003.
        let mut in_transit: HashMap<u64, (RegionId, EntityId)> = HashMap::new();
        let mut next_home = observers + unattended;
        // Where the migration sweep has got to.
        let mut cursor = 0usize;
        let mut reported = Instant::now();
        let mut sent = 0u64;
        let mut given_back = 0u64;
        let mut migrated = 0u64;

        while !stop.load(Ordering::Relaxed) {
            let deadline = Instant::now() + period;

            for (region, id, token) in arrivals.lock().expect("not poisoned").drain(..) {
                // The token says which request this is the answer to, and so
                // where the entity was asked for and what kind it is.
                let Some(&(at, kind)) = wanted.get(token as usize) else { continue };
                held.push(Held { region, id, at, heading: 1, observes: kind.observes() });
                // Step three of docs/adr/0003: the destination has it, so the
                // origin's copy can go back, and only now. Ordered the other way
                // there is a window where the entity exists nowhere.
                if let Some((from, was)) = in_transit.remove(&token) {
                    // Dropped here rather than when the origin reports it gone.
                    // A move already in flight for an entity just given back is
                    // refused, and there is no reason to send another.
                    held.retain(|h| !(h.region == from && h.id == was));
                    if link.despawn(from, &[was]).is_err() {
                        return;
                    }
                    migrated += 1;
                }
            }
            // Anything a region says is gone stops being moved, however it
            // went. Without this the edge would send refused moves forever.
            for (region, id) in departures.lock().expect("not poisoned").drain(..) {
                held.retain(|h| !(h.region == region && h.id == id));
            }

            for h in held.iter_mut() {
                let moved = Fixed::from_raw(h.at.x.raw() + step.raw() * h.heading);
                if (moved.floor_meters() - lane).abs() > RANGE_M {
                    h.heading = -h.heading;
                } else {
                    h.at.x = moved;
                }
            }

            // Everything this edge manages moves, watched or not: an
            // unattended entity is still authoritative and still replicated.
            // Grouped by region, since a move names entities in one.
            let mut moves: HashMap<RegionId, Vec<(EntityId, Pos3)>> = HashMap::new();
            for h in held.iter() {
                moves.entry(h.region).or_default().push((h.id, h.at));
            }
            for (region, batch) in moves {
                if link.move_entities(region, &batch).is_err() {
                    return;
                }
                sent += batch.len() as u64;
            }

            if reported.elapsed() >= Duration::from_secs(1) {
                // An entity already asked for elsewhere is left alone by both
                // of the below: it is about to be given back where it is.
                let leaving: Vec<(RegionId, EntityId)> = in_transit.values().copied().collect();
                let settled = |h: &Held| !leaving.contains(&(h.region, h.id));

                // Churn is clients, so it is observers that come and go. An
                // unattended entity has nobody to disconnect. A replacement is
                // asked for in the region the one it replaces was in, so
                // churn and migration do not fight over where the crowd is.
                let watched: Vec<usize> = held
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| h.observes && settled(h))
                    .map(|(at, _)| at)
                    .collect();
                if churn > 0 && watched.len() >= churn {
                    let leaving_at = &watched[watched.len() - churn..];
                    let mut back: HashMap<RegionId, Vec<EntityId>> = HashMap::new();
                    let mut coming: HashMap<RegionId, Vec<Spawn>> = HashMap::new();
                    for (k, &at) in leaving_at.iter().enumerate() {
                        back.entry(held[at].region).or_default().push(held[at].id);
                        // Clients arriving: ask for replacements, each under a
                        // fresh token.
                        let position = home(next_home + k);
                        wanted.push((position, EntityKind::Observer));
                        coming.entry(held[at].region).or_default().push(Spawn {
                            position,
                            kind: EntityKind::Observer,
                            token: (wanted.len() - 1) as u64,
                        });
                    }
                    next_home += churn;
                    for &at in leaving_at.iter().rev() {
                        held.remove(at);
                    }
                    for (region, ids) in back {
                        if link.despawn(region, &ids).is_err() {
                            return;
                        }
                        given_back += ids.len() as u64;
                    }
                    for (region, spawns) in coming {
                        if link.spawn(region, &spawns).is_err() {
                            return;
                        }
                    }
                }

                // Migration, steps one and two of docs/adr/0003. Ask the other
                // region for the entity, at the position the game chose, and
                // record that its origin copy is waiting on the answer. Step
                // three happens when the answer arrives, above.
                if let Some(other) = to
                    && !held.is_empty()
                {
                    // Whoever the cursor reaches goes to the other region,
                    // whichever region that is. It sweeps rather than taking
                    // the front of the list, because an arrival is pushed to
                    // the back: taking the front would walk the same few back
                    // and forth while the rest never moved. Sweeping settles
                    // the crowd at about half in each region.
                    let mut asking: HashMap<RegionId, Vec<Spawn>> = HashMap::new();
                    let mut chosen = 0;
                    for _ in 0..held.len() {
                        if chosen == migrate {
                            break;
                        }
                        cursor = (cursor + 1) % held.len();
                        let h = &held[cursor];
                        if !h.observes || !settled(h) {
                            continue;
                        }
                        let (from, was, position) = (h.region, h.id, h.at);
                        let there = if from == region { other } else { region };
                        wanted.push((position, EntityKind::Observer));
                        let token = (wanted.len() - 1) as u64;
                        in_transit.insert(token, (from, was));
                        asking.entry(there).or_default().push(Spawn {
                            position,
                            kind: EntityKind::Observer,
                            token,
                        });
                        chosen += 1;
                    }
                    for (there, spawns) in asking {
                        if link.spawn(there, &spawns).is_err() {
                            return;
                        }
                    }
                }

                let travel = match to {
                    Some(other) => format!(
                        " | {migrated} migrated ({} in {other}, {} in flight)",
                        held.iter().filter(|h| h.region == other).count(),
                        in_transit.len(),
                    ),
                    None => String::new(),
                };
                println!(
                    "herd-edge: holding {} ({} observing) | {sent} moves sent | \
                     {} updates | {} records | {} of them its own | \
                     {given_back} handed back{travel}",
                    held.len(),
                    held.iter().filter(|h| h.observes).count(),
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
