//! The two traits a consumer implements.
//!
//! [`Game`] is a region's, called once per tick with a [`Step`]. [`EdgeGame`]
//! is an edge's, called when a client connects, says something, or goes away.
//!
//! They live here rather than beside the tier that calls them because they are
//! the consumer's extension points and belong together, and because neither
//! belongs inside a networking module. See `docs/adr/0006`.

use std::net::SocketAddr;

use crate::entity::EntityId;
use crate::net::edge::{ClientId, EntityKey};
use crate::net::region::protocol::RegionId;
use crate::sim::Step;

/// The consumer's game, called once per tick.
pub trait Game {
    /// Moves entities, spawns and despawns. Everything that is not position is
    /// the consumer's own storage, keyed by [`EntityId`].
    fn step(&mut self, world: &mut Step<'_>);
}

/// The consumer's edge, called when a client arrives, says something, or goes.
///
/// Every method does nothing by default. An edge whose clients spawn, move and
/// despawn needs none of them: the library runs that whole loop itself, and
/// [`ClientId`] and [`EntityKey`] surface only once the consumer originates
/// actions of its own.
///
/// Calls are serialized and made on the I/O path, so **an implementation must
/// not block**, on the same terms as [`PayloadSink`](crate::PayloadSink).
///
/// Sending is deliberately not here. It is on
/// [`EdgeHandle`](crate::net::EdgeHandle), which is cheap to clone and callable
/// from anywhere, because an edge that could only speak from inside a callback
/// would force a consumer to queue its own work until some unrelated event
/// fired. See `docs/adr/0006`.
// The parameter names are the documentation, so they are spelled out rather
// than underscored away.
#[allow(unused_variables)]
pub trait EdgeGame: Send + 'static {
    /// A game client connected. Refuse it with
    /// [`EdgeHandle::disconnect`](crate::net::EdgeHandle::disconnect), from
    /// here or from wherever the decision is actually made.
    fn connected(&mut self, client: ClientId, from: SocketAddr) {}

    /// The last call for this client. Its entities are already despawned and
    /// [`removed`](Self::removed) has already fired for each, so there is
    /// nothing here to clean up on umwelt's behalf.
    fn disconnected(&mut self, client: ClientId) {}

    /// A region allocated an id. `client` is `None` for a detached entity.
    fn spawned(
        &mut self,
        entity: EntityKey,
        client: Option<ClientId>,
        region: RegionId,
        id: EntityId,
    ) {
    }

    /// Gone, whatever caused it — including a despawn nobody asked for.
    fn removed(&mut self, entity: EntityKey, client: Option<ClientId>) {}

    /// Bytes on the consumer's own channel. umwelt does not read them.
    fn message(&mut self, client: ClientId, body: &[u8]) {}
}
