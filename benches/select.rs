//! benches/select.rs
//!
//! Measures the pass that turns gathered candidates into a ranked selection:
//! read each candidate's odometer, difference it against the mark the ghost
//! table returned, weight it by distance band, then take the top N.
//!
//! None of this is in `src`. It models the pass Step 5 would build, so its cost
//! is known before the design leans on it. The scoring function is the shape
//! recorded in the design document; the weight values are placeholders and do
//! not affect cost.
//!
//! The odometer is shared by every viewer, unlike a ghost table, so after the
//! first viewer of a tick it stays resident. Measuring it hot is the realistic
//! case, not an optimiztic one.
//!
//! Two ways of reaching a candidate's reading are measured. **by_id** indexes
//! the odometer by entity id, which is how it is stored, and which a cell-walk
//! order makes a scattered read. **by_slot** indexes an odometer re-ordered
//! into snapshot order once per tick, which makes the same reads run forward.
//! The gap between them is what that re-ordering pass would be worth.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::{DiscoveredEntity, DistSq, EntityId, Fixed, GhostTable};

/// Entities in the region, which sets the odometer's footprint.
const ENTITIES: usize = 50_000;

/// Records fitting an MTU-sized packet with no events pending.
const SLOTS_PER_PACKET: usize = 98;

/// Ghosts a viewer holds, from the cap sweep.
const GHOST_CAP: usize = 256;

/// Candidate counts: the uniform case's measured mean, and the walk cap.
const SIZES: [usize; 2] = [95, 512];

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

/// Placeholder growth curve, one entry per squared-distance band.
fn weights() -> [u16; 64] {
    let mut w = [0u16; 64];
    for (b, slot) in w.iter_mut().enumerate() {
        *slot = 4096u16 >> (b / 4).min(12);
    }
    w
}

/// `drift x weight(band)`. One multiply, no divide, no square root.
///
/// `| 1` guards `ilog2` against a viewer scoring its own entity at zero
/// separation.
#[inline(always)]
fn score(drift: u32, dist_sq: u64, w: &[u16; 64]) -> u32 {
    let band = (dist_sq | 1).ilog2() as usize;
    drift.saturating_mul(w[band] as u32)
}

#[derive(Clone, Copy)]
struct Scored {
    at: u32,
    score: u32,
}

/// `n` candidates with ids scattered across the region, as a cell walk yields
/// them. `snapshot_index` runs 0..n, modeling one contiguous run, which is
/// what a crowded cell produces.
fn candidates(n: usize, seed: u64) -> Vec<DiscoveredEntity> {
    let mut rng = Rng::new(seed);
    let radius_sq = DistSq::from_radius(Fixed::from_meters(256)).raw();
    (0..n)
        .map(|k| {
            DiscoveredEntity::new(
                EntityId::from_raw((rng.next_u64() % ENTITIES as u64) as u32),
                k as u32,
                DistSq::from_raw(rng.next_u64() % radius_sq),
            )
        })
        .collect()
}

fn odometer(seed: u64) -> Vec<u32> {
    let mut rng = Rng::new(seed);
    (0..ENTITIES).map(|_| rng.next_u64() as u32).collect()
}

/// The mark each candidate's ghost carried, as `GhostTable::seen` returned it.
/// Every candidate is a ghost here, which is the steady-state case.
fn marks(n: usize, seed: u64) -> Vec<u32> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| rng.next_u64() as u32).collect()
}

/// Scoring alone, by the two ways of reaching a reading.
fn bench_score(c: &mut Criterion) {
    let mut group = c.benchmark_group("select/score");
    let odo = odometer(0x0D0);
    let w = weights();

    for &n in &SIZES {
        let cands = candidates(n, 0x5C0);
        let ms = marks(n, 0x3A4);
        let mut out: Vec<Scored> = Vec::with_capacity(n);
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("by_id", n), &n, |b, _| {
            b.iter(|| {
                out.clear();
                for (k, e) in cands.iter().enumerate() {
                    let drift = odo[e.id.index()].wrapping_sub(ms[k]);
                    out.push(Scored {
                        at: k as u32,
                        score: score(drift, e.dist_sq.raw(), black_box(&w)),
                    });
                }
                black_box((out[0].at, out.len()));
            })
        });

        group.bench_with_input(BenchmarkId::new("by_slot", n), &n, |b, _| {
            b.iter(|| {
                out.clear();
                for (k, e) in cands.iter().enumerate() {
                    let drift = odo[e.snapshot_index as usize].wrapping_sub(ms[k]);
                    out.push(Scored {
                        at: k as u32,
                        score: score(drift, e.dist_sq.raw(), black_box(&w)),
                    });
                }
                black_box((out[0].at, out.len()));
            })
        });
    }
    group.finish();
}

