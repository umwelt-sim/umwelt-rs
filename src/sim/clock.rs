//! Pacing the tick loop.
//!
//! A [`WorldConfig`](crate::WorldConfig) carries `tick_hz`, so the rate belongs
//! to the library and so does the loop that keeps it. See §Pacing the loop in
//! the design document.
//!
//! Deadlines are absolute, `epoch + n * period`, so a wait that overshoots does
//! not push the whole schedule out behind it.
//!
//! [`tick`](WorldSimulation::tick) is deterministic and this is not: it reads a
//! clock, and what it does about a tick that overruns depends on how long that
//! tick took. A replay drives `tick` directly.

use std::time::{Duration, Instant};

use crate::sim::sink::PayloadSink;
use crate::sim::world::{Game, Outbound, TickStats, WorldSimulation};

/// How long [`Wait::Sleep`] holds the core before a deadline.
///
/// For timer granularity, which is about a millisecond on every platform this
/// targets: no sleep lands exactly on its deadline. It is not a remedy for what
/// a core costs after idling, which needs a far larger fraction of the period
/// and is [`Wait::Hold`]; see §Idle costs speed in the design document.
pub const SPIN_MARGIN: Duration = Duration::from_millis(1);

/// How the loop waits for a deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Wait {
    /// Sleep to within [`SPIN_MARGIN`] of the deadline, then hold the core.
    #[default]
    Sleep,
    /// Hold the core for the whole interval.
    ///
    /// Costs a core and buys back what idling takes from one. Measured at 4x on
    /// an idle region and nothing on a busy one, so this is for a consumer that
    /// has measured that it needs it.
    Hold,
    /// Do not wait. Ticks run back to back, which is throughput rather than a
    /// schedule.
    None,
}

/// What to do about a tick that finishes past the next deadline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overrun {
    /// Run every tick and let real time stretch. Simulated time falls behind
    /// the wall clock and every tick still happens, so a run stays reproducible
    /// against one that kept up.
    #[default]
    Dilate,
    /// Keep the wall clock and skip the ticks that did not fit.
    Drop,
}

/// How a run is paced.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pacing {
    pub wait: Wait,
    pub overrun: Overrun,
    /// Stop after this many ticks. `None` runs until the observer says stop.
    pub ticks: Option<u32>,
}

impl Pacing {
    /// Stops after `ticks`, otherwise the defaults.
    pub fn for_ticks(ticks: u32) -> Pacing {
        Pacing { ticks: Some(ticks), ..Pacing::default() }
    }
}

/// Whether the loop takes another tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// One tick's outcome.
///
/// `took` and `late` answer different questions. The first is what the tick
/// cost, the second is whether the machine is keeping up, and a tick can be
/// cheap and late if something else on the box was not.
#[derive(Clone, Copy, Debug)]
pub struct TickReport {
    pub tick: u32,
    pub stats: TickStats,
    /// Time inside the tick.
    pub took: Duration,
    /// How late the tick started against its deadline. Zero if it started on
    /// time.
    pub late: Duration,
    /// Deadlines skipped before this tick under [`Overrun::Drop`].
    pub dropped: u32,
}

/// What a run did. Percentiles are the caller's: a tick's duration arrives in
/// every [`TickReport`], and holding a histogram is a presentation decision.
#[derive(Clone, Copy, Debug, Default)]
pub struct RunSummary {
    pub ticks: u32,
    /// Ticks that started after their deadline.
    pub late: u32,
    /// Deadlines skipped under [`Overrun::Drop`].
    pub dropped: u32,
    pub worst_tick: Duration,
    pub worst_late: Duration,
    pub elapsed: Duration,
}

impl<G: Game, S: PayloadSink> WorldSimulation<G, S> {
    /// Ticks on a schedule until the observer stops it or the tick count runs
    /// out, discarding per-viewer selections.
    /// The observer is handed the world alongside the report, since deciding
    /// whether to keep going, or what to record, usually means looking at it.
    pub fn run(
        &mut self,
        pacing: Pacing,
        on_tick: impl FnMut(TickReport, &WorldSimulation<G, S>) -> Flow,
    ) -> RunSummary {
        self.run_with(pacing, &|_| {}, on_tick)
    }

