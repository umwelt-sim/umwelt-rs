//! Encoding and decoding for entity update records.
//!
//! A record is an [`EntityId`] followed by a quantized position packed into the
//! fewest whole bytes the configured precision needs. The record size
//! depends on the horizontal and vertical bits derived by a [`WorldConfig`].
//!
//! Multiple records are included per packet. If available, additional game
//! data supplied from outside this library is added to the packet after records.
//!
//! Records carry no game state. A consumer replicating health or orientation
//! appends its own bytes; that is not built.

use crate::config::WorldConfig;
use crate::entity::EntityId;
use crate::pos::Pos3;

/// Encodes and decodes entity records.
#[derive(Debug, Clone)]
pub struct RecordCodec {
    cfg: WorldConfig,
    h_bits: u32,
    v_bits: u32,
    pos_bytes: usize,
}

impl RecordCodec {
    /// A codec for a region described only by its extents.
    ///
    /// The wire layout depends on exactly two numbers: horizontal bits are the
    /// region size's, vertical bits the extent's. View radius, speed cap and
    /// tick rate change nothing a decoder does, which is why a client is never
    /// told them — see `net::edge::protocol::EdgeInfo`. The three the builder
    /// still needs are set to valid values here and reach nothing.
    ///
    /// # Errors
    ///
    /// If the extents do not describe a world at all.
    pub fn for_extents(
        region_size_m: i32,
        vertical_extent_m: i32,
    ) -> Result<RecordCodec, crate::config::ConfigError> {
        let cfg = WorldConfig::builder()
            .region_size_m(region_size_m)
            .vertical_extent_m(vertical_extent_m)
            .horizontal_view_radius_m((region_size_m / 16).max(1))
            .max_horizontal_speed_m_per_sec(1)
            .tick_hz(20)
            .build()?;
        Ok(RecordCodec::new(&cfg))
    }

    /// The layout this world implies.
    pub fn new(cfg: &WorldConfig) -> RecordCodec {
        let h_bits = cfg.horizontal_bits();
        let v_bits = cfg.vertical_bits();
        let total = 2 * h_bits + v_bits;
        assert!(total <= 128, "position needs {total} bits, more than a u128 holds");
        RecordCodec { cfg: *cfg, h_bits, v_bits, pos_bytes: total.div_ceil(8) as usize }
    }

    /// The number of bytes a record occupies
    #[inline]
    pub fn record_bytes(&self) -> usize {
        4 + self.pos_bytes
    }

    /// The number of bytes the encoded position occupies
    #[inline]
    pub fn position_bytes(&self) -> usize {
        self.pos_bytes
    }

    /// Appends one record to the output buffer
    #[inline]
    pub fn encode(&self, id: EntityId, pos: Pos3, out: &mut Vec<u8>) {
        out.extend_from_slice(&id.raw().to_le_bytes());
        let (x, y, z) = self.cfg.quantize_pos(pos);
        let packed = (x as u128)
            | ((y as u128) << self.h_bits)
            | ((z as u128) << (2 * self.h_bits));
        out.extend_from_slice(&packed.to_le_bytes()[..self.pos_bytes]);
    }

    /// Reads one record from the front of `buf`.
    ///
    /// Returns `None` if `buf` is shorter than one record.
    #[inline]
    pub fn decode(&self, buf: &[u8]) -> Option<(EntityId, Pos3)> {
        if buf.len() < self.record_bytes() {
            return None;
        }
        let id = EntityId::from_raw(u32::from_le_bytes(buf[0..4].try_into().ok()?));
        let mut raw = [0u8; 16];
        raw[..self.pos_bytes].copy_from_slice(&buf[4..4 + self.pos_bytes]);
        let packed = u128::from_le_bytes(raw);
        let hmask = (1u128 << self.h_bits) - 1;
        let vmask = (1u128 << self.v_bits) - 1;
        Some((
            id,
            self.cfg.dequantize_pos(
                (packed & hmask) as u32,
                ((packed >> self.h_bits) & hmask) as u32,
                ((packed >> (2 * self.h_bits)) & vmask) as u32,
            ),
        ))
    }
}

#[cfg(test)]
mod extent_tests {
    use super::*;
    use crate::pos::Pos3;

