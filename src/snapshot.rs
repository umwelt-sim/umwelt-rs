//! Cell-ordered entity data, and the sort that produces it.
//!
//! [`CellSnapshot`] holds entities in cell order with each entity's position
//! stored alongside its id, so one cell is a contiguous read. `starts` holds one
//! entry per cell plus one; cell `c` occupies `starts[c]..starts[c + 1]`.
//!
//! [`CellSnapshot::update`] rewrites it from position arrays indexed by entity
//! id, by counting sort. Entity id is the index into those arrays. A
//! [`LiveSet`] says which of those slots hold a live entity; the rest are
//! skipped.

use crate::config::WorldConfig;
use crate::entity::{EntityId, LiveSet};
use crate::fixed::Fixed;
use crate::pos::{CellCoord, CellId, Pos2, Pos3};

/// Marks a cell in `sub_index` as not subdivided.
const NOT_SUBDIVIDED: u32 = u32::MAX;

/// Sub-cells per axis within a subdivided cell. Power of two.
pub const DEFAULT_SUB_AXIS: u32 = 8;

/// Population at or above which a cell is subdivided.
pub const DEFAULT_SUB_THRESHOLD: u32 = 512;

/// Largest permitted `sub_axis`. Bounded because the visit-order table is
/// `sub_axis^4` bytes and its entries are `u8`, so `sub_axis * sub_axis` must
/// fit in one.
pub const MAX_SUB_AXIS: u32 = 16;

/// One cell's entities. Parallel slices, all of the same length.
#[derive(Debug, Clone, Copy)]
pub struct CellOccupants<'a> {
    /// Ascending by entity id.
    pub ids: &'a [EntityId],
    pub xs: &'a [Fixed],
    pub ys: &'a [Fixed],
    pub zs: &'a [Fixed],
    /// Index of `ids[0]` within the snapshot's entity arrays.
    base: u32,
}

impl<'a> CellOccupants<'a> {
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The full position of the entity at slot `i`.
    #[inline(always)]
    pub fn pos(&self, i: usize) -> Pos3 {
        Pos3::new(self.xs[i], self.ys[i], self.zs[i])
    }

    /// The horizontal position of the entity at slot `i`.
    #[inline(always)]
    pub fn horizontal(&self, i: usize) -> Pos2 {
        Pos2::new(self.xs[i], self.ys[i])
    }

    /// The index of the entity at `i` within the snapshot's entity arrays.
    ///
    /// Valid only for the snapshot this run came from, and only until its next
    /// [`CellSnapshot::update`].
    #[inline(always)]
    pub fn snapshot_index(&self, i: usize) -> u32 {
        debug_assert!(i < self.len());
        self.base + i as u32
    }
}

/// Entities in cell order for one tick.
///
/// Read-only after a sort. Holds no state belonging to the sort that produced
/// it, so an edge can hold one without the machinery that built it.
#[derive(Debug, Clone)]
pub struct CellSnapshot {
    ids: Vec<EntityId>,
    xs: Vec<Fixed>,
    ys: Vec<Fixed>,
    zs: Vec<Fixed>,
    /// Length `cells + 1`. Cell `c` occupies `starts[c]..starts[c + 1]`.
    starts: Vec<u32>,
    /// Write head per cell during an update. A field rather than a local so
    /// `update` does not allocate.
    cursor: Vec<u32>,
    cells: usize,
    cfg: WorldConfig,

    /// Index into `sub_starts` per cell, or `NOT_SUBDIVIDED`.
    sub_index: Vec<u32>,
    /// For each subdivided cell, `sub_axis * sub_axis + 1` absolute offsets
    /// into the entity arrays. Sparse: only dense cells occupy space here.
    sub_starts: Vec<u32>,
    sub_axis: u32,
    sub_threshold: u32,
    /// Scratch for the sub-cell counting sort, which is not in place.
    scratch_ids: Vec<EntityId>,
    scratch_xs: Vec<Fixed>,
    scratch_ys: Vec<Fixed>,
    scratch_zs: Vec<Fixed>,
    sub_cursor: Vec<u32>,
    /// Sub-cell visit order, nearest first, for every possible origin.
    /// `sub_order[o * buckets .. (o + 1) * buckets]` is the order from origin
    /// `o`. Built once at construction; independent of entity data.
    sub_order: Vec<u8>,
    /// Cell offsets from a viewer's own cell, nearest first. A viewer sits at
    /// the center of its subscription, so one order serves every viewer.
    cell_order: Vec<(i8, i8)>,
}

/// The sub-cell ranges of a subdivided cell.
///
/// Offsets are absolute into the snapshot's entity arrays, so a sub-cell's
/// occupants are a slice with no further arithmetic.
#[derive(Debug, Clone, Copy)]
pub struct SubCells<'a> {
    axis: u32,
    starts: &'a [u32],
    ids: &'a [EntityId],
    xs: &'a [Fixed],
    ys: &'a [Fixed],
    zs: &'a [Fixed],
}

impl<'a> SubCells<'a> {
    /// Sub-cells per axis. The grid is `axis * axis`.
    #[inline(always)]
    pub fn axis(&self) -> u32 {
        self.axis
    }

