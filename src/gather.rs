//! The gather pass and its output.
//!
//! The pass itself is [`CellSnapshot::gather_into`]. Gathering
//! is the act of fetching cell and entity candidates from a
//! portion of the world. There are different gathers for different
//! purposes.

use crate::entity::EntityId;
use crate::fixed::DistSq;
use crate::pos::{CellCoord, Pos2, Pos3};
use crate::snapshot::{CellOccupants, CellSnapshot};
use crate::subscription::Subscription;

/// One entity within a viewer's range, and its distance from that viewer.
///
/// The distance is squared. Ordering survives squaring, so comparison and
/// sorting need no square root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiscoveredEntity {
    /// Which entity.
    pub id: EntityId,
    /// Index into the entity arrays of the snapshot this was gathered from.
    /// Reading the position needs no id lookup. Invalidated by the next
    /// [`CellSnapshot::update`].
    pub snapshot_index: u32,
    /// Squared distance from the viewer.
    pub dist_sq: DistSq,
}

impl DiscoveredEntity {
    /// Create a discovered entity from the three parts a cell 
    /// traversal already has in hand.
    #[inline]
    pub const fn new(
        id: EntityId,
        snapshot_index: u32,
        dist_sq: DistSq,
    ) -> DiscoveredEntity {
        DiscoveredEntity { id, snapshot_index, dist_sq }
    }
}

/// Entities gathered for one viewer, in cell-walk order.
///
/// Cell-walk order is not relevance order. Callers needing the nearest entities
/// must sort or select.
///
/// Held per worker thread and reused across viewers. [`clear`](Self::clear)
/// retains the allocation.
///
/// 128-byte alignment to avoid invalidating caches on other cores when processing
/// in parallel.
#[repr(align(128))]
#[derive(Debug, Clone, Default)]
pub struct DiscoveredEntities {
    items: Vec<DiscoveredEntity>,
}

impl DiscoveredEntities {
    /// Empty, allocating nothing until something is pushed.
    pub fn new() -> DiscoveredEntities {
        DiscoveredEntities { items: Vec::new() }
    }

    /// Empty, with room for `n` before it grows. What a worker reuses.
    pub fn with_capacity(n: usize) -> DiscoveredEntities {
        DiscoveredEntities { items: Vec::with_capacity(n) }
    }

    /// Appends one, keeping cell-walk order.
    #[inline]
    pub fn push(&mut self, found: DiscoveredEntity) {
        self.items.push(found);
    }

    /// Empties the buffer without releasing its allocation.
    #[inline]
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// How many were gathered.
    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the walk found nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Entries the buffer holds before it reallocates.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.items.capacity()
    }

    /// What was gathered, in cell-walk order.
    #[inline]
    pub fn as_slice(&self) -> &[DiscoveredEntity] {
        &self.items
    }

    /// The same, for a caller that sorts in place.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [DiscoveredEntity] {
        &mut self.items
    }

    /// Iterator over what was gathered, in cell-walk order.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, DiscoveredEntity> {
        self.items.iter()
    }
}

impl<'a> IntoIterator for &'a DiscoveredEntities {
    type Item = &'a DiscoveredEntity;
    type IntoIter = core::slice::Iter<'a, DiscoveredEntity>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl CellSnapshot {
    /// Appends the entities within view of `viewer` to `out`.
    ///
    /// Walks every cell in `sub`. An entity is in view when its horizontal
    /// distance from `viewer` is within `horizontal_view_radius`. The recorded
    /// distance is the full 3D separation.
    ///
    /// The horizontal test matches the 2D cell grid, so the region in view is a
    /// vertical cylinder. Height affects the recorded distance, not membership.
    ///
    /// Does not clear `out`. Does not exclude the viewer's own entity.
    pub fn gather_into(
        &self,
        viewer: Pos3,
        sub: Subscription,
        out: &mut DiscoveredEntities,
    ) {
        self.gather_into_capped(viewer, sub, usize::MAX, out);
    }

