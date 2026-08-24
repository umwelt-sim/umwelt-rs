//! benches/odometer.rs
//!
//! Measures `Odometer::accumulate`: one sequential pass over the position
//! arrays adding each live entity's displacement since the previous call.
//!
//! The figure to watch is microseconds per call against a 50 ms tick. This is
//! the entire cost of the staleness signal priority scoring reads, and it is
//! paid once per tick rather than once per viewer, which is the reason the
//! signal is per-entity rather than per-(viewer, entity).
//!
//! Displacement values do not affect the instruction path — every live slot
//! does the same three `abs_diff`, two saturating adds, one wrapping add and
//! three stores regardless of how far anything moved — so positions are held
//! fixed and the measurement is of the pass alone.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::{EntityId, Fixed, LiveSet, Odometer, WorldConfig};

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

struct Population {
    xs: Vec<Fixed>,
    ys: Vec<Fixed>,
    zs: Vec<Fixed>,
    live: LiveSet,
}

/// `n` slots spread across the region, every one of them live.
fn population(cfg: &WorldConfig, n: usize, seed: u64) -> Population {
    let mut rng = Rng::new(seed);
    let extent = cfg.region_size().raw() as u32;
    let vertical = cfg.vertical_extent().raw() as u32;
    let mut p = Population {
        xs: Vec::with_capacity(n),
        ys: Vec::with_capacity(n),
        zs: Vec::with_capacity(n),
        live: LiveSet::with_capacity(n),
    };
    for i in 0..n {
        p.xs.push(Fixed::from_raw(rng.below(extent) as i32));
        p.ys.push(Fixed::from_raw(rng.below(extent) as i32));
        p.zs.push(Fixed::from_raw(rng.below(vertical) as i32));
        p.live.insert(EntityId::from_raw(i as u32));
    }
    p
}

/// Grown to `n` slots before timing, so no timed call reallocates.
fn warmed(p: &Population, n: usize) -> Odometer {
    let mut odo = Odometer::with_capacity(n);
    odo.accumulate(&p.xs, &p.ys, &p.zs, &p.live);
    odo
}

/// Cost against slot count, with every slot live.
fn bench_slot_count(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("odometer/all_live");

    for &n in &[10_000usize, 50_000, 100_000] {
        let p = population(&cfg, n, 0xB0DE);
        let mut odo = warmed(&p, n);

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                odo.accumulate(
                    black_box(&p.xs),
                    black_box(&p.ys),
                    black_box(&p.zs),
                    black_box(&p.live),
                )
            })
        });
    }
    group.finish();
}

/// What dead slots cost. `accumulate` walks every slot and tests liveness, so a
/// long-running region with heavy churn pays for slots ever allocated rather
/// than entities currently alive — the same property already recorded for
/// `CellSnapshot::update`. Slot count is held at 50,000 and only the live
/// fraction moves.
fn bench_live_fraction(c: &mut Criterion) {
    let cfg = WorldConfig::default();
    let mut group = c.benchmark_group("odometer/live_fraction");
    let n = 50_000usize;

    for &(label, keep) in &[("all", 1usize), ("half", 2), ("quarter", 4)] {
        let mut p = population(&cfg, n, 0xDEAD);
        let mut odo = warmed(&p, n);
        for i in 0..n {
            if i % keep != 0 {
                p.live.remove(EntityId::from_raw(i as u32));
            }
        }

        group.throughput(Throughput::Elements((n / keep) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(label), &keep, |b, _| {
            b.iter(|| {
                odo.accumulate(
                    black_box(&p.xs),
                    black_box(&p.ys),
                    black_box(&p.zs),
                    black_box(&p.live),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_slot_count, bench_live_fraction);
criterion_main!(benches);
