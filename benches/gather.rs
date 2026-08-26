//! benches/gather.rs
//!
//! Measures the gather: for each viewer, walk the subscribed cells of a
//! cell-ordered snapshot and keep the entities within the view radius.
//!
//! Two scenarios. The ratio between them is what the benchmark reports.
//!
//! Uniform spreads entities evenly, which bounds per-viewer work by the
//! subscription size. It establishes the floor and will look acceptable
//! regardless of design quality.
//!
//! Hot cell puts a large fraction of the population in one cell. Per-viewer
//! work stops being bounded by geometry and becomes proportional to the crowd.
//!
//! The figure to watch in the uniform case is nanoseconds per entity examined.
//! Sequential access with the prefetcher working should land near 1 ns; random
//! access to main memory lands near 100 ns. Which one applies decides whether
//! the layout was worth building.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::{
    CellSnapshot, DiscoveredEntities, EntityId, Fixed, LiveSet, Pos3, Subscription,
    WorldConfig,
};

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

/// Entity positions as struct-of-arrays, matching the intended storage layout.
struct Entities {
    xs: Vec<Fixed>,
    ys: Vec<Fixed>,
    zs: Vec<Fixed>,
    live: LiveSet,
}

impl Entities {
    fn with_capacity(n: usize) -> Self {
        Entities {
            xs: Vec::with_capacity(n),
            ys: Vec::with_capacity(n),
            zs: Vec::with_capacity(n),
            live: LiveSet::with_capacity(n),
        }
    }

    fn push(&mut self, p: Pos3) {
        let id = EntityId::from_raw(self.xs.len() as u32);
        self.xs.push(p.x);
        self.ys.push(p.y);
        self.zs.push(p.z);
        self.live.insert(id);
    }
}

/// Entities spread evenly across the region.
fn uniform(cfg: &WorldConfig, n: usize, seed: u64) -> Entities {
    let mut rng = Rng::new(seed);
    let extent = cfg.region_size().raw() as u32;
    let vertical = cfg.vertical_extent().raw() as u32;
    let mut e = Entities::with_capacity(n);
    for _ in 0..n {
        e.push(Pos3::new(
            Fixed::from_raw(rng.below(extent) as i32),
            Fixed::from_raw(rng.below(extent) as i32),
            Fixed::from_raw(rng.below(vertical) as i32),
        ));
    }
    e
}

/// `crowd` entities inside a single cell, the rest spread evenly.
fn hot_cell(cfg: &WorldConfig, total: usize, crowd: usize, seed: u64) -> (Entities, Pos3) {
    assert!(crowd <= total);
    let mut rng = Rng::new(seed);
    let extent = cfg.region_size().raw() as u32;
    let vertical = cfg.vertical_extent().raw() as u32;
    let cell = cfg.cell_size().raw() as u32;

    // Anchor the crowd on a cell boundary near the middle of the region so the
    // viewer's subscription is a full block rather than a clipped one.
    let origin = (extent / 2) & !(cell - 1);

    let mut e = Entities::with_capacity(total);
    for _ in 0..crowd {
        e.push(Pos3::new(
            Fixed::from_raw((origin + rng.below(cell)) as i32),
            Fixed::from_raw((origin + rng.below(cell)) as i32),
            Fixed::from_raw(rng.below(vertical) as i32),
        ));
    }
    for _ in crowd..total {
        e.push(Pos3::new(
            Fixed::from_raw(rng.below(extent) as i32),
            Fixed::from_raw(rng.below(extent) as i32),
            Fixed::from_raw(rng.below(vertical) as i32),
        ));
    }

    // A viewer standing in the middle of the crowd.
    let center = Fixed::from_raw((origin + cell / 2) as i32);
    (e, Pos3::new(center, center, Fixed::ZERO))
}

fn snapshot_of(cfg: &WorldConfig, e: &Entities) -> CellSnapshot {
    let mut s = CellSnapshot::new(cfg);
    s.update(&e.xs, &e.ys, &e.zs, &e.live);
    s
}

/// Viewers spread evenly, so the walk touches the whole snapshot rather than
/// one hot corner of it.
fn viewers(cfg: &WorldConfig, n: usize, seed: u64) -> Vec<Pos3> {
    let mut rng = Rng::new(seed);
    let extent = cfg.region_size().raw() as u32;
    (0..n)
        .map(|_| {
            Pos3::new(
                Fixed::from_raw(rng.below(extent) as i32),
                Fixed::from_raw(rng.below(extent) as i32),
                Fixed::ZERO,
            )
        })
        .collect()
}

