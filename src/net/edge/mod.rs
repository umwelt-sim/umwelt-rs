//! The edge-to-client link, over QUIC.
//!
//! One edge server, and the game clients connected to it. Many peers, none of
//! them trusted, running on someone else's machine and updated on someone
//! else's schedule — the opposite of `net::region` in every way that matters,
//! which is why the two share no types. See `docs/adr/0006`.
//!
//! - [`protocol`] is the messages and how they are framed.
//! - [`ids`] is the two identities an edge mints.
//!
//! An [`EdgeServer`] holds a [`RegionClient`](crate::net::RegionClient) on one
//! side and a QUIC endpoint on the other, and relays. It connects to nothing
//! and binds nothing: the caller supplies a connected `async_nats::Client` and
//! a bound `quinn::Endpoint`, so credentials, certificates and the crypto
//! provider stay with the deployment.

mod handle;
pub mod ids;
pub mod protocol;
mod server;

pub use handle::{EdgeHandle, EdgeStats};
pub use server::{DEFAULT_HEARTBEAT, EdgeServer};
pub use ids::{ClientId, EntityKey};
pub use protocol::{FromClient, Framer, MAX_MESSAGE_BYTES, ToClient};
