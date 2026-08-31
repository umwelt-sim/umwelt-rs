//! What a game client and its edge say to each other.
//!
//! A different protocol from `net::region`, and deliberately not sharing types
//! with it. That one runs between peers deployed together; this one runs to
//! many clients on someone else's machine, updated on someone else's schedule.
//!
//! umwelt owns the movement and lifetime vocabulary here because that is what
//! it replicates. Everything else a game says to its clients rides in
//! [`FromClient::Message`] and [`ToClient::Message`] as bytes umwelt does not
//! read.
//!
//! # Framing
//!
//! A QUIC datagram carries exactly one message, so the body is the frame. A
//! QUIC stream is a byte sequence, so each message is prefixed with its length
//! as a `u32`. Both carry the same bodies, and the leading kind byte says which
//! message it is either way.

use crate::entity::{EntityId, EntityKind};
use crate::fixed::Fixed;
use crate::id::{EntityHandle, RegionId};
use crate::net::error::NetError;
use crate::net::wire::Cursor;
use crate::pos::Pos3;

/// One kind space across both directions, so a message is never ambiguous
/// about which way it was meant to travel. Not public: a consumer reads
/// [`FromClient`] and [`ToClient`], never the tag in front of one.
pub(crate) const KIND_SPAWN: u8 = 1;
pub(crate) const KIND_MOVE: u8 = 2;
pub(crate) const KIND_DESPAWN: u8 = 3;
pub(crate) const KIND_SPAWNED: u8 = 4;
pub(crate) const KIND_REMOVED: u8 = 5;
pub(crate) const KIND_STATE: u8 = 6;
/// The consumer's own, and the only kind that travels both ways.
pub(crate) const KIND_MESSAGE: u8 = 7;
pub(crate) const KIND_REGION: u8 = 8;
pub(crate) const KIND_MOVES: u8 = 9;
pub(crate) const KIND_TELEPORT: u8 = 10;
pub(crate) const KIND_TELEPORTED: u8 = 11;
pub(crate) const KIND_TELEPORT_FAILED: u8 = 12;

/// The largest body either end will frame on a stream.
///
/// A client that announces a longer one is disconnected rather than trusted: a
/// length prefix from an untrusted peer is otherwise an allocation it chooses.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Bytes a position occupies: three raw [`Fixed`] axes, as on the region wire.
const POS_BYTES: usize = 12;

/// Bytes one move in a batch takes: a handle and a position.
pub(crate) const MOVE_BYTES: usize = 4 + POS_BYTES;

/// Bytes a batch spends before its first move: the kind byte and the count.
pub(crate) const MOVES_HEADER_BYTES: usize = 5;

/// Most moves one [`FromClient::Moves`] may carry.
///
/// A decoder has to bound what it allocates for a claimed count, so this is
/// fixed. A *sender* sizes each batch against what its connection says a
/// datagram may carry and takes whichever is smaller — a path with a smaller
/// MTU than this assumes would otherwise have every batch refused.
///
/// Sending one datagram per entity instead is one per entity per tick: 163,840
/// a second at 8,192 entities and 20 Hz, each carrying sixteen bytes of payload
/// in a twelve-hundred-byte packet.
pub const MAX_MOVES_PER_DATAGRAM: usize = (1200 - MOVES_HEADER_BYTES) / MOVE_BYTES;

/// What a client is told about a region: only what it needs to read that
/// region's packets.
///
/// The two extents are the whole of the wire layout: horizontal bits come from
/// the region size and vertical bits from the extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeInfo {
    /// Which region.
    pub region: RegionId,
    /// Its horizontal extent, in meters.
    pub region_size_m: i32,
    /// Its vertical extent, in meters.
    pub vertical_extent_m: i32,
}

impl EdgeInfo {
    /// Its width on the wire.
    pub const BYTES: usize = 12;
}

