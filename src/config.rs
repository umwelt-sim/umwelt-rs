//! Runtime world configuration.
//!
//! There are two different configuration types with different use cases:
//!
//! - [`WorldConfig`] is **protocol-critical**. Every simulation, edge, and
//!   client touching a region must hold identical values or they will decode
//!   each other's packets into garbage. Compare [`WorldConfig::protocol_hash`]
//!   at connect time and reject on mismatch.
//!
//! - [`ViewConfig`] is **local policy**. It can differ per node and per
//!   client. A phone on cellular data gets a smaller budget than a desktop on
//!   fiber. No two view configs ever need to agree with each other.
//!
//! # Units
//!
//! Distances are [`Fixed`]; see [`crate::fixed`] for the representation and
//! why it is an integer rather than `f32`.
//!
//! Every derived value is derived using integer arithmetic,
//! so nothing needs `sqrt` or `ceil`, which are not available in `const fn`.
//! Additionally, several fields must be powers of two because
//! dividing by a runtime value is far slower than shifting by one.
//! [`WorldConfigBuilder::build`] enforces these and other rules so you should
//! never manually create an instance of [`WorldConfig`] unless you're writing
//! tests for this module.
//!
//! # Axes
//!
//! Axis conventions are documented on [`crate::pos`]. Relevant here: `x` and
//! `y` share an extent, a cell size, and a wire precision. The `z` axis has its
//! own extent, cell size, and wire precision, and the horizontal
//! speed cap does not constrain it.
//!
//! # Serde
//!
//! Do not derive `Deserialize` on [`WorldConfig`]. A derived impl constructs
//! the struct field by field, bypassing validation entirely. Deserialise into
//! [`WorldConfigBuilder`] instead and call `build()`.

use crate::fixed::{FIXED_SHIFT, Fixed};
use crate::pos::{CellCoord, CellId, Pos2, Pos3};
use core::fmt;

/// Largest supported subscription radius in cells. Bounds the inline capacity
/// of subscription sets so they never allocate. 4 gives a 9x9 grid.
pub const MAX_CELL_RADIUS: u32 = 4;

/// Upper bound on cells in one subscription.
pub const MAX_SUB_GRID_CELLS: usize = {
    let axis = (2 * MAX_CELL_RADIUS + 1) as usize;
    axis * axis
};

/// Default worst-case wire error bar per axis: 64 raw units, or 1/16 m.
///
/// A starting point, not a derivation. Whether it is acceptable depends on
/// the client's rendering and interpolation, which this crate cannot see.
pub const DEFAULT_PRECISION: Fixed = Fixed::from_raw(64);

// ---------------------------------------------------------------------------
// Axis
// ---------------------------------------------------------------------------

