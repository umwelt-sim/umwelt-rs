//! # Umwelt
//! A Rust library for managing online simulations at extreme scale
//! 
//! 
//! ```
//! use umwelt::WorldConfig;
//!
//! let cfg = WorldConfig::builder()
//!            .region_size_m(4096)
//!            .vertical_extent_m(1024)
//!            .horizontal_view_radius_m(256)
//!            .max_horizontal_speed_m_per_sec(40)
//!            .tick_hz(20)
//!            .build()?;
//! # Ok::<(), umwelt::ConfigError>(())
//! ```

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/umwelt-tile-small.svg",
    html_favicon_url = "https://raw.githubusercontent.com/umwelt-sim/umwelt-rs/main/assets/logo/favicon.svg"
)]

pub mod fixed;
pub mod pos;
pub mod config;
pub mod entity;
pub mod gather;
pub mod snapshot;
pub mod odometer;
pub mod ghost;
pub mod select;
pub mod budget;
pub mod codec;
pub mod packet;
mod subscription;
pub mod game;
pub mod sim;
pub mod net;

pub use fixed::{Fixed, DistSq, FIXED_ONE, FIXED_SHIFT};
pub use pos::{Pos3, Pos2, CellCoord, CellId};
pub use config::{ConfigError, WorldConfig, WorldConfigBuilder};
pub use entity::{EntityId, LiveSet};
pub use gather::{DiscoveredEntities, DiscoveredEntity};
pub use snapshot::{CellOccupants, CellSnapshot, SubCells};
pub use odometer::Odometer;
pub use ghost::GhostTable;
pub use select::{Policy, Ranked, Selection, Weights, select};
pub use codec::RecordCodec;
pub use packet::{PacketHeader, PacketReader, PacketWriter};
pub use budget::PacketBudget;
pub use game::Game;
pub use sim::{
    ClientLimits, Flow, Handoff, NullSink, Overrun, Pacing, PayloadSink, RecordingSink,
    RunSummary, Step, TickReport, TickStats, ViewerId, Wait, WorldSimulation,
};
pub use subscription::{CellList, Subscription};
pub use net::{RegionClient, RegionId, RegionServer};