    /// The entities in one sub-cell, ascending by entity id.
    #[inline(always)]
    pub fn occupants(&self, sx: u32, sy: u32) -> CellOccupants<'a> {
        debug_assert!(sx < self.axis && sy < self.axis);
        let b = (sy * self.axis + sx) as usize;
        let lo = self.starts[b] as usize;
        let hi = self.starts[b + 1] as usize;
        CellOccupants {
            ids: &self.ids[lo..hi],
            xs: &self.xs[lo..hi],
            ys: &self.ys[lo..hi],
            zs: &self.zs[lo..hi],
            base: lo as u32,
        }
    }

    /// The entities in the sub-cell at linear index `b`, as yielded by
    /// [`CellSnapshot::sub_cell_order`].
    #[inline(always)]
    pub fn occupants_at(&self, b: usize) -> CellOccupants<'a> {
        let lo = self.starts[b] as usize;
        let hi = self.starts[b + 1] as usize;
        CellOccupants {
            ids: &self.ids[lo..hi],
            xs: &self.xs[lo..hi],
            ys: &self.ys[lo..hi],
            zs: &self.zs[lo..hi],
            base: lo as u32,
        }
    }

    /// How many entities occupy the sub-cell at linear index `b`.
    #[inline(always)]
    pub fn count_at(&self, b: usize) -> usize {
        (self.starts[b + 1] - self.starts[b]) as usize
    }

    /// How many entities occupy one sub-cell.
    #[inline(always)]
    pub fn count(&self, sx: u32, sy: u32) -> usize {
        let b = (sy * self.axis + sx) as usize;
        (self.starts[b + 1] - self.starts[b]) as usize
    }
}

impl CellSnapshot {
    /// Allocates the offset table for a region's cell count. Entity capacity
    /// grows on the first sort.
    pub fn new(cfg: &WorldConfig) -> CellSnapshot {
        CellSnapshot::with_subdivision(cfg, DEFAULT_SUB_AXIS, DEFAULT_SUB_THRESHOLD)
    }

    /// Both parameters exist to be swept. `sub_axis` is sub-cells per axis and
    /// must be a power of two no finer than the cell's fractional resolution.
    /// `sub_threshold` is the population at or above which a cell is
    /// subdivided; below the eventual walk cap there is no benefit, since the
    /// whole cell is walked regardless.
    ///
    /// `sub_axis` of 1 disables subdivision.
    ///
    /// # Panics
    ///
    /// If `sub_axis` is zero, not a power of two, or finer than `cell_shift`
    /// permits.
    pub fn with_subdivision(cfg: &WorldConfig, sub_axis: u32, sub_threshold: u32) -> CellSnapshot {
        assert!(sub_axis > 0, "sub_axis must be positive");
        assert!(sub_axis.is_power_of_two(), "sub_axis must be a power of two");
        assert!(
            sub_axis.trailing_zeros() <= cfg.cell_shift(),
            "sub_axis {sub_axis} is finer than cell_shift {} allows",
            cfg.cell_shift()
        );
        assert!(
            sub_axis <= MAX_SUB_AXIS,
            "sub_axis {sub_axis} exceeds MAX_SUB_AXIS {MAX_SUB_AXIS}"
        );
        let cells = cfg.cells_per_region() as usize;
        CellSnapshot {
            ids: Vec::new(),
            xs: Vec::new(),
            ys: Vec::new(),
            zs: Vec::new(),
            starts: vec![0; cells + 1],
            cursor: vec![0; cells],
            cells,
            cfg: *cfg,
            sub_index: vec![NOT_SUBDIVIDED; cells],
            sub_starts: Vec::new(),
            sub_axis,
            sub_threshold,
            scratch_ids: Vec::new(),
            scratch_xs: Vec::new(),
            scratch_ys: Vec::new(),
            scratch_zs: Vec::new(),
            sub_cursor: vec![0; (sub_axis * sub_axis) as usize],
            sub_order: build_sub_order(sub_axis),
            cell_order: build_cell_order(cfg.cell_radius()),
        }
    }

    /// Cell offsets from a viewer's own cell, nearest first.
    #[inline]
    pub fn cell_order(&self) -> &[(i8, i8)] {
        &self.cell_order
    }

    /// Sub-cell visit order for `viewer` looking at `cell`, nearest first.
    ///
    /// Entries are linear sub-cell indices for [`SubCells::occupants_at`]. The
    /// slice always contains every sub-cell exactly once; a caller that stops
    /// early is choosing to.
    ///
    /// The order is keyed on which sub-cell the viewer occupies, not on its
    /// exact position, so it is a table lookup rather than a sort. A viewer
    /// outside `cell` clamps to the nearest edge sub-cell, which gives the
    /// correct near edge and an approximate order behind it.
    #[inline]
    pub fn sub_cell_order(&self, cell: CellCoord, viewer: Pos2) -> &[u8] {
        let buckets = (self.sub_axis * self.sub_axis) as usize;
        let (ox, oy) = self.sub_origin(cell, viewer);
        let o = (oy * self.sub_axis + ox) as usize;
        &self.sub_order[o * buckets..(o + 1) * buckets]
    }