/// What a game client sends its edge.
///
/// A client names entities by a handle it chose, not by the id a region
/// allocated, so it can move one the instant it asks for it. The edge maps
/// handles to regions and ids;.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromClient {
    /// Asks for an entity, in the region the game put this client in.
    ///
    /// The handle is spent once and is this connection's name for it from here
    /// on. The region is named because an edge has no home: it reaches every
    /// region through a wildcard subscription and has no way to know, or any
    /// business deciding, where a player belongs. That is the game's, and the
    /// game is what told this client which region it is in.
    Spawn {
        /// This connection's name for it from here on.
        handle: EntityHandle,
        /// Where the game decided this client belongs.
        region: RegionId,
        /// Where to put it.
        position: Pos3,
        /// What is behind it.
        kind: EntityKind,
    },
    /// A new absolute position. Latest-only, so this rides a datagram.
    Move {
        /// Which entity.
        handle: EntityHandle,
        /// Where it is now.
        position: Pos3,
    },
    /// Several new positions at once, which is what a client with more than a
    /// handful of entities sends. Latest-only, so this rides a datagram too.
    Moves(Vec<(EntityHandle, Pos3)>),
    /// Gives an entity back.
    Despawn {
        /// Which entity.
        handle: EntityHandle,
    },
    /// The game's own, which umwelt does not read.
    Message(Vec<u8>),
    /// Asks the edge to teleport an entity to another region. The handle stays
    /// valid throughout — moves sent during the transition are held at the edge
    /// and forwarded when the destination confirms.
    Teleport {
        /// Which entity.
        handle: EntityHandle,
        /// The destination region.
        region: RegionId,
        /// Where to put it in the destination.
        position: Pos3,
    },
}

impl FromClient {
    /// Whether this is latest-only, and so belongs on a datagram rather than
    /// the stream. A lost `Move` is superseded within a tick; a lost `Spawn`
    /// is not recoverable by anything.
    pub fn is_latest_only(&self) -> bool {
        matches!(self, FromClient::Move { .. } | FromClient::Moves(_))
    }

    /// Appends the encoded message.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            FromClient::Spawn { handle, region, position, kind } => {
                out.push(KIND_SPAWN);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
                put_pos(*position, out);
                out.push(kind.as_u8());
            }
            FromClient::Move { handle, position } => {
                out.push(KIND_MOVE);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                put_pos(*position, out);
            }
            FromClient::Moves(moves) => {
                out.push(KIND_MOVES);
                out.extend_from_slice(&(moves.len() as u32).to_le_bytes());
                for (handle, position) in moves {
                    out.extend_from_slice(&handle.raw().to_le_bytes());
                    put_pos(*position, out);
                }
            }
            FromClient::Despawn { handle } => {
                out.push(KIND_DESPAWN);
                out.extend_from_slice(&handle.raw().to_le_bytes());
            }
            FromClient::Message(body) => {
                out.push(KIND_MESSAGE);
                out.extend_from_slice(body);
            }
            FromClient::Teleport { handle, region, position } => {
                out.push(KIND_TELEPORT);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
                put_pos(*position, out);
            }
        }
    }

    /// Reads one back from a whole frame.
    pub fn decode(frame: &[u8]) -> Result<FromClient, NetError> {
        let (&kind, body) =
            frame.split_first().ok_or(NetError::Malformed("client message"))?;
        match kind {
            KIND_SPAWN => {
                let mut c = Cursor::new(body, "client spawn");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                let position = get_pos(&mut c)?;
                let kind = EntityKind::from_u8(c.u8()?)
                    .ok_or(NetError::Malformed("client spawn kind"))?;
                c.finish()?;
                Ok(FromClient::Spawn { handle, region, position, kind })
            }
            KIND_MOVE => {
                let mut c = Cursor::new(body, "client move");
                let handle = EntityHandle::from_raw(c.u32()?);
                let position = get_pos(&mut c)?;
                c.finish()?;
                Ok(FromClient::Move { handle, position })
            }
            KIND_MOVES => {
                let mut c = Cursor::new(body, "client moves");
                let count = c.u32()? as usize;
                // The cap bounds what a decoder allocates for a claimed count,
                // so it does not move to suit a caller.
                if count > MAX_MOVES_PER_DATAGRAM {
                    return Err(NetError::Malformed("client moves count"));
                }
                let mut moves = Vec::with_capacity(count);
                for _ in 0..count {
                    moves.push((EntityHandle::from_raw(c.u32()?), get_pos(&mut c)?));
                }
                c.finish()?;
                Ok(FromClient::Moves(moves))
            }
            KIND_DESPAWN => {
                let mut c = Cursor::new(body, "client despawn");
                let handle = EntityHandle::from_raw(c.u32()?);
                c.finish()?;
                Ok(FromClient::Despawn { handle })
            }
            KIND_MESSAGE => Ok(FromClient::Message(body.to_vec())),
            KIND_TELEPORT => {
                let mut c = Cursor::new(body, "client teleport");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                let position = get_pos(&mut c)?;
                c.finish()?;
                Ok(FromClient::Teleport { handle, region, position })
            }
            got => Err(NetError::Unexpected { expected: "a client command", got }),
        }
    }
}