/// Which axis group an error refers to. `x` and `y` are configured together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A distance was zero or negative. [`Fixed`] is signed, so this has to be
    /// checked before any unsigned bit operation.
    NonPositive(&'static str),
    /// Cell size must be a power of two so `pos >> cell_shift` replaces a
    /// division. With a runtime cell size the compiler cannot do this for us.
    CellSizeNotPowerOfTwo { cell_size: Fixed },
    /// Extent must be a power of two so wire quantisation divides evenly.
    ExtentNotPowerOfTwo { axis: Axis, extent: Fixed },
    /// A region must contain a whole number of cells.
    RegionNotWholeCells {
        region_size: Fixed,
        cell_size: Fixed,
    },
    /// View radius larger than the region makes subscription meaningless.
    RadiusExceedsRegion { radius: Fixed, region_size: Fixed },
    /// Subscription radius beyond [`MAX_CELL_RADIUS`]. Usually means the cell
    /// size is far too small relative to the view radius.
    CellRadiusTooLarge { cell_radius: u32, max: u32 },
    /// An entity able to cross more than one cell boundary per axis per tick
    /// breaks the strip-delta subscription update, which assumes at most one
    /// row and one column change per move. Horizontal movement only; vertical
    /// speed never crosses a cell boundary.
    SpeedExceedsCellPerTick {
        move_per_tick: Fixed,
        cell_size: Fixed,
    },
    /// Tick rate must divide evenly into a second so tick duration is exact.
    TickRateIndivisible { tick_hz: u32 },
    /// Wire bits must be positive and no wider than the axis extent.
    BadPositionBits { axis: Axis, bits: u32, width: u32 },
    /// Bits and precision were both supplied for the same axis. They specify
    /// the same thing from opposite directions; give one.
    ConflictingPositionSpec { axis: Axis },
    /// Requested precision was zero or negative. There is no such bit count.
    BadPrecision { axis: Axis, precision: Fixed },
    /// Requested precision is coarser than the axis extent, leaving no bits.
    PrecisionExceedsExtent {
        axis: Axis,
        precision: Fixed,
        extent: Fixed,
    },
    /// Wire quantisation is coarser than a cell, so a decoded position can
    /// land in a different cell than the true one. Any client doing spatial
    /// reasoning on received positions would disagree with the server.
    QuantErrorExceedsCell { precision: Fixed, cell_size: Fixed },
    /// Header and event reserve have consumed the whole packet.
    BudgetExhausted {
        payload: u32,
        header: u32,
        reserve: u32,
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
            CellSizeNotPowerOfTwo { cell_size } => write!(
                f,
                "cell size {cell_size} is not a power of two; \
                 cell lookup would need a runtime division"
            ),
            ExtentNotPowerOfTwo { axis, extent } => write!(
                f,
                "{axis} extent {extent} is not a power of two; \
                 wire quantisation would not divide evenly"
            ),
            RegionNotWholeCells {
                region_size,
                cell_size,
            } => write!(
                f,
                "region size {region_size} is not a whole multiple of cell size {cell_size}"
            ),
            RadiusExceedsRegion {
                radius,
                region_size,
            } => write!(f, "view radius {radius} exceeds region size {region_size}"),
            CellRadiusTooLarge { cell_radius, max } => write!(
                f,
                "subscription radius of {cell_radius} cells exceeds the maximum of {max}; \
                 cell size is probably too small for the view radius"
            ),
            SpeedExceedsCellPerTick {
                move_per_tick,
                cell_size,
            } => write!(
                f,
                "an entity moves {move_per_tick} per tick but cells are {cell_size}; \
                 it could skip a cell, invalidating strip-delta subscription updates"
            ),
            TickRateIndivisible { tick_hz } => {
                write!(
                    f,
                    "tick rate {tick_hz} Hz does not divide evenly into 1000 ms"
                )
            }
            BadPositionBits { axis, bits, width } => {
                write!(f, "{axis} wire bits {bits} must be in 1..={width}")
            }
            ConflictingPositionSpec { axis } => write!(
                f,
                "both bits and precision were set for the {axis} axis; supply one"
            ),
            BadPrecision { axis, precision } => {
                write!(f, "{axis} precision {precision} must be greater than zero")
            }
            PrecisionExceedsExtent {
                axis,
                precision,
                extent,
            } => write!(
                f,
                "{axis} precision {precision} is coarser than the extent {extent}"
            ),
            QuantErrorExceedsCell {
                precision,
                cell_size,
            } => write!(
                f,
                "wire precision {precision} is coarser than cell size {cell_size}; \
                 a decoded position could land in the wrong cell"
            ),
            BudgetExhausted {
                payload,
                header,
                reserve,
            } => write!(
                f,
                "payload {payload} leaves nothing for state after header {header} \
                 and event reserve {reserve}"
            ),
            Missing(field) => write!(f, "required field `{field}` was not set"),
        }
    }
}

impl core::error::Error for ConfigError {}

// ---------------------------------------------------------------------------
// WorldConfig
// ---------------------------------------------------------------------------

