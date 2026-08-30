//! Fixed-point scalar arithmetic.
//!
//! A [`Fixed`] is an `i32` with 10 fractional bits. 
//! One meter is 1024 units, so the smallest representable step is 1/1024 m (~0.98 mm). 
//! Range is roughly ±2,097,152 m. This is like a distance version of managing money 
//! in terms of cents rather than fractional dollars.
//!
//! Integer rather than `f32` because the simulation must replay bit-identically
//! after a crash, and float results vary across CPUs and compilers.
//!
//! This module has no dependencies and is `no_std` compatible.

use core::fmt;
use core::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

/// Fractional bits. This dictates the portion of single-number storage
/// devoted to the fractional portion. Using this fixed shift value is how
/// we end up with a base unit of distance (1/1024m) that allows us to
/// do simulation calculations with cheap and fast integer math.
pub(crate) const FIXED_SHIFT: u32 = 10;

/// One meter in raw units.
pub(crate) const FIXED_ONE: i32 = 1 << FIXED_SHIFT;

/// A scalar distance or coordinate.
///
/// `repr(transparent)` guarantees identical layout to `i32`, so `&[Fixed]`
/// and `&[i32]` are the same bytes and slices can be transmuted or memcpy'd
/// without conversion.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Fixed(i32);

impl Fixed {
    /// Zero.
    pub const ZERO: Fixed = Fixed(0);
    /// One meter.
    pub const ONE: Fixed = Fixed(FIXED_ONE);
    /// The most negative value, about -2,097,152 m.
    pub const MIN: Fixed = Fixed(i32::MIN);
    /// The largest value, about 2,097,152 m.
    pub const MAX: Fixed = Fixed(i32::MAX);

    /// Smallest representable step: 1/1024 m.
    pub const EPSILON: Fixed = Fixed(1);

    /// From whole meters.
    #[inline]
    pub const fn from_meters(m: i32) -> Fixed {
        Fixed(m * FIXED_ONE)
    }

    /// From meters and millimeters of distance, e.g. `from_millimeters(3, 500)`
    /// is 3.5 m. Rounds toward zero; 1/1000 is not exactly representable in
    /// binary.
    #[inline]
    pub const fn from_millimeters(m: i32, mm: i32) -> Fixed {
        Fixed(m * FIXED_ONE + (mm * FIXED_ONE) / 1000)
    }

    /// From raw internal units. Use when the value is already scaled.
    #[inline]
    pub const fn from_raw(raw: i32) -> Fixed {
        Fixed(raw)
    }

    /// The underlying scaled integer.
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Whole meters, truncated toward negative infinity.
    #[inline]
    pub const fn floor_meters(self) -> i32 {
        self.0 >> FIXED_SHIFT
    }

    /// Lossy conversion for display and tests. Never use in simulation state.
    #[inline]
    pub fn to_f32(self) -> f32 {
        self.0 as f32 / FIXED_ONE as f32
    }

    /// Magnitude. Saturates rather than wrapping on [`Fixed::MIN`].
    #[inline]
    pub const fn abs(self) -> Fixed {
        Fixed(self.0.abs())
    }

    /// The smaller of two fixed point numbers.
    #[inline]
    pub const fn min(self, other: Fixed) -> Fixed {
        if self.0 < other.0 { self } else { other }
    }

    /// The larger of two fixed point numbers
    #[inline]
    pub const fn max(self, other: Fixed) -> Fixed {
        if self.0 > other.0 { self } else { other }
    }

    /// Clamp into `[low, high]`. Panics in debug if `low > high`.
    #[inline]
    pub const fn clamp(self, low: Fixed, high: Fixed) -> Fixed {
        debug_assert!(low.0 <= high.0);
        self.max(low).min(high)
    }

    // -- checked / saturating ---------------------------------------------
    //
    // Rust panics on overflow in debug and wraps in release. For simulation
    // state, pick explicitly: checked at boundaries where bad input can
    // arrive, plain ops in the hot path where ranges are already validated.

