//! The region-to-edge protocol, over NATS.
//!
//! One region simulation, and the edges relaying for it. for why this is NATS
//! rather than a connection this crate implements.
//!
//! - `protocol` is the messages and the versions.
//! - [`subjects`] is which subject carries what, and how to read one back.
//! - [`edges`] is the set of edges one region has heard from.
//! - [`session`] is what a running region and its edges say to each other.

pub(crate) mod client;
mod server;

// The wire itself. Subjects, message kinds, versions and caps: umwelt's, on both
// ends, and never stable for anyone outside the crate.
pub(crate) mod edges;
pub(crate) mod protocol;
pub(crate) mod session;
pub(crate) mod subjects;

pub use edges::{ClaimError, EdgeId, EdgeName, EdgeView, Edges};
pub use server::RegionServer;
pub use session::{Applied, EdgeSink, Inbound, Settled};
