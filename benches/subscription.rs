//! benches/subscription.rs
//!
//! Measures one tick of subscription maintenance: for each viewer, derive the
//! cell from a position and build the subscription.
//!
//! The figure to watch is nanoseconds per viewer as the count rises. Each
//! viewer's work is bounded and independent of the others, so it should stay
//! flat. A rise indicates a memory access problem rather than an algorithmic
//! one.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::internals::Subscription;
use umwelt::{CellCoord, Fixed, Pos2, WorldConfig};

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

/// Viewer positions as struct-of-arrays, matching the intended storage layout.
/// Uniformly distributed: this measures access pattern, not density.
fn viewer_positions(cfg: &WorldConfig, n: usize, seed: u64) -> (Vec<Fixed>, Vec<Fixed>) {
    let mut rng = Rng::new(seed);
    let extent = cfg.region_size().raw() as u32;
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    for _ in 0..n {
        xs.push(Fixed::from_raw(rng.below(extent) as i32));
        ys.push(Fixed::from_raw(rng.below(extent) as i32));
    }
    (xs, ys)
}

/// Cells for viewers already known to have moved, for the delta benchmark
/// once it exists.
fn viewer_cells(cfg: &WorldConfig, xs: &[Fixed], ys: &[Fixed]) -> Vec<CellCoord> {
    xs.iter().zip(ys).map(|(&x, &y)| cfg.cell_of(Pos2::new(x, y))).collect()
}

/// Position to cell to subscription, once per viewer.
fn full_tick(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("subscription/full_tick");

    for n in [100usize, 1_000, 10_000, 100_000] {
        let (xs, ys) = viewer_positions(&cfg, n, 0x5eed);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut acc = 0usize;
                for i in 0..xs.len() {
                    let cell = cfg.cell_of(Pos2::new(xs[i], ys[i]));
                    let sub = Subscription::at_center(&cfg, cell);
                    acc += sub.len();
                }
                black_box(acc)
            })
        });
    }
    group.finish();
}

/// Subscription construction alone, with cells precomputed. Isolates the
/// bounds arithmetic from the position load and cell derivation.
fn from_known_cells(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("subscription/from_known_cells");

    for n in [100usize, 1_000, 10_000, 100_000] {
        let (xs, ys) = viewer_positions(&cfg, n, 0x5eed);
        let cells = viewer_cells(&cfg, &xs, &ys);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut acc = 0usize;
                for &cell in &cells {
                    acc += Subscription::at_center(&cfg, cell).len();
                }
                black_box(acc)
            })
        });
    }
    group.finish();
}

/// Walking the cells of each subscription rather than only constructing it.
/// This is the cost the entity-gathering pass will pay.
fn iterate_cells(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("subscription/iterate_cells");

    for n in [100usize, 1_000, 10_000] {
        let (xs, ys) = viewer_positions(&cfg, n, 0x5eed);
        let cells = viewer_cells(&cfg, &xs, &ys);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut acc = 0u32;
                for &cell in &cells {
                    for c in Subscription::at_center(&cfg, cell).cells() {
                        acc = acc.wrapping_add(cfg.cell_id(c).raw());
                    }
                }
                black_box(acc)
            })
        });
    }
    group.finish();
}

/// Membership tests against a fixed subscription, for comparison against a
/// stored-list implementation later.
fn membership(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let sub = Subscription::at_center(&cfg, CellCoord::new(16, 16));
    let (xs, ys) = viewer_positions(&cfg, 10_000, 0xc0ffee);
    let cells = viewer_cells(&cfg, &xs, &ys);

    let mut group = c.benchmark_group("subscription/membership");
    group.throughput(Throughput::Elements(cells.len() as u64));
    group.bench_function("contains", |b| {
        b.iter(|| {
            let mut hits = 0usize;
            for &c in &cells {
                if sub.contains(c) {
                    hits += 1;
                }
            }
            black_box(hits)
        })
    });
    group.finish();
}

criterion_group!(benches, full_tick, from_known_cells, iterate_cells, membership);
criterion_main!(benches);
