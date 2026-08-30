//! How many state records fit one client's packet.
//!
//! [`PacketBudget`] turns a connection's payload size into the `slots` argument
//! [`select`](crate::select::select) takes. It is per-connection, built from
//! what a client declares at connect plus this protocol's own overheads.
//!
//! The event reserve holds minimum space so that when event support is added,
//! everything still works on budget.

use crate::codec::RecordCodec;

/// Bytes of payload a datagram carries.
///
/// Ethernet MTU is 1500, an IPv6 header takes 40 and UDP 8, leaving 1452. 1200
/// survives tunnels, VPNs and PPPoE without fragmenting, and is what QUIC
/// mandates as its minimum datagram size.
pub const DEFAULT_PAYLOAD_BYTES: u16 = 1200;

/// Sequence number, acknowledgment and its bitfield, tick, and the two record
/// counts.
///
/// Matches [`PacketHeader::BYTES`](crate::packet::PacketHeader::BYTES), which a test
/// pins.
pub const DEFAULT_HEADER_BYTES: u16 = 16;

/// Held back for events, and only while events are pending.
///
/// Without any reserve a dense crowd's position updates fill every packet and a
/// client can stand in a mob without learning it died. **The number itself is a
/// guess.**
pub const DEFAULT_EVENT_RESERVE_BYTES: u16 = 256;

/// One connection's per-packet record budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketBudget {
    payload_bytes: u16,
    header_bytes: u16,
    event_reserve_bytes: u16,
    record_bytes: u16,
}

impl PacketBudget {
    /// # Panics
    ///
    /// If `payload_bytes` does not exceed the header, or if the configuration
    /// produces a record larger than a packet.
    pub fn new(codec: &RecordCodec, payload_bytes: u16) -> PacketBudget {
        PacketBudget::with_overhead(
            codec,
            payload_bytes,
            DEFAULT_HEADER_BYTES,
            DEFAULT_EVENT_RESERVE_BYTES,
        )
    }

    /// Both overheads are guesses, so they are parameters: this exists so they
    /// can be swept.
    ///
    /// # Panics
    ///
    /// If `payload_bytes` does not exceed the header, or if the configuration
    /// produces a record larger than a packet.
    pub fn with_overhead(
        codec: &RecordCodec,
        payload_bytes: u16,
        header_bytes: u16,
        event_reserve_bytes: u16,
    ) -> PacketBudget {
        let record = codec.record_bytes();
        assert!(
            payload_bytes > header_bytes,
            "payload of {payload_bytes} does not cover a {header_bytes} byte header"
        );
        assert!(
            record <= payload_bytes as usize,
            "a {record} byte record does not fit a {payload_bytes} byte payload"
        );
        PacketBudget {
            payload_bytes,
            header_bytes,
            event_reserve_bytes,
            record_bytes: record as u16,
        }
    }

    /// The whole payload, header included.
    #[inline]
    pub fn payload_bytes(&self) -> usize {
        self.payload_bytes as usize
    }

    /// What the header takes before any record fits.
    #[inline]
    pub fn header_bytes(&self) -> usize {
        self.header_bytes as usize
    }

    /// Held back for despawns, which travel ahead of updates.
    #[inline]
    pub fn event_reserve_bytes(&self) -> usize {
        self.event_reserve_bytes as usize
    }

    /// What is left for entity records.
    #[inline]
    pub fn record_bytes(&self) -> usize {
        self.record_bytes as usize
    }

    /// Bytes left for state records once the header and any pending events are
    /// accounted for.
    ///
    /// Events take what they have queued, up to the reserve. Beyond that they
    /// wait, so a backlog cannot starve state entirely.
    #[inline]
    pub fn state_bytes_available(&self, pending_event_bytes: usize) -> usize {
        let held = pending_event_bytes.min(self.event_reserve_bytes as usize);
        (self.payload_bytes as usize).saturating_sub(self.header_bytes as usize + held)
    }

