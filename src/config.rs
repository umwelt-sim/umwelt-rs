//! Runtime world configuration.
//!
//! [`WorldConfig`] is required of any participant exchanging world data, since
//! it carries the protocol-level configuration. Every simulation and edge
//! touching a region must hold an identical one or they decode each other's
//! packets into nonsense. Note that user-facing game clients do not need
//! this to exchange data.
//!
//! A region publishes the five authored numbers along with the digest
//! of everything they imply, and the handshake rebuilds the config
//! from the numbers and refuses it if the digests disagree. Participants
//! use [`protocol_hash`](WorldConfig::protocol_hash) for comparison.
//!
//! # Units
//!
//! Distances use [`Fixed`] point numeric representation. See [`crate::fixed`] f
//! or the representation and why it is an integer rather than `f32`. The short
//! version is that it's cheaper and faster than floats.
//!
//! Some fields must be powers of two in order to take advantage of optimizations,
//! which is what keeps the memory and wire layouts cheap to index.
//! [`WorldConfigBuilder::build`] enforces those rules and the rest,
//! and the fields are private so that a config that exists is a config that passed.
//!
//! # Axes
//!
//! Axis conventions are documented on [`crate::pos`]. Relevant here: `x` and
//! `y` share an extent, a cell size, and a wire precision. The `z` axis has its
//! own extent, cell size, and wire precision, and the horizontal
//! speed cap does not constrain it. It's eaiest to think of the world space as
//! being organized as a 2D space with vertical cylinders along the Z.
//!
//! # Serde
//!
//! Do not derive `Deserialize` on [`WorldConfig`]. A derived impl constructs
//! the struct field by field, bypassing validation entirely. Deserialize into
//! [`WorldConfigBuilder`] instead and call `build()`.

use crate::fixed::{FIXED_SHIFT, Fixed};
use crate::pos::{CellCoord, CellId, Pos2, Pos3};
use core::fmt;

/// Largest supported subscription radius in cells. Bounds the inline capacity
/// of subscription sets so they never allocate. 4 gives a 9x9 grid.
pub(crate) const MAX_CELL_RADIUS: u32 = 4;

/// Upper bound on cells in one subscription.
pub(crate) const MAX_SUB_GRID_CELLS: usize = {
    let axis = (2 * MAX_CELL_RADIUS + 1) as usize;
    axis * axis
};

// ---------------------------------------------------------------------------
// Axis
// ---------------------------------------------------------------------------

/// Which axis group an error refers to. `x` and `y` are configured together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// `x` and `y`
    Horizontal,
    /// `z`, which has its own of each and is not bound by the speed cap.
    Vertical,
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Axis::Horizontal => write!(f, "horizontal"),
            Axis::Vertical => write!(f, "vertical"),
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Contains an explanation for why a WorldConfiguration validation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A distance was zero or negative. [`Fixed`] is signed, so this has to be
    /// checked before any unsigned bit operation.
    NonPositive(&'static str),
    /// Extent must be a power of two so the wire encoding divides evenly.
    ExtentNotPowerOfTwo {
        /// Which axis group.
        axis: Axis,
        /// What was asked for.
        extent: Fixed,
    },
    /// View radius larger than the region makes subscription meaningless.
    RadiusExceedsRegion {
        /// The view radius asked for.
        radius: Fixed,
        /// The region it would have to fit inside.
        region_size: Fixed,
    },
    /// An entity able to cross more than one cell boundary per axis per tick
    /// breaks the strip-delta subscription update, which assumes at most one
    /// row and one column change per move. Horizontal movement only; vertical
    /// speed never crosses a cell boundary.
    SpeedExceedsCellPerTick {
        /// How far the speed cap allows in one tick.
        move_per_tick: Fixed,
        /// The cell it would cross.
        cell_size: Fixed,
    },
    /// Tick rate must divide evenly into a second so tick duration is exact.
    TickRateIndivisible {
        /// What was asked for.
        tick_hz: u32,
    },
    /// A required field was never set on the builder.
    Missing(&'static str),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ConfigError::*;
        match self {
            NonPositive(field) => {
                write!(f, "`{field}` must be greater than zero")
            }
            ExtentNotPowerOfTwo { axis, extent } => write!(
                f,
                "{axis} extent {extent} is not a power of two; \
                 the wire encoding would not divide evenly"
            ),
            RadiusExceedsRegion { radius, region_size } => {
                write!(f, "view radius {radius} exceeds region size {region_size}")
            }
            SpeedExceedsCellPerTick { move_per_tick, cell_size } => write!(
                f,
                "an entity moves {move_per_tick} per tick but cells are {cell_size}; \
                 it could skip a cell, invalidating strip-delta subscription updates"
            ),
            TickRateIndivisible { tick_hz } => {
                write!(f, "tick rate {tick_hz} Hz does not divide evenly into 1000 ms")
            }
            Missing(field) => write!(f, "required field `{field}` was not set"),
        }
    }
}

