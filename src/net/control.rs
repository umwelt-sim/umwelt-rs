//! What a region says about itself, for whoever is watching the tier.
//!
//! A separate protocol from `net::region`, and deliberately not sharing types
//! with it. That one is how a region and its edges do their work; this one is
//! how an operator sees that the work is happening.
//!
//! The library publishes and does not subscribe. Deciding anything from a
//! heartbeat — rebalancing, draining, placing regions — belongs to a control
//! plane tier that is a separate program and is not built.
//!
//! **No cadence is defined here**, and no consumer writes one. A server
//! publishes its own heartbeat on a timer it owns — see
//! [`RegionServer::set_heartbeat_interval`](crate::net::RegionServer::set_heartbeat_interval)
//! and its edge counterpart — because a heartbeat reports state only the
//! server holds, and asking a consumer to carry that state out and hand it
//! back would make the library's own bookkeeping the consumer's problem.
//!
//! How long silence has to last before anyone believes a server has stopped is
//! a deployment judgment, and belongs to whatever is listening.
//!
//! # Subjects
//!
//! Building a subject is umwelt's on both ends, so the functions that do it are
//! crate-private. A watcher outside this crate subscribes to these patterns:
//!
//! | pattern | carries |
//! |---|---|
//! | `umwelt.control.region.*.heartbeat` | every region's [`Heartbeat`] |
//! | `umwelt.control.edge.*.heartbeat` | every edge's [`EdgeHeartbeat`] |
//!
//! That, and what [`Heartbeat::decode`] and [`EdgeHeartbeat::decode`] read, is
//! the whole of what a watcher needs to know about the transport.

use core::fmt;
use std::time::Duration;

use crate::id::RegionId;
use crate::net::error::NetError;
use crate::net::region::edges::EdgeName;
use crate::net::region::protocol::{ProtocolVersion, ServerVersion};
use crate::net::wire::Cursor;

/// One region's heartbeat subject.
pub(crate) fn subject(region: RegionId) -> String {
    format!("umwelt.control.region.{}.heartbeat", region.raw())
}

/// One edge's heartbeat subject.
pub(crate) fn edge_subject(edge: &EdgeName) -> String {
    format!("umwelt.control.edge.{edge}.heartbeat")
}

/// What only the region's own loop can report.
///
/// The tick figures cover the span since the previous heartbeat, not a fixed
/// second: the interval is the consumer's, so the window is too. `late` and
/// `dropped` are counts over that same span, which says more than a sample
/// would.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionLoad {
    /// Ticks in the span.
    pub tick_count: u32,
    /// Entities alive at the end of it.
    pub entities: u32,
    /// Slots ever allocated. Despawn does not reclaim, so this climbs with
    /// churn while `entities` does not. See §Slot growth under churn.
    pub slots: u32,
    /// Viewers registered at the end of it.
    pub viewers: u32,
    /// Time inside a tick, averaged over the span.
    pub mean_tick: Duration,
    /// The longest tick in the span.
    pub worst_tick: Duration,
    /// Ticks that started after their deadline.
    pub late: u32,
    /// Deadlines skipped under `Overrun::Drop`.
    pub dropped: u32,
}

/// A region saying what it is and how it is doing.
///
/// No address, no port, no neighbors. Nothing needs to reach a region: an edge
/// finds it by subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    /// Which region is speaking.
    pub region: RegionId,
    /// What it speaks to its edges.
    pub protocol: ProtocolVersion,
    /// The crate version it runs.
    pub server: ServerVersion,
    /// The world's wire layout. Two regions whose digests differ decode each
    /// other's packets into nonsense, and nothing else here would show it.
    pub protocol_hash: u64,
    /// Edges this region has heard from and not yet expired.
    pub edges: u32,
    /// How it is doing.
    pub load: RegionLoad,
}

impl Heartbeat {
    /// A heartbeat's width on the wire.
    pub const BYTES: usize = 56;

