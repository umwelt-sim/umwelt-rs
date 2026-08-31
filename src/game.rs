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
use crate::packet::TickObservation;
use crate::pos::Pos3;
use crate::sim::Step;

/// What the edge's game decides when a client asks to teleport.
///
/// Returned by [`EdgeGame::teleporting`]. The default is to allow with no
/// carried state. A consumer that needs to transfer inventory, health, or any
/// other game state serializes it into [`Carry`](Self::Carry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TeleportDecision {
    /// Allow the teleport with no game state.
    Allow,
    /// Allow the teleport and carry these bytes to the destination. The
    /// destination's [`EdgeGame::teleport_arrived`] receives them.
    Carry(Vec<u8>),
    /// Deny the request. The entity stays where it is and the client is told.
    Deny,
}

/// The logic and rules for a durable, server-side game. The [`Game::step`]
/// function is called once per tick (default is **20Hz**).
///
/// # Example
///
/// Growth, rot, the mine, and the fighting are all `MildewValley`'s own state,
/// keyed by [`EntityId`]. Umwelt is told where things are and which still
/// exist, nothing else.
///
/// ```
/// # use umwelt::{Game, Step};
/// # struct MildewValley;
/// # impl MildewValley {
/// #     fn grow(&mut self) {}
/// #     fn rot(&mut self, _world: &mut Step<'_>) {}
/// #     fn gather_from_mines(&mut self, _world: &mut Step<'_>) {}
/// #     fn settle_fights(&mut self, _world: &mut Step<'_>) {}
/// # }
/// impl Game for MildewValley {
///     fn step(&mut self, world: &mut Step<'_>) {
///         // Ripening touches only the game's own memory.
///         self.grow();
///         // These three reach the world, and only to despawn, spawn and move.
///         self.rot(world);
///         self.gather_from_mines(world);
///         self.settle_fights(world);
///     }
/// }
/// ```
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

/// Trait that must be implemented by edges. By default, game developers do
/// not have to provide any code as there is a valid default implementation.
/// If you want to provide logic against your own metadata (e.g. a ban list,
/// emitting to a data logger, etc) then provide your own implementation.
///
/// Calls are serialized and made on the I/O path, so **an implementation must
/// not block** or panic, on the same terms as [`PayloadSink`](crate::PayloadSink).
///
/// Sending is deliberately not here. It is on
/// [`EdgeHandle`](crate::net::EdgeHandle), which is cheap to clone and
/// callable from anywhere.
///
/// # Example
///
/// The ban list, the headcount and the chat are the responsibility
/// of the edge. Umwelt relays the valley position updates
/// without reading a byte of any of it.
///
/// ```
/// # use std::net::SocketAddr;
/// # use umwelt::{ClientId, EdgeGame, EdgeHandle};
/// # struct MildewEdge { handle: EdgeHandle }
/// # impl MildewEdge {
/// #     fn is_banned(&self, _from: SocketAddr) -> bool { false }
/// #     fn admit(&mut self, _client: ClientId) {}
/// #     fn show_out(&mut self, _client: ClientId) {}
/// #     fn gossip(&mut self, _from: ClientId, _line: &[u8]) {}
/// # }
/// impl EdgeGame for MildewEdge {
///     fn connected(&mut self, client: ClientId, from: SocketAddr) {
///         // Who is welcome is the game's rule, not umwelt's.
///         if self.is_banned(from) {
///             self.handle.disconnect(client);
///             return;
///         }
///         self.admit(client);
///     }
///
///     fn disconnected(&mut self, client: ClientId) {
///         self.show_out(client);
///     }
///
///     fn message_received(&mut self, client: ClientId, body: &[u8]) {
///         // umwelt carried these bytes without looking at them.
///         self.gossip(client, body);
///     }
/// }
/// ```
#[allow(unused_variables)]
pub trait EdgeGame: Send + 'static {
    /// A game client connected. Refuse it with
    /// [`EdgeHandle::disconnect`](crate::net::EdgeHandle::disconnect), from
    /// here or from wherever the decision is actually made.
    fn connected(&mut self, client: ClientId, from: SocketAddr) {}

    /// The last call for the indicated client. Its entities are already despawned and
    /// [`removed`](Self::removed) has already fired for each, so there is
    /// nothing here to clean up on umwelt's behalf. Logic at this level doesn't
    /// care about the reason for disconnect.
    fn disconnected(&mut self, client: ClientId) {}

    /// A region allocated an id. `client` is `None` for an entity
    /// that exists only to be observed and take up space (frequently
    /// called a "prop").
    fn spawned(
        &mut self,
        entity: EntityKey,
        client: Option<ClientId>,
        region: RegionId,
        id: EntityId,
    ) {
    }

    /// The relay indicates an entity has left the simulation. Reason
    /// for departure isn't passed here.
    fn removed(&mut self, entity: EntityKey, client: Option<ClientId>) {}

    /// A client asked to teleport an entity to another region. Return
    /// [`TeleportDecision::Allow`] or [`TeleportDecision::Carry`] to proceed,
    /// or [`TeleportDecision::Deny`] to refuse.
    ///
    /// `Carry` serializes game state — inventory, health, whatever the game
    /// needs on the other side — as opaque bytes. The destination's
    /// [`teleport_arrived`](Self::teleport_arrived) receives them.
    fn teleporting(
        &mut self,
        entity: EntityKey,
        client: ClientId,
        from: RegionId,
        to: RegionId,
        at: Pos3,
    ) -> TeleportDecision {
        TeleportDecision::Allow
    }

    /// A teleported entity arrived in its destination region. `state` is what
    /// [`teleporting`](Self::teleporting) packed into
    /// [`TeleportDecision::Carry`], or empty if it returned `Allow`.
    fn teleport_arrived(
        &mut self,
        entity: EntityKey,
        client: ClientId,
        from: RegionId,
        to: RegionId,
        state: &[u8],
    ) {
    }

    /// Opaque bytes in the relayed message body. Umwelt never reads
    /// or interprets these bytes.
    ///
    /// The one call here that is not about an entity or a client. Every other
    /// method reports something umwelt did; this reports something the game
    /// said, in a format umwelt has no part in. The only rule imposed on
    /// `body` is a 64 KiB length cap.
    fn message_received(&mut self, client: ClientId, body: &[u8]) {}
}

