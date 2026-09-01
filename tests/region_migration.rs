//! One edge moves an entity from one region into another, which is
//! `docs/adr/0003`.
//!
//! **Requires a running `nats-server`.** Point `NATS_URL` elsewhere if the
//! broker is not on the default port.
//!
//! Two regions run in this process and one edge talks to both through a single
//! `RegionClient`. Reaching the destination costs it no connect and no
//! subscribe: its subscriptions are wildcards over the region, taken once at
//! construction. That is the property `docs/adr/0001` bought and this is what
//! spends it.
//!
//! What it establishes: the sequence needs no message the protocol does not
//! already have. The edge asks the destination for the entity, waits for the
//! add carrying the id the destination allocated, and only then gives the
//! origin's copy back. Bystanders in each region are told what actually
//! happened — the origin's are told to forget it, the destination's are sent
//! it — and the id it held in the origin is refused there afterwards.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use umwelt::internals::region::{Incoming, Presence, RegionClient, Spawn};
use umwelt::internals::{RecordCodec, read_payload};
use umwelt::net::{EdgeId, EdgeName, EdgeSink, Edges, Inbound};
use umwelt::{ClientLimits, EntityId, EntityKind, Flow, Game, Handoff, Overrun};
use umwelt::{Pacing, Pos3, RegionId, RegionServer, Step, Wait};
use umwelt::{WorldConfig, WorldSimulation};

/// Unattended entities the origin holds and the destination does not, so the
/// ids the two regions hand out cannot coincide. The traveler's origin id ends
/// up past everything the destination has allocated, which is what lets the
/// last assertion tell a stale id from a valid one.
const FILLER: u64 = 6;

/// Tokens this edge spends, each exactly once. A presence subject says which
/// edge owns an entity and nothing more, so the token is the only thing tying
/// an arrival to the request that asked for it.
const ORIGIN_BYSTANDER: u64 = 1;
const ORIGIN_FILLER: u64 = 2; // and the FILLER - 1 tokens after it
const ORIGIN_TRAVELER: u64 = 20;
const DESTINATION_BYSTANDER: u64 = 30;
const DESTINATION_TRAVELER: u64 = 40;

/// How long any wait here is given before it is a failure.
const PATIENCE: Duration = Duration::from_secs(20);

fn url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into())
}

/// Distinct per run, so a shared broker does not carry one run's subjects into
/// another's, and distinct from the other integration test's.
fn region_ids() -> (RegionId, RegionId) {
    let run = std::process::id() % 1000;
    (RegionId::from_raw(2_000_000 + run), RegionId::from_raw(2_001_000 + run))
}

/// 100 Hz, so a wait costs tens of milliseconds rather than seconds. Everything
/// else is the default world, whose wire precision is lossless.
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

/// Where the origin's crowd lives, and where the destination's does. Nowhere
/// near each other, as a door into a dungeon would be.
fn origin_home(n: u64) -> Pos3 {
    Pos3::from_meters(100, 100 + n as i32, 0)
}

fn destination_home(n: u64) -> Pos3 {
    Pos3::from_meters(2000, 2000 + n as i32, 0)
}

/// A region's game. It applies what its edges sent and does nothing else.
struct Applier {
    inbound: Arc<Inbound>,
}

impl Game for Applier {
    fn step(&mut self, step: &mut Step<'_>) {
        self.inbound.apply(step);
    }
}

/// One region simulation running in this process.
struct Region {
    cfg: WorldConfig,
    edges: Arc<Edges>,
    inbound: Arc<Inbound>,
    sink: EdgeSink,
    /// Held for the whole run: dropping it aborts the subscriptions.
    _server: RegionServer,
}

impl Region {
    fn serve(
        client: &async_nats::Client,
        runtime: &tokio::runtime::Handle,
        id: RegionId,
        cfg: WorldConfig,
    ) -> Region {
        let edges = Arc::new(Edges::new());
        let inbound = Arc::new(Inbound::new(Arc::clone(&edges)));
        let server = RegionServer::new(
            client.clone(),
            runtime.clone(),
            id,
            cfg,
            Arc::clone(&inbound),
            Duration::from_secs(5),
        )
        .expect("serves");
        let sink = EdgeSink::new(id, client.clone(), runtime.clone(), Arc::clone(&edges));
        Region { cfg, edges, inbound, sink, _server: server }
    }