    /// Replaces `out` with the encoded heartbeat.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.region.raw().to_le_bytes());
        out.extend_from_slice(&self.protocol.raw().to_le_bytes());
        out.extend_from_slice(&self.server.major.to_le_bytes());
        out.extend_from_slice(&self.server.minor.to_le_bytes());
        out.extend_from_slice(&self.server.patch.to_le_bytes());
        out.extend_from_slice(&self.protocol_hash.to_le_bytes());
        out.extend_from_slice(&self.edges.to_le_bytes());
        out.extend_from_slice(&self.load.tick_count.to_le_bytes());
        out.extend_from_slice(&self.load.entities.to_le_bytes());
        out.extend_from_slice(&self.load.slots.to_le_bytes());
        out.extend_from_slice(&self.load.viewers.to_le_bytes());
        out.extend_from_slice(&nanos(self.load.mean_tick).to_le_bytes());
        out.extend_from_slice(&nanos(self.load.worst_tick).to_le_bytes());
        out.extend_from_slice(&self.load.late.to_le_bytes());
        out.extend_from_slice(&self.load.dropped.to_le_bytes());
    }

    /// Reads one back.
    pub fn decode(body: &[u8]) -> Result<Heartbeat, NetError> {
        let mut c = Cursor::new(body, "heartbeat");
        let region = RegionId::from_raw(c.u32()?);
        let protocol = ProtocolVersion::from_raw(c.u16()?);
        let server = ServerVersion { major: c.u16()?, minor: c.u16()?, patch: c.u16()? };
        let protocol_hash = c.u64()?;
        let edges = c.u32()?;
        let load = RegionLoad {
            tick_count: c.u32()?,
            entities: c.u32()?,
            slots: c.u32()?,
            viewers: c.u32()?,
            mean_tick: Duration::from_nanos(u64::from(c.u32()?)),
            worst_tick: Duration::from_nanos(u64::from(c.u32()?)),
            late: c.u32()?,
            dropped: c.u32()?,
        };
        c.finish()?;
        Ok(Heartbeat { region, protocol, server, protocol_hash, edges, load })
    }
}

impl fmt::Display for Heartbeat {
    /// One line, for a watcher that has nothing more elaborate to do with it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} umwelt {} proto {} hash {:#018x} | {} edges | {} entities, {} slots, \
             {} viewers | tick mean {:.2} ms worst {:.2} ms | {} late, {} dropped",
            self.region,
            self.server,
            self.protocol,
            self.protocol_hash,
            self.edges,
            self.load.entities,
            self.load.slots,
            self.load.viewers,
            self.load.mean_tick.as_secs_f64() * 1_000.0,
            self.load.worst_tick.as_secs_f64() * 1_000.0,
            self.load.late,
            self.load.dropped,
        )
    }
}

/// What only an edge can count about itself.
///
/// The counters are over the span since the previous heartbeat, not a fixed
/// second, because the interval is a deployment choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeLoad {
    /// Game clients connected right now.
    pub clients: u32,
    /// Entities this edge manages, across every region.
    pub entities: u32,
    /// How many of those have a client behind them and so cost a viewer.
    pub observers: u32,
    /// State packets put on a client's connection.
    pub relayed: u64,
    /// State packets that reached no client: it had gone, or its datagram
    /// queue was full.
    pub undeliverable: u64,
    /// Commands read off client connections.
    pub commands: u64,
    /// Commands this edge declined: an unknown handle, or one belonging to
    /// another connection.
    ///
    /// Not commands a *region* declined. A region counts those per edge and
    /// never tells the edge, so an edge cannot report a number it does not
    /// have.
    pub refused: u64,
}

/// An edge saying what it is and how it is doing.
///
/// No address. An edge's listening address is unlikely to be usable by
/// whatever reads the control plane, which may be on another host, another VM
/// or in another VPC, and a game client is told where to connect by the game's
/// matchmaking rather than by umwelt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeHeartbeat {
    /// Which edge is speaking.
    pub edge: EdgeName,
    /// What it speaks to regions.
    pub protocol: ProtocolVersion,
    /// The crate version it runs.
    pub server: ServerVersion,
    /// Regions this edge currently holds entities in.
    pub regions: Vec<RegionId>,
    /// How it is doing.
    pub load: EdgeLoad,
}

impl EdgeHeartbeat {
    /// Everything but the name and the region list, both of which vary.
    pub const FIXED_BYTES: usize = 55;

