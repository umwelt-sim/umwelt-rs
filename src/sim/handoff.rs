//! Keeping a slow sink off the tick thread.
//!
//! [`Handoff`] wraps any [`PayloadSink`] so that sending from a tick is a
//! memory copy and nothing else. An I/O thread drains what the tick left and
//! calls the wrapped sink, however long that takes.
//!
//! **The worst a wrapped sink can do is cost a client one frame.** It cannot
//! delay a tick, because nothing on the tick thread waits on it: a slot that is
//! busy is skipped rather than queued behind, and a slot that has not been
//! drained is overwritten. Payloads are latest-only, so a superseded one had no
//! value anyway.
//!
//! One slot per viewer rather than one shared queue. A shared queue under
//! pressure drops whatever arrives after it fills, which is the same viewers
//! every tick; per-viewer slots degrade evenly, with every client losing frames
//! rather than some losing all of them.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use crate::sim::sink::PayloadSink;
use crate::sim::viewer::ViewerId;

/// How long the I/O thread waits before looking again. Latency a payload may
/// sit for beyond the tick that produced it.
const POLL: Duration = Duration::from_millis(1);

/// One viewer's handoff point. Empty means nothing is waiting; a payload is
/// never empty, since it always carries a header.
#[derive(Debug, Default)]
struct Slot {
    buf: Mutex<Vec<u8>>,
    /// A hint for the drain's scan, so it locks only slots worth locking. Set
    /// and cleared under `buf`, so it cannot disagree with what is there.
    ready: AtomicBool,
}

/// Whether a slot is waiting to be drained.
///
/// A store rather than a read-modify-write: the I/O thread needs only that
/// something happened, not how much. Carries its own cache line because workers
/// write it while the I/O thread writes `Progress`.
#[derive(Debug, Default)]
#[repr(align(128))]
struct HasWork(AtomicBool);

/// Written only by the I/O thread.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Progress {
    delivered: AtomicU64,
    dropped: AtomicU64,
}

#[derive(Debug)]
struct Shared<S> {
    inner: S,
    /// Grows when a viewer is served for the first time, which is the only
    /// reason this is a lock at all.
    slots: RwLock<Vec<Slot>>,
    has_work: HasWork,
    progress: Progress,
    running: AtomicBool,
    alive: AtomicBool,
    thread: OnceLock<Thread>,
}