    /// Everything a decoder does comes from the two extents. If that ever stops
    /// being true, a client told only those two would decode packets into
    /// nonsense, and this is what says so.
    #[test]
    fn the_extents_are_the_whole_of_the_wire_layout() {
        for (size, vertical) in [(4096, 1024), (2048, 512), (8192, 1024)] {
            for (radius, speed, hz) in [(256, 40, 20), (128, 5, 50), (512, 100, 10)] {
                let cfg = WorldConfig::builder()
                    .region_size_m(size)
                    .vertical_extent_m(vertical)
                    .horizontal_view_radius_m(radius)
                    .max_horizontal_speed_m_per_sec(speed)
                    .tick_hz(hz)
                    .build()
                    .expect("a valid world");
                let full = RecordCodec::new(&cfg);
                let thin =
                    RecordCodec::for_extents(size, vertical).expect("valid extents");
                assert_eq!(full.record_bytes(), thin.record_bytes(), "{size}/{vertical}");

                let at = Pos3::from_meters(size / 3, vertical / 3, vertical / 5);
                let (mut a, mut b) = (Vec::new(), Vec::new());
                full.encode(EntityId::from_raw(9), at, &mut a);
                thin.encode(EntityId::from_raw(9), at, &mut b);
                assert_eq!(a, b, "{size}/{vertical} at {radius}/{speed}/{hz}");
                assert_eq!(thin.decode(&a), full.decode(&b));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::Fixed;

    fn world(region_m: i32, vertical_m: i32) -> WorldConfig {
        WorldConfig::builder()
            .region_size_m(region_m)
            .vertical_extent_m(vertical_m)
            .horizontal_view_radius_m(256)
            .max_horizontal_speed_m_per_sec(40)
            .tick_hz(20)
            .build()
            .expect("config is valid")
    }

    #[test]
    fn default_config_record_is_twelve_bytes() {
        let cfg = WorldConfig::default();
        let c = RecordCodec::new(&cfg);
        // 22 + 22 + 20 = 64 bits, eight bytes, plus four for the id.
        assert_eq!(cfg.horizontal_bits(), 22);
        assert_eq!(cfg.vertical_bits(), 20);
        assert_eq!(c.position_bytes(), 8);
        assert_eq!(c.record_bytes(), 12);
    }

    #[test]
    fn lossless_round_trips_exactly() {
        let cfg = world(4096, 1024);
        let c = RecordCodec::new(&cfg);
        let mut buf = Vec::new();
        let cases = [
            (0u32, Pos3::from_meters(0, 0, 0)),
            (1, Pos3::from_meters(4095, 4095, 1023)),
            (9001, Pos3::new(Fixed::from_raw(1), Fixed::from_raw(2), Fixed::from_raw(3))),
            (
                u32::MAX,
                Pos3::new(
                    Fixed::from_millis(1234, 567),
                    Fixed::from_millis(89, 12),
                    Fixed::from_millis(500, 999),
                ),
            ),
        ];
        for (id, pos) in cases {
            buf.clear();
            c.encode(EntityId::from_raw(id), pos, &mut buf);
            assert_eq!(buf.len(), c.record_bytes());
            let (got_id, got_pos) = c.decode(&buf).expect("decode");
            assert_eq!(got_id.raw(), id);
            assert_eq!(got_pos, pos, "lossless precision must round-trip exactly");
        }
    }

    #[test]
    fn record_size_follows_region_and_precision() {
        // Bigger region needs more bits at the same precision.
        assert_eq!(RecordCodec::new(&world(4096, 1024)).record_bytes(), 12);
        assert_eq!(RecordCodec::new(&world(16_384, 1024)).record_bytes(), 13);
    }

    #[test]
    fn decode_rejects_a_short_buffer() {
        let cfg = WorldConfig::default();
        let c = RecordCodec::new(&cfg);
        let mut buf = Vec::new();
        c.encode(EntityId::from_raw(1), Pos3::from_meters(1, 2, 3), &mut buf);
        buf.pop();
        assert!(c.decode(&buf).is_none());
    }

    #[test]
    fn records_pack_back_to_back() {
        let cfg = world(4096, 1024);
        let c = RecordCodec::new(&cfg);
        let mut buf = Vec::new();
        let pts: Vec<Pos3> =
            (0..50).map(|i| Pos3::from_meters(i * 3, i * 7, i)).collect();
        for (i, p) in pts.iter().enumerate() {
            c.encode(EntityId::from_raw(i as u32), *p, &mut buf);
        }
        assert_eq!(buf.len(), 50 * c.record_bytes());
        for (i, p) in pts.iter().enumerate() {
            let at = i * c.record_bytes();
            let (id, got) = c.decode(&buf[at..]).expect("decode");
            assert_eq!(id.raw(), i as u32);
            assert_eq!(got, *p);
        }
    }
}