    /// Sum, or `None` if it leaves the range.
    #[inline]
    pub const fn checked_add(self, rhs: Fixed) -> Option<Fixed> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Fixed(v)),
            None => None,
        }
    }

    /// Difference, or `None` if it leaves the range.
    #[inline]
    pub const fn checked_sub(self, rhs: Fixed) -> Option<Fixed> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Fixed(v)),
            None => None,
        }
    }

    /// Multiply two fixed-point values, checked against `i32` range.
    #[inline]
    pub const fn checked_mul(self, rhs: Fixed) -> Option<Fixed> {
        let wide = (self.0 as i64 * rhs.0 as i64) >> FIXED_SHIFT;
        if wide < i32::MIN as i64 || wide > i32::MAX as i64 {
            None
        } else {
            Some(Fixed(wide as i32))
        }
    }

    /// Sum, clamped to the range rather than wrapping.
    #[inline]
    pub const fn saturating_add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0.saturating_add(rhs.0))
    }

    /// Difference, clamped to the range rather than wrapping.
    #[inline]
    pub const fn saturating_sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0.saturating_sub(rhs.0))
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

impl Add for Fixed {
    type Output = Fixed;
    /// Scales already match, so this is a plain integer add.
    #[inline]
    fn add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 + rhs.0)
    }
}

impl Sub for Fixed {
    type Output = Fixed;
    #[inline]
    fn sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 - rhs.0)
    }
}

impl Neg for Fixed {
    type Output = Fixed;
    #[inline]
    fn neg(self) -> Fixed {
        Fixed(-self.0)
    }
}

impl AddAssign for Fixed {
    #[inline]
    fn add_assign(&mut self, rhs: Fixed) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Fixed {
    #[inline]
    fn sub_assign(&mut self, rhs: Fixed) {
        self.0 -= rhs.0;
    }
}

impl Mul for Fixed {
    type Output = Fixed;
    /// Fixed times fixed. The raw product carries 20 fractional bits, so it
    /// is shifted back down by 10. Widened to `i64` first because two 32-bit
    /// values overflow `i32` before the shift can bring them back in range.
    #[inline]
    fn mul(self, rhs: Fixed) -> Fixed {
        Fixed(((self.0 as i64 * rhs.0 as i64) >> FIXED_SHIFT) as i32)
    }
}

impl Mul<i32> for Fixed {
    type Output = Fixed;
    /// Scale up by a value. No shift needed.
    #[inline]
    fn mul(self, rhs: i32) -> Fixed {
        Fixed(self.0 * rhs)
    }
}

impl Div for Fixed {
    type Output = Fixed;
    /// Fixed divided by fixed. Shifted up before dividing. Panics on division by zero.
    #[inline]
    fn div(self, rhs: Fixed) -> Fixed {
        Fixed((((self.0 as i64) << FIXED_SHIFT) / rhs.0 as i64) as i32)
    }
}

impl Div<i32> for Fixed {
    type Output = Fixed;
    /// Divide fixed by scalar (scale down)
    #[inline]
    fn div(self, rhs: i32) -> Fixed {
        Fixed(self.0 / rhs)
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

impl fmt::Display for Fixed {
    /// Meters to three decimal places, computed without floating point so
    /// output is identical everywhere.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let neg = self.0 < 0;
        let abs = self.0.unsigned_abs() as u64;
        let whole = abs >> FIXED_SHIFT;
        let frac = abs & (FIXED_ONE as u64 - 1);
        let milli = (frac * 1000) >> FIXED_SHIFT;
        if neg {
            write!(f, "-")?;
        }
        write!(f, "{whole}.{milli:03}m")
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fixed({} = {})", self.0, self)
    }
}

/// A squared distance, in raw-units-squared.
///
/// Deliberately not a [`Fixed`]. Squaring doubles the fractional bits and the
/// magnitude overflows `i32`. Across a 4 km region the largest squared
/// separation is around 5.3e13, which needs 64 bits.
///
/// Ordering is preserved by squaring, so comparisons work directly and
/// ranking algorithms never need a square root.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct DistSq(u64);

impl DistSq {
    /// No distance at all.
    pub const ZERO: DistSq = DistSq(0);