impl core::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// WorldConfig
// ---------------------------------------------------------------------------

/// Configuration of a world simulation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldConfig {
    // Authored.
    region_size: Fixed,
    vertical_extent: Fixed,
    cell_size: Fixed,
    horizontal_view_radius: Fixed,
    max_horizontal_speed: Fixed,
    tick_hz: u32,
    horizontal_bits: u32,
    vertical_bits: u32,

    // Derived.
    cell_shift: u32,
    cell_mask: i32,
    cells_per_axis: u32,
    cells_per_region: u32,
    cell_radius: u32,
    sub_grid_axis: u32,
    sub_grid_cells: u32,
    tick_ms: u32,
    max_move_per_tick: Fixed,
    horizontal_quant_shift: u32,
    horizontal_precision: Fixed,
    vertical_quant_shift: u32,
    vertical_precision: Fixed,
}

impl WorldConfig {
    /// Starts building a world configuration. 
    /// Every field has a default and [`WorldConfigBuilder::build`] validates the result.
    pub fn builder() -> WorldConfigBuilder {
        WorldConfigBuilder::default()
    }

    // -- authored ---------------------------------------------------------

    /// Region edge length on both horizontal axes.
    pub const fn region_size(&self) -> Fixed {
        self.region_size
    }
    /// Vertical range. Positions are expected within `0..vertical_extent`.
    pub const fn vertical_extent(&self) -> Fixed {
        self.vertical_extent
    }
    /// Cell edge length. Always a power of two.
    pub const fn cell_size(&self) -> Fixed {
        self.cell_size
    }
    /// How far a viewer perceives horizontally. Area of interest is a
    /// cylinder: there is no vertical bound.
    pub const fn horizontal_view_radius(&self) -> Fixed {
        self.horizontal_view_radius
    }
    /// Horizontal speed cap. Entities exceeding this break the strip-delta
    /// invariant.
    pub const fn max_horizontal_speed(&self) -> Fixed {
        self.max_horizontal_speed
    }
    /// Ticks per second when the server is running and managing tick delivery.
    pub const fn tick_hz(&self) -> u32 {
        self.tick_hz
    }
    /// Bits per horizontal axis on the wire.
    pub const fn horizontal_bits(&self) -> u32 {
        self.horizontal_bits
    }
    /// Bits for the vertical axis on the wire.
    pub const fn vertical_bits(&self) -> u32 {
        self.vertical_bits
    }

    // -- derived ----------------------------------------------------------