/// What an edge sends a game client.
///
/// `State` carries the region's packet exactly as the region built it. The edge
/// routed on the four-byte avatar in front of it and replaces those bytes with
/// the region the packet came from, which is not a decode: authoritative state
/// does not lose authority by passing through a relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToClient<'a> {
    /// How to read a region's packets, sent before anything else about that
    /// region.
    ///
    /// Not sent at connect time, because an edge has no home region: it learns
    /// which regions a client cares about when the client asks for one.
    Region(EdgeInfo),
    /// A region allocated an id for the entity this handle asked for.
    Spawned {
        /// The handle that asked.
        handle: EntityHandle,
        /// Where it ended up.
        region: RegionId,
        /// What that region calls it.
        entity: EntityId,
    },
    /// Gone, whatever caused it.
    Removed {
        /// The handle that named it.
        handle: EntityHandle,
    },
    /// What one of this client's entities can see. Named by the handle that
    /// asked for it, not by the entity or the region: the edge knows which
    /// avatar a packet was built for and which of this client's handles that
    /// is, and a game has no use for the other two.
    ///
    /// Latest-only, so this rides a datagram.
    State {
        /// Which of this client's entities is looking.
        handle: EntityHandle,
        /// The region's packet, untouched except for the four bytes in front.
        packet: &'a [u8],
    },
    /// The game's own, which umwelt does not read.
    Message(&'a [u8]),
    /// An entity arrived in its destination region. The handle is the same one
    /// the client has always held. Preceded by a `Spawned` that carries the new
    /// region and entity id.
    Teleported {
        /// The handle that asked.
        handle: EntityHandle,
        /// Where it ended up.
        region: RegionId,
    },
    /// A teleport did not complete. The entity stays in its origin region.
    TeleportFailed {
        /// The handle that asked.
        handle: EntityHandle,
        /// The destination that was refused or unreachable.
        region: RegionId,
    },
}