    /// Replaces `out` with the encoded heartbeat.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        let name = self.edge.as_str().as_bytes();
        // An `EdgeName` is at most 64 bytes, checked when it was built.
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        out.extend_from_slice(&self.protocol.raw().to_le_bytes());
        out.extend_from_slice(&self.server.major.to_le_bytes());
        out.extend_from_slice(&self.server.minor.to_le_bytes());
        out.extend_from_slice(&self.server.patch.to_le_bytes());
        out.extend_from_slice(&(self.regions.len() as u16).to_le_bytes());
        for region in &self.regions {
            out.extend_from_slice(&region.raw().to_le_bytes());
        }
        out.extend_from_slice(&self.load.clients.to_le_bytes());
        out.extend_from_slice(&self.load.entities.to_le_bytes());
        out.extend_from_slice(&self.load.observers.to_le_bytes());
        out.extend_from_slice(&self.load.relayed.to_le_bytes());
        out.extend_from_slice(&self.load.undeliverable.to_le_bytes());
        out.extend_from_slice(&self.load.commands.to_le_bytes());
        out.extend_from_slice(&self.load.refused.to_le_bytes());
    }

    /// Reads one back.
    pub fn decode(body: &[u8]) -> Result<EdgeHeartbeat, NetError> {
        let mut c = Cursor::new(body, "edge heartbeat");
        let len = c.u8()? as usize;
        let name = core::str::from_utf8(c.bytes(len)?)
            .map_err(|_| NetError::Malformed("edge heartbeat"))?;
        // Through the same validation any other name goes through, so a name
        // that could address another edge's subjects does not decode.
        let edge = EdgeName::new(name)?;
        let protocol = ProtocolVersion::from_raw(c.u16()?);
        let server = ServerVersion { major: c.u16()?, minor: c.u16()?, patch: c.u16()? };
        let count = c.u16()? as usize;
        let mut regions = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            regions.push(RegionId::from_raw(c.u32()?));
        }
        let load = EdgeLoad {
            clients: c.u32()?,
            entities: c.u32()?,
            observers: c.u32()?,
            relayed: c.u64()?,
            undeliverable: c.u64()?,
            commands: c.u64()?,
            refused: c.u64()?,
        };
        c.finish()?;
        Ok(EdgeHeartbeat { edge, protocol, server, regions, load })
    }
}

impl fmt::Display for EdgeHeartbeat {
    /// One line, for a watcher that has nothing more elaborate to do with it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "edge {} umwelt {} proto {} | {} clients | {} entities, {} observing | \
             {} regions | relayed {}, undeliverable {} | commands {}, refused {}",
            self.edge,
            self.server,
            self.protocol,
            self.load.clients,
            self.load.entities,
            self.load.observers,
            self.regions.len(),
            self.load.relayed,
            self.load.undeliverable,
            self.load.commands,
            self.load.refused,
        )
    }
}

