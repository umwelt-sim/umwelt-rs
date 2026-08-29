//! Which cells a viewer draws from.
//!
//! A [`Subscription`] is the box of cells within the view radius of a viewer's
//! own cell, clipped to the region. A move that stays inside a cell leaves it
//! unchanged; a move that crosses one changes it by at most a row and a column,
//! which is what makes rebuilding it cheap enough to do every tick.
//!
//! [`CellList`] is that box flattened into the walk order a gather uses.

use crate::config::{MAX_SUB_GRID_CELLS, WorldConfig};
use crate::pos::CellCoord;
use core::fmt;

/// The bounding box describing the region to which a viewer is subscribed.
/// This is more efficient than storing the list of cells to which a viewer
/// is subscribed.
///
/// A viewer subscribes to every cell within `cell_radius` of the cell they
/// occupy, clipped to the region. That set is always bounded by a rectangle.
/// Cell membership within a subscription is determined by the standard
/// 4 comparisons for a rectangle.
///
/// Held per viewer as their current subscription state. Comparing an old
/// subscription against a new one gives the cells entered and exited by a
/// move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subscription {
    /// West edge, in cells, inclusive.
    pub x0: i32,
    /// East edge, in cells, inclusive.
    pub x1: i32,
    /// South edge, in cells, inclusive.
    pub y0: i32,
    /// North edge, in cells, inclusive.
    pub y1: i32,
}

impl Subscription {
    /// Number of cells subscribed.
    #[inline]
    pub const fn len(&self) -> usize {
        ((self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)) as usize
    }

    /// Whether it covers no cells.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.x1 < self.x0 || self.y1 < self.y0
    }

    /// Whether a cell is in the subscription region. Four integer comparisons, independent of
    /// how many cells the subscription covers.
    #[inline]
    pub const fn contains(&self, c: CellCoord) -> bool {
        let x = c.x as i32;
        let y = c.y as i32;
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }

    /// The subscribed cells, in row-major order.
    #[inline]
    pub fn cells(self) -> impl Iterator<Item = CellCoord> {
        let Subscription { x0, x1, y0, y1 } = self;
        (y0..=y1)
            .flat_map(move |y| (x0..=x1).map(move |x| CellCoord::new(x as u16, y as u16)))
    }

    /// The subscription for a viewer occupying `center`.
    ///
    /// Covers a square of side `2 * cell_radius + 1`, clipped to the region. The
    /// square is larger than the circle of radius `horizontal_view_radius`, so
    /// cells near its corners may hold nothing within view range.
    #[inline]
    pub fn at_center(cfg: &WorldConfig, center: CellCoord) -> Subscription {
        let radius = cfg.cell_radius() as i32;
        let max = cfg.cells_per_axis() as i32 - 1;
        let cx = center.x as i32;
        let cy = center.y as i32;

        Subscription {
            x0: (cx - radius).max(0),
            x1: (cx + radius).min(max),
            y0: (cy - radius).max(0),
            y1: (cy + radius).min(max),
        }
    }
}

/// A stored, ordered list of cells. This is an allocation-free structure backed
/// by a fixed-sized array. It is also not `Copy`.
///
/// [`Subscription`] covers the cases where cells are only iterated or tested
/// for membership. `CellList` is for cases needing a stored, comparable list,
/// such as the reference implementation of a subscription delta.
#[derive(Clone)]
pub struct CellList {
    cells: [CellCoord; MAX_SUB_GRID_CELLS],
    len: u16,
}

impl CellList {
    /// Empty.
    #[inline]
    pub const fn new() -> Self {
        Self { cells: [CellCoord::new(0, 0); MAX_SUB_GRID_CELLS], len: 0 }
    }

    /// Cells in the list.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the list is empty.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The cells, in walk order.
    #[inline]
    pub fn as_slice(&self) -> &[CellCoord] {
        &self.cells[..self.len as usize]
    }

    /// Over the cells, in walk order.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, CellCoord> {
        self.as_slice().iter()
    }

    /// Linear scan, bounded by [`MAX_SUB_GRID_CELLS`].
    #[inline]
    pub fn contains(&self, c: CellCoord) -> bool {
        self.as_slice().contains(&c)
    }

    /// Appends a cell.
    ///
    /// # Panics
    ///
    /// If the set is already at [`MAX_SUB_GRID_CELLS`]. `WorldConfigBuilder`
    /// rejects a `cell_radius` above `MAX_CELL_RADIUS`, so a set built from a
    /// subscription derived from a valid config cannot reach this.
    #[inline]
    pub fn push(&mut self, c: CellCoord) {
        assert!(
            (self.len as usize) < MAX_SUB_GRID_CELLS,
            "CellSet overflow: cell_radius exceeds MAX_CELL_RADIUS"
        );
        self.cells[self.len as usize] = c;
        self.len += 1;
    }

    /// Drops every cell.
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for CellList {
    fn default() -> Self {
        Self::new()
    }
}