    /// The viewer's sub-cell within `cell`, clamped to the grid.
    #[inline]
    fn sub_origin(&self, cell: CellCoord, viewer: Pos2) -> (u32, u32) {
        let cell_shift = self.cfg.cell_shift();
        let sub_shift = cell_shift - self.sub_axis.trailing_zeros();
        let last = self.sub_axis as i32 - 1;
        let cx = (cell.x as i32) << cell_shift;
        let cy = (cell.y as i32) << cell_shift;
        // Arithmetic shift floors, so a viewer left of or below the cell goes
        // negative and clamps to zero rather than wrapping.
        let ox = ((viewer.x.raw() - cx) >> sub_shift).clamp(0, last);
        let oy = ((viewer.y.raw() - cy) >> sub_shift).clamp(0, last);
        (ox as u32, oy as u32)
    }

    #[inline]
    pub fn sub_axis(&self) -> u32 {
        self.sub_axis
    }

    #[inline]
    pub fn sub_threshold(&self) -> u32 {
        self.sub_threshold
    }

    /// The sub-cell ranges of a cell, or `None` if it was below the threshold.
    #[inline]
    pub fn sub_cells(&self, cell: CellId) -> Option<SubCells<'_>> {
        let base = *self.sub_index.get(cell.index())?;
        if base == NOT_SUBDIVIDED {
            return None;
        }
        let buckets = (self.sub_axis * self.sub_axis) as usize;
        let b = base as usize;
        Some(SubCells {
            axis: self.sub_axis,
            starts: &self.sub_starts[b..b + buckets + 1],
            ids: &self.ids,
            xs: &self.xs,
            ys: &self.ys,
            zs: &self.zs,
        })
    }

    /// How many cells are currently subdivided.
    pub fn subdivided_cells(&self) -> usize {
        self.sub_index.iter().filter(|&&i| i != NOT_SUBDIVIDED).count()
    }

    /// The configuration this snapshot was built for.
    #[inline(always)]
    pub fn config(&self) -> &WorldConfig {
        &self.cfg
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[inline]
    pub fn cell_count(&self) -> usize {
        self.cells
    }

    /// The entities occupying one cell, ascending by entity id.
    ///
    /// An empty cell yields empty slices.
    #[inline(always)]
    pub fn entities_for_cell(&self, id: CellId) -> CellOccupants<'_> {
        let c = id.index();
        debug_assert!(c < self.cells, "cell {c} out of range for {} cells", self.cells);
        let lo = self.starts[c] as usize;
        let hi = self.starts[c + 1] as usize;
        CellOccupants {
            ids: &self.ids[lo..hi],
            xs: &self.xs[lo..hi],
            ys: &self.ys[lo..hi],
            zs: &self.zs[lo..hi],
            base: lo as u32,
        }
    }

    /// The id at `i` in the snapshot's entity arrays, as yielded by
    /// [`CellOccupants::snapshot_index`].
    #[inline(always)]
    pub fn id_at(&self, i: usize) -> EntityId {
        self.ids[i]
    }

    /// The position at `i` in the snapshot's entity arrays, as yielded by
    /// [`CellOccupants::snapshot_index`].
    #[inline(always)]
    pub fn pos_at(&self, i: usize) -> Pos3 {
        Pos3::new(self.xs[i], self.ys[i], self.zs[i])
    }

    /// How many entities occupy one cell.
    #[inline(always)]
    pub fn count(&self, id: CellId) -> usize {
        let c = id.index();
        (self.starts[c + 1] - self.starts[c]) as usize
    }

    /// Rewrites this snapshot with the live entities in `xs`, `ys`, `zs`
    /// arranged by cell, using a counting sort.
    ///
    /// The three slices are parallel; entity id is the index into them. Slots
    /// absent from `live` are skipped, so a despawned entity does not appear.
    /// Each cell's range is left ascending by entity id.
    ///
    /// Allocates only while entity capacity is growing.
    ///
    /// # Panics
    ///
    /// If the three slices differ in length, or if `live` covers fewer slots
    /// than the arrays hold.
    pub fn update(&mut self, xs: &[Fixed], ys: &[Fixed], zs: &[Fixed], live: &LiveSet) {
        assert_eq!(xs.len(), ys.len(), "position arrays must be parallel");
        assert_eq!(xs.len(), zs.len(), "position arrays must be parallel");
        assert!(
            live.slots() >= xs.len(),
            "live set covers {} slots, position arrays hold {}",
            live.slots(),
            xs.len()
        );

        let cfg = self.cfg;
        let slots = xs.len();
        let n = live.live();
        self.ids.clear();
        self.xs.clear();
        self.ys.clear();
        self.zs.clear();
        self.ids.resize(n, EntityId::from_raw(0));
        self.xs.resize(n, Fixed::ZERO);
        self.ys.resize(n, Fixed::ZERO);
        self.zs.resize(n, Fixed::ZERO);

        // Pass 1: tally into starts[c + 1], so the running total below produces
        // each cell's start offset with no shifting afterwards.
        self.starts.fill(0);
        for i in 0..slots {
            if !live.contains(EntityId::from_raw(i as u32)) {
                continue;
            }
            let c = cfg.cell_id(cfg.cell_of(Pos2::new(xs[i], ys[i]))).index();
            self.starts[c + 1] += 1;
        }

        // Pass 2: running total.
        for c in 0..self.cells {
            self.starts[c + 1] += self.starts[c];
        }

        // Pass 3: scatter in ascending id order, leaving each cell's range
        // sorted by id.
        self.cursor.copy_from_slice(&self.starts[..self.cells]);
        for i in 0..slots {
            if !live.contains(EntityId::from_raw(i as u32)) {
                continue;
            }
            let c = cfg.cell_id(cfg.cell_of(Pos2::new(xs[i], ys[i]))).index();
            let d = self.cursor[c] as usize;
            self.cursor[c] += 1;
            self.ids[d] = EntityId::from_raw(i as u32);
            self.xs[d] = xs[i];
            self.ys[d] = ys[i];
            self.zs[d] = zs[i];
        }

        self.rebuild_subdivisions();
    }

    /// Subdivides every cell at or above `sub_threshold`.
    ///
    /// Runs over the entities in dense cells only, once per update, and is not
    /// per viewer.
    fn rebuild_subdivisions(&mut self) {
        self.sub_starts.clear();
        self.sub_index.fill(NOT_SUBDIVIDED);
        if self.sub_axis <= 1 {
            return;
        }
        for c in 0..self.cells {
            if self.starts[c + 1] - self.starts[c] >= self.sub_threshold {
                self.subdivide(c);
            }
        }
    }

    /// Counting sort of one cell's range by sub-cell.
    ///
    /// The sort is stable and the range is already ascending by entity id, so
    /// each sub-cell comes out ascending by id.
    fn subdivide(&mut self, cell: usize) {
        let lo = self.starts[cell] as usize;
        let hi = self.starts[cell + 1] as usize;
        let n = hi - lo;
        let axis = self.sub_axis as i32;
        let buckets = (self.sub_axis * self.sub_axis) as usize;
        let cell_mask = self.cfg.cell_mask();
        let sub_shift = self.cfg.cell_shift() - self.sub_axis.trailing_zeros();

        let base = self.sub_starts.len();
        self.sub_index[cell] = base as u32;
        self.sub_starts.resize(base + buckets + 1, 0);

        // Pass 1: tally into base + b + 1, so the running total below produces
        // each sub-cell's start offset.
        for i in lo..hi {
            let ox = (self.xs[i].raw() & cell_mask) >> sub_shift;
            let oy = (self.ys[i].raw() & cell_mask) >> sub_shift;
            let b = (oy * axis + ox) as usize;
            self.sub_starts[base + b + 1] += 1;
        }

        // Pass 2: running total, seeded with the cell's own start so every
        // offset is absolute into the entity arrays.
        self.sub_starts[base] = lo as u32;
        for b in 0..buckets {
            self.sub_starts[base + b + 1] += self.sub_starts[base + b];
        }

        // Pass 3: scatter through scratch. Counting sort is not in place and
        // here the source range is also the destination range.
        self.scratch_ids.resize(n, EntityId::from_raw(0));
        self.scratch_xs.resize(n, Fixed::ZERO);
        self.scratch_ys.resize(n, Fixed::ZERO);
        self.scratch_zs.resize(n, Fixed::ZERO);
        self.sub_cursor.copy_from_slice(&self.sub_starts[base..base + buckets]);
        for i in lo..hi {
            let ox = (self.xs[i].raw() & cell_mask) >> sub_shift;
            let oy = (self.ys[i].raw() & cell_mask) >> sub_shift;
            let b = (oy * axis + ox) as usize;
            let d = self.sub_cursor[b] as usize - lo;
            self.sub_cursor[b] += 1;
            self.scratch_ids[d] = self.ids[i];
            self.scratch_xs[d] = self.xs[i];
            self.scratch_ys[d] = self.ys[i];
            self.scratch_zs[d] = self.zs[i];
        }
        self.ids[lo..hi].copy_from_slice(&self.scratch_ids[..n]);
        self.xs[lo..hi].copy_from_slice(&self.scratch_xs[..n]);
        self.ys[lo..hi].copy_from_slice(&self.scratch_ys[..n]);
        self.zs[lo..hi].copy_from_slice(&self.scratch_zs[..n]);
    }
}