/// A tick longer than four seconds saturates rather than wrapping. Anything
/// near it is already a failure the numbers beside it will show.
fn nanos(d: Duration) -> u32 {
    u32::try_from(d.as_nanos()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::region::protocol::PROTOCOL_VERSION;

    fn sample() -> Heartbeat {
        Heartbeat {
            region: RegionId::from_raw(7),
            protocol: PROTOCOL_VERSION,
            server: ServerVersion::CURRENT,
            protocol_hash: 0x0123_4567_89AB_CDEF,
            edges: 4,
            load: RegionLoad {
                tick_count: 1_234_567,
                entities: 8_192,
                slots: 63_712,
                viewers: 8_188,
                mean_tick: Duration::from_micros(15_540),
                worst_tick: Duration::from_micros(17_420),
                late: 3,
                dropped: 0,
            },
        }
    }

    #[test]
    fn a_heartbeat_round_trips() {
        let m = sample();
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(buf.len(), Heartbeat::BYTES);
        assert_eq!(Heartbeat::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_truncated_heartbeat_is_refused() {
        let mut buf = Vec::new();
        sample().encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(
                Heartbeat::decode(&buf[..cut]).is_err(),
                "{cut} bytes must not parse"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut buf = Vec::new();
        sample().encode(&mut buf);
        buf.push(0);
        assert!(Heartbeat::decode(&buf).is_err());
    }

    fn edge_sample() -> EdgeHeartbeat {
        EdgeHeartbeat {
            edge: EdgeName::new("herd-3fd3d8").expect("valid name"),
            protocol: PROTOCOL_VERSION,
            server: ServerVersion::CURRENT,
            regions: vec![RegionId::from_raw(7), RegionId::from_raw(8)],
            load: EdgeLoad {
                clients: 512,
                entities: 1_024,
                observers: 512,
                relayed: 10_240,
                undeliverable: 3,
                commands: 20_480,
                refused: 1,
            },
        }
    }

    #[test]
    fn an_edge_heartbeat_round_trips() {
        let m = edge_sample();
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(EdgeHeartbeat::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn an_edge_heartbeat_is_its_fixed_part_plus_what_varies() {
        let m = edge_sample();
        let mut buf = Vec::new();
        m.encode(&mut buf);
        let varies = m.edge.as_str().len() + m.regions.len() * 4;
        assert_eq!(buf.len(), EdgeHeartbeat::FIXED_BYTES + varies);
    }

    #[test]
    fn an_edge_heartbeat_carries_no_regions_when_it_holds_nothing() {
        let mut m = edge_sample();
        m.regions.clear();
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(EdgeHeartbeat::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_truncated_edge_heartbeat_is_refused() {
        let mut buf = Vec::new();
        edge_sample().encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(
                EdgeHeartbeat::decode(&buf[..cut]).is_err(),
                "{cut} bytes must not parse"
            );
        }
    }

    #[test]
    fn trailing_bytes_after_an_edge_heartbeat_are_refused() {
        let mut buf = Vec::new();
        edge_sample().encode(&mut buf);
        buf.push(0);
        assert!(EdgeHeartbeat::decode(&buf).is_err());
    }

    #[test]
    fn an_edge_name_that_could_address_another_edge_does_not_decode() {
        // The name is validated on the way back in, not only on the way out, so
        // a hand-built message cannot smuggle a wildcard through a watcher.
        let mut buf = Vec::new();
        edge_sample().encode(&mut buf);
        let name = b"herd-3fd3d8";
        let at = 1 + name.iter().position(|&b| b == b'3').expect("in the name");
        buf[at] = b'*';
        assert!(EdgeHeartbeat::decode(&buf).is_err());
    }

    #[test]
    fn an_absurd_tick_saturates_rather_than_wrapping() {
        let mut m = sample();
        m.load.worst_tick = Duration::from_secs(30);
        let mut buf = Vec::new();
        m.encode(&mut buf);
        let back = Heartbeat::decode(&buf).expect("well formed");
        assert_eq!(back.load.worst_tick, Duration::from_nanos(u64::from(u32::MAX)));
    }

    /// Whether a NATS subject matches a `*`-wildcard pattern token by token.
    fn matches(pattern: &str, concrete: &str) {
        let (p, c): (Vec<_>, Vec<_>) =
            (pattern.split('.').collect(), concrete.split('.').collect());
        assert_eq!(p.len(), c.len(), "{concrete} against {pattern}");
        assert!(
            p.iter().zip(&c).all(|(a, b)| *a == "*" || a == b),
            "{concrete} against {pattern}"
        );
    }

    #[test]
    fn the_documented_wildcards_match_what_is_published() {
        // Building a subject is crate-private, so a watcher outside the crate
        // subscribes to the patterns written in this module's doc. Nothing
        // constructs them any more, which is why they are spelled out here:
        // this is what keeps the doc honest if a subject ever changes shape.
        for region in [1u32, 7, 4_000_000] {
            matches(
                "umwelt.control.region.*.heartbeat",
                &subject(RegionId::from_raw(region)),
            );
        }
        for edge in ["herd-3fd3d8", "e1"] {
            let name = EdgeName::new(edge).expect("valid name");
            matches("umwelt.control.edge.*.heartbeat", &edge_subject(&name));
        }
    }

    #[test]
    fn a_heartbeat_prints_one_line() {
        let shown = sample().to_string();
        assert!(shown.contains("region 7"), "{shown}");
        assert!(shown.contains("8192 entities"), "{shown}");
        assert!(!shown.contains('\n'), "a watcher prints one per line");
    }
}
