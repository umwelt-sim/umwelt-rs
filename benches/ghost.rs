//! benches/ghost.rs
//!
//! Measures `GhostTable`: the per-viewer probe that turns a gather's candidates
//! into drift, and the sweep that drops ghosts the client should no longer
//! hold.
//!
//! Two figures per operation, because they answer different questions.
//!
//! **hot** reuses one table across every iteration, so it stays in L1. That is
//! the instruction cost with no memory term — a floor, not a prediction.
//!
//! **scattered** cycles through enough tables to exceed any cache on the
//! machine, in a shuffled order, so each is touched cold. That is the real
//! situation: a worker walks thousands of viewers and returns to each once per
//! tick, by which time its table is long gone from cache. The gap between the
//! two figures is the memory term.
//!
//! The shuffle matters. Visiting tables in index order walks the whole set as
//! one stream, and `ghost/sent_shape` measures that hiding a 3 KB table's misses
//! almost entirely: 296 ns in order against 578 ns shuffled, for identical work.
//! A 12 KB table barely moves, because it is already demand-miss bound within
//! itself. Production sits between the two and depends on how fragmented the
//! per-viewer allocations have become, so the shuffled figure is the
//! pessimistic bound rather than the answer.
//!
//! The figure to compare against is the gather, which shares the same tick and
//! the same per-viewer budget.

use std::collections::HashSet;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::EntityId;
use umwelt::internals::GhostTable;

/// Candidate counts to measure: the uniform case's measured mean, and the
/// default walk cap.
const SIZES: [usize; 2] = [95, 512];

/// Records fitting an MTU-sized packet with no events pending.
const SLOTS_PER_PACKET: usize = 98;

/// Table set size for the cold case. Chosen to exceed any cache on the test
/// machine by a wide margin rather than to match one.
const COLD_BYTES: usize = 32 << 20;

const TICK: u32 = 1_000;
const GRACE: u32 = 5;

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
}

/// `n` distinct ids drawn from a 50,000-entity region, in no particular order.
/// A gather emits candidates in cell-walk order, which is not id order, so the
/// probe sequence must not be sorted.
fn candidates(n: usize, seed: u64) -> Vec<EntityId> {
    let mut rng = Rng::new(seed);
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let e = (rng.next_u64() % 50_000) as u32;
        if seen.insert(e) {
            out.push(EntityId::from_raw(e));
        }
    }
    out
}

/// A table already holding a ghost of every candidate, which is steady state.
fn populated(ids: &[EntityId]) -> GhostTable {
    let mut t = GhostTable::with_capacity(ids.len());
    for (k, &e) in ids.iter().enumerate() {
        t.sent(e, k as u32, TICK);
    }
    t
}

/// Enough copies to exceed cache, so each is cold when the cursor reaches it.
fn cold_set(ids: &[EntityId]) -> Vec<GhostTable> {
    let one = populated(ids);
    let bytes = one.slots() * 12;
    let n = (COLD_BYTES / bytes).max(2);
    (0..n).map(|_| populated(ids)).collect()
}

/// One `seen` per candidate. The reconcile probe, once per candidate per tick.
fn bench_seen(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/seen");

    for &n in &SIZES {
        let ids = candidates(n, 0x6057);
        group.throughput(Throughput::Elements(n as u64));

        let mut one = populated(&ids);
        group.bench_with_input(BenchmarkId::new("hot", n), &n, |b, _| {
            b.iter(|| {
                for &e in &ids {
                    black_box(one.seen(black_box(e), TICK));
                }
            })
        });

        let mut set = cold_set(&ids);
        let sets = set.len();
        let order = shuffled(sets, 0xA1);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("scattered", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.seen(black_box(e), TICK));
                }
            })
        });
    }
    group.finish();
}

