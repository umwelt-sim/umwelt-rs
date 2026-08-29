//! Simulation management.
//!
//! One process owns one region. [`WorldSimulation`] holds its authoritative
//! state, runs the tick loop, and does all per-client replication work against
//! the snapshot it publishes to itself.

mod clock;
mod handoff;
mod sink;
mod viewer;
mod world;

pub use clock::{Flow, Overrun, Pacing, RunSummary, TickReport, Wait};
pub use handoff::Handoff;
pub use sink::{NullSink, PayloadSink, RecordingSink};
pub use viewer::{ClientLimits, ViewerId};
pub(crate) use world::TickSpan;
pub use world::{
    DEFAULT_GHOST_CAP, DEFAULT_GRACE, DEFAULT_WALK_CAP, Step, TickStats, WorldSimulation,
};

#[doc(hidden)]
pub use world::Outbound;
