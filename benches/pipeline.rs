//! benches/pipeline.rs
//!
//! The whole tick, through `WorldSimulation::tick`: the game step, the
//! odometer, the cell sort, and then for every due viewer a subscription, a
//! capped gather, scoring, selection and commit.
//!
//! Every earlier figure for this pipeline was assembled by adding separately
//! measured stages together, two of them by subtraction. This measures the
//! thing itself.
//!
//! A baseline registers no viewers, so subtracting it leaves the per-viewer
//! replication cost with the per-tick work removed.
//!
//! Entities oscillate by a meter rather than traveling, so a population shape
//! holds for arbitrarily many iterations and a crowd stays a crowd. Neighbors
//! move in opposite directions, so candidate sets churn rather than drifting in
//! lockstep.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::select::{Policy, Weights};
use umwelt::sim::{DEFAULT_GHOST_CAP, DEFAULT_GRACE, DEFAULT_WALK_CAP};
use umwelt::{
    ClientLimits, EntityId, Fixed, Game, Handoff, NullSink, Pos3, RecordingSink, Step,
    WorldConfig, WorldSimulation,
};

/// Ticks run before timing, so ghost tables reach a steady size and no timed
/// tick allocates.
const WARMUP_TICKS: usize = 60;

/// xorshift64. Deterministic across runs so successive benchmarks compare.
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
        (self.next_u64() % bound as u64) as u32
    }
}

/// Spawns a fixed population on the first tick, then oscillates it.
struct Scenario {
    pending: Vec<Pos3>,
    step_m: i32,
    phase: bool,
}

impl Scenario {
    fn new(pending: Vec<Pos3>, step_m: i32) -> Scenario {
        Scenario { pending, step_m, phase: false }
    }
}

impl Game for Scenario {
    fn step(&mut self, w: &mut Step<'_>) {
        if !self.pending.is_empty() {
            for p in core::mem::take(&mut self.pending) {
                w.spawn(p);
            }
            return;
        }
        if self.step_m == 0 {
            return;
        }
        self.phase = !self.phase;
        let d = Fixed::from_meters(self.step_m).raw();
        let up = self.phase;
        let (xs, _, _) = w.positions_mut();
        for (i, x) in xs.iter_mut().enumerate() {
            let forward = (i % 2 == 0) == up;
            *x = Fixed::from_raw(if forward { x.raw() + d } else { x.raw() - d });
        }
    }
}

/// Positions at least `margin` meters inside the region, so a meter of
/// oscillation never leaves it.
fn inside(cfg: &WorldConfig, rng: &mut Rng, margin: i32) -> Pos3 {
    let m = Fixed::from_meters(margin).raw() as u32;
    let extent = cfg.region_size().raw() as u32 - 2 * m;
    let vertical = cfg.vertical_extent().raw() as u32;
    Pos3::new(
        Fixed::from_raw((m + rng.below(extent)) as i32),
        Fixed::from_raw((m + rng.below(extent)) as i32),
        Fixed::from_raw(rng.below(vertical) as i32),
    )
}

fn uniform(cfg: &WorldConfig, n: usize, seed: u64) -> Vec<Pos3> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| inside(cfg, &mut rng, 4)).collect()
}

/// `crowd` entities inside one cell near the middle, the rest spread evenly.
fn hot_cell(cfg: &WorldConfig, total: usize, crowd: usize, seed: u64) -> Vec<Pos3> {
    let mut rng = Rng::new(seed);
    let cell = cfg.cell_size().raw() as u32;
    let origin = (cfg.region_size().raw() as u32 / 2) & !(cell - 1);
    let vertical = cfg.vertical_extent().raw() as u32;
    let mut v: Vec<Pos3> = (0..crowd)
        .map(|_| {
            Pos3::new(
                Fixed::from_raw((origin + rng.below(cell)) as i32),
                Fixed::from_raw((origin + rng.below(cell)) as i32),
                Fixed::from_raw(rng.below(vertical) as i32),
            )
        })
        .collect();
    v.extend((crowd..total).map(|_| inside(cfg, &mut rng, 4)));
    v
}

/// A region in use: most cells sparse, a few dense. `dense_share` of the
/// population lands in `clusters` cells and the rest spreads evenly.
fn clustered(
    cfg: &WorldConfig,
    n: usize,
    clusters: usize,
    dense_share: f64,
    seed: u64,
) -> Vec<Pos3> {
    let mut rng = Rng::new(seed);
    let cell = cfg.cell_size().raw() as u32;
    let per_axis = cfg.cells_per_axis();
    let vertical = cfg.vertical_extent().raw() as u32;

    // Cluster cells kept off the region edge so oscillation stays inside.
    let centers: Vec<(u32, u32)> = (0..clusters)
        .map(|_| {
            (
                (1 + rng.below(per_axis - 2)) * cell,
                (1 + rng.below(per_axis - 2)) * cell,
            )
        })
        .collect();

    let dense = (n as f64 * dense_share) as usize;
    let mut v: Vec<Pos3> = (0..dense)
        .map(|k| {
            let (cx, cy) = centers[k % centers.len()];
            Pos3::new(
                Fixed::from_raw((cx + rng.below(cell)) as i32),
                Fixed::from_raw((cy + rng.below(cell)) as i32),
                Fixed::from_raw(rng.below(vertical) as i32),
            )
        })
        .collect();
    v.extend((dense..n).map(|_| inside(cfg, &mut rng, 4)));
    v
}