    /// `log2(cell_size)`. Shifting a coordinate right by this yields its cell
    /// index along that axis, replacing a division.
    pub const fn cell_shift(&self) -> u32 {
        self.cell_shift
    }
    /// `cell_size - 1`. AND a raw coordinate with this to get its offset
    /// within the cell.
    pub const fn cell_mask(&self) -> i32 {
        self.cell_mask
    }
    /// Cells along one horizontal axis.
    pub const fn cells_per_axis(&self) -> u32 {
        self.cells_per_axis
    }
    /// Cells in the whole region, which is the square of the axis count.
    pub const fn cells_per_region(&self) -> u32 {
        self.cells_per_region
    }
    /// Cells from center to edge of a subscription.
    pub const fn cell_radius(&self) -> u32 {
        self.cell_radius
    }
    /// Subscription grid edge in cells: `2 * cell_radius + 1`.
    pub const fn sub_grid_axis(&self) -> u32 {
        self.sub_grid_axis
    }
    /// Total cells in one subscription. Never exceeds 81, the 9x9 grid a
    /// four-cell radius implies.
    pub const fn sub_grid_cells(&self) -> u32 {
        self.sub_grid_cells
    }
    /// Milliseconds between ticks.
    pub const fn tick_ms(&self) -> u32 {
        self.tick_ms
    }
    /// Furthest an entity can travel horizontally in one tick. Guaranteed
    /// less than `cell_size`. This doesn't take into account exceptions
    /// like teleporting/warping.
    pub const fn max_move_per_tick(&self) -> Fixed {
        self.max_move_per_tick
    }
    /// Bits dropped from a horizontal coordinate before it goes on the wire.
    pub const fn horizontal_quant_shift(&self) -> u32 {
        self.horizontal_quant_shift
    }
    /// Worst-case horizontal wire error.
    ///
    /// May be finer than requested: bit counts are whole numbers, so the
    /// derived width rounds in your favor.
    pub const fn horizontal_precision(&self) -> Fixed {
        self.horizontal_precision
    }
    /// Bits dropped from a `z` value on the way to the wire.
    pub const fn vertical_quant_shift(&self) -> u32 {
        self.vertical_quant_shift
    }
    /// Worst-case vertical wire error.
    pub const fn vertical_precision(&self) -> Fixed {
        self.vertical_precision
    }

    // -- cells ------------------------------------------------------------

    /// Cell containing a horizontal position.
    ///
    /// Requires `pos` to be within the region; check with [`Self::contains_2d`]
    /// if the caller cannot guarantee it. Out-of-range input is clamped in
    /// release builds and asserted in debug.
    #[inline]
    pub fn cell_of(&self, pos: Pos2) -> CellCoord {
        debug_assert!(
            self.contains_2d(pos),
            "cell_of called with out-of-region position {pos:?}"
        );
        CellCoord::new(self.axis_to_cell(pos.x), self.axis_to_cell(pos.y))
    }

    #[inline]
    fn axis_to_cell(&self, v: Fixed) -> u16 {
        let raw = v.raw().max(0) as u32;
        let idx = raw >> self.cell_shift;
        idx.min(self.cells_per_axis - 1) as u16
    }

    /// Offset of a coordinate within its own cell.
    #[inline]
    pub const fn offset_in_cell(&self, v: Fixed) -> Fixed {
        Fixed::from_raw(v.raw() & self.cell_mask)
    }

    /// Linear index for a cell, for array lookup.
    ///
    /// Row-major today. If cells are later ordered along a Hilbert curve for
    /// cross-node partitioning only this function changes, so treat [`CellId`]
    /// as opaque and do not assume `y * width + x` anywhere else.
    #[inline]
    pub const fn cell_id(&self, c: CellCoord) -> CellId {
        CellId::from_raw(c.y as u32 * self.cells_per_axis + c.x as u32)
    }

    /// Inverse of [`Self::cell_id`].
    #[inline]
    pub const fn cell_coord(&self, id: CellId) -> CellCoord {
        let raw = id.raw();
        CellCoord::new(
            (raw % self.cells_per_axis) as u16,
            (raw / self.cells_per_axis) as u16,
        )
    }

    /// Whether a horizontal position lies inside the region.
    #[inline]
    pub const fn contains_2d(&self, pos: Pos2) -> bool {
        pos.x.raw() >= 0
            && pos.y.raw() >= 0
            && pos.x.raw() < self.region_size.raw()
            && pos.y.raw() < self.region_size.raw()
    }