fn subs_of(cfg: &WorldConfig, vs: &[Pos3]) -> Vec<Subscription> {
    vs.iter()
        .map(|v| Subscription::at_center(cfg, cfg.cell_of(v.horizontal())))
        .collect()
}

/// Mean candidates a viewer gathers. Reported so per-entity cost is derivable
/// from per-viewer time rather than assumed.
fn mean_candidates(snap: &CellSnapshot, vs: &[Pos3], subs: &[Subscription]) -> f64 {
    let mut out = DiscoveredEntities::with_capacity(4096);
    let mut total = 0usize;
    for (v, s) in vs.iter().zip(subs) {
        out.clear();
        snap.gather_into(*v, *s, &mut out);
        total += out.len();
    }
    total as f64 / vs.len() as f64
}

fn bench_uniform(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let entities = uniform(&cfg, 8_192, 0xA11CE);
    let snap = snapshot_of(&cfg, &entities);

    let mut group = c.benchmark_group("gather/uniform");
    for &n in &[1_000usize, 10_000] {
        let vs = viewers(&cfg, n, 0xBEEF);
        let subs = subs_of(&cfg, &vs);
        let mean = mean_candidates(&snap, &vs, &subs);
        println!("uniform, {n} viewers: {mean:.1} candidates per viewer");

        let mut out = DiscoveredEntities::with_capacity(4096);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                for (v, s) in vs.iter().zip(&subs) {
                    out.clear();
                    snap.gather_into(black_box(*v), black_box(*s), &mut out);
                    black_box(out.len());
                }
            })
        });
    }
    group.finish();
}

/// Total entities the walk touches, before the radius test. Exact, from the
/// snapshot's own per-cell counts.
fn examined(cfg: &WorldConfig, snap: &CellSnapshot, subs: &[Subscription]) -> f64 {
    let total: usize = subs
        .iter()
        .flat_map(|s| s.cells())
        .map(|c| snap.count(cfg.cell_id(c)))
        .sum();
    total as f64 / subs.len() as f64
}

fn mean_cells(subs: &[Subscription]) -> f64 {
    subs.iter().map(|s| s.len()).sum::<usize>() as f64 / subs.len() as f64
}

/// A walk over cells that hold nothing. Isolates the per-cell cost of the outer
/// loop — cell id, bounds check, four slice constructions — with no entity work
/// at all. Whatever remains in the populated benchmarks above this line is
/// per-entity.
fn bench_empty(c: &mut Criterion) {
    let cfg = WorldConfig::default();

    // Everything crammed into one corner, viewers far away in vacant space.
    let mut entities = Entities::with_capacity(8_192);
    for _ in 0..8_192 {
        entities.push(Pos3::from_meters(10, 10, 0));
    }
    let snap = snapshot_of(&cfg, &entities);

    let mut rng = Rng::new(0xEEEE);
    let vs: Vec<Pos3> = (0..1_000)
        .map(|_| {
            // The far half of the region, well clear of the corner.
            let lo = (cfg.region_size().raw() as u32) / 2;
            Pos3::new(
                Fixed::from_raw((lo + rng.below(lo - 1)) as i32),
                Fixed::from_raw((lo + rng.below(lo - 1)) as i32),
                Fixed::ZERO,
            )
        })
        .collect();
    let subs = subs_of(&cfg, &vs);
    println!(
        "empty: {:.2} cells walked, {:.1} entities examined",
        mean_cells(&subs),
        examined(&cfg, &snap, &subs)
    );

    let mut out = DiscoveredEntities::with_capacity(64);
    c.benchmark_group("gather/empty")
        .throughput(Throughput::Elements(vs.len() as u64))
        .bench_function("1000", |b| {
            b.iter(|| {
                for (v, sb) in vs.iter().zip(&subs) {
                    out.clear();
                    snap.gather_into(black_box(*v), black_box(*sb), &mut out);
                    black_box(out.len());
                }
            })
        });
}