/// One full per-viewer pass: probe every candidate, commit the ones that would
/// fit a packet, then sweep. The `no_evict` variant is the same without the
/// sweep, so the difference is what eviction costs in place, on a table the
/// probes have already pulled in.
fn bench_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/tick");

    for &n in &SIZES {
        let ids = candidates(n, 0x7104);
        let commit = n.min(SLOTS_PER_PACKET);
        group.throughput(Throughput::Elements(n as u64));

        let mut set = cold_set(&ids);
        let sets = set.len();
        let order = shuffled(sets, 0xA3);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("no_evict", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.seen(black_box(e), TICK));
                }
                for &e in &ids[..commit] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
            })
        });

        let mut set = cold_set(&ids);
        let sets = set.len();
        let order = shuffled(sets, 0xA4);
        let mut cursor = 0usize;
        let mut departed = Vec::with_capacity(n);
        group.bench_with_input(BenchmarkId::new("with_evict", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.seen(black_box(e), TICK));
                }
                for &e in &ids[..commit] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
                departed.clear();
                t.evict(TICK, GRACE, &mut departed);
            })
        });
    }
    group.finish();
}

/// Commits alone, on a cold table with no prior probes. Isolates `sent` from
/// the probe pass it follows in `ghost/tick`, since `sent` probes the key a
/// second time rather than reusing the slot `seen` already found.
fn bench_sent(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/sent");

    for &n in &SIZES {
        let ids = candidates(n, 0x5E27);
        let commit = n.min(SLOTS_PER_PACKET);
        group.throughput(Throughput::Elements(commit as u64));

        let mut set = cold_set(&ids);
        let sets = set.len();
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[cursor % sets];
                cursor = cursor.wrapping_add(1);
                for &e in &ids[..commit] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
            })
        });
    }
    group.finish();
}

/// Why `sent` costs 2.66 ns against a 256-slot table and 13.5 ns against a
/// 1024-slot one. Commits are held at 98 and two things vary separately: how
/// many bytes the table spans, and how full it is.
///
/// `98in256` and `98in1024` differ in footprint, and the larger one is the
/// *emptier* of the two, so a slowdown there cannot be probe length.
/// `98in1024` and `512in1024` differ only in load factor.
fn bench_sent_shape(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/sent_shape");
    const COMMITS: usize = 98;

    // label, ghosts held, pre-capacity that yields the wanted slot count
    let shapes: [(&str, usize, usize); 3] =
        [("98in256", 98, 128), ("98in1024", 98, 512), ("512in1024", 512, 512)];

    for (label, held, pre) in shapes {
        let ids = candidates(held, 0x4A11);
        let build = || {
            let mut t = GhostTable::with_capacity(pre);
            for (k, &e) in ids.iter().enumerate() {
                t.sent(e, k as u32, TICK);
            }
            t
        };
        let probe = build();
        assert!(probe.slots() == pre * 2, "{label}: got {} slots", probe.slots());
        let bytes = probe.slots() * 12;
        group.throughput(Throughput::Elements(COMMITS as u64));

        let mut one = build();
        group.bench_with_input(BenchmarkId::new("hot", label), &label, |b, _| {
            b.iter(|| {
                for &e in &ids[..COMMITS] {
                    one.sent(black_box(e), black_box(TICK), TICK);
                }
            })
        });

        let n = (COLD_BYTES / bytes).max(2);
        let mut set: Vec<GhostTable> = (0..n).map(|_| build()).collect();
        let sets = set.len();
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("cold", label), &label, |b, _| {
            b.iter(|| {
                let t = &mut set[cursor % sets];
                cursor = cursor.wrapping_add(1);
                for &e in &ids[..COMMITS] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
            })
        });

        // Visiting tables in index order walks the whole set as one stream,
        // which lets the prefetcher hide a small table's misses. Production
        // tables are independent allocations interleaved with everything else,
        // so a shuffled order is the pessimistic bound on the same work.
        let order = shuffled(sets, 0xD15C);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("scattered", label), &label, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids[..COMMITS] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
            })
        });
    }
    group.finish();
}

/// A permutation of `0..n`.
fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut rng = Rng::new(seed);
    let mut v: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