    /// Whether a full position lies inside the region and vertical range.
    #[inline]
    pub const fn contains(&self, pos: Pos3) -> bool {
        self.contains_2d(pos.horizontal())
            && pos.z.raw() >= 0
            && pos.z.raw() < self.vertical_extent.raw()
    }

    // -- wire -------------------------------------------------------------

    /// An `x` or `y` value as it travels.
    #[inline]
    pub const fn quantize_horizontal(&self, v: Fixed) -> u32 {
        (v.raw() as u32) >> self.horizontal_quant_shift
    }

    /// Back from the wire, to within the horizontal precision.
    #[inline]
    pub const fn dequantize_horizontal(&self, wire: u32) -> Fixed {
        Fixed::from_raw((wire << self.horizontal_quant_shift) as i32)
    }

    /// A `z` value as it travels.
    #[inline]
    pub const fn quantize_vertical(&self, v: Fixed) -> u32 {
        (v.raw() as u32) >> self.vertical_quant_shift
    }

    /// Back from the wire, to within the vertical precision.
    #[inline]
    pub const fn dequantize_vertical(&self, wire: u32) -> Fixed {
        Fixed::from_raw((wire << self.vertical_quant_shift) as i32)
    }

    /// Lower precision for a full position for the wire, as `(x, y, z)`.
    #[inline]
    pub const fn quantize_pos(&self, pos: Pos3) -> (u32, u32, u32) {
        (
            self.quantize_horizontal(pos.x),
            self.quantize_horizontal(pos.y),
            self.quantize_vertical(pos.z),
        )
    }

    /// Inverse of [`Self::quantize_pos`], landing at the low edge of each
    /// precision step.
    #[inline]
    pub const fn dequantize_pos(&self, x: u32, y: u32, z: u32) -> Pos3 {
        Pos3::new(
            self.dequantize_horizontal(x),
            self.dequantize_horizontal(y),
            self.dequantize_vertical(z),
        )
    }

    /// True when horizontal positions go on the wire at full internal
    /// precision. Legal, and sometimes wanted, but it means paying full
    /// precision on every packet.
    pub const fn is_lossless_horizontal(&self) -> bool {
        self.horizontal_quant_shift == 0
    }

    /// Distinct wire positions across one cell. A value of 1 means the wire
    /// cannot distinguish positions within a cell at all.
    ///
    /// Whether a given value is acceptable depends on client rendering and
    /// interpolation, which this crate cannot see.
    pub const fn wire_steps_per_cell(&self) -> u32 {
        (self.cell_size.raw() / self.horizontal_precision.raw()) as u32
    }

    // -- misc -------------------------------------------------------------

    /// Stable digest of the fields that affect wire decoding. Exchanged at
    /// connect time and reject on mismatch. This digest only covers the
    /// configuration values that are required for protocol setup and
    /// exchange over the wire. All other configuration values can change
    /// without invalidating communications.
    pub const fn protocol_hash(&self) -> u64 {
        // FNV-1a 64-bit offset, required as hash starting point
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        h = fnv(h, FIXED_SHIFT);
        h = fnv(h, self.region_size.raw() as u32);
        h = fnv(h, self.vertical_extent.raw() as u32);
        h = fnv(h, self.cell_size.raw() as u32);
        h = fnv(h, self.horizontal_view_radius.raw() as u32);
        h = fnv(h, self.horizontal_bits);
        h = fnv(h, self.vertical_bits);
        h
    }