impl<S: PayloadSink> Shared<S> {
    /// Copies `payload` into a slot, superseding anything not yet drained.
    ///
    /// The buffer is kept between calls, so this reuses its capacity rather
    /// than allocating. The I/O thread holds the same lock only to swap a
    /// buffer out, never across a call into the wrapped sink.
    ///
    /// Losing the race drops the frame. Payloads are latest-only, so a dropped
    /// one costs the client a frame and nothing more.
    fn stash(&self, slot: &Slot, payload: &[u8]) {
        match slot.buf.try_lock() {
            Ok(mut buf) => {
                buf.clear();
                buf.extend_from_slice(payload);
                slot.ready.store(true, Ordering::Release);
                self.has_work.0.store(true, Ordering::Release);
            }
            Err(_) => {
                self.progress.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Neither lock may be held across the call into the wrapped sink.
    ///
    /// Holding the slot lock would make every stash for that viewer lose its
    /// race and drop. Holding the `slots` read lock is worse: a viewer served
    /// for the first time needs the write lock to grow, so it would wait on
    /// the wrapped sink from the tick thread, which is the exact failure this
    /// type exists to prevent. Both are held only for a memory copy.
    fn drain(&self, spare: &mut Vec<u8>, ready: &mut Vec<usize>) {
        ready.clear();
        {
            let slots = self.slots.read().expect("not poisoned");
            for (i, slot) in slots.iter().enumerate() {
                if slot.ready.load(Ordering::Acquire) {
                    ready.push(i);
                }
            }
        }
        for &i in ready.iter() {
            {
                // Slots only ever grow, so `i` stays valid. Swapping keeps both
                // buffers and their capacity; clearing marks the slot free.
                let slots = self.slots.read().expect("not poisoned");
                let mut buf = slots[i].buf.lock().expect("not poisoned");
                if buf.is_empty() {
                    slots[i].ready.store(false, Ordering::Release);
                    continue;
                }
                std::mem::swap(&mut *buf, spare);
                buf.clear();
                slots[i].ready.store(false, Ordering::Release);
            }
            self.inner.send(ViewerId::from_raw(i as u32), spare);
            self.progress.delivered.fetch_add(1, Ordering::Relaxed);
        }
        // The pass is the batch. A sink writing to a socket buffers everything
        // above and pays its syscalls here, once, instead of per payload.
        if !ready.is_empty() {
            self.inner.flush();
        }
    }
}

/// Clears `alive` however the I/O thread leaves, panic included.
struct Liveness<'a>(&'a AtomicBool);

impl Drop for Liveness<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A sink that never blocks the tick, wrapping one that might.
#[derive(Debug)]
pub struct Handoff<S: PayloadSink + Send + Sync + 'static> {
    shared: Arc<Shared<S>>,
    worker: Option<JoinHandle<()>>,
}

impl<S: PayloadSink + Send + Sync + 'static> Handoff<S> {
    pub fn new(inner: S) -> Handoff<S> {
        let shared = Arc::new(Shared {
            inner,
            slots: RwLock::new(Vec::new()),
            has_work: HasWork::default(),
            progress: Progress::default(),
            running: AtomicBool::new(true),
            alive: AtomicBool::new(true),
            thread: OnceLock::new(),
        });

        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("umwelt-handoff".into())
                .spawn(move || {
                    let _live = Liveness(&shared.alive);
                    let mut spare = Vec::new();
                    let mut ready = Vec::new();
                    while shared.running.load(Ordering::Acquire) {
                        if shared.has_work.0.swap(false, Ordering::AcqRel) {
                            shared.drain(&mut spare, &mut ready);
                        }
                        std::thread::park_timeout(POLL);
                    }
                    // Whatever the last tick left.
                    shared.drain(&mut spare, &mut ready);
                })
                .expect("spawn the handoff thread")
        };
        shared.thread.set(worker.thread().clone()).ok();
        Handoff { shared, worker: Some(worker) }
    }

    /// The sink being fed. Payloads reach it from the I/O thread, not from a
    /// tick, so a reader must expect to be behind by up to one poll interval,
    /// which is a millisecond.
    #[inline]
    pub fn inner(&self) -> &S {
        &self.shared.inner
    }

    /// Payloads handed to the wrapped sink.
    pub fn delivered(&self) -> u64 {
        self.shared.progress.delivered.load(Ordering::Relaxed)
    }

    /// Payloads lost to a slot that was mid-drain. The cost of never waiting.
    pub fn dropped(&self) -> u64 {
        self.shared.progress.dropped.load(Ordering::Relaxed)
    }

    /// Whether the I/O thread is still running. False once it has stopped, or
    /// if the wrapped sink panicked and took it down.
    pub fn healthy(&self) -> bool {
        self.shared.alive.load(Ordering::Acquire)
    }

    /// Payloads stashed and not yet drained.
    pub fn pending(&self) -> usize {
        let slots = self.shared.slots.read().expect("not poisoned");
        slots.iter().filter(|s| !s.buf.lock().expect("not poisoned").is_empty()).count()
    }

    /// Waits for everything stashed to reach the wrapped sink. Returns whether
    /// it drained within `timeout`.
    ///
    /// Bounded on purpose: a wrapped sink that blocks forever would otherwise
    /// hang whoever waited, which is the failure this whole type exists to
    /// keep off the tick thread and should not be reintroduced here.
    ///
    /// For tests and for shutdown. Calling it from a tick would reintroduce
    /// exactly the wait being avoided. Counting stashes against deliveries
    /// would never converge, since superseding a slot collapses two stashes
    /// into one delivery.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.pending() == 0 {
                return true;
            }
            if !self.healthy() || std::time::Instant::now() >= deadline {
                return false;
            }
            self.shared.has_work.0.store(true, Ordering::Release);
            if let Some(t) = self.shared.thread.get() {
                t.unpark();
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

impl<S: PayloadSink + Send + Sync + 'static> PayloadSink for Handoff<S> {
    fn send(&self, viewer: ViewerId, payload: &[u8]) {
        {
            let slots = self.shared.slots.read().expect("not poisoned");
            if let Some(slot) = slots.get(viewer.index()) {
                self.shared.stash(slot, payload);
                return;
            }
        }
        // Only the first time a viewer is served.
        let mut slots = self.shared.slots.write().expect("not poisoned");
        while slots.len() <= viewer.index() {
            slots.push(Slot::default());
        }
        self.shared.stash(&slots[viewer.index()], payload);
    }
}