/// How per-viewer cost varies with the number of ghosts a viewer holds, at a
/// fixed 512 candidates from the walk cap.
///
/// Footprint is what the commit measurements say dominates, and the ghost count
/// is what sets footprint. Load factor is held at one half in every row, so
/// footprint is the only variable: 1.5, 3, 6 and 12 KB.
///
/// Steady state, so the table sits at its cap and every commit is a refresh
/// rather than an insert. Candidates beyond the cap miss: a capped viewer
/// probes everything the gather found and holds only the ghosts it has room
/// for.
///
/// Commits are `min(held, 98)`, so the 64 row does 64 of them against 98 for
/// the rest. That is what a viewer holding 64 ghosts would really do, but it
/// means that row is not a pure footprint comparison.
fn bench_ghost_cap(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/cap");
    const CANDIDATES: usize = 512;

    let ids = candidates(CANDIDATES, 0x0CA9);

    for &held in &[64usize, 128, 256, 512] {
        let commit = held.min(SLOTS_PER_PACKET);
        let build = || {
            let mut t = GhostTable::with_capacity(held);
            for (k, &e) in ids[..held].iter().enumerate() {
                t.sent(e, k as u32, TICK);
            }
            t
        };
        let probe = build();
        assert_eq!(probe.len(), held);
        let bytes = probe.slots() * 12;
        let n = (COLD_BYTES / bytes).max(2);
        let mut set: Vec<GhostTable> = (0..n).map(|_| build()).collect();
        let sets = set.len();
        let order = shuffled(sets, 0x0CA9);
        let mut cursor = 0usize;
        let mut departed = Vec::with_capacity(held);

        group.throughput(Throughput::Elements(CANDIDATES as u64));
        group.bench_with_input(BenchmarkId::from_parameter(held), &held, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.seen(black_box(e), TICK));
                }
                for &e in &ids[..commit] {
                    t.sent(black_box(e), black_box(TICK), TICK);
                }
                departed.clear();
                t.evict(TICK, GRACE, &mut departed);
            })
        });
    }
    group.finish();
}

/// What the `last_seen` stamp costs. `seen` writes it on every hit, dirtying
/// lines that must be written back; `mark` is the same probe with no write.
/// Same table, same candidates, so the difference is the stamp alone.
fn bench_stamp(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/stamp");
    const CANDIDATES: usize = 512;

    let ids = candidates(CANDIDATES, 0x57A3);

    for &held in &[128usize, 512] {
        let build = || {
            let mut t = GhostTable::with_capacity(512);
            for (k, &e) in ids[..held].iter().enumerate() {
                t.sent(e, k as u32, TICK);
            }
            t
        };
        let bytes = build().slots() * 12;
        let n = (COLD_BYTES / bytes).max(2);
        group.throughput(Throughput::Elements(CANDIDATES as u64));

        let mut set: Vec<GhostTable> = (0..n).map(|_| build()).collect();
        let sets = set.len();
        let order = shuffled(sets, 0x57A3);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("seen", held), &held, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.seen(black_box(e), TICK));
                }
            })
        });

        let set: Vec<GhostTable> = (0..n).map(|_| build()).collect();
        let order = shuffled(sets, 0x57A3);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("mark", held), &held, |b, _| {
            b.iter(|| {
                let t = &set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for &e in &ids {
                    black_box(t.mark(black_box(e)));
                }
            })
        });
    }
    group.finish();
}

/// The sweep alone, with nothing stale, which is the steady-state cost. Cold,
/// so it does not benefit from probes having just walked the table.
fn bench_evict(c: &mut Criterion) {
    let mut group = c.benchmark_group("ghost/evict");

    for &n in &SIZES {
        let ids = candidates(n, 0x3717);
        group.throughput(Throughput::Elements(n as u64));

        let mut set = cold_set(&ids);
        let sets = set.len();
        let order = shuffled(sets, 0xA5);
        let mut cursor = 0usize;
        let mut departed = Vec::with_capacity(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                departed.clear();
                t.evict(black_box(TICK), black_box(GRACE), &mut departed);
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_seen,
    bench_sent,
    bench_sent_shape,
    bench_ghost_cap,
    bench_stamp,
    bench_tick,
    bench_evict
);
criterion_main!(benches);