    /// A copy of the world configuration with `cell_size` overridden, 
    /// for benchmarking the spatial index at sizes the derivation would not pick.
    ///
    /// Not part of the normal path. Cell size derives from the view radius and
    /// a consumer has no reason to set it; this exists so the cell-size sweep
    /// can be re-run.
    ///
    /// # Panics
    ///
    /// If the size is not positive, not a power of two in raw units, does not
    /// divide the region evenly, produces a cell radius above four, or is small
    /// enough that an entity at `max_horizontal_speed` crosses more than one
    /// boundary per tick.
    pub fn with_cell_size_m(&self, m: i32) -> WorldConfig {
        let cell_size = Fixed::from_meters(m);
        let cell_raw = cell_size.raw() as u32;
        let region_raw = self.region_size.raw() as u32;

        assert!(cell_size.raw() > 0, "cell size must be positive");
        assert!(
            cell_raw.is_power_of_two(),
            "cell size {cell_size} is not a power of two"
        );
        assert!(
            region_raw % cell_raw == 0,
            "cell size {cell_size} does not divide region {}",
            self.region_size
        );
        let cell_radius = (self.horizontal_view_radius.raw() as u32).div_ceil(cell_raw);
        assert!(
            cell_radius <= MAX_CELL_RADIUS,
            "cell size {cell_size} gives cell radius {cell_radius}, above {MAX_CELL_RADIUS}"
        );
        assert!(
            (self.max_move_per_tick.raw() as u32) < cell_raw,
            "an entity moving {} per tick crosses a {cell_size} cell",
            self.max_move_per_tick
        );

        WorldConfig {
            cell_size,
            cell_shift: cell_raw.trailing_zeros(),
            cell_mask: (cell_raw - 1) as i32,
            cells_per_axis: region_raw / cell_raw,
            cells_per_region: (region_raw / cell_raw) * (region_raw / cell_raw),
            cell_radius,
            sub_grid_axis: 2 * cell_radius + 1,
            sub_grid_cells: (2 * cell_radius + 1) * (2 * cell_radius + 1),
            ..*self
        }
    }

    /// Uniform-distribution estimate of entities in one viewer's subscription.
    /// A planning aid, not a runtime quantity.
    pub const fn est_entities_in_view(&self, entities_per_region: u32) -> u32 {
        let per_cell = entities_per_region / self.cells_per_region;
        per_cell * self.sub_grid_cells
    }
}