impl FromIterator<CellCoord> for CellList {
    fn from_iter<I: IntoIterator<Item = CellCoord>>(iter: I) -> Self {
        let mut set = CellList::new();
        for c in iter {
            set.push(c);
        }
        set
    }
}

impl From<Subscription> for CellList {
    fn from(sub: Subscription) -> Self {
        sub.cells().collect()
    }
}

impl PartialEq for CellList {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for CellList {}

impl fmt::Debug for CellList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<'a> IntoIterator for &'a CellList {
    type Item = &'a CellCoord;
    type IntoIter = core::slice::Iter<'a, CellCoord>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interior_is_a_full_block() {
        let cfg = WorldConfig::default();
        let sub = Subscription::at_center(&cfg, CellCoord::new(10, 10));
        assert_eq!(sub.len(), 25);
        assert!(sub.contains(CellCoord::new(8, 8)));
        assert!(sub.contains(CellCoord::new(12, 12)));
        assert!(!sub.contains(CellCoord::new(7, 10)));
    }

    #[test]
    fn left_edge_is_clipped() {
        let cfg = WorldConfig::default();
        let sub = Subscription::at_center(&cfg, CellCoord::new(0, 10));
        // x spans 0..=2, y spans 8..=12.
        assert_eq!(sub.len(), 15);
        assert!(sub.contains(CellCoord::new(0, 8)));
        assert!(!sub.contains(CellCoord::new(3, 10)));
    }

    #[test]
    fn far_corner_is_clipped_on_both_axes() {
        let cfg = WorldConfig::default();
        let last = (cfg.cells_per_axis() - 1) as u16;
        let sub = Subscription::at_center(&cfg, CellCoord::new(last, last));
        assert_eq!(sub.len(), 9);
        assert!(sub.contains(CellCoord::new(last, last)));
    }

    #[test]
    fn origin_corner_is_clipped_on_both_axes() {
        let cfg = WorldConfig::default();
        let sub = Subscription::at_center(&cfg, CellCoord::new(0, 0));
        assert_eq!(sub.len(), 9);
    }

    #[test]
    fn cells_are_row_major() {
        let cfg = WorldConfig::default();
        let cells: Vec<_> =
            Subscription::at_center(&cfg, CellCoord::new(10, 10)).cells().collect();
        assert_eq!(cells[0], CellCoord::new(8, 8));
        assert_eq!(cells[1], CellCoord::new(9, 8));
        assert_eq!(cells[5], CellCoord::new(8, 9));
        assert_eq!(cells[24], CellCoord::new(12, 12));
    }

    #[test]
    fn cell_count_matches_len() {
        let cfg = WorldConfig::default();
        for y in 0..cfg.cells_per_axis() as u16 {
            for x in 0..cfg.cells_per_axis() as u16 {
                let sub = Subscription::at_center(&cfg, CellCoord::new(x, y));
                assert_eq!(sub.cells().count(), sub.len());
            }
        }
    }

    #[test]
    fn membership_matches_chebyshev_distance() {
        let cfg = WorldConfig::default();
        let radius = cfg.cell_radius() as u16;
        let center = CellCoord::new(10, 10);
        let sub = Subscription::at_center(&cfg, center);
        for y in 0..cfg.cells_per_axis() as u16 {
            for x in 0..cfg.cells_per_axis() as u16 {
                let c = CellCoord::new(x, y);
                assert_eq!(sub.contains(c), center.chebyshev(c) <= radius);
            }
        }
    }

    #[test]
    fn set_from_subscription_matches_cells() {
        let cfg = WorldConfig::default();
        let sub = Subscription::at_center(&cfg, CellCoord::new(10, 10));
        let set = CellList::from(sub);
        let cells: Vec<_> = sub.cells().collect();
        assert_eq!(set.as_slice(), cells.as_slice());
    }

    #[test]
    fn set_never_exceeds_capacity() {
        let cfg = WorldConfig::default();
        for y in 0..cfg.cells_per_axis() as u16 {
            for x in 0..cfg.cells_per_axis() as u16 {
                let set =
                    CellList::from(Subscription::at_center(&cfg, CellCoord::new(x, y)));
                assert!(set.len() <= MAX_SUB_GRID_CELLS);
            }
        }
    }