    /// Records that fit. This is the `slots` argument to
    /// [`select`](crate::select::select).
    #[inline]
    pub fn slots(&self, pending_event_bytes: usize) -> usize {
        self.state_bytes_available(pending_event_bytes) / self.record_bytes as usize
    }

    /// The most records that can fit, with nothing queued.
    #[inline]
    pub fn max_slots(&self) -> usize {
        self.slots(0)
    }

    /// The fewest, under a backlog at or past the reserve.
    #[inline]
    pub fn min_slots(&self) -> usize {
        self.slots(self.event_reserve_bytes as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorldConfig;

    fn world(region_m: i32) -> WorldConfig {
        WorldConfig::builder()
            .region_size_m(region_m)
            .vertical_extent_m(1024)
            .horizontal_view_radius_m(256)
            .max_horizontal_speed_m_per_sec(40)
            .tick_hz(20)
            .build()
            .expect("config is valid")
    }

    fn budget(region_m: i32) -> PacketBudget {
        PacketBudget::new(&RecordCodec::new(&world(region_m)), DEFAULT_PAYLOAD_BYTES)
    }

    #[test]
    fn an_idle_packet_holds_ninety_eight_records() {
        // 1200 - 16 = 1184, and 1184 / 12 = 98.
        let b = budget(4096);
        assert_eq!(b.record_bytes(), 12);
        assert_eq!(b.state_bytes_available(0), 1184);
        assert_eq!(b.max_slots(), 98);
    }

    #[test]
    fn a_full_backlog_leaves_seventy_seven() {
        // 1200 - (16 + 256) = 928, and 928 / 12 = 77.
        let b = budget(4096);
        assert_eq!(b.state_bytes_available(256), 928);
        assert_eq!(b.min_slots(), 77);
    }

    #[test]
    fn the_reserve_is_a_floor_not_a_subtraction() {
        let b = budget(4096);
        assert_eq!(b.slots(0), 98, "no events pending, so state takes everything");
        assert_eq!(b.slots(100), 90, "events take what they have queued");
        assert_eq!(b.slots(256), 77);
        assert_eq!(b.slots(100_000), 77, "a backlog past the reserve waits its turn");
    }

    #[test]
    fn slots_never_rise_as_events_queue() {
        let b = budget(4096);
        let mut last = usize::MAX;
        for pending in 0..600 {
            let s = b.slots(pending);
            assert!(s <= last, "slots rose at {pending} pending bytes");
            last = s;
        }
    }

    #[test]
    fn a_bigger_region_costs_a_slot() {
        // A 16 km region needs 13 bytes per record, so 1184 / 13 = 91.
        let b = budget(16_384);
        assert_eq!(b.record_bytes(), 13);
        assert_eq!(b.max_slots(), 91);
    }

    #[test]
    fn a_packet_too_small_for_a_record_yields_no_slots() {
        let b = PacketBudget::with_overhead(&RecordCodec::new(&world(4096)), 20, 16, 0);
        assert_eq!(b.state_bytes_available(0), 4);
        assert_eq!(b.max_slots(), 0, "four bytes hold no twelve-byte record");
    }

    #[test]
    fn overheads_are_swept_not_fixed() {
        let codec = RecordCodec::new(&world(4096));
        let b = PacketBudget::with_overhead(&codec, 1200, 8, 512);
        assert_eq!(b.max_slots(), (1200 - 8) / 12);
        assert_eq!(b.min_slots(), (1200 - 8 - 512) / 12);
    }

    #[test]
    fn a_payload_under_the_header_is_refused() {
        let codec = RecordCodec::new(&world(4096));
        assert!(
            std::panic::catch_unwind(|| PacketBudget::with_overhead(&codec, 8, 16, 0))
                .is_err()
        );
    }

    #[test]
    fn the_budget_is_eight_bytes() {
        // One per viewer, so it rides along with the rest of the per-viewer
        // state rather than sitting behind a pointer.
        assert_eq!(size_of::<PacketBudget>(), 8);
    }
}
