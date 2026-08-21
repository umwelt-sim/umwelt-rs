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
use crate::pos::{CellId, Pos2, Pos3};

/// One cell's entities. Parallel slices, all of the same length.
#[derive(Debug, Clone, Copy)]
pub struct CellOccupants<'a> {
    /// Ascending by entity id.
    pub ids: &'a [EntityId],
    pub xs: &'a [Fixed],
    pub ys: &'a [Fixed],
    pub zs: &'a [Fixed],
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
}

impl CellSnapshot {
    /// Allocates the offset table for a region's cell count. Entity capacity
    /// grows on the first sort.
    pub fn new(cfg: &WorldConfig) -> CellSnapshot {
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
        }
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
        }
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

        // Pass 1: tally into starts[c + 1], so the running total below lands on
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
    }
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
