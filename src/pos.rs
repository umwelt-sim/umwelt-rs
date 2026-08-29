//! Position, value types, and cell coordinates
//!
//! The types in this module are abstractions around optimizations made to
//! perform calculations within the simulation as fast as possible. This includes
//! things like performing shifts instead of divide and multiply.
//!
//! "Raw units" are referred to in multiple places in different modules. No matter
//! what the runtime configuration contains, a single raw unit will _always_ be
//! equal to `1/1024` meters or `0.9766` millimeters. The net result of this
//! is that the slowest possible entity moving 1 unit per tick at 20 Hz moves around 2cm/s.
//!
//! # Axes
//!
//! `x` and `y` are horizontal, `z` is up. The cell grid is 2D over x and y, so
//! an area of interest is a vertical cylinder rather than a sphere.
//!
//! # Cells
//!
//! Cells are 2D columns because simulation worlds (at least of the type this library specializes
//! in) are far wider than they are tall. Vertical cells would be mostly empty buckets. A consequence
//! of this is that, at least internally, vertical speed is unconstrained. Falling at terminal
//! velocity never crosses a cell boundary, so it can't be enforced by any max-delta validation during
//! a tick.
//!
//! Height is still important to relevance-based subscriptions. Someone 300m above is far less interesting
//! than someone 300m away on the ground. The grid can't express that, so it has to come out in the
//! priority scoring, which uses full 3D distance while subscriptions use horizontal only. This is why
//! we need both [`Pos3::dist_sq`] and [`Pos3::horizontal_dist_sq`].
//!
//! # Storage
//!
//! [`Pos3`] is a **value type**: functions take and return this directly from the stack without ever performing
//! a pointer de-reference. This is important for hot-path calculations on a ~50ms budget tick.

use core::{
    fmt,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use crate::fixed::{DistSq, Fixed};

/// A 3D position or displacement, region-local.
///
/// Positions and displacements share one type. Keeping them apart (as
/// `Instant` and `Duration` do) catches real errors, but forbids midpoints
/// and interpolation.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos3 {
    /// East.
    pub x: Fixed,
    /// North.
    pub y: Fixed,
    /// Up.
    pub z: Fixed,
}

impl Pos3 {
    /// The origin.
    pub const ZERO: Pos3 = Pos3 { x: Fixed::ZERO, y: Fixed::ZERO, z: Fixed::ZERO };

    /// From whole meters.
    #[inline]
    pub const fn from_meters(x: i32, y: i32, z: i32) -> Pos3 {
        Pos3 {
            x: Fixed::from_meters(x),
            y: Fixed::from_meters(y),
            z: Fixed::from_meters(z),
        }
    }

    /// From three axes.
    #[inline]
    pub const fn new(x: Fixed, y: Fixed, z: Fixed) -> Pos3 {
        Pos3 { x, y, z }
    }

    /// The `x` and `y` of it, which is what distance is measured on.
    #[inline]
    pub const fn horizontal(self) -> Pos2 {
        Pos2 { x: self.x, y: self.y }
    }

    /// Full 3D squared separation (Pythagorean distance). Use for priority scoring.
    ///
    /// Widened to `i64` before squaring: across a 4 km region the largest
    /// squared separation is ~5.3e13, well beyond `i32`.
    #[inline]
    pub const fn dist_sq(self, other: Pos3) -> DistSq {
        let dx = self.x.raw() as i64 - other.x.raw() as i64;
        let dy = self.y.raw() as i64 - other.y.raw() as i64;
        let dz = self.z.raw() as i64 - other.z.raw() as i64;

        DistSq::from_raw((dx * dx + dy * dy + dz * dz) as u64)
    }

    /// Squared separation ignoring height. Use for subscription tests, which
    /// must agree with the 2D cell grid.
    #[inline]
    pub const fn horizontal_dist_sq(self, other: Pos3) -> DistSq {
        let dx = self.x.raw() as i64 - other.x.raw() as i64;
        let dy = self.y.raw() as i64 - other.y.raw() as i64;

        DistSq::from_raw((dx * dx + dy * dy) as u64)
    }

    /// Component-wise midpoint. Only meaningful for positions in the same
    /// region.
    #[inline]
    pub const fn midpoint(self, other: Pos3) -> Pos3 {
        Pos3 {
            x: Fixed::from_raw((self.x.raw() + other.x.raw()) / 2),
            y: Fixed::from_raw((self.y.raw() + other.y.raw()) / 2),
            z: Fixed::from_raw((self.z.raw() + other.z.raw()) / 2),
        }
    }
}

impl Add for Pos3 {
    type Output = Pos3;

    #[inline]
    fn add(self, rhs: Pos3) -> Pos3 {
        Pos3 { x: self.x + rhs.x, y: self.y + rhs.y, z: self.z + rhs.z }
    }
}

impl Sub for Pos3 {
    type Output = Pos3;

    #[inline]
    fn sub(self, rhs: Pos3) -> Pos3 {
        Pos3 { x: self.x - rhs.x, y: self.y - rhs.y, z: self.z - rhs.z }
    }
}

// AddAssign is +=
impl AddAssign for Pos3 {
    #[inline]
    fn add_assign(&mut self, rhs: Pos3) {
        self.x += rhs.x;
        self.y += rhs.y;
        self.z += rhs.z;
    }
}

impl SubAssign for Pos3 {
    #[inline]
    fn sub_assign(&mut self, rhs: Pos3) {
        self.x -= rhs.x;
        self.y -= rhs.y;
        self.z -= rhs.z;
    }
}

impl fmt::Debug for Pos3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {}, {})", self.x, self.y, self.z)
    }
}