/// Shared, protocol-critical world description.
///
/// All fields are private and there is no public literal constructor, so
/// [`WorldConfigBuilder::build`] is the only way in. Derived values are
/// computed once during validation and then read as fields.
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
    /// invariant. Vertical speed is unconstrained.
    pub const fn max_horizontal_speed(&self) -> Fixed {
        self.max_horizontal_speed
    }
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
    pub const fn cells_per_axis(&self) -> u32 {
        self.cells_per_axis
    }
    pub const fn cells_per_region(&self) -> u32 {
        self.cells_per_region
    }
    /// Cells from centre to edge of a subscription.
    pub const fn cell_radius(&self) -> u32 {
        self.cell_radius
    }
    /// Subscription grid edge in cells: `2 * cell_radius + 1`.
    pub const fn sub_grid_axis(&self) -> u32 {
        self.sub_grid_axis
    }
    /// Total cells in one subscription. Never exceeds [`MAX_SUB_GRID_CELLS`].
    pub const fn sub_grid_cells(&self) -> u32 {
        self.sub_grid_cells
    }
    pub const fn tick_ms(&self) -> u32 {
        self.tick_ms
    }
    /// Furthest an entity can travel horizontally in one tick. Guaranteed
    /// less than `cell_size`.
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
    /// derived width rounds in your favour.
    pub const fn horizontal_precision(&self) -> Fixed {
        self.horizontal_precision
    }
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
    #[inline(always)]
    pub fn cell_of(&self, pos: Pos2) -> CellCoord {
        debug_assert!(
            self.contains_2d(pos),
            "cell_of called with out-of-region position {pos:?}"
        );
        CellCoord::new(self.axis_to_cell(pos.x), self.axis_to_cell(pos.y))
    }

    #[inline(always)]
    fn axis_to_cell(&self, v: Fixed) -> u16 {
        let raw = v.raw().max(0) as u32;
        let idx = raw >> self.cell_shift;
        idx.min(self.cells_per_axis - 1) as u16
    }

    /// Offset of a coordinate within its own cell.
    #[inline(always)]
    pub const fn offset_in_cell(&self, v: Fixed) -> Fixed {
        Fixed::from_raw(v.raw() & self.cell_mask)
    }

    /// Linear index for a cell, for array lookup.
    ///
    /// Row-major today. If cells are later ordered along a Hilbert curve for
    /// cross-node partitioning only this function changes, so treat [`CellId`]
    /// as opaque and do not assume `y * width + x` anywhere else.
    #[inline(always)]
    pub const fn cell_id(&self, c: CellCoord) -> CellId {
        CellId::from_raw(c.y as u32 * self.cells_per_axis + c.x as u32)
    }

    /// Inverse of [`Self::cell_id`].
    #[inline(always)]
    pub const fn cell_coord(&self, id: CellId) -> CellCoord {
        let raw = id.raw();
        CellCoord::new(
            (raw % self.cells_per_axis) as u16,
            (raw / self.cells_per_axis) as u16,
        )
    }

    /// Whether a horizontal position lies inside the region.
    #[inline(always)]
    pub const fn contains_2d(&self, pos: Pos2) -> bool {
        pos.x.raw() >= 0
            && pos.y.raw() >= 0
            && pos.x.raw() < self.region_size.raw()
            && pos.y.raw() < self.region_size.raw()
    }

    /// Whether a full position lies inside the region and vertical range.
    #[inline(always)]
    pub const fn contains(&self, pos: Pos3) -> bool {
        self.contains_2d(pos.horizontal())
            && pos.z.raw() >= 0
            && pos.z.raw() < self.vertical_extent.raw()
    }

    // -- wire -------------------------------------------------------------

    #[inline(always)]
    pub const fn quantise_horizontal(&self, v: Fixed) -> u32 {
        (v.raw() as u32) >> self.horizontal_quant_shift
    }

    #[inline(always)]
    pub const fn dequantise_horizontal(&self, wire: u32) -> Fixed {
        Fixed::from_raw((wire << self.horizontal_quant_shift) as i32)
    }

    #[inline(always)]
    pub const fn quantise_vertical(&self, v: Fixed) -> u32 {
        (v.raw() as u32) >> self.vertical_quant_shift
    }

    #[inline(always)]
    pub const fn dequantise_vertical(&self, wire: u32) -> Fixed {
        Fixed::from_raw((wire << self.vertical_quant_shift) as i32)
    }

    /// Quantise a full position for the wire, as `(x, y, z)`.
    #[inline(always)]
    pub const fn quantise_pos(&self, pos: Pos3) -> (u32, u32, u32) {
        (
            self.quantise_horizontal(pos.x),
            self.quantise_horizontal(pos.y),
            self.quantise_vertical(pos.z),
        )
    }

    /// Inverse of [`Self::quantise_pos`], landing at the low edge of each
    /// quantisation bucket.
    #[inline(always)]
    pub const fn dequantise_pos(&self, x: u32, y: u32, z: u32) -> Pos3 {
        Pos3::new(
            self.dequantise_horizontal(x),
            self.dequantise_horizontal(y),
            self.dequantise_vertical(z),
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
    /// interpolation, which this crate cannot see — a number to look at, not
    /// a rule.
    pub const fn wire_steps_per_cell(&self) -> u32 {
        (self.cell_size.raw() / self.horizontal_precision.raw()) as u32
    }

    // -- misc -------------------------------------------------------------

    /// Stable digest of the fields that affect wire decoding. Exchange at
    /// connect time and reject on mismatch.
    ///
    /// Deliberately excludes speed and tick rate, which change how the world
    /// simulates but not how packets decode, so peers may disagree about them
    /// without garbling each other.
    ///
    /// This is a hash, so a mismatch is certain to be caught but a match is
    /// only near-certain. If you would rather have certainty and a message
    /// naming the offending field, send the values themselves — it is a
    /// handful of integers, once per connection.
    pub const fn protocol_hash(&self) -> u64 {
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

    /// Uniform-distribution estimate of entities in one viewer's subscription.
    /// A planning aid, not a runtime quantity — real worlds cluster, so treat
    /// this as a floor and expect hot cells to be far worse.
    pub const fn est_entities_in_view(&self, entities_per_region: u32) -> u32 {
        let per_cell = entities_per_region / self.cells_per_region;
        per_cell * self.sub_grid_cells
    }
}

/// FNV-1a over four bytes. Chosen because it is a `const fn`, is stable across
/// Rust versions and platforms unlike `DefaultHasher`, and carries no
/// dependency. Not cryptographic: it catches misconfiguration, not attack.
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
    /// 4096 m region, 1024 m vertical, 128 m cells, 256 m view radius,
    /// 40 m/s cap, 20 Hz, 1/16 m wire precision.
    ///
    /// Must stay a `build()` call. Replacing this with a struct literal would
    /// skip validation and defeat the private fields.
    fn default() -> Self {
        Self::builder()
            .region_size_m(4096)
            .vertical_extent_m(1024)
            .cell_size_m(128)
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

#[derive(Debug, Clone, Default)]
pub struct WorldConfigBuilder {
    region_size: Option<Fixed>,
    vertical_extent: Option<Fixed>,
    cell_size: Option<Fixed>,
    horizontal_view_radius: Option<Fixed>,
    max_horizontal_speed: Option<Fixed>,
    tick_hz: Option<u32>,
    horizontal_bits: Option<u32>,
    horizontal_precision: Option<Fixed>,
    vertical_bits: Option<u32>,
    vertical_precision: Option<Fixed>,
}

impl WorldConfigBuilder {
    /// Region edge length in whole metres. Must be a power of two in fixed
    /// point, which for whole metres means a power of two in metres.
    pub fn region_size_m(mut self, m: i32) -> Self {
        self.region_size = Some(Fixed::from_meters(m));
        self
    }

    /// Vertical range in whole metres. Also power-of-two constrained.
    pub fn vertical_extent_m(mut self, m: i32) -> Self {
        self.vertical_extent = Some(Fixed::from_meters(m));
        self
    }

    /// Cell edge length in whole metres. Half the view radius is the usual
    /// choice: it gives a 5x5 subscription grid covering roughly twice the
    /// ideal circle, against 2.9x for a 3x3.
    pub fn cell_size_m(mut self, m: i32) -> Self {
        self.cell_size = Some(Fixed::from_meters(m));
        self
    }

    pub fn horizontal_view_radius_m(mut self, m: i32) -> Self {
        self.horizontal_view_radius = Some(Fixed::from_meters(m));
        self
    }

    pub fn max_horizontal_speed_m_per_sec(mut self, m: i32) -> Self {
        self.max_horizontal_speed = Some(Fixed::from_meters(m));
        self
    }

    pub fn tick_hz(mut self, hz: u32) -> Self {
        self.tick_hz = Some(hz);
        self
    }

    /// Bits per horizontal axis on the wire. Mutually exclusive with
    /// [`horizontal_precision`].
    ///
    /// [`horizontal_precision`]: Self::horizontal_precision
    pub fn horizontal_bits(mut self, bits: u32) -> Self {
        self.horizontal_bits = Some(bits);
        self
    }

    /// Worst-case horizontal wire error to tolerate.
    ///
    /// Usually preferable to [`horizontal_bits`]: you know what error is
    /// acceptable, not how many bits buys it. It also keeps precision
    /// independent of region size, so enlarging the world raises the bit count
    /// rather than silently doubling the error.
    ///
    /// Rounded so the achieved precision is at least as fine as requested.
    ///
    /// [`horizontal_bits`]: Self::horizontal_bits
    pub fn horizontal_precision(mut self, p: Fixed) -> Self {
        self.horizontal_precision = Some(p);
        self
    }

    /// Bits for the vertical axis. Mutually exclusive with
    /// [`vertical_precision`].
    ///
    /// [`vertical_precision`]: Self::vertical_precision
    pub fn vertical_bits(mut self, bits: u32) -> Self {
        self.vertical_bits = Some(bits);
        self
    }

    /// Worst-case vertical wire error to tolerate. Defaults to whatever the
    /// horizontal axis resolved to.
    pub fn vertical_precision(mut self, p: Fixed) -> Self {
        self.vertical_precision = Some(p);
        self
    }

    /// Validate and compute all derived values.
    pub fn build(self) -> Result<WorldConfig, ConfigError> {
        use ConfigError::*;

        let region_size = self.region_size.ok_or(Missing("region_size"))?;
        let vertical_extent = self.vertical_extent.ok_or(Missing("vertical_extent"))?;
        let cell_size = self.cell_size.ok_or(Missing("cell_size"))?;
        let horizontal_view_radius = self
            .horizontal_view_radius
            .ok_or(Missing("horizontal_view_radius"))?;
        let max_horizontal_speed = self
            .max_horizontal_speed
            .ok_or(Missing("max_horizontal_speed"))?;
        let tick_hz = self.tick_hz.ok_or(Missing("tick_hz"))?;

        // Fixed is signed, so positivity has to be established before any
        // unsigned bit operation below.
        positive(region_size, "region_size")?;
        positive(vertical_extent, "vertical_extent")?;
        positive(cell_size, "cell_size")?;
        positive(horizontal_view_radius, "horizontal_view_radius")?;
        positive(max_horizontal_speed, "max_horizontal_speed")?;

        let region_raw = region_size.raw() as u32;
        let vertical_raw = vertical_extent.raw() as u32;
        let cell_raw = cell_size.raw() as u32;

        if !cell_raw.is_power_of_two() {
            return Err(CellSizeNotPowerOfTwo { cell_size });
        }
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
        if region_raw % cell_raw != 0 {
            return Err(RegionNotWholeCells {
                region_size,
                cell_size,
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
        if cell_radius > MAX_CELL_RADIUS {
            return Err(CellRadiusTooLarge {
                cell_radius,
                max: MAX_CELL_RADIUS,
            });
        }

        let max_move_per_tick = Fixed::from_raw(max_horizontal_speed.raw() / tick_hz as i32);
        if max_move_per_tick.raw() as u32 >= cell_raw {
            return Err(SpeedExceedsCellPerTick {
                move_per_tick: max_move_per_tick,
                cell_size,
            });
        }

        // Wire widths. Precision is the preferred input; bits is the escape
        // hatch for callers who need an exact field width.
        let horizontal_width = region_raw.trailing_zeros();
        let horizontal_bits = resolve_bits(
            Axis::Horizontal,
            self.horizontal_bits,
            self.horizontal_precision,
            DEFAULT_PRECISION,
            region_size,
            horizontal_width,
        )?;

        let horizontal_quant_shift = horizontal_width - horizontal_bits;
        let horizontal_precision = Fixed::from_raw(1 << horizontal_quant_shift);

        if horizontal_precision.raw() as u32 > cell_raw {
            return Err(QuantErrorExceedsCell {
                precision: horizontal_precision,
                cell_size,
            });
        }

        // Vertical defaults to whatever the horizontal axis resolved to, so a
        // caller who tunes one gets a consistent other.
        let vertical_width = vertical_raw.trailing_zeros();
        let vertical_bits = resolve_bits(
            Axis::Vertical,
            self.vertical_bits,
            self.vertical_precision,
            horizontal_precision,
            vertical_extent,
            vertical_width,
        )?;

        let vertical_quant_shift = vertical_width - vertical_bits;
        let vertical_precision = Fixed::from_raw(1 << vertical_quant_shift);

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
    if v.raw() <= 0 {
        Err(ConfigError::NonPositive(name))
    } else {
        Ok(())
    }
}

/// Resolve one axis's wire width from whichever of bits or precision was given.
fn resolve_bits(
    axis: Axis,
    bits: Option<u32>,
    precision: Option<Fixed>,
    default_precision: Fixed,
    extent: Fixed,
    width: u32,
) -> Result<u32, ConfigError> {
    let resolved = match (bits, precision) {
        (Some(_), Some(_)) => return Err(ConfigError::ConflictingPositionSpec { axis }),
        (Some(b), None) => b,
        (None, Some(p)) => bits_for_precision(axis, p, extent, width)?,
        (None, None) => bits_for_precision(axis, default_precision, extent, width)?,
    };

    if resolved == 0 || resolved > width {
        return Err(ConfigError::BadPositionBits {
            axis,
            bits: resolved,
            width,
        });
    }
    Ok(resolved)
}

/// Fewest bits keeping worst-case error at or below `precision`.
///
/// Error is `1 << quant_shift`, so the largest usable shift is
/// `floor(log2(precision))`.
fn bits_for_precision(
    axis: Axis,
    precision: Fixed,
    extent: Fixed,
    width: u32,
) -> Result<u32, ConfigError> {
    if precision.raw() <= 0 {
        return Err(ConfigError::BadPrecision { axis, precision });
    }
    let shift = (precision.raw() as u32).ilog2();
    if shift >= width {
        return Err(ConfigError::PrecisionExceedsExtent {
            axis,
            precision,
            extent,
        });
    }
    Ok(width - shift)
}

// ---------------------------------------------------------------------------
// ViewConfig
// ---------------------------------------------------------------------------

/// Per-viewer policy. Not protocol-critical: two clients may hold different
/// values without either misbehaving, so this can be negotiated per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewConfig {
    payload_bytes: u32,
    header_bytes: u32,
    event_reserve_bytes: u32,
    send_hz: u32,
    dwell_ticks: u32,
    hysteresis: Fixed,

    state_budget_bytes: u32,
    downstream_bytes_per_sec: u32,
}

impl ViewConfig {
    pub fn builder() -> ViewConfigBuilder {
        ViewConfigBuilder::default()
    }

    /// Bytes per packet available for entity state, after header and reserve.
    ///
    /// A byte budget, not a record count: entity payloads are opaque to this
    /// crate, so packing is a greedy fill rather than a division.
    pub const fn state_budget_bytes(&self) -> u32 {
        self.state_budget_bytes
    }
    /// Held back so a dense crowd cannot starve out a reliable event. Without
    /// it, standing in a mob means never learning that you died.
    pub const fn event_reserve_bytes(&self) -> u32 {
        self.event_reserve_bytes
    }
    pub const fn payload_bytes(&self) -> u32 {
        self.payload_bytes
    }
    pub const fn header_bytes(&self) -> u32 {
        self.header_bytes
    }
    /// Packets per second to this viewer. May be below the world tick rate; a
    /// slow client skips intermediate states, which is correct for position
    /// data.
    pub const fn send_hz(&self) -> u32 {
        self.send_hz
    }
    /// Ticks a cell must stay out of range before its subscription drops. Exit
    /// is the expensive direction, so err toward holding on.
    pub const fn dwell_ticks(&self) -> u32 {
        self.dwell_ticks
    }
    /// Deadband around a cell boundary. Stops a viewer loitering on a boundary
    /// from thrashing subscriptions.
    pub const fn hysteresis(&self) -> Fixed {
        self.hysteresis
    }
    pub const fn downstream_bytes_per_sec(&self) -> u32 {
        self.downstream_bytes_per_sec
    }
    pub const fn downstream_bits_per_sec(&self) -> u32 {
        self.downstream_bytes_per_sec * 8
    }

    /// Entities per packet if every record were `record_bytes` wide. Planning
    /// aid only; real packing is greedy over variable-size records.
    ///
    /// `record_bytes` is a parameter rather than a stored field so benchmarks
    /// can sweep it. No wire format exists yet. The 16 used throughout the
    /// design notes is an assumption: at the default wire precision a quantized
    /// position is 46 bits, and a per-connection ghost id adds roughly 12, so a
    /// bare position update is nearer 8 bytes. At 8 the packet holds 116
    /// records, at 24 it holds 38.
    pub const fn est_records_per_packet(&self, record_bytes: u32) -> u32 {
        if record_bytes == 0 {
            return 0;
        }
        self.state_budget_bytes / record_bytes
    }

    /// Starting point for a given world: one MTU-safe packet per tick,
    /// hysteresis at a sixteenth of a cell, two seconds of dwell.
    ///
    /// Must stay a `build()` call.
    pub fn default_for(world: &WorldConfig) -> Self {
        Self::builder()
            .payload_bytes(1_200)
            .header_bytes(16)
            .event_reserve_bytes(256)
            .send_hz(world.tick_hz())
            .dwell_ticks(world.tick_hz() * 2)
            .hysteresis(world.cell_size() / 16)
            .build()
            .expect("default view config is valid")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ViewConfigBuilder {
    payload_bytes: Option<u32>,
    header_bytes: Option<u32>,
    event_reserve_bytes: Option<u32>,
    send_hz: Option<u32>,
    dwell_ticks: Option<u32>,
    hysteresis: Option<Fixed>,
}

impl ViewConfigBuilder {
    /// Largest UDP payload to emit. 1200 is a conservative choice for a 1500
    /// MTU after IPv6, UDP, transport framing, and tunnel headroom.
    pub fn payload_bytes(mut self, b: u32) -> Self {
        self.payload_bytes = Some(b);
        self
    }
    pub fn header_bytes(mut self, b: u32) -> Self {
        self.header_bytes = Some(b);
        self
    }
    pub fn event_reserve_bytes(mut self, b: u32) -> Self {
        self.event_reserve_bytes = Some(b);
        self
    }
    pub fn send_hz(mut self, hz: u32) -> Self {
        self.send_hz = Some(hz);
        self
    }
    pub fn dwell_ticks(mut self, t: u32) -> Self {
        self.dwell_ticks = Some(t);
        self
    }
    pub fn hysteresis(mut self, h: Fixed) -> Self {
        self.hysteresis = Some(h);
        self
    }

    pub fn build(self) -> Result<ViewConfig, ConfigError> {
        use ConfigError::*;

        let payload_bytes = self.payload_bytes.ok_or(Missing("payload_bytes"))?;
        let header_bytes = self.header_bytes.unwrap_or(16);
        let event_reserve_bytes = self.event_reserve_bytes.unwrap_or(0);
        let send_hz = self.send_hz.ok_or(Missing("send_hz"))?;
        let dwell_ticks = self.dwell_ticks.unwrap_or(0);
        let hysteresis = self.hysteresis.unwrap_or(Fixed::ZERO);

        if hysteresis.raw() < 0 {
            return Err(NonPositive("hysteresis"));
        }
        if send_hz == 0 {
            return Err(NonPositive("send_hz"));
        }

        let overhead = header_bytes.saturating_add(event_reserve_bytes);
        if overhead >= payload_bytes {
            return Err(BudgetExhausted {
                payload: payload_bytes,
                header: header_bytes,
                reserve: event_reserve_bytes,
            });
        }

        Ok(ViewConfig {
            payload_bytes,
            header_bytes,
            event_reserve_bytes,
            send_hz,
            dwell_ticks,
            hysteresis,
            state_budget_bytes: payload_bytes - overhead,
            downstream_bytes_per_sec: payload_bytes * send_hz,
        })
    }
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
            .cell_size_m(128)
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
        assert_eq!(w.horizontal_bits(), 16);
        assert_eq!(w.horizontal_quant_shift(), 6);
        assert_eq!(w.horizontal_precision(), Fixed::from_raw(64));
        assert_eq!(w.vertical_bits(), 14);
        assert_eq!(w.vertical_quant_shift(), 6);
        assert_eq!(w.wire_steps_per_cell(), 2048);
        assert!(!w.is_lossless_horizontal());
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
    fn quantisation_error_is_bounded_on_both_axes() {
        let w = WorldConfig::default();
        for m in 0..1024i32 {
            let p = Pos3::new(
                Fixed::from_meters(m) + Fixed::from_raw(511),
                Fixed::from_meters(m),
                Fixed::from_meters(m) + Fixed::from_raw(511),
            );
            let (qx, qy, qz) = w.quantise_pos(p);
            let back = w.dequantise_pos(qx, qy, qz);
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
    fn precision_and_bits_agree() {
        let by_bits = base().horizontal_bits(16).build().unwrap();
        let by_precision = base()
            .horizontal_precision(Fixed::from_raw(64))
            .build()
            .unwrap();
        assert_eq!(by_bits.horizontal_bits(), by_precision.horizontal_bits());
        assert_eq!(by_bits.protocol_hash(), by_precision.protocol_hash());
    }

    #[test]
    fn precision_rounds_in_callers_favour() {
        // 100 raw units is not a power of two; log2 floors to 6, giving 64.
        let w = base()
            .horizontal_precision(Fixed::from_raw(100))
            .build()
            .unwrap();
        assert_eq!(w.horizontal_precision(), Fixed::from_raw(64));
        assert!(w.horizontal_precision().raw() <= 100);
    }

    #[test]
    fn vertical_defaults_to_horizontal_precision() {
        let w = base()
            .horizontal_precision(Fixed::from_raw(16))
            .build()
            .unwrap();
        assert_eq!(w.vertical_precision(), w.horizontal_precision());
    }

    #[test]
    fn rejects_conflicting_position_spec() {
        let err = base()
            .horizontal_bits(16)
            .horizontal_precision(Fixed::from_raw(64))
            .build()
            .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::ConflictingPositionSpec {
                axis: Axis::Horizontal
            }
        ));
    }

    #[test]
    fn rejects_precision_coarser_than_a_cell() {
        // 4 bits over a 22-bit region leaves an 18-bit shift: 262144 raw
        // units, spanning two 128 m cells.
        let err = base().horizontal_bits(4).build().unwrap_err();
        assert!(matches!(err, ConfigError::QuantErrorExceedsCell { .. }));
    }

    #[test]
    fn precision_equal_to_a_cell_is_allowed() {
        // 17-bit shift gives exactly one cell per bucket. Lossy, but the
        // decoded position resolves to the correct cell.
        let w = base().horizontal_bits(5).build().unwrap();
        assert_eq!(w.horizontal_precision(), w.cell_size());
        assert_eq!(w.wire_steps_per_cell(), 1);
    }

    #[test]
    fn lossless_wire_is_allowed() {
        let w = base()
            .horizontal_precision(Fixed::from_raw(1))
            .build()
            .unwrap();
        assert!(w.is_lossless_horizontal());
        assert_eq!(w.horizontal_bits(), 22);
    }

    #[test]
    fn rejects_non_power_of_two_cell() {
        let err = base().cell_size_m(100).build().unwrap_err();
        assert!(matches!(err, ConfigError::CellSizeNotPowerOfTwo { .. }));
    }

    #[test]
    fn rejects_non_positive_extent() {
        let err = base().region_size_m(0).build().unwrap_err();
        assert!(matches!(err, ConfigError::NonPositive("region_size")));
    }

    #[test]
    fn rejects_entity_that_can_skip_a_cell() {
        let err = base()
            .cell_size_m(1)
            .horizontal_view_radius_m(2)
            .build()
            .unwrap_err();
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
        let a = base().build().unwrap();
        let b = base().horizontal_bits(14).build().unwrap();
        assert_ne!(a.protocol_hash(), b.protocol_hash());
    }

    #[test]
    fn protocol_hash_catches_vertical_change() {
        let a = base().build().unwrap();
        let b = base().vertical_extent_m(512).build().unwrap();
        assert_ne!(a.protocol_hash(), b.protocol_hash());
    }

    #[test]
    fn average_viewer_is_already_over_budget() {
        let w = WorldConfig::default();
        let v = ViewConfig::default_for(&w);
        let in_view = w.est_entities_in_view(8_192);
        let capacity = v.est_records_per_packet(16);
        assert_eq!(in_view, 200);
        assert_eq!(capacity, 58);
        assert!(in_view > capacity * 3);
    }
}