    // Every cell in the region within `cell_radius` of `center`, in row-major
    /// order. Scans the whole region rather than computing bounds, so it shares no
    /// logic with `Subscription::at_center`.
    fn brute_force_cells(cfg: &WorldConfig, center: CellCoord) -> Vec<CellCoord> {
        let axis = cfg.cells_per_axis() as u16;
        let radius = cfg.cell_radius() as u16;
        let mut out = Vec::new();
        for y in 0..axis {
            for x in 0..axis {
                let c = CellCoord::new(x, y);
                if center.chebyshev(c) <= radius {
                    out.push(c);
                }
            }
        }
        out
    }

    /// Every cell in the region, visited in single steps: left to right on even
    /// rows, right to left on odd rows, so consecutive entries are always
    /// adjacent.
    fn snake_path(axis: u16) -> Vec<CellCoord> {
        let mut path = Vec::with_capacity(axis as usize * axis as usize);
        for y in 0..axis {
            if y % 2 == 0 {
                for x in 0..axis {
                    path.push(CellCoord::new(x, y));
                }
            } else {
                for x in (0..axis).rev() {
                    path.push(CellCoord::new(x, y));
                }
            }
        }
        path
    }

    #[test]
    fn snake_path_visits_every_cell_in_single_steps() {
        let cfg = WorldConfig::default();
        let axis = cfg.cells_per_axis() as u16;
        let path = snake_path(axis);

        assert_eq!(path.len(), axis as usize * axis as usize);
        for pair in path.windows(2) {
            assert!(
                pair[0].is_adjacent_move(pair[1]),
                "path step {:?} -> {:?} is not a single move",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn subscription_matches_brute_force_along_a_path() {
        let cfg = WorldConfig::default();
        let axis = cfg.cells_per_axis() as u16;

        for center in snake_path(axis) {
            let sub = Subscription::at_center(&cfg, center);
            let actual: Vec<_> = sub.cells().collect();
            let expected = brute_force_cells(&cfg, center);
            assert_eq!(actual, expected, "mismatch at {center:?}");
        }
    }

    #[test]
    fn subscription_always_contains_its_own_cell() {
        let cfg = WorldConfig::default();
        let axis = cfg.cells_per_axis() as u16;

        for center in snake_path(axis) {
            let sub = Subscription::at_center(&cfg, center);
            assert!(sub.contains(center), "{center:?} not in its own subscription");
        }
    }

    #[test]
    fn bounds_shift_by_at_most_one_per_single_step() {
        // Precondition for a strip-based delta: a single-cell move changes each
        // bound by 0 or 1, so at most one row and one column enter or leave.
        let cfg = WorldConfig::default();
        let axis = cfg.cells_per_axis() as u16;
        let path = snake_path(axis);

        for pair in path.windows(2) {
            let a = Subscription::at_center(&cfg, pair[0]);
            let b = Subscription::at_center(&cfg, pair[1]);

            assert!((b.x0 - a.x0).abs() <= 1, "x0 jumped: {a:?} -> {b:?}");
            assert!((b.x1 - a.x1).abs() <= 1, "x1 jumped: {a:?} -> {b:?}");
            assert!((b.y0 - a.y0).abs() <= 1, "y0 jumped: {a:?} -> {b:?}");
            assert!((b.y1 - a.y1).abs() <= 1, "y1 jumped: {a:?} -> {b:?}");
        }
    }

    #[test]
    fn every_cell_is_reachable_from_some_subscription() {
        // The union of all subscriptions along the path covers the region. Fails
        // if clipping drops cells that should remain reachable.
        let cfg = WorldConfig::default();
        let axis = cfg.cells_per_axis() as u16;
        let total = axis as usize * axis as usize;

        let mut seen = vec![false; total];
        for center in snake_path(axis) {
            for c in Subscription::at_center(&cfg, center).cells() {
                seen[cfg.cell_id(c).index()] = true;
            }
        }

        assert!(seen.iter().all(|&s| s), "some cells were never subscribed");
    }

    #[test]
    fn diagonal_step_changes_both_axes() {
        // The case a strip delta handles specially: a row and a column change at
        // once, overlapping in one corner cell.
        let cfg = WorldConfig::default();
        let a = Subscription::at_center(&cfg, CellCoord::new(10, 10));
        let b = Subscription::at_center(&cfg, CellCoord::new(11, 11));

        assert_eq!(b.x0 - a.x0, 1);
        assert_eq!(b.x1 - a.x1, 1);
        assert_eq!(b.y0 - a.y0, 1);
        assert_eq!(b.y1 - a.y1, 1);

        let before: Vec<_> = a.cells().collect();
        let after: Vec<_> = b.cells().collect();
        let entered = after.iter().filter(|c| !a.contains(**c)).count();
        let exited = before.iter().filter(|c| !b.contains(**c)).count();

        // A 5x5 block moving one step diagonally: 5 + 5 - 1 cells each way.
        assert_eq!(entered, 9);
        assert_eq!(exited, 9);
    }
}
