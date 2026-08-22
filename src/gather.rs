//! The gather pass and its output.
//!
//! The pass itself is [`CellSnapshot::gather_into`]; a gather is a query against
//! a snapshot, so the snapshot is the receiver.

use crate::entity::EntityId;
use crate::fixed::DistSq;
use crate::snapshot::CellSnapshot;
use crate::pos::Pos3;
use crate::subscription::Subscription;

/// One entity within a viewer's range, and its distance from that viewer.
///
/// The distance is squared. Ordering survives squaring, so comparison and
/// sorting need no square root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiscoveredEntity {
    pub id: EntityId,
    pub dist_sq: DistSq,
}

impl DiscoveredEntity {
    #[inline(always)]
    pub const fn new(id: EntityId, dist_sq: DistSq) -> DiscoveredEntity {
        DiscoveredEntity { id, dist_sq }
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
    pub fn new() -> DiscoveredEntities {
        DiscoveredEntities { items: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> DiscoveredEntities {
        DiscoveredEntities { items: Vec::with_capacity(n) }
    }

    #[inline(always)]
    pub fn push(&mut self, found: DiscoveredEntity) {
        self.items.push(found);
    }

    /// Empties the buffer without releasing its allocation.
    #[inline(always)]
    pub fn clear(&mut self) {
        self.items.clear();
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Entries the buffer holds before it reallocates.
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.items.capacity()
    }

    #[inline(always)]
    pub fn as_slice(&self) -> &[DiscoveredEntity] {
        &self.items
    }

    #[inline(always)]
    pub fn as_mut_slice(&mut self) -> &mut [DiscoveredEntity] {
        &mut self.items
    }

    #[inline(always)]
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
        let cfg = self.config();
        let radius_sq = DistSq::from_radius(cfg.horizontal_view_radius());
        let viewer_h = viewer.horizontal();

        for coord in sub.cells() {
            let entities = self.entities_for_cell(cfg.cell_id(coord));
            for i in 0..entities.len() {
                // The horizontal terms are computed twice: once here, once in
                // the 3D distance below.
                if viewer_h.dist_sq(entities.horizontal(i)) > radius_sq {
                    continue;
                }
                out.push(DiscoveredEntity::new(
                    entities.ids[i],
                    viewer.dist_sq(entities.pos(i)),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(id: u32, radius_m: i32) -> DiscoveredEntity {
        DiscoveredEntity::new(
            EntityId::from_raw(id),
            DistSq::from_radius(crate::fixed::Fixed::from_meters(radius_m)),
        )
    }

    #[test]
    fn record_is_sixteen_bytes() {
        // 4 bytes of id and 8 of squared distance, padded to DistSq's
        // 8-byte alignment.
        assert_eq!(size_of::<DiscoveredEntity>(), 16);
    }

    #[test]
    fn buffers_do_not_share_a_cache_line() {
        let v: Vec<DiscoveredEntities> = (0..4).map(|_| DiscoveredEntities::new()).collect();
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
        assert_eq!(ids, vec![7, 2, 5], "gather order is cell-walk order, not distance order");
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
    use crate::snapshot::CellSnapshot;
    use crate::pos::Pos3;
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
        let mut snap = CellSnapshot::new(cfg);
        snap.update(&xs, &ys, &zs, &all_live(xs.len()));
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
