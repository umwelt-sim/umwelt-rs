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
//! [`EdgeClient`] is a game client's side, and it is what a game developer
//! holds: four commands on a [`ClientHandle`], and a
//! [`ClientGame`](crate::ClientGame) called with whatever the edge says back.
//! Which command rides a datagram and which rides a stream is decided here
//! rather than at the call site, and nothing asks a consumer to poll.
//!
//! An [`EdgeServer`] holds a [`RegionClient`](crate::net::RegionClient) on one
//! side and a QUIC endpoint on the other, and relays. It connects to nothing
//! and binds nothing: the caller supplies a connected `async_nats::Client` and
//! a bound `quinn::Endpoint`, so credentials, certificates and the crypto
//! provider stay with the deployment.

mod client;
mod handle;
pub mod ids;
pub mod protocol;
mod server;

pub use client::{ClientHandle, EdgeClient};
pub use handle::{EdgeHandle, EdgeStats};
pub use server::{DEFAULT_HEARTBEAT, EdgeServer};
pub use ids::{ClientId, EntityKey};
pub use protocol::{FromClient, Framer, MAX_MESSAGE_BYTES, ToClient};