impl ToClient<'_> {
    /// Whether this is latest-only, and so belongs on a datagram.
    pub fn is_latest_only(&self) -> bool {
        matches!(self, ToClient::State { .. })
    }

    /// Replaces `out` with the encoded message.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        self.encode_onto(out);
    }

    /// Appends, so a caller framing on a stream can reserve the length prefix
    /// first and fill it in after, rather than encoding into one buffer and
    /// copying into another to put four bytes in front.
    pub(crate) fn encode_onto(&self, out: &mut Vec<u8>) {
        match self {
            ToClient::Region(info) => {
                out.push(KIND_REGION);
                out.extend_from_slice(&info.region.raw().to_le_bytes());
                out.extend_from_slice(&info.region_size_m.to_le_bytes());
                out.extend_from_slice(&info.vertical_extent_m.to_le_bytes());
            }
            ToClient::Spawned { handle, region, entity } => {
                out.push(KIND_SPAWNED);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
                out.extend_from_slice(&entity.raw().to_le_bytes());
            }
            ToClient::Removed { handle } => {
                out.push(KIND_REMOVED);
                out.extend_from_slice(&handle.raw().to_le_bytes());
            }
            ToClient::State { handle, packet } => {
                out.push(KIND_STATE);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(packet);
            }
            ToClient::Message(body) => {
                out.push(KIND_MESSAGE);
                out.extend_from_slice(body);
            }
            ToClient::Teleported { handle, region } => {
                out.push(KIND_TELEPORTED);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
            }
            ToClient::TeleportFailed { handle, region } => {
                out.push(KIND_TELEPORT_FAILED);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
            }
        }
    }

    /// Reads one back, borrowing the frame rather than copying it.
    pub fn decode(frame: &[u8]) -> Result<ToClient<'_>, NetError> {
        let (&kind, body) =
            frame.split_first().ok_or(NetError::Malformed("edge message"))?;
        match kind {
            KIND_REGION => {
                let mut c = Cursor::new(body, "region info");
                let region = RegionId::from_raw(c.u32()?);
                let region_size_m = c.i32()?;
                let vertical_extent_m = c.i32()?;
                c.finish()?;
                Ok(ToClient::Region(EdgeInfo {
                    region,
                    region_size_m,
                    vertical_extent_m,
                }))
            }
            KIND_SPAWNED => {
                let mut c = Cursor::new(body, "spawned");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                let entity = EntityId::from_raw(c.u32()?);
                c.finish()?;
                Ok(ToClient::Spawned { handle, region, entity })
            }
            KIND_REMOVED => {
                let mut c = Cursor::new(body, "removed");
                let handle = EntityHandle::from_raw(c.u32()?);
                c.finish()?;
                Ok(ToClient::Removed { handle })
            }
            KIND_STATE => {
                if body.len() < 4 {
                    return Err(NetError::Malformed("state"));
                }
                let handle = EntityHandle::from_raw(u32::from_le_bytes([
                    body[0], body[1], body[2], body[3],
                ]));
                Ok(ToClient::State { handle, packet: &body[4..] })
            }
            KIND_MESSAGE => Ok(ToClient::Message(body)),
            KIND_TELEPORTED => {
                let mut c = Cursor::new(body, "teleported");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                c.finish()?;
                Ok(ToClient::Teleported { handle, region })
            }
            KIND_TELEPORT_FAILED => {
                let mut c = Cursor::new(body, "teleport failed");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                c.finish()?;
                Ok(ToClient::TeleportFailed { handle, region })
            }
            got => Err(NetError::Unexpected { expected: "an edge message", got }),
        }
    }
}

fn put_pos(pos: Pos3, out: &mut Vec<u8>) {
    out.reserve(POS_BYTES);
    out.extend_from_slice(&pos.x.raw().to_le_bytes());
    out.extend_from_slice(&pos.y.raw().to_le_bytes());
    out.extend_from_slice(&pos.z.raw().to_le_bytes());
}

fn get_pos(c: &mut Cursor<'_>) -> Result<Pos3, NetError> {
    Ok(Pos3::new(
        Fixed::from_raw(c.i32()?),
        Fixed::from_raw(c.i32()?),
        Fixed::from_raw(c.i32()?),
    ))
}

