//! The three traits a consumer implements.
//!
//! [`Game`] is a region's, called once per tick with a [`Step`]. [`EdgeGame`]
//! is an edge's, called when a client connects, says something, or goes away.
//! [`ClientGame`] is a game client's, called when the edge says something back.
//!
//! They live here rather than beside the tier that calls them because they are
//! the consumer's extension points and belong together, and because neither
//! belongs inside a networking module. See `docs/adr/0006`.

use std::net::SocketAddr;

use crate::entity::EntityId;
use crate::net::edge::{ClientId, EntityKey};
use crate::packet::PacketReader;
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

/// The consumer's game client, called when its edge says something.
///
/// Every method does nothing by default. Nothing here polls, waits or retries:
/// the library owns the reading, and a connection that goes reports itself
/// through [`disconnected`](Self::disconnected) rather than as a timeout the
/// consumer has to interpret.
///
/// Calls are serialized and made on the I/O path, so **an implementation must
/// not block**, on the same terms as [`EdgeGame`].
///
/// Sending is on [`ClientHandle`](crate::net::ClientHandle), which the
/// constructor hands over, for the same reason [`EdgeHandle`](crate::net::EdgeHandle)
/// is not a callback argument. See `docs/adr/0006`.
#[allow(unused_variables)]
pub trait ClientGame: Send + 'static {
    /// The entity this handle asked for exists, in this region, under this id.
    ///
    /// A game is the only tier that sees more than one region at a time, so
    /// keeping that map is its job — the region a handle ended up in is the
    /// game's own knowledge, unlike how that region happens to be configured.
    fn spawned(&mut self, handle: u32, region: RegionId, entity: EntityId) {}

    /// Gone, whatever caused it — including a despawn this client never asked
    /// for, because a region's own game can despawn anything.
    fn removed(&mut self, handle: u32) {}

    /// What one of this client's entities can see: which to forget, and where
    /// the rest are.
    ///
    /// Already decoded. A game does not see the packet, the codec, or the world
    /// the region was configured with — it has no say in any of those, and
    /// being handed them would only invite it to act as though it did.
    ///
    /// It does see the region, because the ids inside are that region's and
    /// mean nothing in another one. A game watching two at once has to key by
    /// both.
    ///
    /// Borrowed from the datagram it arrived in, so a consumer that keeps
    /// anything copies it.
    fn state(&mut self, handle: u32, region: RegionId, state: &PacketReader<'_>) {}

    /// The game's own bytes, which umwelt did not read.
    fn message(&mut self, body: &[u8]) {}

    /// The connection is gone. The last call this game will get.
    ///
    /// Reconnecting is the caller's, on the same terms as connecting: it
    /// supplied the connection, and umwelt has no opinion about where the edge
    /// is or how long to wait before trying again.
    fn disconnected(&mut self) {}
}
