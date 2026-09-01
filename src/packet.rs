//! Assembling one client's payload.
//!
//! **Not a game-developer surface.** A game receives
//! [`TickObservation`] through [`ClientGame::observed`](crate::ClientGame::observed)
//! and never assembles or decodes payloads.
//!
//! A payload consists of a header, then despawns, then state records.
//!
//! Despawns come first because they are few and cheap, and because a client
//! that drops before it adds never holds more ghosts than the server thinks it
//! does.

use crate::codec::RecordCodec;
use crate::entity::EntityId;
use crate::pos::Pos3;

/// Bytes a despawn occupies: just [`EntityId`]. A client already
/// holds the position it is being told to forget.
pub(crate) const DESPAWN_BYTES: usize = 4;

/// Fixed-size preamble.
///
/// Sixteen bytes. The sequence number is used but acknowledgments are not
/// yet implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PacketHeader {
    /// Which tick's snapshot this was built from.
    pub tick: u32,
    /// Counts up once per packet sent to this client, and wraps.
    pub sequence: u16,
    /// Highest sequence received from this client. Zero until acks exist.
    pub ack: u16,
    /// The thirty-two sequences before `ack`, one bit each. Zero until acks
    /// exist.
    pub ack_bits: u32,
    /// Despawn records in this packet.
    pub despawns: u16,
    /// Entity records in this packet.
    pub updates: u16,
}

impl PacketHeader {
    /// The header's width on the wire.
    pub const BYTES: usize = 16;

    /// Appends the header. Little-endian, like everything else on this wire.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.extend_from_slice(&self.sequence.to_le_bytes());
        out.extend_from_slice(&self.ack.to_le_bytes());
        out.extend_from_slice(&self.ack_bits.to_le_bytes());
        out.extend_from_slice(&self.despawns.to_le_bytes());
        out.extend_from_slice(&self.updates.to_le_bytes());
    }

    /// `None` if `buf` is shorter than a header.
    pub fn decode(buf: &[u8]) -> Option<PacketHeader> {
        if buf.len() < PacketHeader::BYTES {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        let u32_at =
            |i: usize| u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        Some(PacketHeader {
            tick: u32_at(0),
            sequence: u16_at(4),
            ack: u16_at(6),
            ack_bits: u32_at(8),
            despawns: u16_at(12),
            updates: u16_at(14),
        })
    }
}

/// Builds payloads into a buffer it keeps.
///
/// Held per worker thread and reused across viewers for a single
/// allocation.
#[derive(Debug, Clone)]
#[doc(hidden)]
pub struct PacketWriter {
    codec: RecordCodec,
    buf: Vec<u8>,
}

impl PacketWriter {
    /// A writer for packets of this width, in this world's layout.
    pub fn new(codec: RecordCodec, payload_bytes: usize) -> PacketWriter {
        PacketWriter { codec, buf: Vec::with_capacity(payload_bytes) }
    }

    /// Assembles one payload and returns it.
    ///
    /// `despawns` and `updates` are written in full: a caller is responsible
    /// for having sized them against
    /// [`state_bytes_available`](crate::budget::PacketBudget::state_bytes_available),
    /// and doing otherwise is a programmer error rather than a runtime one.
    ///
    /// The returned slice borrows this writer's buffer, so the next call
    /// overwrites it.
    ///
    /// # Panics
    ///
    /// If the counts do not fit a `u16`.
    pub fn build<I>(
        &mut self,
        tick: u32,
        sequence: u16,
        despawns: &[EntityId],
        updates: I,
    ) -> &[u8]
    where
        I: IntoIterator<Item = (EntityId, Pos3, u16)>,
    {
        self.buf.clear();
        // Reserved, then rewritten once the counts are known.
        PacketHeader::default().encode(&mut self.buf);

        for id in despawns {
            self.buf.extend_from_slice(&id.raw().to_le_bytes());
        }

        let mut updated = 0usize;
        for (id, pos, tag) in updates {
            self.codec.encode(id, pos, tag, &mut self.buf);
            updated += 1;
        }

        let header = PacketHeader {
            tick,
            sequence,
            ack: 0,
            ack_bits: 0,
            despawns: u16::try_from(despawns.len()).expect("despawn count fits a u16"),
            updates: u16::try_from(updated).expect("update count fits a u16"),
        };
        let mut head = Vec::with_capacity(PacketHeader::BYTES);
        header.encode(&mut head);
        self.buf[..PacketHeader::BYTES].copy_from_slice(&head);
        &self.buf
    }