impl<S: PayloadSink + Send + Sync + 'static> Drop for Handoff<S> {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        if let Some(t) = self.shared.thread.get() {
            t.unpark();
        }
        if let Some(w) = self.worker.take() {
            // A wrapped sink that panicked took the thread with it. Nothing to
            // do about it here, and nothing worth panicking a second time over.
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::sink::RecordingSink;

    fn v(n: u32) -> ViewerId {
        ViewerId::from_raw(n)
    }

    #[test]
    fn payloads_reach_the_wrapped_sink() {
        let h = Handoff::new(RecordingSink::new());
        h.send(v(0), b"first");
        h.send(v(2), b"second");
        assert!(h.flush(Duration::from_secs(5)));
        assert_eq!(h.inner().latest(v(0)).as_deref(), Some(&b"first"[..]));
        assert_eq!(h.inner().latest(v(2)).as_deref(), Some(&b"second"[..]));
        assert_eq!(h.delivered(), 2);
    }

    #[test]
    fn a_slow_sink_does_not_delay_the_sender() {
        struct Slow;
        impl PayloadSink for Slow {
            fn send(&self, _v: ViewerId, _p: &[u8]) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        let h = Handoff::new(Slow);
        let payload = vec![0u8; 1200];
        let t = std::time::Instant::now();
        // Every one of these is a viewer seen for the first time, so every one
        // has to grow the slot vector while the I/O thread is mid-drain.
        for k in 0..60 {
            h.send(v(k), &payload);
        }
        let took = t.elapsed();
        // Delivering even one of these takes 5 ms. Sixty would take three
        // hundred if the sender waited on any of it.
        assert!(took < Duration::from_millis(50), "sending waited on the sink: {took:?}");
    }

    #[test]
    fn a_blocking_sink_does_not_stop_the_sender_at_all() {
        struct Forever(AtomicBool);
        impl PayloadSink for Forever {
            fn send(&self, _v: ViewerId, _p: &[u8]) {
                self.0.store(true, Ordering::Release);
                loop {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }

        let h = Handoff::new(Forever(AtomicBool::new(false)));
        let payload = vec![7u8; 64];
        let t = std::time::Instant::now();
        for tick in 0..500u32 {
            h.send(v(tick % 8), &payload);
            std::thread::yield_now();
        }
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "a sink stuck forever must not reach the sender"
        );
        // The thread is wedged inside the wrapped sink, which is exactly the
        // case this type is here to survive.
        std::mem::forget(h);
    }

    #[test]
    fn a_superseded_payload_is_replaced_not_queued() {
        let h = Handoff::new(RecordingSink::new());
        for k in 0..50u8 {
            h.send(v(0), &[k; 8]);
        }
        assert!(h.flush(Duration::from_secs(5)));
        assert!(h.delivered() < 50, "supersedes must collapse, not queue: {}", h.delivered());

        // With nothing in flight the next stash cannot lose a race, so what a
        // client ends up holding is the freshest frame produced.
        h.send(v(0), &[99u8; 8]);
        assert!(h.flush(Duration::from_secs(5)));
        assert_eq!(h.inner().latest(v(0)).expect("delivered"), vec![99u8; 8]);
    }

    #[test]
    fn everything_stashed_is_delivered_on_drop() {
        let seen = Arc::new(RecordingSink::new());
        struct Fanout(Arc<RecordingSink>);
        impl PayloadSink for Fanout {
            fn send(&self, viewer: ViewerId, payload: &[u8]) {
                self.0.send(viewer, payload);
            }
        }

        let h = Handoff::new(Fanout(Arc::clone(&seen)));
        for k in 0..16u32 {
            h.send(v(k), b"bye");
        }
        drop(h);
        for k in 0..16u32 {
            assert_eq!(seen.latest(v(k)).as_deref(), Some(&b"bye"[..]), "viewer {k} lost");
        }
    }

    #[test]
    fn a_panicking_sink_takes_only_its_own_thread() {
        struct Boom;
        impl PayloadSink for Boom {
            fn send(&self, _v: ViewerId, _p: &[u8]) {
                panic!("sink exploded");
            }
        }

        let h = Handoff::new(Boom);
        h.send(v(0), b"x");
        // The I/O thread dies; sending keeps working and keeps not blocking.
        for _ in 0..200 {
            h.send(v(0), b"x");
            if !h.healthy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!h.healthy(), "a panicked I/O thread must be reportable");
        h.send(v(1), b"still fine");
    }

    #[test]
    fn a_stash_racing_a_drain_is_never_delivered_empty() {
        // The bug an atomic outside the mutex allowed: a producer stashing
        // between the drain's scan and its swap left the slot marked ready
        // after the payload had gone, so the next drain shipped the spare.
        struct Reject;
        impl PayloadSink for Reject {
            fn send(&self, viewer: ViewerId, payload: &[u8]) {
                assert!(!payload.is_empty(), "viewer {viewer:?} got an empty frame");
            }
        }

        let h = Handoff::new(Reject);
        for round in 0..2000u32 {
            h.send(v(round % 4), &[1u8; 64]);
            if round % 3 == 0 {
                std::thread::yield_now();
            }
        }
        assert!(h.flush(Duration::from_secs(5)));
        assert!(h.healthy(), "an empty frame would have panicked the I/O thread");
    }

    #[test]
    fn many_senders_share_one_handoff() {
        let h = Handoff::new(RecordingSink::new());
        std::thread::scope(|scope| {
            for t in 0..8u32 {
                let h = &h;
                scope.spawn(move || {
                    for _ in 0..500 {
                        h.send(v(t), &[t as u8; 32]);
                    }
                });
            }
        });
        assert!(h.flush(Duration::from_secs(5)));
        assert!(h.delivered() > 0);
        assert!(
            h.delivered() + h.dropped() <= 4000,
            "nothing can be delivered or dropped that was never sent"
        );
        for t in 0..8u32 {
            assert_eq!(h.inner().latest(v(t)).as_deref(), Some(&[t as u8; 32][..]));
        }
    }
}
