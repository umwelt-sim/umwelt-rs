//! examples/harness.rs
//!
//! Measures replication quality rather than cost. Everything benchmarked so far
//! answers how long a tick takes; nothing answers whether what the tick sent
//! was worth sending.
//!
//! It models each client's belief — the position it was last told for every
//! entity it holds a ghost of — and each tick compares that belief against the
//! truth. Reported per configuration:
//!
//! - mean and 99th-percentile client-side position error
//! - the same, split by how far the entity is from the viewer
//! - **never updated**: entities that were in a viewer's ghost set and were
//!   never once sent to it, which is the starvation question the 133 of 200 in
//!   the design document asked. Starvation and coverage are counted over the
//!   ghost set, since a candidate past it is one the cap declines to replicate
//!   by design rather than one being starved. Error is counted over every ghost
//!   the client holds, in the set or not, because a stale ghost is rendered
//!   either way and that is what `grace` costs.
//! - **unrepresented**: ghost-set-tick pairs where the client held no ghost
//! - records per viewer per tick, and ghost arrivals plus departures
//!
//! The population is deliberately mixed. Props never move, idlers shuffle,
//! walkers walk and sprinters run. A population where everything moves at one
//! speed cannot distinguish scoring on displacement from scoring on elapsed
//! time, which is the whole reason the odometer exists. **The mix below is
//! chosen, not measured against any real game.**
//!
//! Viewers are drawn from the walkers, so candidate sets churn and the grace
//! period is exercised. Run with `cargo run --release --example harness`.

use std::collections::HashMap;
use std::sync::Mutex;

use umwelt::internals::{NEAR_BAND, Outbound};
use umwelt::sim::{DEFAULT_GHOST_CAP, DEFAULT_GRACE};
use umwelt::{ClientLimits, EntityId, Fixed, Game, Policy, Pos3, Step, Weights};
use umwelt::{WorldConfig, WorldSimulation};

/// Dense enough that a viewer's candidate set exceeds a packet, or there is no
/// selection pressure and every curve behaves identically.
const ENTITIES: usize = 60_000;
const VIEWERS: usize = 200;

/// 20 seconds at 20 Hz, matching the scratch simulation in the design document.
const TICKS: u32 = 400;

/// Ticks discarded before measuring, so a client's belief is not counted while
/// it is still being filled.
const WARMUP: u32 = 40;

const RAW_PER_M: i64 = 1 << 10;

/// Motion classes as (share, meters per second) and their names. Chosen, not
/// measured against any real game.
///
/// The fast class is the one that decides whether the growth curve is cosmetic.
/// Walkers accumulate so little drift between packets that every curve keeps
/// them accurate; something crossing the view radius in eight seconds does not.
const MIX: [(f64, f64, &str); 5] = [
    (0.35, 0.0, "props"),
    (0.25, 0.2, "idlers"),
    (0.25, 1.5, "walkers"),
    (0.10, 6.0, "sprinters"),
    (0.05, 30.0, "vehicles"),
];

/// Index into `MIX` of the class most sweeps draw viewers from.
const WALKING_VIEWERS: usize = 2;

/// Vehicles. A viewer crossing a crowd rather than standing in one, which is
/// the case `grace` exists for.
const DRIVING_VIEWERS: usize = 4;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, bound: u32) -> u32 {
        (self.next_u64() % bound.max(1) as u64) as u32
    }
    /// A unit-ish heading scaled to `speed` raw units per tick.
    fn heading(&mut self, speed: i32) -> (i32, i32) {
        if speed == 0 {
            return (0, 0);
        }
        // Eight compass directions, which is enough variety and needs no
        // trigonometry, so the harness stays integer like the library.
        let d = self.below(8);
        let (sx, sy) = match d {
            0 => (1, 0),
            1 => (1, 1),
            2 => (0, 1),
            3 => (-1, 1),
            4 => (-1, 0),
            5 => (-1, -1),
            6 => (0, -1),
            _ => (1, -1),
        };
        (sx * speed, sy * speed)
    }
}

/// Entities that move at their own speed and bounce off the region edge.
struct Crowd {
    pending: Vec<Pos3>,
    vx: Vec<i32>,
    vy: Vec<i32>,
    lo: i32,
    hi: i32,
}