fn bench_hot_cell(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("gather/hot_cell");

    // One viewer standing inside the crowd. Per-viewer cost is the figure;
    // multiply by the crowd size to get the tick cost, since in a town square
    // the entities are the viewers.
    for &crowd in &[512usize, 2_048, 8_192] {
        let (entities, viewer) = hot_cell(&cfg, 8_192.max(crowd), crowd, 0xC0FFEE);
        let snap = snapshot_of(&cfg, &entities);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut probe = DiscoveredEntities::with_capacity(16_384);
        probe.clear();
        snap.gather_into(viewer, sub, &mut probe);
        println!("hot cell, crowd {crowd}: {} candidates for one viewer", probe.len());

        let mut out = DiscoveredEntities::with_capacity(16_384);
        group.throughput(Throughput::Elements(probe.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(crowd), &crowd, |b, _| {
            b.iter(|| {
                out.clear();
                snap.gather_into(black_box(viewer), black_box(sub), &mut out);
                black_box(out.len());
            })
        });
    }
    group.finish();
}

/// Fans viewers across threads. One buffer per thread, reused across every
/// viewer that thread handles.
///
/// The thread count is `per_thread.len()`. Buffers are passed in rather than
/// allocated per call so the measurement excludes a malloc per thread per tick.
fn parallel_gather(
    snap: &CellSnapshot,
    vs: &[Pos3],
    subs: &[Subscription],
    per_thread: &mut [DiscoveredEntities],
) {
    let threads = per_thread.len();
    assert!(threads > 0, "parallel_gather needs at least one buffer");
    let chunk = vs.len().div_ceil(threads);
    std::thread::scope(|s| {
        for ((vc, sc), buf) in
            vs.chunks(chunk).zip(subs.chunks(chunk)).zip(per_thread.iter_mut())
        {
            s.spawn(move || {
                for (v, sb) in vc.iter().zip(sc) {
                    buf.clear();
                    snap.gather_into(black_box(*v), black_box(*sb), buf);
                    black_box(buf.len());
                }
            });
        }
    });
}

/// One buffer per thread, each sized to hold one viewer's candidates.
fn per_thread_buffers(threads: usize, capacity: usize) -> Vec<DiscoveredEntities> {
    (0..threads).map(|_| DiscoveredEntities::with_capacity(capacity)).collect()
}

/// Scoped-thread spawn and join with no work in between. Whatever this costs is
/// included in every parallel figure below and can be subtracted from it.
fn bench_spawn_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("gather/spawn_only");
    for &t in &[1usize, 2, 4, 8] {
        group.bench_with_input(BenchmarkId::from_parameter(t), &t, |b, &t| {
            b.iter(|| {
                std::thread::scope(|s| {
                    for _ in 0..t {
                        s.spawn(|| black_box(0u64));
                    }
                })
            })
        });
    }
    group.finish();
}

