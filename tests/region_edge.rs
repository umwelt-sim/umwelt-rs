//! Several edges attach to a region, populate it, move their entities, and are
//! sent the movement back.
//!
//! The automated half of the smoke test. `examples/herd-sim.rs` and
//! `examples/herd-edge.rs` are the same shape at crowd volumes, driven by hand.
//!
//! What it establishes end to end: an edge asks for entities and is told the
//! ids the region allocated, the region applies the positions the edge sends,
//! and the payload that comes back carries those positions. The last step is
//! the one that could not be checked before the link existed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use umwelt::net::{
    EdgeSink, EntityKind, Incoming, Inbound, RegionClient, RegionId, RegionServer, SharedSecret,
};
use umwelt::sim::{ClientLimits, Flow, Game, Handoff, Overrun, Pacing, Step, Wait};
use umwelt::{EntityId, PacketReader, Pos3, RecordCodec, WorldConfig, WorldSimulation};

const KEY: &[u8] = b"a secret both ends hold";
const EDGES: usize = 3;
/// Entities with a game client behind them. Each gets a viewer.
const OBSERVERS: usize = 24;
/// Entities with nothing behind them: replicated to whoever can see them, and
/// sent nothing themselves. No viewer, and none of the per-viewer pipeline.
const UNATTENDED: usize = 8;
const PER_EDGE: usize = OBSERVERS + UNATTENDED;

/// The region's game: everything it does is what the edges asked for.
struct Applier {
    inbound: Arc<Inbound>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
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
    let server = RegionServer::bind(
        "127.0.0.1:0",
        RegionId::from_raw(7),
        cfg,
        Arc::new(SharedSecret::new(KEY)),
    )
    .expect("binds a loopback port");
    let addr = server.local_addr();
    let stop_server = server.shutdown_handle();

    let edges = Arc::clone(server.edges());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let sink = EdgeSink::new(Arc::clone(&edges));

    let mut sim = WorldSimulation::new(cfg, Applier { inbound: Arc::clone(&inbound) })
        .with_sink(Handoff::new(sink.clone()));

    let stop = AtomicBool::new(false);
    // Positions that made the whole round trip: sent by an edge, applied by the
    // region, replicated, and decoded back on the edge that sent them.
    let confirmed = AtomicU64::new(0);

    std::thread::scope(|scope| {
        // The region's accept loop. Each attached edge gets a thread that only
        // queues what it reads.
        scope.spawn(|| {
            server
                .run(|edge| {
                    let _ = inbound.serve(&edge);
                })
                .expect("the accept loop runs until it is stopped")
        });

        // The region's tick loop. settle runs between ticks, which is where a
        // viewer can be registered.
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

        // The edges.
        for e in 0..EDGES {
            let confirmed = &confirmed;
            let stop = &stop;
            scope.spawn(move || {
                let client =
                    RegionClient::connect(addr, KEY).expect("handshake completes");
                assert_eq!(client.region(), RegionId::from_raw(7));
                let codec = RecordCodec::new(client.config());

                let mut asked: Vec<(Pos3, EntityKind)> =
                    (0..OBSERVERS).map(|n| (home(e, n), EntityKind::Observer)).collect();
                asked.extend(
                    (0..UNATTENDED)
                        .map(|n| (home(e, OBSERVERS + n), EntityKind::Unattended)),
                );
                client.spawn(&asked).expect("asks for its crowd");

                // The region allocates the ids; the edge finds out here.
                let mut all: Vec<(EntityId, Option<umwelt::sim::ViewerId>)> = Vec::new();
                let mut body = Vec::new();
                while all.len() < PER_EDGE {
                    match client.receive(&mut body).expect("the region answers") {
                        Incoming::Spawned(reply) => all.extend(reply.entities),
                        Incoming::Updates(_) => {}
                    }
                }
                assert_eq!(all.len(), PER_EDGE, "every entity asked for came back");

                // The kinds came back the way they went out: the observers were
                // given viewers and the unattended entities were not.
                let watched = all.iter().filter(|(_, v)| v.is_some()).count();
                assert_eq!(watched, OBSERVERS, "only observers get a viewer");
                assert!(
                    all[OBSERVERS..].iter().all(|(_, v)| v.is_none()),
                    "an unattended entity must not cost a viewer"
                );

                // Every id is distinct, across every edge as well as within one.
                let mut sorted: Vec<u32> = all.iter().map(|(id, _)| id.raw()).collect();
                sorted.sort_unstable();
                sorted.dedup();
                assert_eq!(sorted.len(), PER_EDGE, "the region handed out duplicate ids");

                let mine: Vec<(EntityId, umwelt::sim::ViewerId)> =
                    all.iter().filter_map(|(id, v)| v.map(|v| (*id, v))).collect();
                // Everything this edge manages moves, watched or not.
                let movable: Vec<EntityId> = all.iter().map(|(id, _)| *id).collect();

                std::thread::scope(|inner| {
                    // Send movement until the test is done.
                    inner.spawn(|| {
                        let mut at = 0i32;
                        while !stop.load(Ordering::Relaxed) {
                            at = (at + 1) % 64;
                            let moves: Vec<(EntityId, Pos3)> = movable
                                .iter()
                                .enumerate()
                                .map(|(n, id)| {
                                    let base = home(e, n);
                                    (*id, Pos3::from_meters(
                                        base.x.floor_meters() + at,
                                        base.y.floor_meters(),
                                        0,
                                    ))
                                })
                                .collect();
                            if client.move_entities(&moves).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    });

                    // Receive the movement back.
                    let mut body = Vec::new();
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(message) = client.receive(&mut body) else { return };
                        let Incoming::Updates(update) = message else { continue };
                        let Some(reader) = PacketReader::new(&codec, update.payload) else {
                            continue;
                        };
                        // A viewer always sees its own avatar: it is at distance
                        // zero from itself.
                        let Some(&(avatar, _)) =
                            mine.iter().find(|(_, v)| *v == update.viewer)
                        else {
                            continue;
                        };
                        for (id, pos) in reader.updates() {
                            if id == avatar && pos.x.floor_meters() > 100 + e as i32 * 40 {
                                confirmed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            });
        }

        // Every edge attached, every entity exists, and movement is coming back.
        wait_until("every edge to attach", || edges.len() == EDGES);
        wait_until("every entity to be claimed", || {
            (0..EDGES as u32).all(|e| {
                edges.entity_count(umwelt::net::EdgeId::from_raw(e)) == PER_EDGE
            })
        });
        wait_until("movement to make the round trip", || {
            confirmed.load(Ordering::Relaxed) >= 100
        });

        assert_eq!(edges.len(), EDGES);
        assert_eq!(inbound.refused(), 0, "nothing an edge sent was declined");
        assert!(sink.sent() > 0, "payloads reached the edges");
        assert_eq!(sink.failed(), 0, "no write to an edge failed");

        stop.store(true, Ordering::Relaxed);
        stop_server.stop();
    });
}
