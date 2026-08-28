//! Several edges populate a region, move their entities, and are sent the
//! movement back.
//!
//! **Requires a running `nats-server`.** ADR 0001 chose that over a transport
//! seam: an in-memory transport built only for tests would be a second
//! implementation of the thing under test. Point `NATS_URL` elsewhere if the
//! broker is not on the default port.
//!
//! What it establishes: an edge asks for entities and is told the ids the
//! region allocated, the region applies the positions the edge sends, and the
//! payload that comes back carries those positions.

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{
    EdgeName, EdgeSink, EntityKind, Incoming, Inbound, Presence, RegionClient, RegionId,
    RegionServer, Spawn,
};
use umwelt::sim::{ClientLimits, Flow, Handoff, Overrun, Pacing, Step, Wait};
use umwelt::{EntityId, Game, PacketReader, Pos3, RecordCodec, WorldConfig, WorldSimulation};

const EDGES: usize = 3;
/// Entities with a game client behind them. Each gets a viewer.
const OBSERVERS: usize = 24;
/// Entities with nothing behind them: replicated to whoever can see them, and
/// sent nothing themselves.
const UNATTENDED: usize = 8;
const PER_EDGE: usize = OBSERVERS + UNATTENDED;

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// Distinct per run, so a shared broker does not carry one run's subjects into
/// another's.
fn region_id() -> RegionId {
    RegionId::from_raw(1_000_000 + std::process::id() % 1000)
}

/// The region's game. It applies what the edges sent, and after a while
/// despawns one entity of its own accord, which no edge asked for and which
/// only a presence message can report.
struct Applier {
    inbound: Arc<Inbound>,
    ticks: u32,
    culled: Arc<Mutex<Option<EntityId>>>,
}

/// Late enough that every edge has its population and is moving it.
const CULL_AT: u32 = 200;

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
        self.ticks += 1;
        if self.ticks == CULL_AT {
            // The first live slot will do; it belongs to some edge.
            if let Some(id) = step.live().iter().next() {
                step.despawn(id);
                *self.culled.lock().expect("not poisoned") = Some(id);
            }
        }
    }
}

/// 100 Hz, so the test does not spend a second per twenty ticks. Everything
/// else is the default world, whose wire precision is lossless, which is what
/// lets the test compare a position it sent against the one that came back.
fn config() -> WorldConfig {
    WorldConfig::builder()
        .region_size_m(4096)
        .vertical_extent_m(1024)
        .horizontal_view_radius_m(256)
        .max_horizontal_speed_m_per_sec(40)
        .tick_hz(100)
        .build()
        .expect("config is valid")
}

