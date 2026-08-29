//! Where finished payloads go.
//!
//! [`WorldSimulation`](super::WorldSimulation) does not own a socket. It hands
//! each finished per-client payload to a [`PayloadSink`] the consumer supplies
//! at construction, so a test can drive a simulation with no network at all and
//! a consumer with an unusual deployment can substitute their own.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sim::viewer::ViewerId;

/// Receives one client's assembled payload.
///
/// Called once per served viewer per tick, from every worker thread at once,
/// which is why this is `Sync` and takes `&self`.
///
/// **An implementation must not block.** The tick is waiting on it, so an
/// implementation that waits on a socket turns a slow edge into a missed tick.
/// The intended shape is to hand off to an I/O thread and drop rather than
/// queue, since payloads are latest-only and a stale one has no value.
///
/// `payload` borrows a worker's buffer and is overwritten by the next viewer,
/// so a sink that keeps the bytes copies them.
pub trait PayloadSink: Sync {
    /// Takes one viewer's payload. Called once per served viewer per tick.
    fn send(&self, viewer: ViewerId, payload: &[u8]);

    /// The burst of sends is over: push anything held back.
    ///
    /// A sink that writes to a socket wants to buffer many payloads and pay one
    /// syscall, not pay one per payload. It cannot know when the burst ends, so
    /// it is told: [`Handoff`](crate::Handoff) calls this at the end of every
    /// drain pass, and a caller driving a sink directly calls it when a tick's
    /// payloads are done.
    ///
    /// The default does nothing, which is right for a sink that holds nothing
    /// back.
    fn flush(&self) {}
}

/// Discards every payload.
///
/// The default, and what a benchmark measuring the simulation rather than the
/// transport wants.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullSink;

impl PayloadSink for NullSink {
    #[inline]
    fn send(&self, _viewer: ViewerId, _payload: &[u8]) {}
}

/// Keeps the most recent payload per viewer.
///
/// For tests and examples. It allocates and takes a lock per send, so it is not
/// a production path.
#[derive(Debug, Default)]
pub struct RecordingSink {
    latest: Mutex<Vec<Vec<u8>>>,
    sends: AtomicU64,
}

impl RecordingSink {
    /// Holding nothing.
    pub fn new() -> RecordingSink {
        RecordingSink::default()
    }

    /// Payloads handed over since construction, across every viewer.
    pub fn sends(&self) -> u64 {
        self.sends.load(Ordering::Relaxed)
    }

    /// The last payload for one viewer, if it has ever been served.
    pub fn latest(&self, viewer: ViewerId) -> Option<Vec<u8>> {
        let held = self.latest.lock().expect("not poisoned");
        held.get(viewer.index()).filter(|p| !p.is_empty()).cloned()
    }

    /// Drops what was recorded, keeping the counts.
    pub fn clear(&self) {
        self.latest.lock().expect("not poisoned").clear();
        self.sends.store(0, Ordering::Relaxed);
    }
}

impl PayloadSink for RecordingSink {
    fn send(&self, viewer: ViewerId, payload: &[u8]) {
        let mut held = self.latest.lock().expect("not poisoned");
        if held.len() <= viewer.index() {
            held.resize(viewer.index() + 1, Vec::new());
        }
        let slot = &mut held[viewer.index()];
        slot.clear();
        slot.extend_from_slice(payload);
        self.sends.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: u32) -> ViewerId {
        ViewerId::from_raw(n)
    }

    #[test]
    fn a_null_sink_keeps_nothing() {
        let s = NullSink;
        s.send(v(0), b"discarded");
        assert_eq!(size_of::<NullSink>(), 0, "the default costs nothing to hold");
    }

    #[test]
    fn a_recording_sink_keeps_the_latest_per_viewer() {
        let s = RecordingSink::new();
        s.send(v(0), b"first");
        s.send(v(3), b"other");
        s.send(v(0), b"second");

        assert_eq!(s.latest(v(0)).as_deref(), Some(&b"second"[..]));
        assert_eq!(s.latest(v(3)).as_deref(), Some(&b"other"[..]));
        assert_eq!(s.latest(v(1)), None, "a viewer never served has nothing");
        assert_eq!(s.sends(), 3);
    }

    #[test]
    fn a_recording_sink_takes_sends_from_several_threads() {
        let s = RecordingSink::new();
        std::thread::scope(|scope| {
            for t in 0..8u32 {
                let s = &s;
                scope.spawn(move || {
                    for _ in 0..100 {
                        s.send(v(t), &[t as u8; 16]);
                    }
                });
            }
        });
        assert_eq!(s.sends(), 800);
        for t in 0..8u32 {
            assert_eq!(s.latest(v(t)).as_deref(), Some(&[t as u8; 16][..]));
        }
    }

    #[test]
    fn clearing_forgets_everything() {
        let s = RecordingSink::new();
        s.send(v(2), b"x");
        s.clear();
        assert_eq!(s.sends(), 0);
        assert_eq!(s.latest(v(2)), None);
    }
}