    /// Ticks until told to stop.
    fn run(&self, stop: &AtomicBool) {
        let mut sim = WorldSimulation::new(
            self.cfg,
            Applier { inbound: Arc::clone(&self.inbound) },
        )
        .with_sink(Handoff::new(self.sink.clone()));
        sim.run(
            Pacing { wait: Wait::Sleep, overrun: Overrun::Dilate, ticks: None },
            |_, sim| {
                self.inbound.settle(sim, &self.sink, ClientLimits::default());
                if stop.load(Ordering::Relaxed) { Flow::Stop } else { Flow::Continue }
            },
        );
    }

    /// This test's only edge, which is the first one either region admitted.
    fn only_edge(&self) -> EdgeId {
        EdgeId::from_raw(0)
    }
}

/// The edge, and everything it has been told.
///
/// One loop drives all of it, so none of this is shared and none of it is
/// locked. A real edge server has game client sockets to service as well and
/// would put the receive side on its own thread; nothing here needs that.
struct Edge {
    link: RegionClient,
    codec: RecordCodec,
    regions: [RegionId; 2],
    /// Where each token asked for its entity, so an arrival can be placed.
    wanted: HashMap<(RegionId, u64), Pos3>,
    /// What this edge manages: region, id, and where it was asked for.
    held: Vec<(RegionId, EntityId, Pos3)>,
    /// The id each token's entity got.
    added: HashMap<(RegionId, u64), EntityId>,
    removed: HashSet<(RegionId, EntityId)>,
    /// `(region, avatar, entity)` for every entity a packet told that avatar
    /// about, and for every one it told the avatar to forget.
    seen: HashSet<(RegionId, EntityId, EntityId)>,
    forgot: HashSet<(RegionId, EntityId, EntityId)>,
    /// When the first packet addressed to an entity arrived.
    first_packet: HashMap<(RegionId, EntityId), Instant>,
    /// How far along its walk everything held is.
    walk: i32,
}

impl Edge {
    fn new(link: RegionClient, codec: RecordCodec, regions: [RegionId; 2]) -> Edge {
        Edge {
            link,
            codec,
            regions,
            wanted: HashMap::new(),
            held: Vec::new(),
            added: HashMap::new(),
            removed: HashSet::new(),
            seen: HashSet::new(),
            forgot: HashSet::new(),
            first_packet: HashMap::new(),
            walk: 0,
        }
    }

    /// Asks a region for one entity, under a token spent here and nowhere else.
    fn ask(&mut self, region: RegionId, token: u64, kind: EntityKind, at: Pos3) {
        self.wanted.insert((region, token), at);
        self.link
            .spawn(region, &[Spawn { position: at, kind, token }])
            .expect("asks for an entity");
    }

    fn give_back(&mut self, region: RegionId, entity: EntityId) {
        self.link.despawn(region, &[entity]).expect("gives an entity back");
    }

    fn id_of(&self, region: RegionId, token: u64) -> Option<EntityId> {
        self.added.get(&(region, token)).copied()
    }

    fn id(&self, region: RegionId, token: u64) -> EntityId {
        self.id_of(region, token).expect("the region reported this token")
    }

    /// Takes everything waiting, having waited `within` for the first of it.
    fn pump(&mut self, within: Duration) {
        if let Some(first) = self.link.receive_timeout(within) {
            self.take(first);
            while let Some(more) = self.link.try_receive() {
                self.take(more);
            }
        }
    }

