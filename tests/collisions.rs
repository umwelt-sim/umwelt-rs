//! A region reports what reaches its box to the edge that owns the entity,
//! and to nobody for an entity of its own.
//!
//! **Requires a running `nats-server`.** Point `NATS_URL` elsewhere if the
//! broker is not on the default port.
//!
//! What it establishes: an observer spawned within a view radius of the box
//! is reported as a view collision when first served; moved to the middle it
//! is reported cleared; a move aimed past the box is reported as a boundary
//! collision carrying the target as asked, once, and the entity stands at the
//! box; and an entity the region's own game walks into the wall every tick is
//! reported to no edge at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use umwelt::internals::region::{Incoming, Presence, RegionClient, Spawn};
use umwelt::internals::{RecordCodec, read_payload};
use umwelt::net::{EdgeName, EdgeSink, Edges, Inbound};
use umwelt::{ClientLimits, EntityId, EntityKind, Fixed, Flow, Game, Handoff, Overrun};
use umwelt::{Pacing, Pos3, RegionId, RegionServer, Step, Wait, WorldPos};
use umwelt::{WorldConfig, WorldSimulation};

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

fn region_id() -> RegionId {
    RegionId::from_raw(6_000_000 + std::process::id() % 1000)
}

/// Applies what the edge sent, and walks an entity of its own into the east
/// wall on every tick. That entity has no edge, so its collisions reach nobody.
struct Applier {
    inbound: Arc<Inbound>,
    own: Option<EntityId>,
    own_id: Arc<Mutex<Option<EntityId>>>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
        let size = step.config().region_size();
        match self.own {
            None => {
                let id = step.spawn(Pos3::from_meters(4000, 3000, 0), 0);
                self.own = Some(id);
                *self.own_id.lock().expect("not poisoned") = Some(id);
            }
            Some(id) => step.move_to(
                id,
                Pos3::new(
                    Fixed::from_raw(size.raw() + 4096),
                    Fixed::from_meters(3000),
                    Fixed::ZERO,
                ),
            ),
        }
    }
}

fn wait_for(what: &str, stop: &AtomicBool, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    panic!("timed out waiting for {what}");
}