/// The consumer's game client, called when its edge says something.
///
/// Nothing here polls, waits or retries: the library owns the reading,
/// and a connection that goes away reports itself
/// through [`disconnected`](Self::disconnected) rather than as a timeout the
/// consumer has to interpret.
///
/// Calls are serialized and made on the I/O path, so **an implementation must
/// not block** or panic.
///
/// Sending is on [`ClientHandle`](crate::net::ClientHandle), which the
/// constructor hands over.
///
/// # Example
///
/// The scene the player looks at belongs to `Farm`. Umwelt decodes IDs and
/// positions and supplies no meaning or interpretation of the entity positions
/// in the game.
///
/// ```
/// # use umwelt::{ClientGame, EntityHandle, EntityId, TickObservation, RegionId};
/// # struct Farm;
/// # impl Farm {
/// #     fn remember(&mut self, _h: EntityHandle, _r: RegionId, _e: EntityId) {}
/// #     fn forget(&mut self, _handle: EntityHandle) {}
/// #     fn drop_gone(&mut self, _region: RegionId, _seen: &TickObservation<'_>) {}
/// #     fn redraw(&mut self, _region: RegionId, _seen: &TickObservation<'_>) {}
/// #     fn clear_scene(&mut self) {}
/// # }
/// impl ClientGame for Farm {
///     fn spawned(&mut self, handle: EntityHandle, region: RegionId, entity: EntityId) {
///         // Which region a handle landed in is the game's to keep track of.
///         self.remember(handle, region, entity);
///     }
///
///     fn removed(&mut self, handle: EntityHandle) {
///         self.forget(handle);
///     }
///
///     fn observed(
///         &mut self,
///         _handle: EntityHandle,
///         region: RegionId,
///         observation: &TickObservation<'_>,
///     ) {
///         // An id belongs to its region, so the scene is keyed by both.
///         self.drop_gone(region, observation);
///         self.redraw(region, observation);
///     }
///
///     fn disconnected(&mut self) {
///         self.clear_scene();
///     }
/// }
/// ```
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
    ///
    /// This is what the library is for, and the only call here that carries
    /// the world. A region worked out which entities this one can see, fitted
    /// them to its budget, and umwelt decoded the result. A client that
    /// ignored it would have nothing to draw.
    ///
    /// Not to be confused with
    /// [`message_received`](Self::message_received), which carries bytes the
    /// game chose to send and umwelt never looked inside.
    fn observed(
        &mut self,
        handle: EntityHandle,
        region: RegionId,
        observation: &TickObservation<'_>,
    ) {
    }

    /// An entity arrived in its destination region after a
    /// [`teleport`](crate::ClientHandle::teleport). Same handle the client has
    /// always held. [`spawned`](Self::spawned) fires first with the new region
    /// and entity id, so the game has both when this arrives.
    fn teleported(&mut self, handle: EntityHandle, region: RegionId) {}

    /// A teleport did not complete — the destination was unreachable, the
    /// spawn was refused, or the edge's game denied the request. The entity
    /// stays in its origin region.
    fn teleport_failed(&mut self, handle: EntityHandle, region: RegionId) {}

    /// The game's own bytes, which umwelt did not read.
    ///
    /// Nothing in `body` has a meaning umwelt knows: no format, no version,
    /// and the only rule imposed on it is a 64 KiB length cap.
    ///
    /// The other side of the contrast with [`observed`](Self::observed), which
    /// carries the view umwelt computed for this client. This carries whatever
    /// the game put in it.
    fn message_received(&mut self, body: &[u8]) {}

    /// The connection is gone. The last call this game will get.
    ///
    /// Reconnecting is the caller's, on the same terms as connecting: it
    /// supplied the connection, and umwelt has no opinion about where the edge
    /// is or how long to wait before trying again.
    fn disconnected(&mut self) {}
}