    fn take(&mut self, message: Incoming) {
        match message {
            Incoming::Presence { region, what: Presence::Added { entity, token } } => {
                let at = *self
                    .wanted
                    .get(&(region, token))
                    .expect("a token this edge never sent came back");
                self.held.push((region, entity, at));
                self.added.insert((region, token), entity);
            }
            Incoming::Presence { region, what: Presence::Removed { entity } } => {
                self.held.retain(|&(r, id, _)| (r, id) != (region, entity));
                self.removed.insert((region, entity));
            }
            Incoming::State { region, entity, packet } => {
                self.first_packet.entry((region, entity)).or_insert_with(Instant::now);
                // Read out before either set is touched: the reader borrows the
                // codec, which lives on the same struct.
                let Some(reader) = read_payload(&self.codec, &packet) else {
                    return;
                };
                let forgot: Vec<EntityId> = reader.despawns().collect();
                let seen: Vec<EntityId> = reader.updates().map(|(id, _, _)| id).collect();
                for id in forgot {
                    self.forgot.insert((region, entity, id));
                }
                for id in seen {
                    self.seen.insert((region, entity, id));
                }
            }
        }
    }

    /// Moves everything this edge holds, in whichever region holds it.
    ///
    /// Entities have to move for records to be worth sending, and an entity
    /// that has just arrived is a record whether it moved or not.
    fn push(&mut self) {
        self.walk = (self.walk + 1) % 16;
        for region in self.regions {
            let moves: Vec<(EntityId, Pos3)> = self
                .held
                .iter()
                .filter(|&&(r, _, _)| r == region)
                .map(|&(_, id, at)| {
                    (
                        id,
                        Pos3::from_meters(
                            at.x.floor_meters() + self.walk,
                            at.y.floor_meters(),
                            0,
                        ),
                    )
                })
                .collect();
            if !moves.is_empty() {
                self.link.move_entities(region, &moves).expect("moves what it holds");
            }
        }
    }

    /// Keeps the edge running until `done`, or fails.
    fn until(&mut self, what: &str, done: impl Fn(&Edge) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while !done(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.pump(Duration::from_millis(5));
            self.push();
        }
    }
}