#[test]
fn a_region_reports_collisions_to_the_owning_edge_only() {
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let client = runtime.block_on(async_nats::connect(url())).expect("connects to nats");
    let region = region_id();
    let cfg = WorldConfig::default();
    let size = cfg.region_size();
    let cell = cfg.cell_size().raw();

    let edges = Arc::new(Edges::new());
    let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
    let _server = RegionServer::new(
        client.clone(),
        runtime.handle().clone(),
        region,
        cfg,
        Arc::clone(&inbound),
        Duration::from_secs(5),
    )
    .expect("serves");
    let sink = EdgeSink::new(
        region,
        client.clone(),
        runtime.handle().clone(),
        Arc::clone(&edges),
    );
    let own_id: Arc<Mutex<Option<EntityId>>> = Arc::new(Mutex::new(None));
    let mut sim = WorldSimulation::new(
        cfg,
        Applier { inbound: Arc::clone(&inbound), own: None, own_id: Arc::clone(&own_id) },
    )
    .with_sink(Handoff::new(sink.clone()));

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _guard = StopOnDrop(&stop);
        let sink_for_loop = sink.clone();
        let inbound_for_loop = Arc::clone(&inbound);
        let stop_for_loop = &stop;
        scope.spawn(move || {
            sim.run(
                Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
                |_, sim| {
                    inbound_for_loop.settle(sim, &sink_for_loop, ClientLimits::default());
                    if stop_for_loop.load(Ordering::Relaxed) {
                        Flow::Stop
                    } else {
                        Flow::Continue
                    }
                },
            )
        });

        let name =
            EdgeName::new(format!("collide-{}", std::process::id())).expect("valid name");
        let edge_runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge_client =
            edge_runtime.block_on(async_nats::connect(url())).expect("connects to nats");
        let link = RegionClient::new(edge_client, edge_runtime.handle().clone(), name)
            .expect("subscribes");
        let offer =
            link.info(region, Duration::from_secs(5)).expect("the region answers");
        let codec = RecordCodec::new(&offer.config);

        // Everything the region says about what this edge owns.
        let mut heard: Vec<Presence> = Vec::new();
        let mut latest: Option<WorldPos> = None;
        let pump = |link: &RegionClient,
                    heard: &mut Vec<Presence>,
                    latest: &mut Option<WorldPos>,
                    mine: Option<EntityId>| {
            while let Some(message) = link.receive_timeout(Duration::from_millis(20)) {
                match message {
                    Incoming::Presence { what, .. } => heard.push(what),
                    Incoming::State { packet, .. } => {
                        if let (Some(reader), Some(me)) =
                            (read_payload(&codec, &packet), mine)
                        {
                            for (id, pos, _) in reader.updates() {
                                if id == me {
                                    *latest = Some(pos);
                                }
                            }
                        }
                    }
                }
            }
        };

        // An observer one and a half cells from the east side: within a view
        // radius of the box, so its first serve reports the view reaching it.
        let near = Pos3::new(
            Fixed::from_raw(size.raw() - cell - cell / 2),
            Fixed::from_meters(2048),
            Fixed::ZERO,
        );
        link.spawn(
            region,
            &[Spawn { position: near, kind: EntityKind::observer(0), token: 1 }],
        )
        .expect("asks");
        let mut me: Option<EntityId> = None;
        wait_for("the spawn to be reported", &stop, || {
            pump(&link, &mut heard, &mut latest, me);
            me = heard.iter().find_map(|p| match p {
                Presence::Added { entity, token: 1 } => Some(*entity),
                _ => None,
            });
            me.is_some()
        });
        let me = me.expect("added");

        wait_for("the view collision", &stop, || {
            pump(&link, &mut heard, &mut latest, Some(me));
            heard.iter().any(|p| matches!(p, Presence::ViewCollision { entity, position } if *entity == me && *position == near))
        });

        // To the middle, and the view no longer reaches the box.
        let mid = Pos3::from_meters(2048, 2048, 0);
        link.move_entities(region, &[(me, mid)]).expect("moves");
        wait_for("the view to clear", &stop, || {
            pump(&link, &mut heard, &mut latest, Some(me));
            heard
                .iter()
                .any(|p| matches!(p, Presence::ViewCleared { entity } if *entity == me))
        });

        // Aimed past the box: clamped there, and reported once with the target
        // as asked. A second identical move is being held against the wall,
        // which costs no second report.
        let beyond = Pos3::new(
            Fixed::from_raw(size.raw() + Fixed::from_meters(10).raw()),
            Fixed::from_meters(2048),
            Fixed::ZERO,
        );
        link.move_entities(region, &[(me, beyond)]).expect("moves");
        wait_for("the boundary collision", &stop, || {
            pump(&link, &mut heard, &mut latest, Some(me));
            heard.iter().any(|p| matches!(p, Presence::BoundaryCollision { entity, target } if *entity == me && *target == beyond))
        });
        link.move_entities(region, &[(me, beyond)]).expect("moves");
        wait_for("the clamped position to come back", &stop, || {
            pump(&link, &mut heard, &mut latest, Some(me));
            latest.is_some_and(|at| at.x == size.raw() as i64 - 1)
        });
        std::thread::sleep(Duration::from_millis(300));
        pump(&link, &mut heard, &mut latest, Some(me));
        let boundary_reports = heard
            .iter()
            .filter(|p| matches!(p, Presence::BoundaryCollision { entity, .. } if *entity == me))
            .count();
        assert_eq!(boundary_reports, 1, "held against the wall is one report");

        // The region's own entity has been walking into the wall the whole
        // time, and nothing about it reached this edge.
        let own = own_id.lock().expect("not poisoned").expect("the game spawned its own");
        assert!(
            heard.iter().all(|p| !matches!(p,
                Presence::BoundaryCollision { entity, .. }
                | Presence::ViewCollision { entity, .. }
                | Presence::ViewCleared { entity } if *entity == own)),
            "a region-owned entity has no edge to report to"
        );

        stop.store(true, Ordering::Relaxed);
    });
}