fn policy(ghost_cap: usize) -> Policy {
    Policy { ghost_cap, grace: DEFAULT_GRACE, weights: Weights::inverse_distance(), ..Policy::default() }
}

/// Builds a simulation, spawns `positions`, registers `viewers` of them as
/// clients, and warms it up. Viewers are drawn uniformly from the entity list,
/// so in a clustered world they land where the density is.
fn build(
    positions: Vec<Pos3>,
    viewers: usize,
    step_m: i32,
    walk_cap: usize,
    ghost_cap: usize,
    seed: u64,
) -> WorldSimulation<Scenario> {
    let cfg = WorldConfig::default();
    let n = positions.len();
    let mut sim =
        WorldSimulation::with_replication(cfg, Scenario::new(positions, step_m), walk_cap, policy(ghost_cap));

    // First tick spawns. Ids are assigned in order, so they are 0..n.
    sim.tick();
    assert_eq!(sim.entity_count(), n);

    let mut rng = Rng::new(seed);
    let mut chosen: Vec<u32> = (0..n as u32).collect();
    for i in (1..chosen.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        chosen.swap(i, j);
    }
    for &e in chosen.iter().take(viewers) {
        sim.register_viewer(EntityId::from_raw(e), ClientLimits::default());
    }

    for _ in 0..WARMUP_TICKS {
        sim.tick();
    }
    sim
}

/// Prints what a scenario actually produces, so the timings can be read.
fn describe(label: &str, sim: &mut WorldSimulation<Scenario>) {
    let s = sim.tick();
    let per = |x: u64| if s.viewers == 0 { 0.0 } else { x as f64 / s.viewers as f64 };
    println!(
        "{label}: {} viewers, {:.1} candidates, {:.1} records, {:.2} new, {:.2} departed per viewer",
        s.viewers,
        per(s.candidates),
        per(s.records),
        per(s.new_ghosts),
        per(s.departed)
    );
}