impl Game for Crowd {
    fn step(&mut self, w: &mut Step<'_>) {
        if !self.pending.is_empty() {
            for p in std::mem::take(&mut self.pending) {
                w.spawn(p);
            }
            return;
        }
        let (lo, hi) = (self.lo, self.hi);
        let (vx, vy) = (&mut self.vx, &mut self.vy);
        let (xs, ys, _) = w.positions_mut();
        for i in 0..xs.len() {
            let nx = xs[i].raw() + vx[i];
            if nx < lo || nx > hi {
                vx[i] = -vx[i];
            } else {
                xs[i] = Fixed::from_raw(nx);
            }
            let ny = ys[i].raw() + vy[i];
            if ny < lo || ny > hi {
                vy[i] = -vy[i];
            } else {
                ys[i] = Fixed::from_raw(ny);
            }
        }
    }
}

/// Builds the population and returns it alongside the entities of each motion
/// class, since a sweep chooses which class its viewers are drawn from.
fn crowd(cfg: &WorldConfig, seed: u64) -> (Crowd, [Vec<u32>; MIX.len()]) {
    let mut rng = Rng::new(seed);
    let margin = Fixed::from_meters(8).raw();
    let extent = cfg.region_size().raw() - 2 * margin;
    let tick_hz = cfg.tick_hz() as f64;

    let mut pending = Vec::with_capacity(ENTITIES);
    let (mut vx, mut vy) = (Vec::with_capacity(ENTITIES), Vec::with_capacity(ENTITIES));
    let mut by_class: [Vec<u32>; MIX.len()] = std::array::from_fn(|_| Vec::new());

    // Cumulative shares, so a single draw picks a class.
    let mut cuts = [0.0f64; MIX.len()];
    let mut acc = 0.0;
    for (k, (share, _, _)) in MIX.iter().enumerate() {
        acc += share;
        cuts[k] = acc;
    }

    for i in 0..ENTITIES {
        pending.push(Pos3::new(
            Fixed::from_raw(margin + rng.below(extent as u32) as i32),
            Fixed::from_raw(margin + rng.below(extent as u32) as i32),
            Fixed::ZERO,
        ));
        let roll = rng.below(10_000) as f64 / 10_000.0;
        let class = cuts.iter().position(|&c| roll < c).unwrap_or(MIX.len() - 1);
        let per_tick = (MIX[class].1 / tick_hz * RAW_PER_M as f64).round() as i32;
        let (dx, dy) = rng.heading(per_tick);
        vx.push(dx);
        vy.push(dy);
        by_class[class].push(i as u32);
    }
    (Crowd { pending, vx, vy, lo: margin, hi: margin + extent }, by_class)
}

/// What one client believes, and what it has ever been told.
struct Client {
    told: HashMap<EntityId, Pos3>,
    ever_seen: Vec<bool>,
    ever_told: Vec<bool>,
}

impl Client {
    fn new() -> Client {
        Client {
            told: HashMap::new(),
            ever_seen: vec![false; ENTITIES],
            ever_told: vec![false; ENTITIES],
        }
    }
}

/// What one viewer's replication produced this tick, captured from the
/// callback so it can be evaluated once the simulation is free again.
#[derive(Default)]
struct Observed {
    candidates: Vec<EntityId>,
    sent: Vec<(EntityId, usize)>,
    departed: Vec<EntityId>,
    /// Records for entities the client did not already hold.
    arrivals: u64,
    bytes: u64,
}

/// Coarse separation bands for the per-distance report, in meters.
const BAND_EDGES: [i32; 3] = [32, 64, 128];
const BAND_NAMES: [&str; 4] = ["0-32 m", "32-64 m", "64-128 m", "128 m+"];

/// Quarter-octave buckets, so a percentile is not pinned to a power of two.
fn bucket_of(v: i64) -> usize {
    let e = (v as u64).max(1);
    let oct = e.ilog2();
    if oct < 2 {
        return oct as usize * 4;
    }
    let sub = ((e >> (oct - 2)) & 3) as u32;
    ((oct * 4 + sub) as usize).min(127)
}