fn build_cell_order(cell_radius: u32) -> Vec<(i8, i8)> {
    let r = cell_radius as i32;
    let mut v: Vec<(i32, (i8, i8))> = Vec::with_capacity(((2 * r + 1) * (2 * r + 1)) as usize);
    for dy in -r..=r {
        for dx in -r..=r {
            v.push((dx * dx + dy * dy, (dx as i8, dy as i8)));
        }
    }
    v.sort_by_key(|&(d2, _)| d2);
    v.into_iter().map(|(_, o)| o).collect()
}

/// Visit order from every origin, nearest first by squared distance between
/// sub-cell coordinates.
///
/// Sub-cell centers are all offset identically within their cells, so ordering
/// by coordinate difference is the same as ordering by center distance. Ties
/// resolve by linear index, which a stable sort preserves.
fn build_sub_order(axis: u32) -> Vec<u8> {
    let buckets = (axis * axis) as usize;
    let mut table = Vec::with_capacity(buckets * buckets);
    let mut scratch: Vec<(u32, u8)> = Vec::with_capacity(buckets);
    for oy in 0..axis as i32 {
        for ox in 0..axis as i32 {
            scratch.clear();
            for sy in 0..axis as i32 {
                for sx in 0..axis as i32 {
                    let dx = sx - ox;
                    let dy = sy - oy;
                    let d2 = (dx * dx + dy * dy) as u32;
                    scratch.push((d2, (sy * axis as i32 + sx) as u8));
                }
            }
            scratch.sort_by_key(|&(d2, _)| d2);
            table.extend(scratch.iter().map(|&(_, b)| b));
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits positions into the three parallel arrays `rebuild` takes.
    /// Every slot live.
    fn all_live(n: usize) -> LiveSet {
        let mut l = LiveSet::with_capacity(n);
        for i in 0..n {
            l.insert(EntityId::from_raw(i as u32));
        }
        l
    }

    fn axes(pts: &[Pos3]) -> (Vec<Fixed>, Vec<Fixed>, Vec<Fixed>) {
        (
            pts.iter().map(|p| p.x).collect(),
            pts.iter().map(|p| p.y).collect(),
            pts.iter().map(|p| p.z).collect(),
        )
    }

    fn scatter(n: usize, seed: u64) -> Vec<Pos3> {
        let mut s = seed;
        let mut next = move |m: i32| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) as u32 % m as u32) as i32
        };
        (0..n).map(|_| Pos3::from_meters(next(4096), next(4096), next(1024))).collect()
    }

    #[test]
    fn every_entity_lands_in_its_own_cell() {
        let cfg = WorldConfig::default();
        let pts = [
            Pos3::from_meters(0, 0, 0),
            Pos3::from_meters(127, 127, 5),
            Pos3::from_meters(128, 0, 5),
            Pos3::from_meters(4095, 4095, 5),
            Pos3::from_meters(300, 700, 5),
        ];
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        for (i, p) in pts.iter().enumerate() {
            let cid = cfg.cell_id(cfg.cell_of(p.horizontal()));
            let cell = snap.entities_for_cell(cid);
            assert!(
                cell.ids.contains(&EntityId::from_raw(i as u32)),
                "entity {i} missing from {cid:?}"
            );
        }
    }

    #[test]
    fn counts_sum_to_the_population() {
        let cfg = WorldConfig::default();
        let pts = scatter(2000, 0x1234_5678_9abc_def0);
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        let summed: usize =
            (0..snap.cell_count()).map(|c| snap.count(CellId::from_raw(c as u32))).sum();
        assert_eq!(summed, pts.len());
        assert_eq!(snap.len(), pts.len());
    }

    #[test]
    fn positions_travel_with_their_ids() {
        let cfg = WorldConfig::default();
        let pts = scatter(1000, 0xfeed_face_dead_beef);
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        for c in 0..snap.cell_count() {
            let cell = snap.entities_for_cell(CellId::from_raw(c as u32));
            for i in 0..cell.len() {
                assert_eq!(cell.pos(i), pts[cell.ids[i].index()]);
            }
        }
    }

    #[test]
    fn cells_come_out_sorted_by_id() {
        let cfg = WorldConfig::default();
        let pts = scatter(4000, 0x0bad_c0de_0bad_c0de);
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        for c in 0..snap.cell_count() {
            let cell = snap.entities_for_cell(CellId::from_raw(c as u32));
            assert!(cell.ids.windows(2).all(|w| w[0] < w[1]), "cell {c} is not ascending by id");
        }
    }

    #[test]
    fn empty_cells_are_empty() {
        let cfg = WorldConfig::default();
        let pts = [Pos3::from_meters(10, 10, 0)];
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        let occupied = cfg.cell_id(cfg.cell_of(Pos2::from_meters(10, 10)));
        let mut empties = 0;
        for c in 0..snap.cell_count() {
            let cid = CellId::from_raw(c as u32);
            if cid == occupied {
                continue;
            }
            assert!(snap.entities_for_cell(cid).is_empty());
            empties += 1;
        }
        assert_eq!(empties, snap.cell_count() - 1);
    }

    #[test]
    fn rebuild_follows_movement() {
        let cfg = WorldConfig::default();
        let mut pts = vec![Pos3::from_meters(10, 10, 0)];
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        let before = cfg.cell_id(cfg.cell_of(Pos2::from_meters(10, 10)));
        assert_eq!(snap.count(before), 1);

        pts[0] = Pos3::from_meters(2000, 2000, 0);
        let (xs, ys, zs) = axes(&pts);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        let after = cfg.cell_id(cfg.cell_of(Pos2::from_meters(2000, 2000)));
        assert_ne!(before, after);
        assert_eq!(snap.count(before), 0);
        assert_eq!(snap.count(after), 1);
    }

    // -- subdivision ---------------------------------------------------

    /// `n` entities scattered inside the single cell containing `m` metres.
    fn crowd_in_one_cell(cfg: &WorldConfig, n: usize, m: i32, seed: u64) -> Vec<Pos3> {
        let cell = cfg.cell_size().raw() as u32;
        let origin = (Fixed::from_meters(m).raw() as u32) & !(cell - 1);
        let mut s = seed;
        let mut next = move |bound: u32| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u32 % bound
        };
        (0..n)
            .map(|_| {
                Pos3::new(
                    Fixed::from_raw((origin + next(cell)) as i32),
                    Fixed::from_raw((origin + next(cell)) as i32),
                    Fixed::ZERO,
                )
            })
            .collect()
    }

    fn sorted(cfg: &WorldConfig, pts: &[Pos3], axis: u32, threshold: u32) -> CellSnapshot {
        let (xs, ys, zs) = axes(pts);
        let mut snap = CellSnapshot::with_subdivision(cfg, axis, threshold);
        snap.update(&xs, &ys, &zs, &all_live(pts.len()));
        snap
    }

    // -- distance-ordered walk -----------------------------------------

    /// The world position of a sub-cell's low corner.
    fn sub_corner(cfg: &WorldConfig, cell: CellCoord, axis: u32, sx: u32, sy: u32) -> Pos2 {
        let sub_shift = cfg.cell_shift() - axis.trailing_zeros();
        let cx = (cell.x as i32) << cfg.cell_shift();
        let cy = (cell.y as i32) << cfg.cell_shift();
        Pos2::new(
            Fixed::from_raw(cx + ((sx as i32) << sub_shift)),
            Fixed::from_raw(cy + ((sy as i32) << sub_shift)),
        )
    }

    #[test]
    fn order_visits_every_sub_cell_once() {
        let cfg = WorldConfig::default();
        let snap = CellSnapshot::with_subdivision(&cfg, 8, 512);
        let cell = CellCoord::new(16, 16);
        for sy in 0..8 {
            for sx in 0..8 {
                let v = sub_corner(&cfg, cell, 8, sx, sy);
                let order = snap.sub_cell_order(cell, v);
                assert_eq!(order.len(), 64);
                let mut seen = order.to_vec();
                seen.sort_unstable();
                seen.dedup();
                assert_eq!(seen.len(), 64, "order from ({sx}, {sy}) has duplicates");
            }
        }
    }

    #[test]
    fn order_starts_at_the_viewers_own_sub_cell() {
        let cfg = WorldConfig::default();
        let snap = CellSnapshot::with_subdivision(&cfg, 8, 512);
        let cell = CellCoord::new(16, 16);
        for sy in 0..8u32 {
            for sx in 0..8u32 {
                let v = sub_corner(&cfg, cell, 8, sx, sy);
                let order = snap.sub_cell_order(cell, v);
                assert_eq!(order[0] as u32, sy * 8 + sx, "from ({sx}, {sy})");
            }
        }
    }

    #[test]
    fn order_is_non_decreasing_in_distance() {
        let cfg = WorldConfig::default();
        let snap = CellSnapshot::with_subdivision(&cfg, 8, 512);
        let cell = CellCoord::new(16, 16);
        for oy in 0..8i32 {
            for ox in 0..8i32 {
                let v = sub_corner(&cfg, cell, 8, ox as u32, oy as u32);
                let order = snap.sub_cell_order(cell, v);
                let d2 = |b: u8| -> i32 {
                    let (sx, sy) = ((b % 8) as i32, (b / 8) as i32);
                    (sx - ox) * (sx - ox) + (sy - oy) * (sy - oy)
                };
                for w in order.windows(2) {
                    assert!(
                        d2(w[0]) <= d2(w[1]),
                        "order from ({ox}, {oy}) goes {} then {}, distances {} then {}",
                        w[0], w[1], d2(w[0]), d2(w[1])
                    );
                }
            }
        }
    }

    #[test]
    fn viewer_outside_the_cell_clamps_to_the_near_edge() {
        let cfg = WorldConfig::default();
        let snap = CellSnapshot::with_subdivision(&cfg, 8, 512);
        let cell = CellCoord::new(16, 16);

        // Two cells to the left, same row: nearest sub-cell column is 0.
        let left = sub_corner(&cfg, CellCoord::new(14, 16), 8, 4, 3);
        assert_eq!(snap.sub_cell_order(cell, left)[0] % 8, 0);

        // Two cells to the right: nearest column is 7.
        let right = sub_corner(&cfg, CellCoord::new(18, 16), 8, 4, 3);
        assert_eq!(snap.sub_cell_order(cell, right)[0] % 8, 7);

        // Below: nearest row is 0.
        let below = sub_corner(&cfg, CellCoord::new(16, 14), 8, 3, 4);
        assert_eq!(snap.sub_cell_order(cell, below)[0] / 8, 0);

        // Above: nearest row is 7.
        let above = sub_corner(&cfg, CellCoord::new(16, 18), 8, 3, 4);
        assert_eq!(snap.sub_cell_order(cell, above)[0] / 8, 7);
    }

    #[test]
    fn order_walks_a_real_crowd_nearest_first() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 3000, 2048, 0x9999);
        let snap = sorted(&cfg, &pts, 8, 512);
        let coord = cfg.cell_of(pts[0].horizontal());
        let cid = cfg.cell_id(coord);
        let sub = snap.sub_cells(cid).unwrap();

        // Stand in sub-cell (0, 0) and walk. Each entity taken should be no
        // closer than the one before it, allowing for sub-cell granularity:
        // compare sub-cell distance, not entity distance.
        let viewer = sub_corner(&cfg, coord, 8, 0, 0);
        let order = snap.sub_cell_order(coord, viewer);

        let mut taken = 0;
        let mut prev_d2 = 0i32;
        for &b in order {
            let (sx, sy) = ((b % 8) as i32, (b / 8) as i32);
            let d2 = sx * sx + sy * sy;
            assert!(d2 >= prev_d2);
            prev_d2 = d2;
            taken += sub.count_at(b as usize);
        }
        assert_eq!(taken, pts.len(), "the full walk must reach every entity");
    }

    #[test]
    fn a_cap_takes_the_near_sub_cells() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 4000, 2048, 0xABCD);
        let snap = sorted(&cfg, &pts, 8, 512);
        let coord = cfg.cell_of(pts[0].horizontal());
        let sub = snap.sub_cells(cfg.cell_id(coord)).unwrap();
        let viewer = sub_corner(&cfg, coord, 8, 0, 0);

        // Take sub-cells until 512 entities are gathered, then check that the
        // furthest one taken is no further than the nearest one skipped.
        let order = snap.sub_cell_order(coord, viewer);
        let d2 = |b: u8| -> i32 {
            let (sx, sy) = ((b % 8) as i32, (b / 8) as i32);
            sx * sx + sy * sy
        };
        let mut n = 0;
        let mut cut = order.len();
        for (i, &b) in order.iter().enumerate() {
            n += sub.count_at(b as usize);
            if n >= 512 {
                cut = i + 1;
                break;
            }
        }
        assert!(cut < order.len(), "512 should not require the whole cell");
        let worst_taken = order[..cut].iter().map(|&b| d2(b)).max().unwrap();
        let best_skipped = order[cut..].iter().map(|&b| d2(b)).min().unwrap();
        assert!(worst_taken <= best_skipped);
    }

    #[test]
    fn sparse_cells_are_not_subdivided() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 100, 2048, 0xAAAA);
        let snap = sorted(&cfg, &pts, 8, 512);
        assert_eq!(snap.subdivided_cells(), 0);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        assert!(snap.sub_cells(cid).is_none());
    }

    #[test]
    fn dense_cells_are_subdivided() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 2000, 2048, 0xBBBB);
        let snap = sorted(&cfg, &pts, 8, 512);
        assert_eq!(snap.subdivided_cells(), 1);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let sub = snap.sub_cells(cid).expect("dense cell should be subdivided");
        assert_eq!(sub.axis(), 8);
    }

    #[test]
    fn threshold_boundary_is_inclusive() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 512, 2048, 0xCCCC);
        assert_eq!(sorted(&cfg, &pts, 8, 512).subdivided_cells(), 1);
        assert_eq!(sorted(&cfg, &pts, 8, 513).subdivided_cells(), 0);
    }

    #[test]
    fn every_entity_is_in_its_own_sub_cell() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 3000, 2048, 0xDDDD);
        let snap = sorted(&cfg, &pts, 8, 512);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let sub = snap.sub_cells(cid).unwrap();

        let shift = cfg.cell_shift() - 3; // axis 8
        let mask = cfg.cell_mask();
        let mut seen = 0;
        for sy in 0..sub.axis() {
            for sx in 0..sub.axis() {
                let occ = sub.occupants(sx, sy);
                for i in 0..occ.len() {
                    let ox = (occ.xs[i].raw() & mask) >> shift;
                    let oy = (occ.ys[i].raw() & mask) >> shift;
                    assert_eq!(ox as u32, sx, "entity in wrong sub-cell column");
                    assert_eq!(oy as u32, sy, "entity in wrong sub-cell row");
                }
                seen += occ.len();
            }
        }
        assert_eq!(seen, pts.len(), "sub-cell counts must sum to the cell");
    }

    #[test]
    fn sub_cells_stay_ascending_by_id() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 4000, 2048, 0xEEEE);
        let snap = sorted(&cfg, &pts, 8, 512);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let sub = snap.sub_cells(cid).unwrap();
        for sy in 0..sub.axis() {
            for sx in 0..sub.axis() {
                let occ = sub.occupants(sx, sy);
                assert!(
                    occ.ids.windows(2).all(|w| w[0] < w[1]),
                    "sub-cell ({sx}, {sy}) is not ascending by id"
                );
            }
        }
    }

    #[test]
    fn positions_survive_the_second_sort() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 2500, 2048, 0xF00D);
        let snap = sorted(&cfg, &pts, 8, 512);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let occ = snap.entities_for_cell(cid);
        assert_eq!(occ.len(), pts.len());
        for i in 0..occ.len() {
            assert_eq!(occ.pos(i), pts[occ.ids[i].index()]);
        }
    }

    #[test]
    fn subdivision_follows_movement() {
        let cfg = WorldConfig::default();
        let mut pts = crowd_in_one_cell(&cfg, 1000, 2048, 0x1234);
        let cid = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let cell_lo = (Fixed::from_meters(2048).raw() as u32) & !(cfg.cell_size().raw() as u32 - 1);

        // Park entity 0 in sub-cell (0, 0), then move it to (7, 7).
        pts[0] = Pos3::new(Fixed::from_raw(cell_lo as i32), Fixed::from_raw(cell_lo as i32), Fixed::ZERO);
        let snap = sorted(&cfg, &pts, 8, 512);
        assert!(snap.sub_cells(cid).unwrap().occupants(0, 0).ids.contains(&EntityId::from_raw(0)));

        let far = cell_lo + cfg.cell_size().raw() as u32 - 1;
        pts[0] = Pos3::new(Fixed::from_raw(far as i32), Fixed::from_raw(far as i32), Fixed::ZERO);
        let snap = sorted(&cfg, &pts, 8, 512);
        let sub = snap.sub_cells(cid).unwrap();
        assert!(!sub.occupants(0, 0).ids.contains(&EntityId::from_raw(0)));
        assert!(sub.occupants(7, 7).ids.contains(&EntityId::from_raw(0)));
    }

    #[test]
    fn axis_of_one_disables_subdivision() {
        let cfg = WorldConfig::default();
        let pts = crowd_in_one_cell(&cfg, 2000, 2048, 0x5555);
        assert_eq!(sorted(&cfg, &pts, 1, 512).subdivided_cells(), 0);
    }

    #[test]
    fn two_dense_cells_get_independent_ranges() {
        let cfg = WorldConfig::default();
        let mut pts = crowd_in_one_cell(&cfg, 1000, 1024, 0x7777);
        pts.extend(crowd_in_one_cell(&cfg, 1000, 3072, 0x8888));
        let snap = sorted(&cfg, &pts, 8, 512);
        assert_eq!(snap.subdivided_cells(), 2);

        let a = cfg.cell_id(cfg.cell_of(pts[0].horizontal()));
        let b = cfg.cell_id(cfg.cell_of(pts[1500].horizontal()));
        assert_ne!(a, b);
        let (sa, sb) = (snap.sub_cells(a).unwrap(), snap.sub_cells(b).unwrap());
        let count = |s: &SubCells| -> usize {
            (0..s.axis()).flat_map(|y| (0..s.axis()).map(move |x| (x, y))).map(|(x, y)| s.count(x, y)).sum()
        };
        assert_eq!(count(&sa), 1000);
        assert_eq!(count(&sb), 1000);
    }

    #[test]
    fn despawned_entities_are_absent() {
        let cfg = WorldConfig::default();
        let pts = [
            Pos3::from_meters(100, 100, 0),
            Pos3::from_meters(100, 100, 0),
            Pos3::from_meters(100, 100, 0),
        ];
        let (xs, ys, zs) = axes(&pts);
        let cid = cfg.cell_id(cfg.cell_of(Pos2::from_meters(100, 100)));

        let mut live = all_live(3);
        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &live);
        assert_eq!(snap.count(cid), 3);
        assert_eq!(snap.len(), 3);

        live.remove(EntityId::from_raw(1));
        snap.update(&xs, &ys, &zs, &live);

        assert_eq!(snap.count(cid), 2, "the despawned entity is still in the cell");
        assert_eq!(snap.len(), 2);
        let ids = snap.entities_for_cell(cid).ids;
        assert_eq!(ids, &[EntityId::from_raw(0), EntityId::from_raw(2)]);
    }

    #[test]
    fn surviving_ids_do_not_shift() {
        let cfg = WorldConfig::default();
        let pts: Vec<Pos3> = (0..64).map(|i| Pos3::from_meters(100 + i, 100, 0)).collect();
        let (xs, ys, zs) = axes(&pts);

        let mut live = all_live(pts.len());
        for dead in [0u32, 5, 63] {
            live.remove(EntityId::from_raw(dead));
        }

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &live);

        assert_eq!(snap.len(), 61);
        for c in 0..snap.cell_count() {
            let cell = snap.entities_for_cell(CellId::from_raw(c as u32));
            for i in 0..cell.len() {
                let id = cell.ids[i];
                assert!(live.contains(id), "{id:?} is dead but present");
                assert_eq!(cell.pos(i), pts[id.index()], "{id:?} points at the wrong slot");
            }
        }
    }

    #[test]
    fn one_cell_can_hold_everything() {
        let cfg = WorldConfig::default();
        let pts: Vec<Pos3> =
            (0..5000).map(|i| Pos3::from_meters(2048 + (i % 100), 2048, 0)).collect();
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));

        let hot = cfg.cell_id(cfg.cell_of(Pos2::from_meters(2048, 2048)));
        assert_eq!(snap.count(hot), 5000);
    }

    #[test]
    fn rebuild_stops_allocating_after_warmup() {
        let cfg = WorldConfig::default();
        let pts = scatter(3000, 0xabcd_ef01_2345_6789);
        let (xs, ys, zs) = axes(&pts);

        let mut snap = CellSnapshot::new(&cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));
        let settled = snap.ids.capacity();

        for _tick in 0..50 {
            snap.update(&xs, &ys, &zs, &all_live(xs.len()));
        }
        assert_eq!(snap.ids.capacity(), settled);
    }
}

#[cfg(test)]
mod thread_safety {
    use super::*;

    /// The replication phase runs many threads over one snapshot. Nothing in it
    /// may become non-Sync without this failing.
    #[test]
    fn snapshot_is_shareable_across_threads() {
        fn assert_sync<T: Sync + Send>() {}
        assert_sync::<CellSnapshot>();
        assert_sync::<CellOccupants<'_>>();
    }
}
