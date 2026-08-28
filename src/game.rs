//! The traits a consumer implements.
//!
//! [`Game`] is a region's, called once per tick with a [`Step`]. An edge's
//! will sit beside it.
//!
//! They live here rather than beside the tier that calls them because they are
//! the consumer's extension points and belong together, and because neither
//! belongs inside a networking module. See `docs/adr/0006`.

use crate::sim::Step;

/// The consumer's game, called once per tick.
pub trait Game {
    /// Moves entities, spawns and despawns. Everything that is not position is
    /// the consumer's own storage, keyed by [`EntityId`].
    fn step(&mut self, world: &mut Step<'_>);
}