    /// The payload most recently built, until the next call overwrites it.
    #[inline]
    pub fn payload(&self) -> &[u8] {
        &self.buf
    }
}

/// What one entity saw on one tick: which to forget, and where the rest are.
///
/// A borrowed view over a payload that has already arrived. It does no I/O and
/// owns no buffer — reading it is walking bytes somebody else received.
///
/// Every one of these is a single tick's worth, and says which tick in
/// [`tick`](Self::tick). A client is handed one every
/// [`send_period`](crate::ClientLimits::send_period) ticks, which defaults to
/// every tick.
///
/// This is what [`ClientGame::observed`](crate::ClientGame::observed) receives.
/// The crate's own tests and quality harness also build one, to model a client
/// from the bytes rather than from the server's own decision.
pub struct TickObservation<'a> {
    codec: &'a RecordCodec,
    header: PacketHeader,
    body: &'a [u8],
}

impl<'a> TickObservation<'a> {
    /// `None` if `buf` is too short to hold the header and the body its counts
    /// claim.
    ///
    /// Crate-private: a game is handed one of these and never builds one.
    pub(crate) fn new(
        codec: &'a RecordCodec,
        buf: &'a [u8],
    ) -> Option<TickObservation<'a>> {
        let header = PacketHeader::decode(buf)?;
        let want = PacketHeader::BYTES
            + header.despawns as usize * DESPAWN_BYTES
            + header.updates as usize * codec.record_bytes();
        if buf.len() < want {
            return None;
        }
        Some(TickObservation { codec, header, body: &buf[PacketHeader::BYTES..want] })
    }

    /// Which tick this observation was built from.
    ///
    /// A region stamps every payload with the tick whose snapshot produced it.
    /// Two observations from the same region compare by this.
    #[inline]
    pub fn tick(&self) -> u32 {
        self.header.tick
    }

    /// The whole preamble, which is umwelt's own bookkeeping: sequence numbers
    /// and the acknowledgment fields nothing populates yet. Nothing outside a
    /// test reads it, so it does not exist outside one.
    #[cfg(test)]
    #[inline]
    pub(crate) fn header(&self) -> PacketHeader {
        self.header
    }

    /// Entities the client should drop.
    pub fn despawns(&self) -> impl Iterator<Item = EntityId> + '_ {
        (0..self.header.despawns as usize).map(move |k| {
            let at = k * DESPAWN_BYTES;
            EntityId::from_raw(u32::from_le_bytes([
                self.body[at],
                self.body[at + 1],
                self.body[at + 2],
                self.body[at + 3],
            ]))
        })
    }

    /// Positions and tags the client should adopt. An entity it does not hold
    /// is one it is being told about for the first time.
    pub fn updates(&self) -> impl Iterator<Item = (EntityId, Pos3, u16)> + '_ {
        let base = self.header.despawns as usize * DESPAWN_BYTES;
        let stride = self.codec.record_bytes();
        (0..self.header.updates as usize).map(move |k| {
            self.codec.decode(&self.body[base + k * stride..]).expect("sized")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorldConfig;
    use crate::fixed::Fixed;

    fn codec() -> RecordCodec {
        RecordCodec::new(&WorldConfig::default())
    }

    fn id(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    #[test]
    fn a_header_is_the_sixteen_bytes_the_budget_assumed() {
        assert_eq!(PacketHeader::BYTES, crate::budget::DEFAULT_HEADER_BYTES as usize);
        let mut buf = Vec::new();
        PacketHeader::default().encode(&mut buf);
        assert_eq!(buf.len(), PacketHeader::BYTES);
    }

    #[test]
    fn a_header_round_trips() {
        let h = PacketHeader {
            tick: 0xDEAD_BEEF,
            sequence: 0x1234,
            ack: 0x5678,
            ack_bits: 0xAABB_CCDD,
            despawns: 7,
            updates: 91,
        };
        let mut buf = Vec::new();
        h.encode(&mut buf);
        assert_eq!(PacketHeader::decode(&buf), Some(h));
    }

    #[test]
    fn a_short_buffer_holds_no_header() {
        assert_eq!(PacketHeader::decode(&[0u8; 15]), None);
    }

    #[test]
    fn a_payload_round_trips() {
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let gone = [id(4), id(9)];
        let moved = vec![
            (id(1), Pos3::from_meters(100, 200, 5), 0u16),
            (
                id(2),
                Pos3::new(
                    Fixed::from_millimeters(7, 500),
                    Fixed::ZERO,
                    Fixed::from_meters(3),
                ),
                42,
            ),
            (id(3), Pos3::from_meters(4095, 4095, 1023), 999),
        ];

        let bytes = w.build(77, 5, &gone, moved.clone());
        let r = TickObservation::new(&c, bytes).expect("well formed");

        assert_eq!(r.header().tick, 77);
        assert_eq!(r.header().sequence, 5);
        assert_eq!(r.header().despawns, 2);
        assert_eq!(r.header().updates, 3);
        assert_eq!(r.despawns().collect::<Vec<_>>(), gone);
        assert_eq!(r.updates().collect::<Vec<_>>(), moved);
    }

    #[test]
    fn an_empty_payload_is_just_a_header() {
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let bytes = w.build(1, 1, &[], std::iter::empty::<(EntityId, Pos3, u16)>());
        assert_eq!(bytes.len(), PacketHeader::BYTES);
        let r = TickObservation::new(&c, bytes).expect("well formed");
        assert_eq!(r.despawns().count(), 0);
        assert_eq!(r.updates().count(), 0);
    }

    #[test]
    fn a_payload_is_a_header_then_its_records() {
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let gone: Vec<EntityId> = (0..5).map(id).collect();
        let moved: Vec<(EntityId, Pos3, u16)> =
            (0..30).map(|k| (id(100 + k), Pos3::from_meters(k as i32, 0, 0), 0)).collect();
        // 16 header + 5 × 4 despawns + 30 × 14 records = 16 + 20 + 420 = 456
        assert_eq!(w.build(1, 1, &gone, moved).len(), 16 + 5 * 4 + 30 * 14);
    }

    #[test]
    fn a_full_packet_of_records_fits_the_budget() {
        // 1200 - 16 = 1184, and 1184 / 14 = 84 with 8 bytes left over.
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let moved: Vec<(EntityId, Pos3, u16)> =
            (0..84).map(|k| (id(k), Pos3::from_meters(k as i32, 0, 0), 0)).collect();
        assert_eq!(w.build(1, 1, &[], moved).len(), 16 + 84 * 14);
        assert!(w.build(1, 1, &[], Vec::<(EntityId, Pos3, u16)>::new()).len() <= 1200);
    }

    #[test]
    fn a_truncated_payload_is_refused() {
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let moved = vec![(id(1), Pos3::from_meters(1, 2, 3), 0u16)];
        let bytes = w.build(1, 1, &[id(9)], moved).to_vec();
        for cut in 1..bytes.len() {
            assert!(
                TickObservation::new(&c, &bytes[..cut]).is_none(),
                "a payload {cut} bytes short must not parse"
            );
        }
    }

    #[test]
    fn the_buffer_is_reused_across_payloads() {
        let c = codec();
        let mut w = PacketWriter::new(c.clone(), 1200);
        let moved: Vec<(EntityId, Pos3, u16)> =
            (0..84).map(|k| (id(k), Pos3::from_meters(k as i32, 0, 0), 0)).collect();
        w.build(1, 1, &[], moved.clone());
        let cap = w.buf.capacity();
        for tick in 2..200u32 {
            w.build(tick, tick as u16, &[id(1)], moved.clone());
        }
        assert_eq!(w.buf.capacity(), cap, "steady state must not reallocate");
    }
}