fn bucket_value(bucket: usize) -> u64 {
    let oct = bucket as u32 / 4;
    let sub = bucket as u32 % 4;
    if oct < 2 { 1 << oct } else { ((4 + sub) as u64) << (oct - 2) }
}

fn band_of(sep_raw: i64) -> usize {
    let m = (sep_raw / RAW_PER_M) as i32;
    BAND_EDGES.iter().position(|&e| m < e).unwrap_or(3)
}

struct Metrics {
    /// Error counts by band, quarter-octave buckets on raw units.
    hist: [[u64; 128]; 4],
    err_sum: [u128; 4],
    /// Error over separation, in milliradians. What a viewer perceives is
    /// angular: a meter of error five meters away is not a meter of error two
    /// hundred meters away, and a mean over raw meters cannot tell them apart.
    ang_sum: [u128; 4],
    represented: [u64; 4],
    unrepresented: u64,
    /// Candidate observations and records, split by separation, so the update
    /// rate each band receives is visible rather than inferred.
    seen_by_band: [u64; 4],
    sent_by_band: [u64; 4],
    records: u64,
    arrivals: u64,
    bytes: u64,
    departures: u64,
    served: u64,
    never_told: u64,
    ever_seen: u64,
}

impl Metrics {
    fn observe(&mut self, err_raw: i64, sep_raw: i64) {
        let b = band_of(sep_raw);
        self.represented[b] += 1;
        self.err_sum[b] += err_raw as u128;
        // Floored at a meter: closer than that, angular error is dominated by
        // the divisor and says nothing about what a viewer perceives.
        self.ang_sum[b] += (err_raw as u128 * 1000) / (sep_raw.max(RAW_PER_M) as u128);
        self.hist[b][bucket_of(err_raw)] += 1;
    }

    fn mean_mrad(&self, band: Option<usize>) -> f64 {
        let (sum, n) = match band {
            Some(b) => (self.ang_sum[b], self.represented[b]),
            None => (self.ang_sum.iter().sum::<u128>(), self.total_represented()),
        };
        if n == 0 { 0.0 } else { sum as f64 / n as f64 }
    }

    fn total_represented(&self) -> u64 {
        self.represented.iter().sum()
    }

    fn mean_m(&self, band: Option<usize>) -> f64 {
        let (sum, n) = match band {
            Some(b) => (self.err_sum[b], self.represented[b]),
            None => (self.err_sum.iter().sum::<u128>(), self.total_represented()),
        };
        if n == 0 { 0.0 } else { sum as f64 / n as f64 / RAW_PER_M as f64 }
    }

    /// Upper edge of the bucket holding the `q` quantile, in meters. Log-spaced
    /// buckets, so this is a bound rather than an interpolation.
    fn quantile_m(&self, q: f64) -> f64 {
        let total = self.total_represented();
        if total == 0 {
            return 0.0;
        }
        let target = (total as f64 * q) as u64;
        let mut acc = 0u64;
        for bucket in 0..128 {
            acc += (0..4).map(|b| self.hist[b][bucket]).sum::<u64>();
            if acc >= target {
                return bucket_value(bucket) as f64 / RAW_PER_M as f64;
            }
        }
        f64::INFINITY
    }
}

impl Default for Metrics {
    fn default() -> Metrics {
        Metrics {
            hist: [[0; 128]; 4],
            err_sum: [0; 4],
            ang_sum: [0; 4],
            represented: [0; 4],
            unrepresented: 0,
            seen_by_band: [0; 4],
            sent_by_band: [0; 4],
            records: 0,
            arrivals: 0,
            bytes: 0,
            departures: 0,
            served: 0,
            never_told: 0,
            ever_seen: 0,
        }
    }
}

/// One configuration to measure. Built from [`Sweep::base`] and varied one
/// field at a time, so a sweep reads as the single thing it changes.
#[derive(Clone, Copy)]
struct Sweep {
    label: &'static str,
    weights: Weights,
    ghost_cap: usize,
    grace: u32,
    viewer_class: usize,
    send_period: u8,
    seed: u64,
}