/// A horizontal position. What the cell grid actually operates on.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Pos2 {
    /// East.
    pub x: Fixed,
    /// North.
    pub y: Fixed,
}

impl Pos2 {
    /// The origin.
    pub const ZERO: Pos2 = Pos2 { x: Fixed::ZERO, y: Fixed::ZERO };

    /// From two axes.
    #[inline]
    pub const fn new(x: Fixed, y: Fixed) -> Pos2 {
        Pos2 { x, y }
    }

    /// From whole meters.
    #[inline]
    pub const fn from_meters(x: i32, y: i32) -> Pos2 {
        Pos2 { x: Fixed::from_meters(x), y: Fixed::from_meters(y) }
    }

    /// Squared distance between the two, which is what ranking compares.
    #[inline]
    pub const fn dist_sq(self, other: Pos2) -> DistSq {
        let dx = self.x.raw() as i64 - other.x.raw() as i64;
        let dy = self.y.raw() as i64 - other.y.raw() as i64;
        DistSq::from_raw((dx * dx + dy * dy) as u64)
    }

    /// Lift to 3D at the given height.
    #[inline]
    pub const fn at_height(self, z: Fixed) -> Pos3 {
        Pos3 { x: self.x, y: self.y, z }
    }
}

impl fmt::Debug for Pos2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.x, self.y)
    }
}

/// A cell's 2D grid coordinate.
///
/// `u16` per axis supports 65,536 cells across a region, which should
/// be far more than anyone needs. Conversion from a position lives on `WorldConfig`,
/// which owns the cell size. This type on its own knows nothing of world sizes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CellCoord {
    /// Column, counting east.
    pub x: u16,
    /// Row, counting north.
    pub y: u16,
}

impl CellCoord {
    /// From a cell's column and row.
    #[inline]
    pub const fn new(x: u16, y: u16) -> CellCoord {
        CellCoord { x, y }
    }

    /// Chebyshev distance: the number of steps between two cell coordinates
    /// on the grid. The optimization is that Chebyshev distance treats the
    /// adjacent diagonal cells as a distance of 1.    
    ///
    /// This is the metric the square subscription grid uses. A cell is subscribed
    /// when its Chebyshev distance from the viewer is within some threshold.
    #[inline]
    pub const fn chebyshev(self, other: CellCoord) -> u16 {
        let dx = self.x.abs_diff(other.x);
        let dy = self.y.abs_diff(other.y);
        if dx > dy { dx } else { dy }
    }

    /// Whether two coordinates differ by at most one step per axis. This
    /// is used by rules and other configuration settings for move enforcement.
    #[inline]
    pub const fn is_adjacent_move(self, other: CellCoord) -> bool {
        self.x.abs_diff(other.x) <= 1 && self.y.abs_diff(other.y) <= 1
    }
}

impl fmt::Debug for CellCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cell({}, {})", self.x, self.y)
    }
}

/// A cell's linear index within a region for array lookup.
///
/// Currently row-major: `y * cells_per_axis + x`. If cells are later ordered
/// some other way (e.g. a Hilbert curve for cross-node partitioning), this type
/// remains ignorant as the raw value considered opaque.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct CellId(u32);

impl CellId {
    /// From the raw value.
    #[inline]
    pub const fn from_raw(raw: u32) -> CellId {
        CellId(raw)
    }

    /// The raw value.
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The id as an index into a per-cell array.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for CellId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CellId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_ignores_height() {
        let a = Pos3::from_meters(0, 0, 0);
        let b = Pos3::from_meters(3, 4, 1000);
        assert_eq!(
            a.horizontal_dist_sq(b),
            Pos2::from_meters(0, 0).dist_sq(Pos2::from_meters(3, 4))
        );
        assert!(a.dist_sq(b) > a.horizontal_dist_sq(b));
    }

    #[test]
    fn dist_sq_matches_pythagoras() {
        let a = Pos3::from_meters(0, 0, 0);
        let b = Pos3::from_meters(3, 4, 0);
        assert_eq!(a.dist_sq(b), DistSq::from_radius(Fixed::from_meters(5)));
    }

    #[test]
    fn dist_sq_survives_region_diagonal() {
        let a = Pos3::from_meters(0, 0, 0);
        let b = Pos3::from_meters(4096, 4096, 1024);
        // Would overflow if the subtraction were done in i32.
        let d = a.dist_sq(b);
        assert!(d.raw() > 0);
        assert!(d > a.dist_sq(Pos3::from_meters(4095, 4095, 1024)));
    }

    #[test]
    fn dist_sq_is_symmetric() {
        let a = Pos3::from_meters(100, 200, 30);
        let b = Pos3::from_meters(700, 50, 90);
        assert_eq!(a.dist_sq(b), b.dist_sq(a));
    }

    #[test]
    fn chebyshev_counts_diagonals_as_one() {
        let a = CellCoord::new(5, 5);
        assert_eq!(a.chebyshev(CellCoord::new(7, 7)), 2);
        assert_eq!(a.chebyshev(CellCoord::new(7, 5)), 2);
        assert_eq!(a.chebyshev(CellCoord::new(5, 5)), 0);
    }

    #[test]
    fn adjacency_matches_single_step() {
        let a = CellCoord::new(5, 5);
        assert!(a.is_adjacent_move(CellCoord::new(6, 6)));
        assert!(a.is_adjacent_move(CellCoord::new(4, 5)));
        assert!(!a.is_adjacent_move(CellCoord::new(7, 5)));
    }

    #[test]
    fn add_sub_round_trip() {
        let a = Pos3::from_meters(10, 20, 30);
        let d = Pos3::from_meters(1, 2, 3);
        assert_eq!((a + d) - d, a);
    }
}