/// Where one edge's crowd starts: a column per edge, spread down the y axis.
fn home(edge: usize, n: usize) -> Pos3 {
    Pos3::from_meters(100 + edge as i32 * 40, 100 + n as i32, 0)
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn edges_populate_a_region_and_are_sent_the_movement_back() {
    let cfg = config();
    let region = region_id();
    let edges = Arc::new(umwelt::net::Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));

    // The test owns the connection, as a deployment would.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let client = runtime
        .block_on(async_nats::connect(url()))
        .expect("a nats-server must be running; see the module doc");
    // Held for the whole run: dropping it aborts the subscriptions.
    let _server = RegionServer::new(
        client.clone(),
        runtime.handle().clone(),
        region,
        cfg,
        Arc::clone(&inbound),
        Duration::from_secs(5),
    )
    .expect("serves");
    let sink =
        EdgeSink::new(region, client.clone(), runtime.handle().clone(), Arc::clone(&edges));

    let culled: Arc<Mutex<Option<EntityId>>> = Arc::new(Mutex::new(None));
    let mut sim = WorldSimulation::new(
        cfg,
        Applier { inbound: Arc::clone(&inbound), ticks: 0, culled: Arc::clone(&culled) },
    )
    .with_sink(Handoff::new(sink.clone()));

    let stop = AtomicBool::new(false);
    // Positions that made the whole round trip: sent by an edge, applied by the
    // region, replicated, and decoded back on the edge that sent them.
    let confirmed = AtomicU64::new(0);
    // Entities the region reported gone, whoever caused it.
    let removed: Mutex<Vec<EntityId>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        let sink_for_loop = sink.clone();
        let inbound_for_loop = Arc::clone(&inbound);
        let stop_for_loop = &stop;
        scope.spawn(move || {
            sim.run(
                Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
                |_report, sim| {
                    inbound_for_loop.settle(sim, &sink_for_loop, ClientLimits::default());
                    if stop_for_loop.load(Ordering::Relaxed) {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    }
                },
            )
        });

        for e in 0..EDGES {
            let confirmed = &confirmed;
            let stop = &stop;
            let removed = &removed;
            scope.spawn(move || {
                let name = EdgeName::new(format!("test-{}-{e}", std::process::id()))
                    .expect("valid name");
                let edge_runtime = tokio::runtime::Runtime::new().expect("a runtime");
                let edge_client = edge_runtime
                    .block_on(async_nats::connect(url()))
                    .expect("connects to nats");
                let link =
                    RegionClient::new(edge_client, edge_runtime.handle().clone(), name)
                        .expect("subscribes");
                let offer =
                    link.info(region, Duration::from_secs(5)).expect("the region answers");
                assert_eq!(offer.region, region);
                let codec = RecordCodec::new(&offer.config);

                // The token is how an arrival is matched to the request that
                // asked for it, since a presence subject says only which edge
                // owns the entity.
                let mut asked: Vec<Spawn> = (0..OBSERVERS)
                    .map(|n| Spawn {
                        position: home(e, n),
                        kind: EntityKind::Observer,
                        token: n as u64,
                    })
                    .collect();
                asked.extend((0..UNATTENDED).map(|n| Spawn {
                    position: home(e, OBSERVERS + n),
                    kind: EntityKind::Unattended,
                    token: (OBSERVERS + n) as u64,
                }));
                link.spawn(region, &asked).expect("asks for its crowd");

                // The region allocates the ids; the edge learns them here.
                let mut mine: Vec<Option<EntityId>> = vec![None; PER_EDGE];
                let deadline = Instant::now() + Duration::from_secs(20);
                while mine.iter().any(Option::is_none) {
                    assert!(Instant::now() < deadline, "the region never reported the spawns");
                    let Some(message) = link.receive_timeout(Duration::from_millis(200)) else {
                        continue;
                    };
                    let Incoming::Presence { what, .. } = message else { continue };
                    if let Presence::Added { entity, token } = what {
                        let at = token as usize;
                        assert!(at < PER_EDGE, "a token this edge never sent came back");
                        mine[at] = Some(entity);
                    }
                }
                let movable: Vec<EntityId> = mine.iter().map(|m| m.expect("filled")).collect();

                let mut sorted: Vec<u32> = movable.iter().map(|id| id.raw()).collect();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), PER_EDGE, "the region handed out duplicate ids");

                std::thread::scope(|inner| {
                    inner.spawn(|| {
                        let mut at = 0i32;
                        while !stop.load(Ordering::Relaxed) {
                            at = (at + 1) % 64;
                            let moves: Vec<(EntityId, Pos3)> = movable
                                .iter()
                                .enumerate()
                                .map(|(n, id)| {
                                    let base = home(e, n);
                                    (
                                        *id,
                                        Pos3::from_meters(
                                            base.x.floor_meters() + at,
                                            base.y.floor_meters(),
                                            0,
                                        ),
                                    )
                                })
                                .collect();
                            if link.move_entities(region, &moves).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    });

                    while !stop.load(Ordering::Relaxed) {
                        let Some(message) = link.receive_timeout(Duration::from_millis(200))
                        else {
                            continue;
                        };
                        match message {
                            Incoming::State { entity, packet, .. } => {
                                let Some(reader) = PacketReader::new(&codec, &packet) else {
                                    continue;
                                };
                                // A packet is named by the avatar it was built
                                // for, and that avatar always sees itself: it is
                                // at distance zero from itself.
                                for (id, pos) in reader.updates() {
                                    if id == entity
                                        && pos.x.floor_meters() > 100 + e as i32 * 40
                                    {
                                        confirmed.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            }
                            Incoming::Presence { what: Presence::Removed { entity }, .. } => {
                                removed.lock().expect("not poisoned").push(entity);
                            }
                            Incoming::Presence { .. } => {}
                        }
                    }
                });
            });
        }

        wait_until("every edge to be heard from", || edges.len() == EDGES);
        wait_until("every entity to be claimed", || {
            (0..EDGES as u32)
                .all(|e| edges.entity_count(umwelt::net::EdgeId::from_raw(e)) == PER_EDGE)
        });
        wait_until("movement to make the round trip", || {
            confirmed.load(Ordering::Relaxed) >= 100
        });

        // The game despawned one entity that no edge asked about, and only a
        // presence message can carry that. Nothing reported it before.
        wait_until("the game's own despawn to be reported", || {
            let culled = *culled.lock().expect("not poisoned");
            culled.is_some_and(|id| removed.lock().expect("not poisoned").contains(&id))
        });

        assert_eq!(edges.len(), EDGES);
        assert!(sink.sent() > 0, "payloads reached the edges");
        assert_eq!(sink.failed(), 0, "no publish failed");

        stop.store(true, Ordering::Relaxed);
    });
}