impl Sweep {
    fn base() -> Sweep {
        Sweep {
            label: "1/d",
            weights: curve(2),
            ghost_cap: DEFAULT_GHOST_CAP,
            grace: DEFAULT_GRACE,
            viewer_class: WALKING_VIEWERS,
            send_period: 1,
            seed: 0x5EED,
        }
    }
}

struct Run {
    sweep: Sweep,
    /// Mean ghosts held per viewer at the end of the run. Not the cap: grace
    /// lets a table keep entries the ghost set no longer contains.
    ghosts: f64,
    metrics: Metrics,
}

/// A weight table, built by the library so a sweep measures the curve that
/// ships rather than one the harness invented. `k_halves` is the exponent in
/// halves: 2 is `1/d`.
fn curve(k_halves: u32) -> Weights {
    Weights::inverse_power(k_halves)
}

/// What a curve spans across the view radius, for the report.
fn span(w: Weights, cfg: &WorldConfig) -> (u32, u32) {
    let r = cfg.horizontal_view_radius().raw() as u64;
    (w.at(NEAR_BAND), w.at((r * r).ilog2() as usize))
}

fn run(s: Sweep) -> Run {
    let cfg = WorldConfig::default();
    let (game, by_class) = crowd(&cfg, s.seed);
    let policy = Policy {
        ghost_cap: s.ghost_cap,
        grace: s.grace,
        unseen_drift: cfg.horizontal_view_radius().raw() as u32,
        weights: s.weights,
    };
    let mut sim = WorldSimulation::with_replication(cfg, game, s.ghost_cap, policy);
    sim.tick();

    // A ViewerId is not an EntityId. Avatars are scattered ids drawn from one
    // motion class, so the two must be kept side by side.
    let mut viewers = Vec::with_capacity(VIEWERS);
    let limits = ClientLimits { send_period: s.send_period, ..ClientLimits::default() };
    for &e in by_class[s.viewer_class].iter().take(VIEWERS) {
        viewers.push(sim.register_viewer(EntityId::from_raw(e), limits));
    }

    let mut clients: Vec<Client> = (0..viewers.len()).map(|_| Client::new()).collect();
    let obs: Vec<Mutex<Observed>> =
        (0..viewers.len()).map(|_| Mutex::new(Observed::default())).collect();
    let mut m = Metrics::default();

    for tick in 1..=TICKS {
        for o in &obs {
            let mut o = o.lock().unwrap();
            o.candidates.clear();
            o.sent.clear();
            o.departed.clear();
            o.arrivals = 0;
            o.bytes = 0;
        }

        let capture = |out: Outbound<'_>| {
            let cands = out.candidates.as_slice();
            let mut o = obs[out.viewer.index()].lock().unwrap();
            o.candidates.extend(cands.iter().map(|e| e.id));
            o.sent.extend(out.selection.records().iter().map(|r| {
                let e = cands[r.index()];
                (e.id, band_of(isqrt(e.dist_sq.raw())))
            }));
            o.departed.extend(out.selection.departed().iter().copied());
            o.arrivals =
                out.selection.records().iter().filter(|r| r.is_new()).count() as u64;
            o.bytes = out.bytes.len() as u64;
        };
        sim.tick_with(&capture);

        let measuring = tick > WARMUP;
        for (vi, _) in viewers.iter().enumerate() {
            let o = obs[vi].lock().unwrap();
            if o.candidates.is_empty() && o.sent.is_empty() {
                continue;
            }
            let c = &mut clients[vi];

            // Measure the belief this tick's packet is about to correct. Doing
            // it after the sends would compare a position against itself and
            // report no error at all.
            if measuring {
                m.served += 1;
                m.records += o.sent.len() as u64;
                m.arrivals += o.arrivals;
                m.bytes += o.bytes;
                m.departures += o.departed.len() as u64;

                let avatar = sim.avatar_of(viewers[vi]).expect("registered");
                let at = sim.position(avatar).unwrap_or(Pos3::ZERO);
                for (_, b) in &o.sent {
                    m.sent_by_band[*b] += 1;
                }
                for (k, &e) in o.candidates.iter().enumerate() {
                    // The gather does not exclude a viewer's own entity, and a
                    // client does not need a ghost of itself.
                    if e == avatar {
                        continue;
                    }
                    let Some(truth) = sim.position(e) else { continue };
                    let sep = isqrt(truth.dist_sq(at).raw());
                    // Only the ghost set is owed a record: the gather overshoots
                    // it, and a candidate past it is one selection declines to
                    // score by design rather than one being starved. Counting
                    // those made a cap look worse the more its walk overshot.
                    let owed = k < s.ghost_cap;
                    if owed {
                        m.seen_by_band[band_of(sep)] += 1;
                    }
                    match c.told.get(&e) {
                        // A ghost is a belief the client renders whether or not
                        // the entity is still in the set being refreshed, so
                        // what `grace` holds past the set is counted here.
                        Some(believed) => {
                            m.observe(isqrt(truth.dist_sq(*believed).raw()), sep)
                        }
                        None if owed => m.unrepresented += 1,
                        None => {}
                    }
                }
            }

            for &e in o.candidates.iter().take(s.ghost_cap) {
                c.ever_seen[e.index()] = true;
            }
            for &e in &o.departed {
                c.told.remove(&e);
            }
            for &(e, _) in &o.sent {
                if let Some(p) = sim.position(e) {
                    c.told.insert(e, p);
                    c.ever_told[e.index()] = true;
                }
            }
        }
    }

    for c in &clients {
        for e in 0..ENTITIES {
            if c.ever_seen[e] {
                m.ever_seen += 1;
                if !c.ever_told[e] {
                    m.never_told += 1;
                }
            }
        }
    }
    let ghosts = viewers.iter().map(|&v| sim.ghost_count(v)).sum::<usize>() as f64
        / viewers.len() as f64;
    Run { sweep: s, ghosts, metrics: m }
}

