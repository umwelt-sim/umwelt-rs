//! Where an observer's view reaches past its region, and where its shadow
//! stands in the neighbor (`docs/adr/0010`).

use crate::config::WorldConfig;
use crate::map::Placement;
use crate::pos::{Pos3, WorldPos};

/// The squares around `square` that an observer at `local` sees into, by the
/// rule the region uses to report a view collision: its subscription box,
/// `cell_radius` cells each way, reaches past the region on that side. Across
/// one side, or across two sides and the corner between them.
pub(crate) fn squares_in_view(
    cfg: &WorldConfig,
    square: Placement,
    local: Pos3,
) -> Vec<Placement> {
    let c = cfg.cell_of(local.horizontal());
    let r = cfg.cell_radius() as i32;
    let last = cfg.cells_per_axis() as i32 - 1;
    let (cx, cy) = (c.x as i32, c.y as i32);
    let east = cx + r > last;
    let west = cx - r < 0;
    let north = cy + r > last;
    let south = cy - r < 0;
    let (col, row) = (square.col, square.row);
    let mut out = Vec::with_capacity(3);
    if east {
        out.push(Placement::new(col + 1, row));
    }
    if west {
        out.push(Placement::new(col - 1, row));
    }
    if north {
        out.push(Placement::new(col, row + 1));
    }
    if south {
        out.push(Placement::new(col, row - 1));
    }
    if east && north {
        out.push(Placement::new(col + 1, row + 1));
    }
    if east && south {
        out.push(Placement::new(col + 1, row - 1));
    }
    if west && north {
        out.push(Placement::new(col - 1, row + 1));
    }
    if west && south {
        out.push(Placement::new(col - 1, row - 1));
    }
    out
}

/// Where a shadow stands in `neighbor` for an observer at `owner`: the
/// owner's position in the neighbor's frame, held inside the neighbor's box,
/// which puts it on the seam nearest the owner. `None` if the owner is too
/// far from the neighbor for its frame to hold the number at all.
pub(crate) fn shadow_point(
    cfg: &WorldConfig,
    owner: WorldPos,
    neighbor: Placement,
) -> Option<Pos3> {
    owner.to_local(neighbor, cfg.region_size()).map(|at| cfg.clamp(at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::Fixed;

    fn cfg() -> WorldConfig {
        WorldConfig::default()
    }

    #[test]
    fn the_middle_sees_into_no_square() {
        let at = Placement::new(3, 3);
        assert!(squares_in_view(&cfg(), at, Pos3::from_meters(2048, 2048, 0)).is_empty());
    }

    #[test]
    fn near_a_side_it_sees_into_one_square_and_near_a_corner_three() {
        let c = cfg();
        let at = Placement::new(3, 3);
        let cell = c.cell_size().raw();
        let near_east = Pos3::new(
            Fixed::from_raw(c.region_size().raw() - cell - cell / 2),
            Fixed::from_meters(2048),
            Fixed::ZERO,
        );
        assert_eq!(squares_in_view(&c, at, near_east), vec![Placement::new(4, 3)]);
        let near_south_west = Pos3::from_meters(10, 10, 0);
        assert_eq!(
            squares_in_view(&c, at, near_south_west),
            vec![Placement::new(2, 3), Placement::new(3, 2), Placement::new(2, 2)]
        );
    }

    #[test]
    fn a_shadow_stands_on_the_seam_nearest_its_owner() {
        let c = cfg();
        let size = c.region_size();
        // 100 m west of the seam with square (1, 0), 200 m north, 3 m up.
        let owner = WorldPos::from_local(
            Pos3::new(
                Fixed::from_raw(size.raw() - Fixed::from_meters(100).raw()),
                Fixed::from_meters(200),
                Fixed::from_meters(3),
            ),
            Placement::new(0, 0),
            size,
        );
        let point = shadow_point(&c, owner, Placement::new(1, 0)).expect("fits");
        assert_eq!(
            point,
            Pos3::new(Fixed::ZERO, Fixed::from_meters(200), Fixed::from_meters(3))
        );
        // Across a corner, both axes are held.
        let point = shadow_point(&c, owner, Placement::new(1, -1)).expect("fits");
        assert_eq!(point.x, Fixed::ZERO);
        assert_eq!(point.y.raw(), size.raw() - 1);
    }
}