    /// [`Self::gather_into`], stopping once `out` holds `cap` entries.
    ///
    /// Cells are visited outward from the viewer's own cell, and a subdivided
    /// cell is visited by sub-cell outward from the viewer's own sub-cell, so a
    /// caller that stops early keeps the nearer entities.
    ///
    /// `cap` counts everything in `out`, including entries present before the
    /// call. It gets checked at cell and sub-cell boundaries rather than per
    /// entity, so `out` may exceed it by up to the population of the last cell
    /// or sub-cell walked.
    ///
    /// The walk enumerates offsets from the viewer's own cell and rejects
    /// anything outside `sub`, which covers `sub` exactly when it was built by
    /// `Subscription::at_center` on the viewer's cell.
    pub fn gather_into_capped(
        &self,
        viewer: Pos3,
        sub: Subscription,
        cap: usize,
        out: &mut DiscoveredEntities,
    ) {
        let cfg = self.config();
        let radius_sq = DistSq::from_radius(cfg.horizontal_view_radius());
        let viewer_h = viewer.horizontal();
        let center = cfg.cell_of(viewer_h);
        debug_assert!(
            sub.contains(center),
            "subscription does not contain the viewer's own cell"
        );

        for &(dx, dy) in self.cell_order() {
            let cx = center.x as i32 + dx as i32;
            let cy = center.y as i32 + dy as i32;
            if cx < sub.x0 || cx > sub.x1 || cy < sub.y0 || cy > sub.y1 {
                continue;
            }
            let coord = CellCoord::new(cx as u16, cy as u16);
            let cid = cfg.cell_id(coord);

            match self.sub_cells(cid) {
                Some(grid) => {
                    for &b in self.sub_cell_order(coord, viewer_h) {
                        take(
                            viewer,
                            viewer_h,
                            radius_sq,
                            grid.occupants_at(b as usize),
                            out,
                        );
                        if out.len() >= cap {
                            return;
                        }
                    }
                }
                None => {
                    take(viewer, viewer_h, radius_sq, self.entities_for_cell(cid), out);
                    if out.len() >= cap {
                        return;
                    }
                }
            }
        }
    }
}

// Bench shows inline(always) rather than inline. Regular inline is 9%
// slower than always.