/// FNV-1a over four bytes. Chosen because it is a `const fn`, is stable across
/// Rust versions and platforms unlike `DefaultHasher`, and carries no
/// dependency. Not cryptographic strength, only used to catch configuration
/// mismatch.
const fn fnv(mut h: u64, v: u32) -> u64 {
    let bytes = v.to_le_bytes();
    let mut i = 0;
    while i < 4 {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

impl Default for WorldConfig {
    /// 4096 m region, 1024 m vertical, 256 m view radius, 40 m/s cap, 20 Hz.
    /// Cell size derives to 128 m and wire precision is lossless.    
    fn default() -> Self {
        Self::builder()
            .region_size_m(4096)
            .vertical_extent_m(1024)
            .horizontal_view_radius_m(256)
            .max_horizontal_speed_m_per_sec(40)
            .tick_hz(20)
            .build()
            .expect("default world config is valid")
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// The validating builder used to create checked instances of world configuration.
/// Never create world configurations outside the builder except for internal testing.
#[derive(Debug, Clone, Default)]
pub struct WorldConfigBuilder {
    region_size: Option<Fixed>,
    vertical_extent: Option<Fixed>,
    horizontal_view_radius: Option<Fixed>,
    max_horizontal_speed: Option<Fixed>,
    tick_hz: Option<u32>,
}

impl WorldConfigBuilder {
    /// Region edge length in whole meters. Must be a power of two in fixed
    /// point, which for whole meters means a power of two in meters.
    pub fn region_size_m(mut self, m: i32) -> Self {
        self.region_size = Some(Fixed::from_meters(m));
        self
    }

    /// Vertical range in whole meters. Also power-of-two constrained.
    pub fn vertical_extent_m(mut self, m: i32) -> Self {
        self.vertical_extent = Some(Fixed::from_meters(m));
        self
    }

    /// How far an observer sees, in meters. Rounds up to whole cells.
    pub fn horizontal_view_radius_m(mut self, m: i32) -> Self {
        self.horizontal_view_radius = Some(Fixed::from_meters(m));
        self
    }

    /// The fastest anything may travel horizontally, in meters per second.
    ///
    /// Bounds how far an entity moves in one tick, which is what lets a
    /// subscription be rebuilt from a cell rather than from scratch.
    pub fn max_horizontal_speed_m_per_sec(mut self, m: i32) -> Self {
        self.max_horizontal_speed = Some(Fixed::from_meters(m));
        self
    }

    /// Ticks per second. Must be a power of two.
    pub fn tick_hz(mut self, hz: u32) -> Self {
        self.tick_hz = Some(hz);
        self
    }

    /// Validate and compute all derived values.
    pub fn build(self) -> Result<WorldConfig, ConfigError> {
        use ConfigError::*;

        let region_size = self.region_size.ok_or(Missing("region_size"))?;
        let vertical_extent = self.vertical_extent.ok_or(Missing("vertical_extent"))?;
        let horizontal_view_radius =
            self.horizontal_view_radius.ok_or(Missing("horizontal_view_radius"))?;
        let max_horizontal_speed =
            self.max_horizontal_speed.ok_or(Missing("max_horizontal_speed"))?;
        let tick_hz = self.tick_hz.ok_or(Missing("tick_hz"))?;

        // Fixed is signed, so positivity has to be established before any
        // unsigned bit operation below.
        positive(region_size, "region_size")?;
        positive(vertical_extent, "vertical_extent")?;
        positive(horizontal_view_radius, "horizontal_view_radius")?;
        positive(max_horizontal_speed, "max_horizontal_speed")?;

        let region_raw = region_size.raw() as u32;
        let vertical_raw = vertical_extent.raw() as u32;

        // Cell size is derived, not supplied.
        let cell_raw = 1u32 << ((horizontal_view_radius.raw() as u32 / 2).max(1).ilog2());
        let cell_size = Fixed::from_raw(cell_raw as i32);

        if !region_raw.is_power_of_two() {
            return Err(ExtentNotPowerOfTwo {
                axis: Axis::Horizontal,
                extent: region_size,
            });
        }
        if !vertical_raw.is_power_of_two() {
            return Err(ExtentNotPowerOfTwo {
                axis: Axis::Vertical,
                extent: vertical_extent,
            });
        }
        if horizontal_view_radius.raw() > region_size.raw() {
            return Err(RadiusExceedsRegion {
                radius: horizontal_view_radius,
                region_size,
            });
        }
        if tick_hz == 0 || 1_000 % tick_hz != 0 {
            return Err(TickRateIndivisible { tick_hz });
        }

        let cell_radius = (horizontal_view_radius.raw() as u32).div_ceil(cell_raw);
        debug_assert!(
            cell_radius <= MAX_CELL_RADIUS,
            "derived cell size broke the radius bound"
        );

        let max_move_per_tick =
            Fixed::from_raw(max_horizontal_speed.raw() / tick_hz as i32);
        if max_move_per_tick.raw() as u32 >= cell_raw {
            return Err(SpeedExceedsCellPerTick {
                move_per_tick: max_move_per_tick,
                cell_size,
            });
        }

        // Wire widths. Precision is one raw unit (1/1024 m) so a position
        // keeps every bit the simulation computed with and the precision-reduction
        // shift is zero. 
        //
        // Coarser precision would save bytes and round off motion: anything
        // moving less than one step per tick would stand still and then jump.
        // That threshold is `precision * tick_hz`, which at 1/16 m and 20 Hz is
        // 1.25 m/s, which is inside the range of a walking human.
        let horizontal_bits = region_raw.trailing_zeros();
        let horizontal_quant_shift = 0;
        let horizontal_precision = Fixed::from_raw(1);
        let vertical_bits = vertical_raw.trailing_zeros();
        let vertical_quant_shift = 0;
        let vertical_precision = Fixed::from_raw(1);

        let cells_per_axis = region_raw / cell_raw;
        let sub_grid_axis = 2 * cell_radius + 1;

        Ok(WorldConfig {
            region_size,
            vertical_extent,
            cell_size,
            horizontal_view_radius,
            max_horizontal_speed,
            tick_hz,
            horizontal_bits,
            vertical_bits,

            cell_shift: cell_raw.trailing_zeros(),
            cell_mask: cell_size.raw() - 1,
            cells_per_axis,
            cells_per_region: cells_per_axis * cells_per_axis,
            cell_radius,
            sub_grid_axis,
            sub_grid_cells: sub_grid_axis * sub_grid_axis,
            tick_ms: 1_000 / tick_hz,
            max_move_per_tick,
            horizontal_quant_shift,
            horizontal_precision,
            vertical_quant_shift,
            vertical_precision,
        })
    }
}

fn positive(v: Fixed, name: &'static str) -> Result<(), ConfigError> {
    if v.raw() <= 0 { Err(ConfigError::NonPositive(name)) } else { Ok(()) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WorldConfigBuilder {
        WorldConfig::builder()
            .region_size_m(4096)
            .vertical_extent_m(1024)
            .horizontal_view_radius_m(256)
            .max_horizontal_speed_m_per_sec(40)
            .tick_hz(20)
    }

    #[test]
    fn default_matches_hand_computed_values() {
        let w = WorldConfig::default();
        assert_eq!(w.cell_shift(), 17);
        assert_eq!(w.cells_per_axis(), 32);
        assert_eq!(w.cells_per_region(), 1024);
        assert_eq!(w.cell_radius(), 2);
        assert_eq!(w.sub_grid_axis(), 5);
        assert_eq!(w.sub_grid_cells(), 25);
        assert_eq!(w.tick_ms(), 50);
        assert_eq!(w.max_move_per_tick(), Fixed::from_meters(2));
        assert_eq!(w.horizontal_bits(), 22);
        assert_eq!(w.horizontal_quant_shift(), 0);
        assert_eq!(w.horizontal_precision(), Fixed::from_raw(1));
        assert_eq!(w.vertical_bits(), 20);
        assert_eq!(w.vertical_quant_shift(), 0);
        assert_eq!(w.wire_steps_per_cell(), 131_072);
        assert!(w.is_lossless_horizontal());
    }

    #[test]
    fn cell_lookup_agrees_with_division() {
        let w = WorldConfig::default();
        for m in 0..4096i32 {
            let p = Pos2::from_meters(m, m);
            let expected = (Fixed::from_meters(m).raw() / w.cell_size().raw()) as u16;
            assert_eq!(w.cell_of(p).x, expected);
        }
    }

    #[test]
    fn offset_in_cell_agrees_with_remainder() {
        let w = WorldConfig::default();
        for m in 0..4096i32 {
            let v = Fixed::from_meters(m);
            assert_eq!(w.offset_in_cell(v).raw(), v.raw() % w.cell_size().raw());
        }
    }

    #[test]
    fn cell_id_round_trips() {
        let w = WorldConfig::default();
        for y in 0..w.cells_per_axis() as u16 {
            for x in 0..w.cells_per_axis() as u16 {
                let c = CellCoord::new(x, y);
                assert_eq!(w.cell_coord(w.cell_id(c)), c);
            }
        }
    }

    #[test]
    fn precision_error_is_bounded_on_both_axes() {
        let w = WorldConfig::default();
        for m in 0..1024i32 {
            let p = Pos3::new(
                Fixed::from_meters(m) + Fixed::from_raw(511),
                Fixed::from_meters(m),
                Fixed::from_meters(m) + Fixed::from_raw(511),
            );
            let (qx, qy, qz) = w.quantize_pos(p);
            let back = w.dequantize_pos(qx, qy, qz);
            assert!((p.x - back.x).abs() < w.horizontal_precision());
            assert!((p.z - back.z).abs() < w.vertical_precision());
            assert_eq!(p.y, back.y);
        }
    }

    #[test]
    fn contains_rejects_out_of_range() {
        let w = WorldConfig::default();
        assert!(w.contains(Pos3::from_meters(0, 0, 0)));
        assert!(w.contains(Pos3::from_meters(4095, 4095, 1023)));
        assert!(!w.contains(Pos3::from_meters(4096, 0, 0)));
        assert!(!w.contains(Pos3::from_meters(0, 0, 1024)));
        assert!(!w.contains(Pos3::from_meters(-1, 0, 0)));
    }

    #[test]
    fn rejects_non_positive_extent() {
        let err = base().region_size_m(0).build().unwrap_err();
        assert!(matches!(err, ConfigError::NonPositive("region_size")));
    }

    #[test]
    fn cell_size_derives_to_half_the_view_radius() {
        let w = WorldConfig::default();
        assert_eq!(w.cell_size(), Fixed::from_meters(128));
        assert_eq!(w.horizontal_view_radius(), Fixed::from_meters(256));
        assert_eq!(w.cell_radius(), 2);
    }

    #[test]
    fn derived_cell_size_floors_to_a_power_of_two() {
        // 300 m radius halves to 150 m, which floors to 128 m.
        let w = base().horizontal_view_radius_m(300).build().unwrap();
        assert_eq!(w.cell_size(), Fixed::from_meters(128));
        assert_eq!(w.cell_radius(), 3);
    }

    #[test]
    fn derived_cell_size_never_breaks_the_radius_bound() {
        for r in 4..=2048 {
            let w = base().horizontal_view_radius_m(r).build();
            if let Ok(w) = w {
                assert!(
                    w.cell_radius() <= MAX_CELL_RADIUS,
                    "radius {r} m gave cell radius {}",
                    w.cell_radius()
                );
            }
        }
    }

    #[test]
    fn cell_size_override_recomputes_the_grid() {
        let w = WorldConfig::default().with_cell_size_m(64);
        assert_eq!(w.cell_size(), Fixed::from_meters(64));
        assert_eq!(w.cells_per_axis(), 64);
        assert_eq!(w.cells_per_region(), 4096);
        assert_eq!(w.cell_radius(), 4);
        assert_eq!(w.sub_grid_axis(), 9);
        // Wire widths are untouched, but cell size is protocol-critical: a
        // client decoding a CellCoord has to agree on what a cell is.
        assert_eq!(w.horizontal_bits(), WorldConfig::default().horizontal_bits());
        assert_ne!(w.protocol_hash(), WorldConfig::default().protocol_hash());
    }

    #[test]
    #[should_panic(expected = "not a power of two")]
    fn cell_size_override_rejects_non_power_of_two() {
        WorldConfig::default().with_cell_size_m(100);
    }

    #[test]
    #[should_panic(expected = "above")]
    fn cell_size_override_rejects_too_many_cells_in_view() {
        // 32 m cells against a 256 m radius needs a radius of 8.
        WorldConfig::default().with_cell_size_m(32);
    }

    #[test]
    fn rejects_entity_that_can_skip_a_cell() {
        // A 2 m view radius derives a 1 m cell, and 40 m/s at 20 Hz moves 2 m
        // per tick.
        let err = base().horizontal_view_radius_m(2).build().unwrap_err();
        assert!(matches!(err, ConfigError::SpeedExceedsCellPerTick { .. }));
    }

    #[test]
    fn protocol_hash_ignores_simulation_only_fields() {
        let a = base().build().unwrap();
        let b = base().max_horizontal_speed_m_per_sec(30).build().unwrap();
        assert_eq!(a.protocol_hash(), b.protocol_hash());
    }

    #[test]
    fn protocol_hash_catches_wire_layout_change() {
        // Bits per axis derive from region extent, so this is the only way to
        // change them.
        let a = base().build().unwrap();
        let b = base().region_size_m(2048).build().unwrap();
        assert_ne!(a.horizontal_bits(), b.horizontal_bits());
        assert_ne!(a.protocol_hash(), b.protocol_hash());
    }

    #[test]
    fn protocol_hash_catches_vertical_change() {
        let a = base().build().unwrap();
        let b = base().vertical_extent_m(512).build().unwrap();
        assert_ne!(a.protocol_hash(), b.protocol_hash());
    }
}