/// Reads length-prefixed messages off a QUIC stream.
///
/// A stream is a byte sequence with no message boundaries, so each body is
/// preceded by its length. A length past [`MAX_MESSAGE_BYTES`] is refused
/// rather than allocated: on this link the peer is not trusted, and a length
/// prefix it chose is otherwise an allocation it chose.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    /// Holding no partial frame.
    pub fn new() -> Framer {
        Framer::default()
    }

    /// Writes one framed message into `out`, which is cleared first.
    pub fn frame(body: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
    }

    /// Adds bytes read off the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Takes the next complete message, if one has arrived.
    ///
    ///
    /// `Err` means the peer announced a body longer than
    /// [`MAX_MESSAGE_BYTES`], which is not recoverable: the stream cannot be
    /// resynchronized, so the caller drops the connection.
    pub fn take(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]])
            as usize;
        if len > MAX_MESSAGE_BYTES {
            return Err(NetError::Malformed("client frame length"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand, since every message in here names one.
    fn h(raw: u32) -> EntityHandle {
        EntityHandle::from_raw(raw)
    }

    fn pos() -> Pos3 {
        Pos3::from_meters(100, 200, 5)
    }

    fn up() -> Vec<FromClient> {
        vec![
            FromClient::Spawn {
                handle: h(7),
                region: RegionId::from_raw(9),
                position: pos(),
                kind: EntityKind::Observer,
            },
            FromClient::Spawn {
                handle: h(0),
                region: RegionId::from_raw(4_000_000_000),
                position: pos(),
                kind: EntityKind::Unattended,
            },
            FromClient::Move { handle: h(9), position: pos() },
            FromClient::Moves(vec![(h(1), pos()), (h(2), pos()), (h(3), pos())]),
            FromClient::Moves(Vec::new()),
            FromClient::Despawn { handle: h(4_000_000_000) },
            FromClient::Message(b"the game's own".to_vec()),
            FromClient::Message(Vec::new()),
            FromClient::Teleport {
                handle: h(5),
                region: RegionId::from_raw(42),
                position: pos(),
            },
        ]
    }

    fn down() -> Vec<ToClient<'static>> {
        vec![
            ToClient::Region(EdgeInfo {
                region: RegionId::from_raw(9),
                region_size_m: 4096,
                vertical_extent_m: 1024,
            }),
            ToClient::Spawned {
                handle: h(7),
                region: RegionId::from_raw(9),
                entity: EntityId::from_raw(42),
            },
            ToClient::Removed { handle: h(7) },
            ToClient::State { handle: h(7), packet: b"a packet" },
            ToClient::State { handle: h(7), packet: b"" },
            ToClient::Message(b"the game's own"),
            ToClient::Teleported {
                handle: h(5),
                region: RegionId::from_raw(42),
            },
            ToClient::TeleportFailed {
                handle: h(5),
                region: RegionId::from_raw(42),
            },
        ]
    }

    #[test]
    fn client_messages_round_trip() {
        let mut buf = Vec::new();
        for m in up() {
            m.encode(&mut buf);
            assert_eq!(FromClient::decode(&buf).expect("well formed"), m, "{m:?}");
        }
    }

    #[test]
    fn edge_messages_round_trip() {
        let mut buf = Vec::new();
        for m in down() {
            m.encode(&mut buf);
            assert_eq!(ToClient::decode(&buf).expect("well formed"), m, "{m:?}");
        }
    }

    #[test]
    fn a_truncated_client_message_is_refused() {
        let mut buf = Vec::new();
        for m in up() {
            // A `Message` is bytes with no shape, so any prefix of one is a
            // shorter valid message. Everything else has fields to run out of.
            if matches!(m, FromClient::Message(_)) {
                continue;
            }
            m.encode(&mut buf);
            for cut in 0..buf.len() {
                assert!(
                    FromClient::decode(&buf[..cut]).is_err(),
                    "{m:?} at {cut} bytes must not parse"
                );
            }
        }
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let mut buf = Vec::new();
        for m in up() {
            if matches!(m, FromClient::Message(_)) {
                continue;
            }
            m.encode(&mut buf);
            buf.push(0);
            assert!(FromClient::decode(&buf).is_err(), "{m:?} with a trailing byte");
        }
    }

    #[test]
    fn a_full_batch_of_moves_fits_one_datagram() {
        // What batching saves is packets, not bytes: sixty-eight moves are
        // 1,093 bytes together against 1,156 apart, but one datagram against
        // sixty-eight, each of which would carry its own UDP and QUIC headers
        // and cost a send.
        let batch: Vec<(EntityHandle, Pos3)> =
            (0..MAX_MOVES_PER_DATAGRAM as u32).map(|n| (h(n), pos())).collect();
        let mut framed = Vec::new();
        FromClient::Moves(batch.clone()).encode(&mut framed);
        assert!(
            framed.len() <= 1200,
            "a full batch is {} bytes and must fit the protocol's own cap",
            framed.len()
        );
        assert_eq!(
            FromClient::decode(&framed).expect("well formed"),
            FromClient::Moves(batch)
        );
    }

    #[test]
    fn a_batch_past_the_cap_is_refused() {
        let mut body = vec![KIND_MOVES];
        body.extend_from_slice(&(u32::MAX).to_le_bytes());
        assert!(
            FromClient::decode(&body).is_err(),
            "an absurd count must not be believed"
        );
    }

    #[test]
    fn the_wire_sizes_are_what_they_look_like() {
        let mut buf = Vec::new();
        FromClient::Move { handle: h(1), position: pos() }.encode(&mut buf);
        assert_eq!(buf.len(), 1 + 4 + POS_BYTES, "kind, handle, position");
        FromClient::Spawn {
            handle: h(1),
            region: RegionId::from_raw(1),
            position: pos(),
            kind: EntityKind::Observer,
        }
        .encode(&mut buf);
        assert_eq!(buf.len(), 1 + 4 + 4 + POS_BYTES + 1, "a region and a kind byte");
        FromClient::Despawn { handle: h(1) }.encode(&mut buf);
        assert_eq!(buf.len(), 1 + 4);
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        assert!(matches!(
            FromClient::decode(&[200, 0, 0, 0, 0]),
            Err(NetError::Unexpected { got: 200, .. })
        ));
        assert!(matches!(
            ToClient::decode(&[200]),
            Err(NetError::Unexpected { got: 200, .. })
        ));
    }

    #[test]
    fn a_message_going_the_wrong_way_is_refused() {
        // One kind space across both directions, so a `Spawned` arriving at an
        // edge does not decode as something an edge acts on.
        let mut buf = Vec::new();
        ToClient::Spawned {
            handle: h(1),
            region: RegionId::from_raw(1),
            entity: EntityId::from_raw(1),
        }
        .encode(&mut buf);
        assert!(FromClient::decode(&buf).is_err());

        FromClient::Despawn { handle: h(1) }.encode(&mut buf);
        assert!(ToClient::decode(&buf).is_err());
    }

    #[test]
    fn only_the_latest_only_kinds_ride_datagrams() {
        for m in up() {
            let expected = matches!(m, FromClient::Move { .. } | FromClient::Moves(_));
            assert_eq!(m.is_latest_only(), expected, "{m:?}");
        }
        for m in down() {
            assert_eq!(m.is_latest_only(), matches!(m, ToClient::State { .. }), "{m:?}");
        }
    }

    #[test]
    fn a_framer_reassembles_messages_split_across_reads() {
        let mut buf = Vec::new();
        let mut wire = Vec::new();
        let mut framed = Vec::new();
        for m in up() {
            m.encode(&mut buf);
            Framer::frame(&buf, &mut framed);
            wire.extend_from_slice(&framed);
        }

        // One byte at a time, which is the worst a stream can do.
        let mut f = Framer::new();
        let mut got = Vec::new();
        for b in &wire {
            f.push(&[*b]);
            while let Some(body) = f.take().expect("well formed") {
                got.push(FromClient::decode(&body).expect("well formed"));
            }
        }
        assert_eq!(got, up());
    }

    #[test]
    fn a_framer_refuses_a_length_it_would_have_to_allocate() {
        let mut f = Framer::new();
        f.push(&(u32::MAX).to_le_bytes());
        assert!(f.take().is_err(), "an absurd length must not be believed");
    }

    #[test]
    fn a_framer_holds_a_partial_length_prefix() {
        let mut f = Framer::new();
        f.push(&[1, 2, 3]);
        assert_eq!(f.take().expect("not yet a length"), None);
    }
}