#[test]
fn an_edge_moves_an_entity_from_one_region_into_another() {
    let cfg = config();
    let (origin_id, destination_id) = region_ids();

    // The test owns the connection, as a deployment would.
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    let client = runtime
        .block_on(async_nats::connect(url()))
        .expect("a nats-server must be running; see the module doc");

    let origin = Region::serve(&client, runtime.handle(), origin_id, cfg);
    let destination = Region::serve(&client, runtime.handle(), destination_id, cfg);

    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| origin.run(&stop));
        scope.spawn(|| destination.run(&stop));

        let name =
            EdgeName::new(format!("migrate-{}", std::process::id())).expect("valid name");
        let edge_runtime = tokio::runtime::Runtime::new().expect("a runtime");
        let edge_client =
            edge_runtime.block_on(async_nats::connect(url())).expect("connects to nats");
        // One client, two regions. The second costs nothing to reach.
        let link = RegionClient::new(edge_client, edge_runtime.handle().clone(), name)
            .expect("subscribes");
        let offer =
            link.info(origin_id, Duration::from_secs(5)).expect("the origin answers");
        let away = link
            .info(destination_id, Duration::from_secs(5))
            .expect("the destination answers");
        assert_eq!(offer.config, away.config, "both regions run the same world");
        let mut edge =
            Edge::new(link, RecordCodec::new(&offer.config), [origin_id, destination_id]);

        // The origin's population: a bystander who stays put, filler to push
        // the origin's ids past anything the destination will allocate, and the
        // traveler.
        edge.ask(origin_id, ORIGIN_BYSTANDER, EntityKind::observer(0), origin_home(0));
        for k in 0..FILLER {
            edge.ask(
                origin_id,
                ORIGIN_FILLER + k,
                EntityKind::unattended(0),
                origin_home(1 + k),
            );
        }
        edge.ask(
            origin_id,
            ORIGIN_TRAVELER,
            EntityKind::observer(0),
            origin_home(1 + FILLER),
        );
        // And one bystander waiting in the destination.
        edge.ask(
            destination_id,
            DESTINATION_BYSTANDER,
            EntityKind::observer(0),
            destination_home(0),
        );

        let population = FILLER as usize + 3;
        edge.until("both regions to report the population", |e| {
            e.held.len() == population
        });

        let bystander = edge.id(origin_id, ORIGIN_BYSTANDER);
        let traveler = edge.id(origin_id, ORIGIN_TRAVELER);
        let waiting = edge.id(destination_id, DESTINATION_BYSTANDER);

        // Before it goes anywhere, the origin's bystander is being sent it.
        edge.until("the origin's bystander to be sent the traveler", |e| {
            e.seen.contains(&(origin_id, bystander, traveler))
        });

        // ---- the sequence in docs/adr/0003 -----------------------------
        //
        // Spawn first, despawn second. Ordered the other way there is a window
        // where the entity exists nowhere, and nothing here needs that window.

        // 1. Ask the destination for it, at the position the game chose.
        let asked_at = Instant::now();
        edge.ask(
            destination_id,
            DESTINATION_TRAVELER,
            EntityKind::observer(0),
            destination_home(1),
        );

        // 2. Wait for the destination's add, which carries the id it allocated.
        edge.until("the destination to report the traveler", |e| {
            e.id_of(destination_id, DESTINATION_TRAVELER).is_some()
        });
        let arrived_at = Instant::now();
        let landed = edge.id(destination_id, DESTINATION_TRAVELER);

        // 3. Only now, give the origin's copy back.
        edge.give_back(origin_id, traveler);
        edge.until("the origin to report the traveler gone", |e| {
            e.removed.contains(&(origin_id, traveler))
        });

        // ---- what everyone was told -------------------------------------

        // The destination allocated its own id. The filler put the origin's
        // past every slot the destination has handed out, so these cannot
        // coincide by luck.
        assert_ne!(landed, traveler, "the destination reused the origin's id");
        assert!(
            traveler.raw() > landed.raw(),
            "the setup meant to leave the origin's ids past the destination's, \
             but the traveler was {traveler:?} in the origin and {landed:?} here"
        );

        // The origin's bystander is told to forget it, which is what happened
        // there. The destination's is sent it, which is what happened here.
        edge.until("the origin's bystander to be told to forget the traveler", |e| {
            e.forgot.contains(&(origin_id, bystander, traveler))
        });
        edge.until("the destination's bystander to be sent the traveler", |e| {
            e.seen.contains(&(destination_id, waiting, landed))
        });

        // And the traveler itself is now served by the destination: an avatar
        // always sees itself, so its own packets name it.
        edge.until("the traveler to be served by the destination", |e| {
            e.seen.contains(&(destination_id, landed, landed))
        });

        // The origin no longer counts it, and the destination does.
        edge.until("the origin to stop counting the traveler", |_| {
            origin.edges.entity_count(origin.only_edge()) == population - 2
        });
        assert_eq!(
            destination.edges.entity_count(destination.only_edge()),
            2,
            "the destination holds the bystander and the traveler"
        );

        // The id the traveler had in the origin means nothing here. A move
        // under it is refused rather than applied, which is why the consumer
        // has to rekey the state it holds against the id the destination gave.
        let before = destination.inbound.refused();
        edge.link
            .move_entities(destination_id, &[(traveler, destination_home(1))])
            .expect("publishes");
        edge.until("the destination to refuse the origin's id", |_| {
            destination.inbound.refused() > before
        });

        // What the transition cost, from asking the destination to the first
        // packet it addressed to the entity. `docs/adr/0003` claims tens of
        // milliseconds against the seconds a connect and handshake would take.
        let first = *edge
            .first_packet
            .get(&(destination_id, landed))
            .expect("the destination served it");
        eprintln!(
            "migration: asked to added {:?}, added to first packet {:?}, \
             asked to first packet {:?}",
            arrived_at - asked_at,
            first - arrived_at,
            first - asked_at,
        );

        assert_eq!(origin.sink.failed(), 0, "no publish failed in the origin");
        assert_eq!(destination.sink.failed(), 0, "no publish failed in the destination");

        stop.store(true, Ordering::Relaxed);
    });
}