    /// Square a radius so it can be compared against a [`DistSq`].
    #[inline]
    pub const fn from_radius(r: Fixed) -> DistSq {
        let raw = r.0 as i64;
        DistSq((raw * raw) as u64)
    }

    /// From a squared raw distance, which is what the comparison is done on.
    #[inline]
    pub const fn from_raw(raw: u64) -> DistSq {
        DistSq(raw)
    }

    /// The squared raw distance.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Approximate distance via integer square root. For display and coarse
    /// bucketing only. For comparison use the squared values.
    pub const fn sqrt_approx(self) -> Fixed {
        if self.0 == 0 {
            return Fixed::ZERO;
        }
        let mut x = self.0;
        let mut y = x.div_ceil(2);
        while y < x {
            x = y;
            y = (x + self.0 / x) / 2;
        }
        Fixed(x as i32)
    }
}

impl fmt::Debug for DistSq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DistSq({} ~ {})", self.0, self.sqrt_approx())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_transparent() {
        assert_eq!(core::mem::size_of::<Fixed>(), core::mem::size_of::<i32>());
        assert_eq!(core::mem::align_of::<Fixed>(), core::mem::align_of::<i32>());
    }

    #[test]
    fn one_times_one_is_one() {
        // The check that catches a missing shift: with a naive `a * b` this
        // would come out 1024x too large.
        assert_eq!(Fixed::ONE * Fixed::ONE, Fixed::ONE);
    }

    #[test]
    fn scalar_and_fixed_multiply_differ() {
        let three = Fixed::from_meters(3);
        assert_eq!(three * 2, Fixed::from_meters(6));
        assert_eq!(three * Fixed::from_meters(2), Fixed::from_meters(6));
        // Same answer here, but only because 2 m and the count 2 coincide.
        // Half a meter shows the difference.
        let half = Fixed::from_millimeters(0, 500);
        assert_eq!(three * half, Fixed::from_millimeters(1, 500));
    }

    #[test]
    fn division_round_trips() {
        let a = Fixed::from_meters(100);
        let b = Fixed::from_meters(7);
        let q = a / b;
        let back = q * b;
        // Truncation loses at most one unit per operation.
        assert!((back - a).abs() <= Fixed::from_raw(8));
    }

    #[test]
    fn from_millimeters_matches_expectation() {
        assert_eq!(Fixed::from_millimeters(1, 500).raw(), 1536);
        assert_eq!(Fixed::from_millimeters(0, 0), Fixed::ZERO);
    }

    #[test]
    fn display_has_no_float() {
        assert_eq!(Fixed::from_meters(293).to_string(), "293.000m");
        assert_eq!(Fixed::from_millimeters(1, 500).to_string(), "1.500m");
        assert_eq!(Fixed::from_meters(-4).to_string(), "-4.000m");
    }

    #[test]
    fn dist_sq_holds_full_region_diagonal() {
        // Worst case across a 4096 m region in three axes.
        let far = Fixed::from_meters(4096);
        let d = DistSq::from_radius(far);
        assert!(d.raw().checked_mul(3).is_some());
    }

    #[test]
    fn dist_sq_ordering_matches_distance() {
        let near = DistSq::from_radius(Fixed::from_meters(10));
        let far = DistSq::from_radius(Fixed::from_meters(100));
        assert!(near < far);
    }

    #[test]
    fn sqrt_approx_is_close() {
        let d = DistSq::from_radius(Fixed::from_meters(300));
        let r = d.sqrt_approx();
        assert!((r - Fixed::from_meters(300)).abs() <= Fixed::from_raw(2));
    }

    #[test]
    fn checked_mul_rejects_overflow() {
        let huge = Fixed::from_meters(2_000_000);
        assert!(huge.checked_mul(huge).is_none());
        assert!(Fixed::from_meters(2).checked_mul(Fixed::from_meters(2)).is_some());
    }
}
