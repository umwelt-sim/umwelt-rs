//! What a seam costs the region on the far side of it.
//!
//! The same border strip, one view radius deep along the west side and
//! oscillating in place, is served two ways: to observers standing in the
//! strip, and to shadows standing on the west side itself, which is where the
//! edge puts a shadow for an observer across the seam (`docs/adr/0010`). The
//! record says a shadow is served by the machinery every viewer already goes
//! through and costs what any viewer costs; this is the measurement.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use umwelt::internals::spawn_shadow;
use umwelt::{
    ClientLimits, EntityId, Fixed, Game, Pos3, Step, WorldConfig, WorldSimulation,
};

/// Entities in the strip. Enough that a viewer's candidate set exceeds its
/// ghost cap, or there is no selection pressure and nothing to measure.
const ENTITIES: usize = 8192;
const VIEWERS: usize = 1000;
const WARMUP_TICKS: u32 = 30;

/// xorshift64*, seeded. Not for anything but a reproducible layout.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u32) -> u32 {
        (self.next_u64() % bound as u64) as u32
    }
}

/// The strip, and the shadows standing at its seam if any. Entities are
/// spawned on the first tick and oscillate by a meter after; shadows never
/// move, which is what a shadow does while its owner stands still.
struct Strip {
    pending: Vec<Pos3>,
    seam: Vec<Pos3>,
    shadows: Vec<EntityId>,
    phase: bool,
}

impl Game for Strip {
    fn step(&mut self, w: &mut Step<'_>) {
        if !self.pending.is_empty() {
            for p in core::mem::take(&mut self.pending) {
                w.spawn(p, 0);
            }
            for p in core::mem::take(&mut self.seam) {
                self.shadows.push(spawn_shadow(w, p));
            }
            return;
        }
        self.phase = !self.phase;
        let d = Fixed::from_meters(1).raw();
        let up = self.phase;
        // The bulk path covers the strip and not the shadows.
        let (xs, _, _, live) = w.positions_mut();
        for id in live.iter() {
            let i = id.index();
            let forward = (i % 2 == 0) == up;
            xs[i] =
                Fixed::from_raw(if forward { xs[i].raw() + d } else { xs[i].raw() - d });
        }
    }
}

/// `n` entities within one view radius of the west side, anywhere along it,
/// a meter in from the side so the oscillation stays inside.
fn strip(cfg: &WorldConfig, n: usize, rng: &mut Rng) -> Vec<Pos3> {
    let deep = cfg.horizontal_view_radius().raw() as u32;
    let long = cfg.region_size().raw() as u32;
    let margin = Fixed::from_meters(2).raw();
    (0..n)
        .map(|_| {
            Pos3::new(
                Fixed::from_raw(margin + rng.below(deep - margin as u32 * 2) as i32),
                Fixed::from_raw(rng.below(long) as i32),
                Fixed::ZERO,
            )
        })
        .collect()
}

/// `n` points on the west side itself, spread along it.
fn seam(cfg: &WorldConfig, n: usize, rng: &mut Rng) -> Vec<Pos3> {
    let long = cfg.region_size().raw() as u32;
    (0..n)
        .map(|_| {
            Pos3::new(Fixed::ZERO, Fixed::from_raw(rng.below(long) as i32), Fixed::ZERO)
        })
        .collect()
}

/// A warmed-up region serving `VIEWERS` viewers over the strip: shadows at the
/// seam if `shadows`, otherwise observers drawn from the strip itself.
fn build(shadows: bool) -> WorldSimulation<Strip> {
    let cfg = WorldConfig::default();
    let mut rng = Rng(0xC0FFEE_5EA3);
    let pending = strip(&cfg, ENTITIES, &mut rng);
    let seam = if shadows { seam(&cfg, VIEWERS, &mut rng) } else { Vec::new() };
    let mut sim = WorldSimulation::new(
        cfg,
        Strip { pending, seam, shadows: Vec::new(), phase: false },
    );
    sim.set_thread_count(1);
    sim.tick();
    assert_eq!(sim.entity_count(), ENTITIES, "shadows are not in the world");
    let viewers: Vec<EntityId> = if shadows {
        sim.game().shadows.clone()
    } else {
        (0..ENTITIES)
            .step_by(ENTITIES / VIEWERS)
            .take(VIEWERS)
            .map(|i| EntityId::from_raw(i as u32))
            .collect()
    };
    for id in viewers {
        sim.register_viewer(id, ClientLimits::default());
    }
    for _ in 0..WARMUP_TICKS {
        sim.tick();
    }
    sim
}

fn bench_seam(c: &mut Criterion) {
    let mut group = c.benchmark_group("seam");
    for (name, shadows) in [("observers_in_strip", false), ("shadows_at_seam", true)] {
        let mut sim = build(shadows);
        let s = sim.tick();
        let per = |x: u64| x as f64 / s.viewers.max(1) as f64;
        println!(
            "{name}: {} viewers, {:.1} candidates, {:.1} records per viewer, one thread",
            s.viewers,
            per(s.candidates),
            per(s.records)
        );
        group.throughput(Throughput::Elements(VIEWERS as u64));
        group.bench_function(BenchmarkId::new(name, VIEWERS), |b| b.iter(|| sim.tick()));
    }
    group.finish();
}

criterion_group!(benches, bench_seam);
criterion_main!(benches);