/// Integer square root, so the harness reports separations without floats
/// deciding anything.
fn isqrt(v: u64) -> i64 {
    v.isqrt() as i64
}

/// The four tables reported for one sweep.
fn report(title: &str, runs: &[Run]) {
    println!("\n=== {title} ===\n");
    println!(
        "{:<11} {:>5} {:>5} {:>9} {:>7} {:>7} {:>7} {:>7} {:>8} {:>6} {:>6} {:>7} {:>7}",
        "curve",
        "cap",
        "grace",
        "ang mrad",
        "p99 m",
        "never",
        "unrep",
        "cand",
        "rec/pkt",
        "arr",
        "dep",
        "B/pkt",
        "ghosts",
    );
    for r in runs {
        let m = &r.metrics;
        let served = m.served.max(1) as f64;
        println!(
            "{:<11} {:>5} {:>5} {:>9.2} {:>7.3} {:>6.1}% {:>6.1}% {:>7.1} {:>8.1} {:>6.2} {:>6.2} {:>7.0} {:>7.1}",
            r.sweep.label,
            r.sweep.ghost_cap,
            r.sweep.grace,
            m.mean_mrad(None),
            m.quantile_m(0.99),
            100.0 * m.never_told as f64 / m.ever_seen.max(1) as f64,
            100.0 * m.unrepresented as f64
                / (m.unrepresented + m.total_represented()).max(1) as f64,
            m.seen_by_band.iter().sum::<u64>() as f64 / served,
            m.records as f64 / served,
            m.arrivals as f64 / served,
            m.departures as f64 / served,
            m.bytes as f64 / served,
            r.ghosts,
        );
    }

    for (what, value) in [
        ("updates per candidate-tick, by separation", 0u8),
        ("mean angular error by separation, milliradians", 1),
        ("mean error by separation, meters", 2),
    ] {
        println!("\n{what}");
        print!("{:<11} {:>5} {:>5}", "curve", "cap", "grace");
        for n in BAND_NAMES {
            print!(" {n:>10}");
        }
        println!();
        for r in runs {
            print!("{:<11} {:>5} {:>5}", r.sweep.label, r.sweep.ghost_cap, r.sweep.grace);
            for b in 0..4 {
                match value {
                    0 => {
                        let seen = r.metrics.seen_by_band[b].max(1);
                        print!(
                            " {:>10.3}",
                            r.metrics.sent_by_band[b] as f64 / seen as f64
                        )
                    }
                    1 => print!(" {:>10.2}", r.metrics.mean_mrad(Some(b))),
                    _ => print!(" {:>10.3}", r.metrics.mean_m(Some(b))),
                }
            }
            println!();
        }
    }
}