/// Taking the top N once the candidates are scored. Three shapes: one partial
/// selection at the packet limit, a full sort, and the two-stage form a ghost
/// cap needs — top `GHOST_CAP` for the ghost set, then top `SLOTS_PER_PACKET`
/// within those for the packet.
fn bench_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("select/rank");
    let odo = odometer(0x0D0);
    let w = weights();

    for &n in &SIZES {
        let cands = candidates(n, 0x5C0);
        let ms = marks(n, 0x3A4);
        let scored: Vec<Scored> = cands
            .iter()
            .enumerate()
            .map(|(k, e)| Scored {
                at: k as u32,
                score: score(odo[e.id.index()].wrapping_sub(ms[k]), e.dist_sq.raw(), &w),
            })
            .collect();
        let mut buf = scored.clone();
        group.throughput(Throughput::Elements(n as u64));

        let packet = n.min(SLOTS_PER_PACKET);
        group.bench_with_input(BenchmarkId::new("nth_packet", n), &n, |b, _| {
            b.iter(|| {
                buf.copy_from_slice(&scored);
                if packet < buf.len() {
                    buf.select_nth_unstable_by(packet, |a, b| b.score.cmp(&a.score));
                }
                black_box((buf[0].at, buf[0].score));
            })
        });

        group.bench_with_input(BenchmarkId::new("full_sort", n), &n, |b, _| {
            b.iter(|| {
                buf.copy_from_slice(&scored);
                buf.sort_unstable_by(|a, b| b.score.cmp(&a.score));
                black_box((buf[0].at, buf[0].score));
            })
        });

        let cap = n.min(GHOST_CAP);
        group.bench_with_input(BenchmarkId::new("two_stage", n), &n, |b, _| {
            b.iter(|| {
                buf.copy_from_slice(&scored);
                if cap < buf.len() {
                    buf.select_nth_unstable_by(cap, |a, b| b.score.cmp(&a.score));
                }
                let head = &mut buf[..cap];
                if packet < head.len() {
                    head.select_nth_unstable_by(packet, |a, b| b.score.cmp(&a.score));
                }
                black_box((buf[0].at, buf[0].score));
            })
        });
    }
    group.finish();
}

/// Enough tables to exceed any cache, so each is cold when reached.
const COLD_BYTES: usize = 32 << 20;

const TICK: u32 = 1_000;

/// A new ghost outranks every refresh, which is the paper's "status change
/// first, then priority".
const NEW: u32 = u32::MAX;

fn shuffled(n: usize, seed: u64) -> Vec<usize> {
    let mut rng = Rng::new(seed);
    let mut v: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}

/// A table holding a ghost of each id in `held`.
fn table_of(held: &[EntityId]) -> GhostTable {
    let mut t = GhostTable::with_capacity(held.len().max(1));
    for (k, &e) in held.iter().enumerate() {
        t.sent(e, k as u32, TICK);
    }
    t
}

fn cold_tables(held: &[EntityId]) -> Vec<GhostTable> {
    let bytes = table_of(held).slots() * 12;
    let n = (COLD_BYTES / bytes).max(2);
    (0..n).map(|_| table_of(held)).collect()
}

/// Probing and scoring in one pass against two passes over the same
/// candidates. The split form writes every mark to a buffer and reads it back;
/// the fused form keeps it in a register.
///
/// Which candidates are ghosts matters for branch prediction. **clustered**
/// makes the first `held` candidates the ghosts, which is what a
/// distance-ordered walk plus a relevance-ranked ghost set produces.
/// **interleaved** alternates, which is the adversarial case.
fn bench_fused(c: &mut Criterion) {
    let mut group = c.benchmark_group("select/fuse");
    let odo = odometer(0x0D0);
    let w = weights();

    // 95 candidates all held, which is the uniform case under any cap; 512
    // candidates against a 256 ghost cap, which is the crowded steady state.
    for &(n, held) in &[(95usize, 95usize), (512, GHOST_CAP)] {
        let cands = candidates(n, 0x5C0);
        let clustered: Vec<EntityId> = cands[..held].iter().map(|e| e.id).collect();
        let interleaved: Vec<EntityId> =
            cands.iter().step_by(n.div_ceil(held)).map(|e| e.id).take(held).collect();
        let mut out: Vec<Scored> = Vec::with_capacity(n);
        let mut ms: Vec<Option<u32>> = vec![None; n];
        group.throughput(Throughput::Elements(n as u64));

        let mut set = cold_tables(&clustered);
        let sets = set.len();
        let order = shuffled(sets, 0xF05E);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("fused_clustered", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                out.clear();
                for (k, e) in cands.iter().enumerate() {
                    let s = match t.seen(e.id, TICK) {
                        Some(mark) => {
                            let drift = odo[e.id.index()].wrapping_sub(mark);
                            score(drift, e.dist_sq.raw(), &w)
                        }
                        None => NEW,
                    };
                    out.push(Scored { at: k as u32, score: s });
                }
                black_box((out[0].at, out.len()));
            })
        });

        let mut set = cold_tables(&clustered);
        let order = shuffled(sets, 0xF05E);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("split", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                for (k, e) in cands.iter().enumerate() {
                    ms[k] = t.seen(e.id, TICK);
                }
                out.clear();
                for (k, e) in cands.iter().enumerate() {
                    let s = match ms[k] {
                        Some(mark) => {
                            let drift = odo[e.id.index()].wrapping_sub(mark);
                            score(drift, e.dist_sq.raw(), &w)
                        }
                        None => NEW,
                    };
                    out.push(Scored { at: k as u32, score: s });
                }
                black_box((out[0].at, out.len()));
            })
        });

        let mut set = cold_tables(&interleaved);
        let sets = set.len();
        let order = shuffled(sets, 0xF05E);
        let mut cursor = 0usize;
        group.bench_with_input(BenchmarkId::new("fused_interleaved", n), &n, |b, _| {
            b.iter(|| {
                let t = &mut set[order[cursor % sets]];
                cursor = cursor.wrapping_add(1);
                out.clear();
                for (k, e) in cands.iter().enumerate() {
                    let s = match t.seen(e.id, TICK) {
                        Some(mark) => {
                            let drift = odo[e.id.index()].wrapping_sub(mark);
                            score(drift, e.dist_sq.raw(), &w)
                        }
                        None => NEW,
                    };
                    out.push(Scored { at: k as u32, score: s });
                }
                black_box((out[0].at, out.len()));
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_score, bench_select, bench_fused);
criterion_main!(benches);