/// Cost against viewer count, on an evenly spread region. The zero-viewer row
/// is the per-tick work every other row also pays.
fn bench_uniform(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/uniform");
    group.sample_size(30);

    for &viewers in &[0usize, 1_000, 10_000] {
        let mut sim = build(
            uniform(&cfg, 8_192, 0xA11CE),
            viewers,
            1,
            DEFAULT_WALK_CAP,
            DEFAULT_GHOST_CAP,
            0xBEEF,
        );
        describe(&format!("uniform/{viewers}"), &mut sim);
        group.throughput(Throughput::Elements(viewers.max(1) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(viewers), &viewers, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// What the accumulator saves. Same world, same viewers; in one nothing moves,
/// so no client copy is ever wrong and no record should be sent.
fn bench_still_versus_moving(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/motion");
    group.sample_size(30);

    for &(label, step_m) in &[("still", 0i32), ("moving", 1)] {
        let mut sim = build(
            uniform(&cfg, 8_192, 0xA11CE),
            10_000,
            step_m,
            DEFAULT_WALK_CAP,
            DEFAULT_GHOST_CAP,
            0xBEEF,
        );
        describe(&format!("motion/{label}"), &mut sim);
        group.throughput(Throughput::Elements(10_000));
        group.bench_with_input(BenchmarkId::from_parameter(label), &label, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// The town square: a crowd in one cell where every entity is also a viewer.
fn bench_town_square(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/town_square");
    group.sample_size(20);

    for &crowd in &[2_048usize, 8_192] {
        let mut sim = build(
            hot_cell(&cfg, crowd, crowd, 0xC0FFEE),
            crowd,
            1,
            DEFAULT_WALK_CAP,
            DEFAULT_GHOST_CAP,
            0xD00D,
        );
        describe(&format!("town_square/{crowd}"), &mut sim);
        group.throughput(Throughput::Elements(crowd as u64));
        group.bench_with_input(BenchmarkId::from_parameter(crowd), &crowd, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// A region in use rather than a single population shape: 50,000 entities with
/// most cells sparse and eight dense ones holding sixty percent, and viewers
/// drawn from the population so they land where the density is.
fn bench_clustered(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/clustered");
    group.sample_size(20);

    for &viewers in &[1_000usize, 10_000] {
        let mut sim = build(
            clustered(&cfg, 50_000, 8, 0.6, 0x5EED),
            viewers,
            1,
            DEFAULT_WALK_CAP,
            DEFAULT_GHOST_CAP,
            0xF00D,
        );
        describe(&format!("clustered/{viewers}"), &mut sim);
        group.throughput(Throughput::Elements(viewers as u64));
        group.bench_with_input(BenchmarkId::from_parameter(viewers), &viewers, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// The ghost cap against the real pipeline, to check the isolated sweep.
fn bench_ghost_cap(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/ghost_cap");
    group.sample_size(20);

    for &cap in &[64usize, 128, 256, 512] {
        let mut sim = build(
            hot_cell(&cfg, 8_192, 8_192, 0xC0FFEE),
            8_192,
            1,
            cap,
            cap,
            0xD00D,
        );
        describe(&format!("ghost_cap/{cap}"), &mut sim);
        group.throughput(Throughput::Elements(8_192));
        group.bench_with_input(BenchmarkId::from_parameter(cap), &cap, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// The walk cap only matters above the ghost cap. Anything gathered past the
/// ghost set is scored by nothing and discarded, so this is the cost of that
/// waste.
fn bench_walk_cap(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/walk_cap");
    group.sample_size(20);

    for &walk in &[256usize, 512, 1024] {
        let mut sim = build(
            hot_cell(&cfg, 8_192, 8_192, 0xC0FFEE),
            8_192,
            1,
            walk,
            DEFAULT_GHOST_CAP,
            0xD00D,
        );
        describe(&format!("walk_cap/{walk}"), &mut sim);
        group.throughput(Throughput::Elements(8_192));
        group.bench_with_input(BenchmarkId::from_parameter(walk), &walk, |b, _| {
            b.iter(|| black_box(sim.tick()))
        });
    }
    group.finish();
}

/// Thread scaling. Viewers partition across workers by contiguous range; the
/// snapshot and odometer are read-only and each viewer owns its own ghosts, so
/// nothing is shared for writing except the chunk-boundary cache lines.
///
/// Scoped threads are spawned per tick, so a low viewer count pays that cost
/// against less work. The one-thread row takes a serial path with no spawn at
/// all, which is what makes the spawn overhead visible.
fn bench_threads(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut counts: Vec<usize> = Vec::new();
    let mut t = 1;
    while t < cores {
        counts.push(t);
        t *= 2;
    }
    counts.push(cores);
    println!("threads: available_parallelism reports {cores}, sweeping {counts:?}");

    for (label, positions, viewers) in [
        ("uniform", uniform(&cfg, 8_192, 0xA11CE), 8_192usize),
        ("town_square", hot_cell(&cfg, 8_192, 8_192, 0xC0FFEE), 8_192),
    ] {
        let mut group = c.benchmark_group(format!("pipeline/threads/{label}"));
        group.sample_size(20);
        for &n in &counts {
            let mut sim = build(
                positions.clone(),
                viewers,
                1,
                DEFAULT_WALK_CAP,
                DEFAULT_GHOST_CAP,
                0xBEEF,
            );
            sim.set_thread_count(n);
            for _ in 0..10 {
                sim.tick();
            }
            group.throughput(Throughput::Elements(viewers as u64));
            group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
                b.iter(|| black_box(sim.tick()))
            });
        }
        group.finish();
    }
}

/// What a sink costs the tick. NullSink discards; RecordingSink allocates and
/// takes a lock per send, which is roughly the floor for a sink that does
/// anything at all. The difference is what a badly behaved one buys you.
fn bench_sink(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("pipeline/sink");
    group.sample_size(30);

    let positions = uniform(&cfg, 8_192, 0xA11CE);

    let mut null = build(positions.clone(), 8_192, 1, DEFAULT_WALK_CAP, DEFAULT_GHOST_CAP, 0xBEEF);
    group.throughput(Throughput::Elements(8_192));
    group.bench_function("null", |b| b.iter(|| black_box(null.tick())));
    drop(null);

    let mut recording = build(positions, 8_192, 1, DEFAULT_WALK_CAP, DEFAULT_GHOST_CAP, 0xBEEF)
        .with_sink(RecordingSink::new());
    for _ in 0..10 {
        recording.tick();
    }
    group.bench_function("recording", |b| b.iter(|| black_box(recording.tick())));
    drop(recording);

    // The same recording sink, reached through the handoff. If the locks cost
    // anything, this is where it shows.
    let mut handed = build(uniform(&cfg, 8_192, 0xA11CE), 8_192, 1, DEFAULT_WALK_CAP, DEFAULT_GHOST_CAP, 0xBEEF)
        .with_sink(Handoff::new(RecordingSink::new()));
    for _ in 0..10 {
        handed.tick();
    }
    group.bench_function("handoff_recording", |b| b.iter(|| black_box(handed.tick())));
    drop(handed);

    let mut null_handed = build(uniform(&cfg, 8_192, 0xA11CE), 8_192, 1, DEFAULT_WALK_CAP, DEFAULT_GHOST_CAP, 0xBEEF)
        .with_sink(Handoff::new(NullSink));
    for _ in 0..10 {
        null_handed.tick();
    }
    group.bench_function("handoff_null", |b| b.iter(|| black_box(null_handed.tick())));
    group.finish();
}

criterion_group!(
    benches,
    bench_uniform,
    bench_still_versus_moving,
    bench_town_square,
    bench_clustered,
    bench_ghost_cap,
    bench_walk_cap,
    bench_threads,
    bench_sink
);
criterion_main!(benches);
