//! Traits to be implemented by game developers.
//!
//! If you're developing a game then this module is where you should start.
//!
//! ## Simulation
//! A simulation is a single region of the overall universe of the game. You define
//! your game logic (e.g. physics, combat, decay, etc) within the callback from the
//! [`Game`] trait. The simulation server calls the [`Game::step`] handler, which
//! you define.
//!
//! ## Edge
//! While a simulation needs to be resilient and to restore itself after a crash,
//! edges are disposable. Edges are started to support game client load, relaying
//! communication between the game client and the simulation. Edges can be deployed
//! without writing any new code, or you can implement your own [`EdgeGame`].
//!
//! ## Game Client
//! The game client is the user-facing component of your game. Implementing
//! [`ClientGame`] and establishing a **QUIC** connection to an edge provided
//! by **umwelt** gives your multi-billion dollar viral game the necessary plumbing.

use std::net::SocketAddr;

use crate::entity::EntityId;
use crate::id::{ClientId, EntityHandle, EntityKey, RegionId};
use crate::packet::PacketReader;
use crate::sim::Step;

/// The logic and rules for a durable, server-side game. The [`Game::step`]
/// function is called once per tick (default is **20Hz**).
pub trait Game {
    /// Moves entities, spawns and despawns. Everything that is not position is
    /// the consumer's own storage, keyed by [`EntityId`]. The implementer
    /// of this function must not block and must not panic, nor can it
    /// start its own async runtime.
    /// The `world` parameter is a no-allocation output buffer. The
    /// [`Step`] struct provides appropriate access to this buffer.
    /// 
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
/// [`EdgeHandle`](crate::net::EdgeHandle), which is cheap to clone and
/// callable from anywhere, because an edge that could only speak from inside a
/// callback would force a consumer to queue its own work until some unrelated
/// event fired.
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
/// constructor hands over, for the same reason
/// [`EdgeHandle`](crate::net::EdgeHandle) is not a callback argument.
#[allow(unused_variables)]
pub trait ClientGame: Send + 'static {
    /// The entity this handle asked for exists, in this region, under this id.
    ///
    /// A game is the only tier that sees more than one region at a time, so
    /// keeping that map is its job — the region a handle ended up in is the
    /// game's own knowledge, unlike how that region happens to be configured.
    fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {}

    /// Gone, whatever caused it — including a despawn this client never asked
    /// for, because a region's own game can despawn anything.
    fn removed(&mut self, handle: EntityHandle) {}

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
    fn state(
        &mut self,
        handle: EntityHandle,
        region: RegionId,
        state: &PacketReader<'_>,
    ) {
    }

    /// The game's own bytes, which umwelt did not read.
    fn message(&mut self, body: &[u8]) {}

    /// The connection is gone. The last call this game will get.
    ///
    /// Reconnecting is the caller's, on the same terms as connecting: it
    /// supplied the connection, and umwelt has no opinion about where the edge
    /// is or how long to wait before trying again.
    fn disconnected(&mut self) {}
}