/// Tests one run of entities against the view radius and appends survivors.
#[inline(always)]
fn take(
    viewer: Pos3,
    viewer_h: Pos2,
    radius_sq: DistSq,
    entities: CellOccupants<'_>,
    out: &mut DiscoveredEntities,
) {
    for i in 0..entities.len() {
        // The horizontal terms are computed twice: once here, once in
        // the 3D distance below.
        if viewer_h.dist_sq(entities.horizontal(i)) > radius_sq {
            continue;
        }
        out.push(DiscoveredEntity::new(
            entities.ids[i],
            entities.snapshot_index(i),
            viewer.dist_sq(entities.pos(i)),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(id: u32, radius_m: i32) -> DiscoveredEntity {
        DiscoveredEntity::new(
            EntityId::from_raw(id),
            0,
            DistSq::from_radius(crate::fixed::Fixed::from_meters(radius_m)),
        )
    }

    #[test]
    fn record_is_sixteen_bytes() {
        // 4 bytes of id, 4 of snapshot index, 8 of squared distance. The index
        // occupies padding that DistSq's 8-byte alignment already required.
        assert_eq!(size_of::<DiscoveredEntity>(), 16);
    }

    #[test]
    fn buffers_do_not_share_a_cache_line() {
        let v: Vec<DiscoveredEntities> =
            (0..4).map(|_| DiscoveredEntities::new()).collect();
        for w in v.windows(2) {
            let a = &w[0] as *const _ as usize;
            let b = &w[1] as *const _ as usize;
            assert!(b - a >= 128, "adjacent buffers are {} bytes apart", b - a);
        }
    }

    #[test]
    fn clear_keeps_the_allocation() {
        let mut d = DiscoveredEntities::with_capacity(256);
        let before = d.capacity();
        for i in 0..200 {
            d.push(found(i, 10));
        }
        d.clear();
        assert!(d.is_empty());
        assert_eq!(d.capacity(), before, "clear must not release the buffer");
    }

    #[test]
    fn walk_order_is_preserved() {
        let mut d = DiscoveredEntities::new();
        d.push(found(7, 200));
        d.push(found(2, 10));
        d.push(found(5, 100));

        let ids: Vec<u32> = d.iter().map(|e| e.id.raw()).collect();
        assert_eq!(
            ids,
            vec![7, 2, 5],
            "gather order is cell-walk order, not distance order"
        );
    }

    #[test]
    fn squared_distance_preserves_ordering() {
        let near = found(1, 10);
        let far = found(2, 100);
        assert!(near.dist_sq < far.dist_sq);
    }

    #[test]
    fn reuse_across_viewers_does_not_grow() {
        let mut d = DiscoveredEntities::with_capacity(512);
        let before = d.capacity();
        for _viewer in 0..1000 {
            d.clear();
            for i in 0..185 {
                d.push(found(i, 50));
            }
        }
        assert_eq!(d.capacity(), before);
    }
    // -- gather ------------------------------------------------------------

    use crate::config::WorldConfig;
    use crate::entity::LiveSet;
    use crate::pos::Pos3;
    use crate::snapshot::CellSnapshot;
    use crate::subscription::Subscription;

    fn all_live(n: usize) -> LiveSet {
        let mut l = LiveSet::with_capacity(n);
        for i in 0..n {
            l.insert(EntityId::from_raw(i as u32));
        }
        l
    }

    /// Sorts `pts` into a snapshot. Entity id is the index into `pts`.
    fn snapshot_of(cfg: &WorldConfig, pts: &[Pos3]) -> CellSnapshot {
        let xs: Vec<_> = pts.iter().map(|p| p.x).collect();
        let ys: Vec<_> = pts.iter().map(|p| p.y).collect();
        let zs: Vec<_> = pts.iter().map(|p| p.z).collect();
        let tags = vec![0u16; xs.len()];
        let mut snap = CellSnapshot::new(cfg);
        snap.update(&xs, &ys, &zs, &tags, &all_live(xs.len()));
        snap
    }

    fn run(cfg: &WorldConfig, pts: &[Pos3], viewer: Pos3) -> DiscoveredEntities {
        let snap = snapshot_of(cfg, pts);
        let sub = Subscription::at_center(cfg, cfg.cell_of(viewer.horizontal()));
        let mut out = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut out);
        out
    }

    fn ids(out: &DiscoveredEntities) -> Vec<u32> {
        let mut v: Vec<u32> = out.iter().map(|e| e.id.raw()).collect();
        v.sort_unstable();
        v
    }

    // -- ordered walk and cap -------------------------------------------

    /// `n` entities scattered inside the cell containing `m` meters.
    fn crowd(cfg: &WorldConfig, n: usize, m: i32, seed: u64) -> Vec<Pos3> {
        let cell = cfg.cell_size().raw() as u32;
        let origin = (crate::fixed::Fixed::from_meters(m).raw() as u32) & !(cell - 1);
        let mut s = seed;
        let mut next = move |bound: u32| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u32 % bound
        };
        (0..n)
            .map(|_| {
                Pos3::new(
                    crate::fixed::Fixed::from_raw((origin + next(cell)) as i32),
                    crate::fixed::Fixed::from_raw((origin + next(cell)) as i32),
                    crate::fixed::Fixed::ZERO,
                )
            })
            .collect()
    }

    #[test]
    fn ragged_crowd_is_handled() {
        // 10,000 is not a power of two, unlike cell size, sub-grid axis, and
        // cell shift. Partial sub-cells and a cap that does not divide evenly.
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 10_000, 2048, 0x7A99);
        let viewer = pts[0];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut full = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut full);

        let radius_sq = DistSq::from_radius(cfg.horizontal_view_radius());
        let expected = pts
            .iter()
            .filter(|p| viewer.horizontal().dist_sq(p.horizontal()) <= radius_sq)
            .count();
        assert_eq!(full.len(), expected, "ragged population must still be found in full");

        for &cap in &[7usize, 333, 1_001, 9_999] {
            let mut out = DiscoveredEntities::new();
            snap.gather_into_capped(viewer, sub, cap, &mut out);
            assert!(
                out.len() >= cap.min(expected),
                "cap {cap} under-filled at {}",
                out.len()
            );
            assert!(out.len() <= expected, "cap {cap} over-filled at {}", out.len());
        }
    }

    #[test]
    fn uncapped_gather_matches_a_brute_force_scan() {
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 3000, 2048, 0x1111);
        let viewer = pts[0];
        let out = run(&cfg, &pts, viewer);

        let radius_sq = DistSq::from_radius(cfg.horizontal_view_radius());
        let mut expected: Vec<u32> = pts
            .iter()
            .enumerate()
            .filter(|(_, p)| viewer.horizontal().dist_sq(p.horizontal()) <= radius_sq)
            .map(|(i, _)| i as u32)
            .collect();
        expected.sort_unstable();
        assert_eq!(ids(&out), expected, "the ordered walk must not change what is found");
    }

    #[test]
    fn cap_limits_the_output() {
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 4000, 2048, 0x2222);
        let viewer = pts[0];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut out = DiscoveredEntities::new();
        snap.gather_into_capped(viewer, sub, 100, &mut out);
        assert!(out.len() >= 100, "cap should be reached, got {}", out.len());
        assert!(
            out.len() < 600,
            "overshoot is bounded by one sub-cell, got {}",
            out.len()
        );
    }

    #[test]
    fn cap_keeps_the_nearest() {
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 4000, 2048, 0x3333);
        let viewer = pts[0];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut full = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut full);
        let mut capped = DiscoveredEntities::new();
        snap.gather_into_capped(viewer, sub, 200, &mut capped);

        let worst_kept = capped.iter().map(|e| e.dist_sq).max().unwrap();
        let worst_overall = full.iter().map(|e| e.dist_sq).max().unwrap();
        assert!(worst_kept < worst_overall, "the cap must drop the far entities");

        let nearest = full.iter().map(|e| e.dist_sq).min().unwrap();
        assert!(
            capped.iter().any(|e| e.dist_sq == nearest),
            "the nearest entity must survive the cap"
        );
    }

    #[test]
    fn cap_counts_entries_already_present() {
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 4000, 2048, 0x4444);
        let viewer = pts[0];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut out = DiscoveredEntities::new();
        snap.gather_into_capped(viewer, sub, 300, &mut out);
        let first = out.len();

        // Already over the cap, so a second call adds at most one sub-cell.
        snap.gather_into_capped(viewer, sub, 300, &mut out);
        assert!(
            out.len() - first < 600,
            "a call starting over the cap should stop almost immediately"
        );
    }

    #[test]
    fn snapshot_index_resolves_in_an_undivided_cell() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        let pts: Vec<Pos3> =
            (0..40).map(|i| Pos3::from_meters(2048 + i * 5, 2048 + i * 3, i)).collect();
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));
        let mut out = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut out);

        assert!(!out.is_empty());
        for e in out.iter() {
            let i = e.snapshot_index as usize;
            assert_eq!(
                snap.id_at(i),
                e.id,
                "index must name the entity it was found with"
            );
            assert_eq!(
                snap.pos_at(i),
                pts[e.id.index()],
                "index must reach that entity's position"
            );
        }
    }

    #[test]
    fn snapshot_index_resolves_in_a_subdivided_cell() {
        let cfg = WorldConfig::default();
        let pts = crowd(&cfg, 3000, 2048, 0x6666);
        let viewer = pts[0];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));
        assert!(
            snap.sub_cells(cfg.cell_id(cfg.cell_of(viewer.horizontal()))).is_some(),
            "test is meaningless unless the cell is subdivided"
        );

        let mut out = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut out);

        assert!(!out.is_empty());
        for e in out.iter() {
            let i = e.snapshot_index as usize;
            assert_eq!(
                snap.id_at(i),
                e.id,
                "index must name the entity it was found with"
            );
            assert_eq!(
                snap.pos_at(i),
                pts[e.id.index()],
                "index must reach that entity's position"
            );
        }
    }

    #[test]
    fn cells_are_visited_nearest_first() {
        let cfg = WorldConfig::default();
        // One entity in the viewer's own cell, one two cells away but in range.
        let pts = [Pos3::from_meters(2100, 2050, 0), Pos3::from_meters(2300, 2050, 0)];
        let viewer = Pos3::from_meters(2060, 2050, 0);
        let out = run(&cfg, &pts, viewer);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out.as_slice()[0].id.raw(),
            0,
            "the entity in the viewer's own cell must come first"
        );
    }

    #[test]
    fn keeps_entities_inside_the_radius() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        // 200 m away against a 256 m radius.
        let pts = [Pos3::from_meters(2248, 2048, 0)];
        assert_eq!(ids(&run(&cfg, &pts, viewer)), vec![0]);
    }

    #[test]
    fn drops_subscribed_cells_that_are_out_of_range() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        // Cell 18 on both axes, inside the 14..=18 subscription block, with a
        // diagonal separation past 256 m.
        let corner = Pos3::from_meters(2348, 2348, 0);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));
        assert!(
            sub.contains(cfg.cell_of(corner.horizontal())),
            "test is meaningless unless the cell is subscribed"
        );
        assert!(run(&cfg, &[corner], viewer).is_empty());
    }

    #[test]
    fn unsubscribed_cells_are_never_walked() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        let pts = [Pos3::from_meters(100, 100, 0)];
        assert!(run(&cfg, &pts, viewer).is_empty());
    }

    #[test]
    fn height_alone_does_not_exclude() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        // 10 m away horizontally, 500 m overhead.
        let pts = [Pos3::from_meters(2058, 2048, 500)];
        assert_eq!(ids(&run(&cfg, &pts, viewer)), vec![0]);
    }

    #[test]
    fn recorded_distance_is_three_dimensional() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        // 3-4-12 offset, so the full separation is exactly 13 m.
        let pts = [Pos3::from_meters(2051, 2052, 12)];
        let out = run(&cfg, &pts, viewer);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.as_slice()[0].dist_sq,
            DistSq::from_radius(crate::fixed::Fixed::from_meters(13))
        );
    }

    #[test]
    fn appends_rather_than_replacing() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        let pts = [Pos3::from_meters(2148, 2048, 0)];
        let snap = snapshot_of(&cfg, &pts);
        let sub = Subscription::at_center(&cfg, cfg.cell_of(viewer.horizontal()));

        let mut out = DiscoveredEntities::new();
        snap.gather_into(viewer, sub, &mut out);
        snap.gather_into(viewer, sub, &mut out);
        assert_eq!(out.len(), 2, "gather must not clear; the caller owns that");
    }

    #[test]
    fn finds_everything_in_a_crowded_cell() {
        let cfg = WorldConfig::default();
        let viewer = Pos3::from_meters(2048, 2048, 0);
        let pts: Vec<Pos3> =
            (0..5000).map(|i| Pos3::from_meters(2048 + (i % 50), 2048, 0)).collect();
        assert_eq!(run(&cfg, &pts, viewer).len(), 5000);
    }
}