    /// [`run`](Self::run), handing each served viewer's work to `on_viewer`
    /// before the buffers are reused.
    ///
    /// A tick that overruns is never made up by running extra ticks. That is
    /// the spiral of death: the loop falls behind, runs more to catch up, takes
    /// longer, falls further behind. [`Overrun`] chooses which of the two
    /// honest answers applies instead.
    pub fn run_with(
        &mut self,
        pacing: Pacing,
        on_viewer: &(impl Fn(Outbound<'_>) + Sync),
        mut on_tick: impl FnMut(TickReport, &WorldSimulation<G, S>) -> Flow,
    ) -> RunSummary {
        let period = Duration::from_millis(1_000 / self.config().tick_hz() as u64);
        let started = Instant::now();
        let mut summary = RunSummary::default();

        // The origin of the schedule. Dilating moves it, which is the whole of
        // what dilating means: every deadline after this one arrives later.
        let mut epoch = started;
        let mut slot: u32 = 0;

        loop {
            if pacing.ticks.is_some_and(|max| summary.ticks >= max) {
                break;
            }
            slot += 1;
            let deadline = epoch + period * slot;

            let mut late = Duration::ZERO;
            let mut dropped = 0;
            match Instant::now() {
                now if now < deadline => match pacing.wait {
                    Wait::None => {}
                    w => {
                        let remaining = deadline - now;
                        if w == Wait::Sleep && remaining > SPIN_MARGIN {
                            std::thread::sleep(remaining - SPIN_MARGIN);
                        }
                        while Instant::now() < deadline {
                            std::hint::spin_loop();
                        }
                    }
                },
                now if pacing.wait == Wait::None => {
                    // No schedule to be late against.
                    let _ = now;
                }
                now => {
                    late = now - deadline;
                    summary.late += 1;
                    summary.worst_late = summary.worst_late.max(late);
                    match pacing.overrun {
                        Overrun::Dilate => epoch += late,
                        Overrun::Drop => {
                            dropped = (late.as_nanos() / period.as_nanos().max(1)) as u32;
                            slot += dropped;
                            summary.dropped += dropped;
                        }
                    }
                }
            }

            let at = Instant::now();
            let stats = self.tick_with(on_viewer);
            let took = at.elapsed();

            summary.ticks += 1;
            summary.worst_tick = summary.worst_tick.max(took);
            let report = TickReport { tick: self.tick_count(), stats, took, late, dropped };
            if on_tick(report, self) == Flow::Stop {
                break;
            }
        }

        summary.elapsed = started.elapsed();
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorldConfig;
    use crate::sim::world::Step;

    /// Does nothing, or sleeps to force an overrun.
    struct Idle(Duration);

    impl Game for Idle {
        fn step(&mut self, _: &mut Step<'_>) {
            if !self.0.is_zero() {
                std::thread::sleep(self.0);
            }
        }
    }

    /// One millisecond per tick, so a test that has to feel the schedule does
    /// not have to wait a second to do it.
    fn fast() -> WorldConfig {
        WorldConfig::builder()
            .region_size_m(4096)
            .vertical_extent_m(1024)
            .horizontal_view_radius_m(256)
            .max_horizontal_speed_m_per_sec(40)
            .tick_hz(1_000)
            .build()
            .expect("config is valid")
    }

    fn sim(step: Duration) -> WorldSimulation<Idle> {
        WorldSimulation::new(fast(), Idle(step))
    }

    #[test]
    fn runs_the_ticks_it_was_asked_for() {
        let mut w = sim(Duration::ZERO);
        let mut seen = 0;
        let s = w.run(
            Pacing { wait: Wait::None, ..Pacing::for_ticks(10) },
            |_, _| {
                seen += 1;
                Flow::Continue
            },
        );
        assert_eq!(s.ticks, 10);
        assert_eq!(seen, 10);
        assert_eq!(w.tick_count(), 10);
    }

    #[test]
    fn an_observer_can_stop_the_run() {
        let mut w = sim(Duration::ZERO);
        let s = w.run(Pacing { wait: Wait::None, ticks: None, ..Pacing::default() }, |r, w| {
            assert_eq!(w.tick_count(), r.tick, "the observer sees the world it is told about");
            if r.tick == 3 { Flow::Stop } else { Flow::Continue }
        });
        assert_eq!(s.ticks, 3, "a run with no tick limit ends when told to");
    }

    #[test]
    fn a_paced_run_cannot_finish_early() {
        let mut w = sim(Duration::ZERO);
        let s = w.run(Pacing::for_ticks(20), |_, _| Flow::Continue);
        let period = Duration::from_millis(1);
        assert!(
            s.elapsed >= period * 19,
            "20 ticks at 1 kHz took {:?}, which is faster than the schedule allows",
            s.elapsed
        );
    }

    #[test]
    fn overrunning_never_runs_extra_ticks() {
        // Five milliseconds of game step against a one millisecond period.
        for overrun in [Overrun::Dilate, Overrun::Drop] {
            let mut w = sim(Duration::from_millis(5));
            let s = w.run(Pacing { overrun, ..Pacing::for_ticks(5) }, |_, _| Flow::Continue);
            assert_eq!(s.ticks, 5, "{overrun:?} ran a tick it was not asked for");
            assert!(s.late > 0, "{overrun:?} should have noticed it was late");
        }
    }

    #[test]
    fn dropping_skips_deadlines_and_dilating_does_not() {
        let mut dropping = sim(Duration::from_millis(5));
        let s = dropping.run(
            Pacing { overrun: Overrun::Drop, ..Pacing::for_ticks(5) },
            |_, _| Flow::Continue,
        );
        assert!(s.dropped > 0, "a 5 ms tick on a 1 ms period leaves deadlines behind");

        let mut dilating = sim(Duration::from_millis(5));
        let s = dilating.run(
            Pacing { overrun: Overrun::Dilate, ..Pacing::for_ticks(5) },
            |_, _| Flow::Continue,
        );
        assert_eq!(s.dropped, 0, "dilating keeps every tick");
    }
}
