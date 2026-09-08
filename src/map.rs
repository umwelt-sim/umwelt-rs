//! The world map: which region sits where, and nothing else.
//!
//! A [`WorldMap`] is a set of placements, each a region and the square it
//! fills on a grid of region-sized squares. It is written by whatever starts
//! regions and read by every edge once at startup (`docs/adr/0010`). A region
//! never reads it, and a region that is not in it is its own frame with no
//! neighbors.
//!
//! A region fills its whole square, so placements do not overlap and no side
//! is split: each of a region's four sides faces one square, and every
//! boundary coordinate along that side is adjacent to the same region.
//!
//! On the wire the map is a plain array of bytes, encoded the way every other
//! wire struct in this crate is: four bytes of format version, then one
//! 12-byte record per placement holding the region as a `u32` and the square's
//! column and row as two `i32`, all little-endian. No count, no name, no
//! envelope.

use core::fmt;
use std::collections::HashMap;

use crate::fixed::Fixed;
use crate::id::RegionId;
use crate::pos::{Pos3, WorldPos};

/// Which square of the grid a region fills.
///
/// Columns run east and rows run north, in units of one region size, from a
/// square somebody chose to call `(0, 0)`. Negative values are fine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    /// East of the origin square, in squares.
    pub col: i32,
    /// North of the origin square, in squares.
    pub row: i32,
}

impl Placement {
    /// A square by column and row.
    #[inline]
    pub const fn new(col: i32, row: i32) -> Placement {
        Placement { col, row }
    }

    /// The square across one side of this one.
    #[inline]
    pub const fn beside(self, side: Side) -> Placement {
        match side {
            Side::East => Placement::new(self.col.wrapping_add(1), self.row),
            Side::West => Placement::new(self.col.wrapping_sub(1), self.row),
            Side::North => Placement::new(self.col, self.row.wrapping_add(1)),
            Side::South => Placement::new(self.col, self.row.wrapping_sub(1)),
        }
    }
}

impl fmt::Display for Placement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "square ({}, {})", self.col, self.row)
    }
}

/// One of a square's four sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// Toward larger `x`.
    East,
    /// Toward smaller `x`.
    West,
    /// Toward larger `y`.
    North,
    /// Toward smaller `y`.
    South,
}

/// Why a map was refused.
#[derive(Debug)]
pub enum MapError {
    /// The bytes are not four plus a multiple of twelve.
    BadLength(usize),
    /// A format version this build does not read.
    UnknownVersion(u32),
    /// A region placed twice.
    DuplicateRegion(RegionId),
    /// Two regions in one square.
    DuplicateSquare(Placement),
    /// A region id with the top bit set, which a composed entity name cannot
    /// carry (`docs/adr/0010`).
    RegionIdTooWide(RegionId),
    /// The broker could not be read or written.
    Broker(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::BadLength(n) => write!(f, "a map of {n} bytes is not 4 + 12n"),
            MapError::UnknownVersion(v) => {
                write!(f, "map format version {v} is not read here")
            }
            MapError::DuplicateRegion(r) => write!(f, "{r} is placed twice"),
            MapError::DuplicateSquare(s) => write!(f, "{s} holds two regions"),
            MapError::RegionIdTooWide(r) => write!(f, "{r} does not fit 31 bits"),
            MapError::Broker(e) => write!(f, "map on the broker: {e}"),
        }
    }
}

impl std::error::Error for MapError {}

/// Which region sits where.
///
/// Immutable once built or decoded. An edge reads one at startup and keeps it
/// for its lifetime; a changed map reaches edges by restarting them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldMap {
    placements: Vec<(RegionId, Placement)>,
    by_region: HashMap<RegionId, Placement>,
    by_square: HashMap<Placement, RegionId>,
}

impl WorldMap {
    /// The format version this build writes and reads.
    pub const VERSION: u32 = 1;
    /// The JetStream key-value bucket the map lives in.
    pub const BUCKET: &'static str = "umwelt";
    /// The key within that bucket.
    pub const KEY: &'static str = "map";
    /// Bytes per placement on the wire.
    const RECORD: usize = 12;

    /// A map with nothing on it.
    pub fn new() -> WorldMap {
        WorldMap::default()
    }

    /// Places a region. Refused if the region or the square is already taken,
    /// or if the region's id does not fit 31 bits.
    pub fn place(&mut self, region: RegionId, at: Placement) -> Result<(), MapError> {
        if region.raw() >> 31 != 0 {
            return Err(MapError::RegionIdTooWide(region));
        }
        if self.by_region.contains_key(&region) {
            return Err(MapError::DuplicateRegion(region));
        }
        if self.by_square.contains_key(&at) {
            return Err(MapError::DuplicateSquare(at));
        }
        self.placements.push((region, at));
        self.by_region.insert(region, at);
        self.by_square.insert(at, region);
        Ok(())
    }

