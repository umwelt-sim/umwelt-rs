//! # Umwelt
//! A Rust library for managing online simulations at extreme scale
//! 
//! 
//! ```
//! let cfg = WorldConfig::builder()
//!            .region_size_m(4096)
//!            .vertical_extent_m(1024)
//!            .horizontal_view_radius_m(256)
//!            .max_horizontal_speed_m_per_sec(40)
//!            .tick_hz(20);
//! ```

pub mod fixed;
pub mod pos;
pub mod config;
pub mod entity;
pub mod gather;
pub mod snapshot;
pub mod codec;
mod subscription;

pub use fixed::{Fixed, DistSq, FIXED_ONE, FIXED_SHIFT};
pub use pos::{Pos3, Pos2, CellCoord, CellId};
pub use config::{ConfigError, WorldConfig, WorldConfigBuilder};
pub use entity::{EntityId, LiveSet};
pub use gather::{DiscoveredEntities, DiscoveredEntity};
pub use snapshot::{CellOccupants, CellSnapshot, SubCells};
pub use codec::RecordCodec;
pub use subscription::{CellList, Subscription};