fn main() {
    println!(
        "{ENTITIES} entities, {VIEWERS} viewers drawn from the walkers, {TICKS} ticks at 20 Hz, \
         first {WARMUP} discarded."
    );
    print!("mix (chosen, not measured):");
    for (share, speed, name) in MIX {
        print!(" {:.0}% {name} at {speed} m/s,", share * 100.0);
    }
    println!("\n");

    let cfg = WorldConfig::default();
    let curves: [(&'static str, u32); 6] = [
        ("flat", 0),
        ("1/sqrt(d)", 1),
        ("1/d", 2),
        ("d^-1.5", 3),
        ("1/d^2", 4),
        ("1/d^4", 8),
    ];
    println!("weight across the view radius, near to far:");
    for (label, k) in curves {
        let (n, f) = span(curve(k), &cfg);
        println!("  {label:<10} {n:>6} -> {f:<6} ({}x)", n / f.max(1));
    }

    let mut runs = Vec::new();
    for cap in [256usize, 512] {
        for (label, k) in curves {
            runs.push(run(Sweep {
                label,
                weights: curve(k),
                ghost_cap: cap,
                ..Sweep::base()
            }));
        }
    }
    report("curve, at two caps", &runs);

    // A packet holds the same 98 records whatever the cap is, so the cap trades
    // how many entities a client can know about against how often each is
    // refreshed.
    let caps: Vec<Run> = [64usize, 128, 160, 192, 224, 256, 384, 512, 1024]
        .into_iter()
        .map(|cap| run(Sweep { ghost_cap: cap, ..Sweep::base() }))
        .collect();
    report("ghost cap", &caps);

    // Grace holds a ghost after it leaves the ghost set. Too little and a
    // boundary crossing costs a despawn and a re-send at unseen priority; too
    // much and a client keeps ghosts nothing is refreshing. Swept against a
    // walking viewer and a driving one, since a viewer that crosses a crowd
    // churns its ghost set and one standing in a crowd does not.
    for (class, speed) in [
        (WALKING_VIEWERS, MIX[WALKING_VIEWERS].1),
        (DRIVING_VIEWERS, MIX[DRIVING_VIEWERS].1),
    ] {
        let graces: Vec<Run> = [0u32, 1, 2, 3, 5, 10, 20]
            .into_iter()
            .map(|g| run(Sweep { grace: g, viewer_class: class, ..Sweep::base() }))
            .collect();
        report(&format!("grace, viewers at {speed} m/s"), &graces);
    }

    // What a moving viewer costs, with grace held at the default: the same
    // sweep the pipeline benchmark cannot run, since its viewers stand still.
    let movers: Vec<Run> = [WALKING_VIEWERS, DRIVING_VIEWERS]
        .into_iter()
        .map(|class| run(Sweep { viewer_class: class, ..Sweep::base() }))
        .collect();
    report("viewer speed, at the default grace", &movers);

    // A viewer's ghosts are stamped only on the ticks it is served, so a send
    // period above the grace makes every ghost depart and arrive again on every
    // turn. This is where that shows up rather than in a note.
    let periods: Vec<Run> =
        [("period 1", 1u8), ("period 2", 2), ("period 4", 4), ("period 8", 8)]
            .into_iter()
            .map(|(label, send_period)| {
                run(Sweep { label, send_period, ..Sweep::base() })
            })
            .collect();
    report("send period, at the default grace", &periods);

    // Coverage moves around between populations in a way the error columns do
    // not, so the same caps are run against three crowds before anything is
    // concluded from a `never` figure.
    let seeds: Vec<Run> =
        [("seed 5EED", 0x5EEDu64), ("seed C0FFEE", 0xC0FFEE), ("seed BEEF", 0xBEEF)]
            .into_iter()
            .flat_map(|(label, seed)| {
                [160usize, 192, 224, 256].into_iter().map(move |ghost_cap| {
                    run(Sweep { label, ghost_cap, seed, ..Sweep::base() })
                })
            })
            .collect();
    report("population, at four caps", &seeds);
}
