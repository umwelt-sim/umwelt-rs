#![doc = include_str!("../README.md")]

#![warn(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/umwelt-tile-small.svg",
    html_favicon_url = "https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/favicon.svg"
)]

pub mod config;
pub mod entity;
pub mod fixed;
pub mod game;
pub mod id;
pub mod net;
pub mod packet;
pub mod pos;
pub mod sim;

mod budget;
mod codec;
mod gather;
mod ghost;
mod odometer;
mod select;
mod snapshot;
mod subscription;

#[doc(hidden)]
pub mod internals;

pub use config::{ConfigError, WorldConfig, WorldConfigBuilder};
pub use entity::{EntityId, EntityKind, LiveIter, LiveSet};
pub use fixed::{DistSq, Fixed};
pub use game::{ClientGame, EdgeGame, Game};
pub use id::{ClientId, EntityHandle, EntityKey, RegionId};
pub use net::{ClientHandle, EdgeClient, EdgeHandle, EdgeServer, NetError, RegionServer};
pub use packet::{PacketHeader, PacketReader};
pub use pos::{CellCoord, CellId, Pos2, Pos3};
// From modules the crate keeps to itself. A consumer names these — `Policy` and
// `Weights` to tune replication, the rest to read what a region reports — and
// nothing else in those modules.
pub use select::{Policy, Weights};
pub use sim::{
    ClientLimits, Flow, Handoff, NullSink, Overrun, Pacing, PayloadSink, RecordingSink,
    RunSummary, Step, TickReport, TickStats, ViewerId, Wait, WorldSimulation,
};