    /// How many regions are placed.
    pub fn len(&self) -> usize {
        self.placements.len()
    }

    /// Whether nothing is placed, which is how an edge with no map starts.
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty()
    }

    /// Every placement, in the order written.
    pub fn iter(&self) -> impl Iterator<Item = (RegionId, Placement)> + '_ {
        self.placements.iter().copied()
    }

    /// Where a region sits, or `None` for a region off the map.
    pub fn placement_of(&self, region: RegionId) -> Option<Placement> {
        self.by_region.get(&region).copied()
    }

    /// Which region fills a square, if any.
    pub fn region_at(&self, square: Placement) -> Option<RegionId> {
        self.by_square.get(&square).copied()
    }

    /// The region across one side of another, if a placed one is there.
    pub fn neighbor(&self, region: RegionId, side: Side) -> Option<RegionId> {
        self.region_at(self.placement_of(region)?.beside(side))
    }

    /// The placed region a world position falls in, and the position inside
    /// it. `None` where no placed region is, or for a position too far from
    /// the origin for its square to be named.
    ///
    /// `region_size` is the world config's, which every placed region shares.
    /// The local position is always inside the box on `x` and `y`; `z` passes
    /// through and is the region's to accept.
    pub fn locate(&self, at: WorldPos, region_size: Fixed) -> Option<(RegionId, Pos3)> {
        let size = region_size.raw() as i64;
        let col = i32::try_from(at.x.div_euclid(size)).ok()?;
        let row = i32::try_from(at.y.div_euclid(size)).ok()?;
        let square = Placement::new(col, row);
        let region = self.region_at(square)?;
        let local = at.to_local(square, region_size)?;
        Some((region, local))
    }

    /// The map as the bytes the bucket holds.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.placements.len() * WorldMap::RECORD);
        out.extend_from_slice(&WorldMap::VERSION.to_le_bytes());
        for (region, at) in &self.placements {
            out.extend_from_slice(&region.raw().to_le_bytes());
            out.extend_from_slice(&at.col.to_le_bytes());
            out.extend_from_slice(&at.row.to_le_bytes());
        }
        out
    }

    /// The map an edge starts with: the bucket's key, decoded.
    ///
    /// `Ok(None)` when the broker has no bucket or the bucket has no key,
    /// which is how a deployment of islands looks and how a broker without
    /// JetStream looks. An edge then treats every region as an island until
    /// it is restarted (`docs/adr/0010`). A key that is present but does not
    /// decode, or a broker that cannot be read, is an error rather than an
    /// empty map, so a bad deployment does not pass for a plain one.
    pub async fn read(nats: &async_nats::Client) -> Result<Option<WorldMap>, MapError> {
        let js = async_nats::jetstream::new(nats.clone());
        let store = match js.get_key_value(WorldMap::BUCKET).await {
            Ok(store) => store,
            Err(_) => return Ok(None),
        };
        let bytes =
            store.get(WorldMap::KEY).await.map_err(|e| MapError::Broker(Box::new(e)))?;
        match bytes {
            Some(bytes) => WorldMap::decode(&bytes).map(Some),
            None => Ok(None),
        }
    }

    /// Writes the map to the bucket, creating the bucket if it is not there.
    /// Returns the bucket's revision, which is the map's version.
    ///
    /// For whatever places regions. Nothing in the library calls this on its
    /// own, and nothing watches the key afterward.
    pub async fn write(&self, nats: &async_nats::Client) -> Result<u64, MapError> {
        let js = async_nats::jetstream::new(nats.clone());
        let store = match js.get_key_value(WorldMap::BUCKET).await {
            Ok(store) => store,
            Err(_) => js
                .create_key_value(async_nats::jetstream::kv::Config {
                    bucket: WorldMap::BUCKET.to_string(),
                    ..Default::default()
                })
                .await
                .map_err(|e| MapError::Broker(Box::new(e)))?,
        };
        store
            .put(WorldMap::KEY, self.encode().into())
            .await
            .map_err(|e| MapError::Broker(Box::new(e)))
    }

    /// A map from the bytes the bucket holds. Refused whole on any fault.
    pub fn decode(bytes: &[u8]) -> Result<WorldMap, MapError> {
        if bytes.len() < 4 || (bytes.len() - 4) % WorldMap::RECORD != 0 {
            return Err(MapError::BadLength(bytes.len()));
        }
        let version = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if version != WorldMap::VERSION {
            return Err(MapError::UnknownVersion(version));
        }
        let mut map = WorldMap::new();
        for record in bytes[4..].chunks_exact(WorldMap::RECORD) {
            let u = |at: usize| {
                u32::from_le_bytes([
                    record[at],
                    record[at + 1],
                    record[at + 2],
                    record[at + 3],
                ])
            };
            let region = RegionId::from_raw(u(0));
            let at = Placement::new(u(4) as i32, u(8) as i32);
            map.place(region, at)?;
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(n: u32) -> RegionId {
        RegionId::from_raw(n)
    }

    fn three() -> WorldMap {
        let mut m = WorldMap::new();
        m.place(r(1), Placement::new(0, 0)).unwrap();
        m.place(r(2), Placement::new(1, 0)).unwrap();
        m.place(r(3), Placement::new(-1, -2)).unwrap();
        m
    }

    #[test]
    fn the_bytes_are_four_plus_twelve_per_placement_and_round_trip() {
        let m = three();
        let bytes = m.encode();
        assert_eq!(bytes.len(), 4 + 3 * 12);
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        assert_eq!(WorldMap::decode(&bytes).unwrap(), m);
        assert_eq!(WorldMap::decode(&WorldMap::new().encode()).unwrap(), WorldMap::new());
    }

    #[test]
    fn a_wrong_length_is_refused() {
        assert!(matches!(WorldMap::decode(&[]), Err(MapError::BadLength(0))));
        assert!(matches!(
            WorldMap::decode(&[1, 0, 0, 0, 9]),
            Err(MapError::BadLength(5))
        ));
    }

    #[test]
    fn an_unknown_version_is_refused() {
        assert!(matches!(
            WorldMap::decode(&2u32.to_le_bytes()),
            Err(MapError::UnknownVersion(2))
        ));
    }

    #[test]
    fn a_region_placed_twice_or_a_square_held_twice_is_refused() {
        let mut m = three();
        assert!(matches!(
            m.place(r(1), Placement::new(5, 5)),
            Err(MapError::DuplicateRegion(_))
        ));
        assert!(matches!(
            m.place(r(9), Placement::new(1, 0)),
            Err(MapError::DuplicateSquare(_))
        ));
        let mut bytes = three().encode();
        bytes.extend_from_slice(&three().encode()[4..16]);
        assert!(matches!(WorldMap::decode(&bytes), Err(MapError::DuplicateRegion(_))));
    }

    #[test]
    fn a_region_id_past_31_bits_is_refused() {
        let mut m = WorldMap::new();
        assert!(matches!(
            m.place(r(0x8000_0000), Placement::new(0, 0)),
            Err(MapError::RegionIdTooWide(_))
        ));
        assert!(m.place(r(0x7fff_ffff), Placement::new(0, 0)).is_ok());
    }

    #[test]
    fn neighbors_follow_placement_and_walls_are_none() {
        let m = three();
        assert_eq!(m.neighbor(r(1), Side::East), Some(r(2)));
        assert_eq!(m.neighbor(r(2), Side::West), Some(r(1)));
        assert_eq!(m.neighbor(r(1), Side::West), None);
        assert_eq!(m.neighbor(r(1), Side::North), None);
        assert_eq!(m.neighbor(r(3), Side::South), None);
        assert_eq!(
            m.neighbor(r(42), Side::East),
            None,
            "an unplaced region has no neighbors"
        );
    }

    #[test]
    fn a_world_position_locates_its_region_and_local_position() {
        let m = three();
        let size = Fixed::from_meters(4096);
        // Inside region 2's square, 10 m past its west side.
        let at = WorldPos::from_meters(4096 + 10, 20, 3);
        let (region, local) = m.locate(at, size).unwrap();
        assert_eq!(region, r(2));
        assert_eq!(local, Pos3::from_meters(10, 20, 3));
        // The last raw unit of region 1 and the first of region 2.
        let edge = WorldPos::from_raw(size.raw() as i64 - 1, 0, 0);
        assert_eq!(m.locate(edge, size).unwrap().0, r(1));
        let first = WorldPos::from_raw(size.raw() as i64, 0, 0);
        assert_eq!(m.locate(first, size).unwrap(), (r(2), Pos3::ZERO));
        // Negative squares floor toward negative infinity.
        let south_west = WorldPos::from_meters(-1, -4097, 0);
        let (region, local) = m.locate(south_west, size).unwrap();
        assert_eq!(region, r(3));
        assert_eq!(local, Pos3::from_meters(4095, 4095, 0));
        // An empty square is nowhere.
        assert_eq!(m.locate(WorldPos::from_meters(9000, 9000, 0), size), None);
        // Too far to name a square.
        assert_eq!(m.locate(WorldPos::from_raw(i64::MAX, 0, 0), size), None);
    }
}