fn bench_parallel_uniform(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let entities = uniform(&cfg, 8_192, 0xA11CE);
    let snap = snapshot_of(&cfg, &entities);
    let vs = viewers(&cfg, 10_000, 0xBEEF);
    let subs = subs_of(&cfg, &vs);

    let mut group = c.benchmark_group("gather/parallel_uniform");
    for &t in &[1usize, 2, 4, 8] {
        let mut per_thread = per_thread_buffers(t, 1_024);
        group.throughput(Throughput::Elements(vs.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(t), &t, |b, _| {
            b.iter(|| parallel_gather(&snap, &vs, &subs, &mut per_thread))
        });
    }
    group.finish();
}

/// The town square. Every entity in one cell, and every entity is also a viewer,
/// so the tick cost is the full crowd walking the full crowd.
fn bench_parallel_town_square(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("gather/town_square");

    for &crowd in &[2_048usize, 4_096, 8_192] {
        let (entities, _) = hot_cell(&cfg, crowd, crowd, 0xC0FFEE);
        let snap = snapshot_of(&cfg, &entities);
        let vs: Vec<Pos3> = (0..entities.xs.len())
            .map(|i| Pos3::new(entities.xs[i], entities.ys[i], entities.zs[i]))
            .collect();
        let subs = subs_of(&cfg, &vs);
        println!("town square {crowd}: {:.0} examined per viewer", examined(&cfg, &snap, &subs));

        for &t in &[1usize, 2, 4, 8] {
            let mut per_thread = per_thread_buffers(t, crowd.next_power_of_two());
            group.throughput(Throughput::Elements(vs.len() as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("crowd{crowd}"), t),
                &t,
                |b, _| b.iter(|| parallel_gather(&snap, &vs, &subs, &mut per_thread)),
            );
        }
    }
    group.finish();
}

/// Cost of `update` with and without the sub-cell sort, on an 8,192 crowd in one
/// cell. The whole population is dense, so this is the worst case for the second
/// sort.
fn bench_subdivision(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let (entities, _) = hot_cell(&cfg, 8_192, 8_192, 0xC0FFEE);

    let mut group = c.benchmark_group("snapshot/update");
    for &(label, axis) in &[("subdivision_off", 1u32), ("axis_8", 8u32), ("axis_16", 16u32)] {
        let mut snap = CellSnapshot::with_subdivision(&cfg, axis, 512);
        group.bench_function(label, |b| {
            b.iter(|| {
                snap.update(
                    black_box(&entities.xs),
                    black_box(&entities.ys),
                    black_box(&entities.zs),
                    &entities.live,
                )
            })
        });
    }
    group.finish();
}

/// The town square with a walk cap. Every entity is a viewer, so the tick cost
/// is the crowd walking itself. `cap` of `usize::MAX` is the uncapped baseline.
fn bench_capped_town_square(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("gather/capped_town_square");

    // 10,000 is deliberately not a power of two. Cell size, sub-grid axis, and
    // cell shift all are, so a ragged population exercises partial sub-cells,
    // uneven thread chunking, and a cap that does not divide evenly.
    for &crowd in &[8_192usize, 10_000] {
    let (entities, _) = hot_cell(&cfg, crowd, crowd, 0xC0FFEE);
    let snap = snapshot_of(&cfg, &entities);
    let vs: Vec<Pos3> = (0..entities.xs.len())
        .map(|i| Pos3::new(entities.xs[i], entities.ys[i], entities.zs[i]))
        .collect();
    let subs = subs_of(&cfg, &vs);

    for &threads in &[1usize, 4] {
    for &(label, cap) in &[
        ("uncapped", usize::MAX),
        ("cap_2048", 2_048usize),
        ("cap_1024", 1_024),
        ("cap_512", 512),
        ("cap_256", 256),
    ] {
        let mut per_thread = per_thread_buffers(threads, 16_384);
        group.throughput(Throughput::Elements(vs.len() as u64));
        group.bench_function(format!("crowd{crowd}/t{threads}/{label}"), |b| {
            b.iter(|| {
                let threads = per_thread.len();
                let chunk = vs.len().div_ceil(threads);
                std::thread::scope(|s| {
                    for ((vc, sc), buf) in vs
                        .chunks(chunk)
                        .zip(subs.chunks(chunk))
                        .zip(per_thread.iter_mut())
                    {
                        let snap = &snap;
                        s.spawn(move || {
                            for (v, sb) in vc.iter().zip(sc) {
                                buf.clear();
                                snap.gather_into_capped(
                                    black_box(*v),
                                    black_box(*sb),
                                    black_box(cap),
                                    buf,
                                );
                                black_box(buf.len());
                            }
                        });
                    }
                });
            })
        });
    }
    }
    }
    group.finish();
}

/// Same population and same region, different cell size, using the
/// `with_cell_size_m` escape hatch. Holds the working set constant at 8,192
/// entities and varies how many sit in one contiguous run.
fn bench_cell_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("gather/cell_size");
    for &m in &[64i32, 128, 256, 512] {
        let cfg = WorldConfig::default().with_cell_size_m(m);
        let entities = uniform(&cfg, 8_192, 0xA11CE);
        let snap = snapshot_of(&cfg, &entities);
        let vs = viewers(&cfg, 1_000, 0xBEEF);
        let subs = subs_of(&cfg, &vs);

        let cells = mean_cells(&subs);
        let ex = examined(&cfg, &snap, &subs);
        println!(
            "cell {m} m: radius {}, {:.2} cells walked, {:.1} per cell, {:.1} examined, {:.1} kept",
            cfg.cell_radius(),
            cells,
            ex / cells,
            ex,
            mean_candidates(&snap, &vs, &subs)
        );

        let mut out = DiscoveredEntities::with_capacity(16_384);
        group.throughput(Throughput::Elements(vs.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(m), &m, |b, _| {
            b.iter(|| {
                for (v, sb) in vs.iter().zip(&subs) {
                    out.clear();
                    snap.gather_into(black_box(*v), black_box(*sb), &mut out);
                    black_box(out.len());
                }
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cell_size,
    bench_capped_town_square,
    bench_subdivision,
    bench_uniform,
    bench_empty,
    bench_hot_cell,
    bench_spawn_overhead,
    bench_parallel_uniform,
    bench_parallel_town_square
);
criterion_main!(benches);
