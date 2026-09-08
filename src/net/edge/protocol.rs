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
use crate::id::{EntityHandle, RegionId};
use crate::map::Placement;
use crate::net::error::NetError;
use crate::net::wire::Cursor;
use crate::pos::WorldPos;

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
pub(crate) const KIND_ENTITY_MESSAGE: u8 = 13;
pub(crate) const KIND_TELEPORT_INTO: u8 = 14;
pub(crate) const KIND_SPAWN_INTO: u8 = 15;

/// Names a kind for an error message, without echoing the peer's bytes.
///
/// This link's own table. `net::region` numbers its messages separately, and
/// the same byte names something different there.
pub(crate) fn kind_name(kind: u8) -> &'static str {
    match kind {
        KIND_SPAWN => "spawn",
        KIND_MOVE => "move",
        KIND_DESPAWN => "despawn",
        KIND_SPAWNED => "spawned",
        KIND_REMOVED => "removed",
        KIND_STATE => "state",
        KIND_MESSAGE => "message",
        KIND_REGION => "region",
        KIND_MOVES => "moves",
        KIND_TELEPORT => "teleport",
        KIND_TELEPORTED => "teleported",
        KIND_TELEPORT_FAILED => "teleport failed",
        KIND_ENTITY_MESSAGE => "entity message",
        KIND_TELEPORT_INTO => "teleport into",
        KIND_SPAWN_INTO => "spawn into",
        _ => "unknown",
    }
}

/// The largest body either end will frame on a stream.
///
/// A client that announces a longer one is disconnected rather than trusted: a
/// length prefix from an untrusted peer is otherwise an allocation it chooses.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Bytes a position occupies: three raw [`Fixed`] axes, as on the region wire.
/// A `WorldPos` on this wire: three little-endian `i64`.
const WORLD_BYTES: usize = 24;

/// Bytes one move in a batch takes: a handle and a position.
pub(crate) const MOVE_BYTES: usize = 4 + WORLD_BYTES;

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
    /// Where the region sits on the world map, or `None` for a region off
    /// the map, whose world frame is its own (`docs/adr/0010`). This is the
    /// frame the client rebuilds world positions with.
    pub placement: Option<Placement>,
}

impl EdgeInfo {
    /// Its width on the wire: the three fields, a flag byte, and a placement
    /// written whether or not the flag is set.
    pub const BYTES: usize = 21;
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
        /// Where to put it, in world coordinates. The edge finds the region
        /// on the map; a position no placed region covers is refused.
        position: WorldPos,
        /// What is behind it.
        kind: EntityKind,
    },
    /// Asks for an entity in a named region, which is the one way to start
    /// in a region off the map. `position` is in that region's frame: the
    /// map's if it is placed, its own if not.
    SpawnInto {
        /// This connection's name for it from here on.
        handle: EntityHandle,
        /// The region to start in.
        region: RegionId,
        /// Where to put it, in the region's frame.
        position: WorldPos,
        /// What is behind it.
        kind: EntityKind,
    },
    /// A new absolute position. Latest-only, so this rides a datagram.
    Move {
        /// Which entity.
        handle: EntityHandle,
        /// Where it is now, in world coordinates.
        position: WorldPos,
    },
    /// Several new positions at once, which is what a client with more than a
    /// handful of entities sends. Latest-only, so this rides a datagram too.
    Moves(Vec<(EntityHandle, WorldPos)>),
    /// Gives an entity back.
    Despawn {
        /// Which entity.
        handle: EntityHandle,
    },
    /// The game's own bytes, delivered to
    /// [`EdgeGame::message_received`](crate::EdgeGame::message_received).
    /// umwelt does not read them.
    Message(Vec<u8>),
    /// The game's own bytes, addressed to the region an entity lives in.
    /// The edge resolves the handle and relays automatically; the message
    /// arrives at [`Game::message_received`](crate::Game::message_received)
    /// with the entity's id as the sender.
    EntityMessage {
        /// Which entity this message is from.
        handle: EntityHandle,
        /// The game's own bytes.
        body: Vec<u8>,
    },
    /// Asks the edge to teleport an entity to a world position, which the edge
    /// resolves through the map. The handle stays valid throughout: moves sent
    /// during the transition are held at the edge and forwarded when the
    /// destination confirms.
    Teleport {
        /// Which entity.
        handle: EntityHandle,
        /// Where to put it, in world coordinates.
        at: WorldPos,
    },
    /// Asks the edge to teleport an entity into a named region, which is the
    /// one door into a region off the map. `at` is in that region's frame:
    /// the map's if it is placed, its own if not.
    TeleportInto {
        /// Which entity.
        handle: EntityHandle,
        /// The destination region.
        region: RegionId,
        /// Where to put it, in the destination's frame.
        at: WorldPos,
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
            FromClient::Spawn { handle, position, kind } => {
                out.push(KIND_SPAWN);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                put_world(*position, out);
                kind.encode_wire(out);
            }
            FromClient::SpawnInto { handle, region, position, kind } => {
                out.push(KIND_SPAWN_INTO);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
                put_world(*position, out);
                kind.encode_wire(out);
            }
            FromClient::Move { handle, position } => {
                out.push(KIND_MOVE);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                put_world(*position, out);
            }
            FromClient::Moves(moves) => {
                out.push(KIND_MOVES);
                out.extend_from_slice(&(moves.len() as u32).to_le_bytes());
                for (handle, position) in moves {
                    out.extend_from_slice(&handle.raw().to_le_bytes());
                    put_world(*position, out);
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
            FromClient::EntityMessage { handle, body } => {
                out.push(KIND_ENTITY_MESSAGE);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(body);
            }
            FromClient::Teleport { handle, at } => {
                out.push(KIND_TELEPORT);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                put_world(*at, out);
            }
            FromClient::TeleportInto { handle, region, at } => {
                out.push(KIND_TELEPORT_INTO);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
                put_world(*at, out);
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
                let position = get_world(&mut c)?;
                let kind = EntityKind::decode_wire(&mut c)?;
                c.finish()?;
                Ok(FromClient::Spawn { handle, position, kind })
            }
            KIND_SPAWN_INTO => {
                let mut c = Cursor::new(body, "client spawn into");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                let position = get_world(&mut c)?;
                let kind = EntityKind::decode_wire(&mut c)?;
                c.finish()?;
                Ok(FromClient::SpawnInto { handle, region, position, kind })
            }
            KIND_MOVE => {
                let mut c = Cursor::new(body, "client move");
                let handle = EntityHandle::from_raw(c.u32()?);
                let position = get_world(&mut c)?;
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
                    moves.push((EntityHandle::from_raw(c.u32()?), get_world(&mut c)?));
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
            KIND_ENTITY_MESSAGE => {
                let mut c = Cursor::new(body, "client entity message");
                let handle = EntityHandle::from_raw(c.u32()?);
                let body = c.rest().to_vec();
                Ok(FromClient::EntityMessage { handle, body })
            }
            KIND_TELEPORT => {
                let mut c = Cursor::new(body, "client teleport");
                let handle = EntityHandle::from_raw(c.u32()?);
                let at = get_world(&mut c)?;
                c.finish()?;
                Ok(FromClient::Teleport { handle, at })
            }
            KIND_TELEPORT_INTO => {
                let mut c = Cursor::new(body, "client teleport into");
                let handle = EntityHandle::from_raw(c.u32()?);
                let region = RegionId::from_raw(c.u32()?);
                let at = get_world(&mut c)?;
                c.finish()?;
                Ok(FromClient::TeleportInto { handle, region, at })
            }
            got => Err(NetError::Unexpected {
                expected: "a client command",
                got,
                name: kind_name(got),
            }),
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
        /// Which of this client's entities is looking. Near a seam the same
        /// handle receives packets from two regions, its own and its
        /// shadow's.
        handle: EntityHandle,
        /// Which region built it, and so which frame its positions are in.
        region: RegionId,
        /// The region's packet, untouched except for the bytes in front.
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
                let at = info.placement.unwrap_or(Placement::new(0, 0));
                out.push(info.placement.is_some() as u8);
                out.extend_from_slice(&at.col.to_le_bytes());
                out.extend_from_slice(&at.row.to_le_bytes());
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
            ToClient::State { handle, region, packet } => {
                out.push(KIND_STATE);
                out.extend_from_slice(&handle.raw().to_le_bytes());
                out.extend_from_slice(&region.raw().to_le_bytes());
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
                let placed = c.u8()?;
                let col = c.i32()?;
                let row = c.i32()?;
                c.finish()?;
                let placement = match placed {
                    0 => None,
                    1 => Some(Placement::new(col, row)),
                    _ => return Err(NetError::Malformed("region placement flag")),
                };
                Ok(ToClient::Region(EdgeInfo {
                    region,
                    region_size_m,
                    vertical_extent_m,
                    placement,
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
                if body.len() < 8 {
                    return Err(NetError::Malformed("state"));
                }
                let handle = EntityHandle::from_raw(u32::from_le_bytes([
                    body[0], body[1], body[2], body[3],
                ]));
                let region = RegionId::from_raw(u32::from_le_bytes([
                    body[4], body[5], body[6], body[7],
                ]));
                Ok(ToClient::State { handle, region, packet: &body[8..] })
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
            got => Err(NetError::Unexpected {
                expected: "an edge message",
                got,
                name: kind_name(got),
            }),
        }
    }
}

fn put_world(at: WorldPos, out: &mut Vec<u8>) {
    out.reserve(WORLD_BYTES);
    out.extend_from_slice(&at.x.to_le_bytes());
    out.extend_from_slice(&at.y.to_le_bytes());
    out.extend_from_slice(&at.z.to_le_bytes());
}

fn get_world(c: &mut Cursor<'_>) -> Result<WorldPos, NetError> {
    Ok(WorldPos::from_raw(c.i64()?, c.i64()?, c.i64()?))
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
    /// How far into `buf` has been handed out. Taking a message advances this
    /// rather than removing bytes from the front, because removing them moves
    /// everything behind down and one read holding many messages would pay
    /// that per message. The dead prefix is reclaimed once per read instead.
    at: usize,
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
    ///
    /// Whatever previous reads handed out is dropped here, so the buffer is
    /// compacted once per read rather than once per message taken from it.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.at > 0 {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Takes the next complete message, if one has arrived.
    ///
    ///
    /// `Err` means the peer announced a body longer than
    /// [`MAX_MESSAGE_BYTES`], which is not recoverable: the stream cannot be
    /// resynchronized, so the caller drops the connection.
    pub fn take(&mut self) -> Result<Option<Vec<u8>>, NetError> {
        let head = self.at;
        let waiting = self.buf.len() - head;
        if waiting < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([
            self.buf[head],
            self.buf[head + 1],
            self.buf[head + 2],
            self.buf[head + 3],
        ]) as usize;
        if len > MAX_MESSAGE_BYTES {
            return Err(NetError::Malformed("client frame length"));
        }
        if waiting < 4 + len {
            return Ok(None);
        }
        self.at = head + 4 + len;
        Ok(Some(self.buf[head + 4..head + 4 + len].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand, since every message in here names one.
    fn h(raw: u32) -> EntityHandle {
        EntityHandle::from_raw(raw)
    }

    fn pos() -> WorldPos {
        WorldPos::from_meters(-100, 200_000, 5)
    }

    fn up() -> Vec<FromClient> {
        vec![
            FromClient::Spawn {
                handle: h(7),
                position: pos(),
                kind: EntityKind::observer(0),
            },
            FromClient::Spawn {
                handle: h(0),
                position: WorldPos::from_raw(i64::MIN, i64::MAX, 0),
                kind: EntityKind::unattended(0),
            },
            FromClient::SpawnInto {
                handle: h(8),
                region: RegionId::from_raw(4_000_000_000),
                position: pos(),
                kind: EntityKind::observer(3),
            },
            FromClient::Move { handle: h(9), position: pos() },
            FromClient::Moves(vec![(h(1), pos()), (h(2), pos()), (h(3), pos())]),
            FromClient::Moves(Vec::new()),
            FromClient::Despawn { handle: h(4_000_000_000) },
            FromClient::Message(b"the game's own".to_vec()),
            FromClient::Message(Vec::new()),
            FromClient::EntityMessage { handle: h(3), body: b"plant lettuce".to_vec() },
            FromClient::EntityMessage { handle: h(3), body: Vec::new() },
            FromClient::Teleport { handle: h(5), at: pos() },
            FromClient::TeleportInto {
                handle: h(5),
                region: RegionId::from_raw(42),
                at: pos(),
            },
        ]
    }

    fn down() -> Vec<ToClient<'static>> {
        vec![
            ToClient::Region(EdgeInfo {
                region: RegionId::from_raw(9),
                region_size_m: 4096,
                vertical_extent_m: 1024,
                placement: Some(Placement::new(-3, 7)),
            }),
            ToClient::Region(EdgeInfo {
                region: RegionId::from_raw(10),
                region_size_m: 4096,
                vertical_extent_m: 1024,
                placement: None,
            }),
            ToClient::Spawned {
                handle: h(7),
                region: RegionId::from_raw(9),
                entity: EntityId::from_raw(42),
            },
            ToClient::Removed { handle: h(7) },
            ToClient::State {
                handle: h(7),
                region: RegionId::from_raw(9),
                packet: b"a packet",
            },
            ToClient::State { handle: h(7), region: RegionId::from_raw(10), packet: b"" },
            ToClient::Message(b"the game's own"),
            ToClient::Teleported { handle: h(5), region: RegionId::from_raw(42) },
            ToClient::TeleportFailed { handle: h(5), region: RegionId::from_raw(42) },
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
            // shorter valid message. `EntityMessage` has a fixed handle before
            // a variable tail, so the same applies past the handle.
            if matches!(m, FromClient::Message(_) | FromClient::EntityMessage { .. }) {
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
            if matches!(m, FromClient::Message(_) | FromClient::EntityMessage { .. }) {
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
        let batch: Vec<(EntityHandle, WorldPos)> =
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
        assert_eq!(buf.len(), 1 + 4 + WORLD_BYTES, "kind, handle, world position");
        FromClient::Spawn {
            handle: h(1),
            position: pos(),
            kind: EntityKind::observer(0),
        }
        .encode(&mut buf);
        assert_eq!(
            buf.len(),
            1 + 4 + WORLD_BYTES + 3,
            "a world position and a 3-byte kind"
        );
        ToClient::Region(EdgeInfo {
            region: RegionId::from_raw(1),
            region_size_m: 4096,
            vertical_extent_m: 1024,
            placement: None,
        })
        .encode(&mut buf);
        assert_eq!(buf.len(), 1 + EdgeInfo::BYTES, "a region's frame is fixed width");
        FromClient::Despawn { handle: h(1) }.encode(&mut buf);
        assert_eq!(buf.len(), 1 + 4);
    }

    /// The two links number their messages separately, so an error about a
    /// byte that arrived here must name it from this table. Kind 4 is
    /// `spawned` on this link and `keepalive` on the region link; kind 5 is
    /// `removed` here and `game message` there.
    #[test]
    fn an_unexpected_kind_is_named_by_this_links_table() {
        let refused = ToClient::decode(&[KIND_SPAWN]).expect_err("a client kind");
        let NetError::Unexpected { got, name, .. } = refused else {
            panic!("expected an unexpected-kind error, got {refused:?}");
        };
        assert_eq!(got, KIND_SPAWN);
        assert_eq!(name, "spawn");

        assert_eq!(kind_name(KIND_SPAWNED), "spawned");
        assert_eq!(kind_name(KIND_REMOVED), "removed");
        assert_eq!(kind_name(200), "unknown");
        for kind in 1..=KIND_ENTITY_MESSAGE {
            assert_ne!(kind_name(kind), "unknown", "kind {kind} needs a name");
        }
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

    /// Taking a message advances a cursor instead of removing bytes, and the
    /// dead prefix is dropped on the next push. A burst arriving in one read
    /// exercises the cursor, and a message straddling the compaction is what
    /// would corrupt if the prefix were dropped by the wrong amount.
    #[test]
    fn a_framer_survives_compaction_with_a_message_straddling_it() {
        let mut buf = Vec::new();
        let mut framed = Vec::new();
        let mut wire = Vec::new();
        for m in up() {
            m.encode(&mut buf);
            Framer::frame(&buf, &mut framed);
            wire.extend_from_slice(&framed);
        }

        // A whole burst in one read: every message must come out in order.
        let mut f = Framer::new();
        f.push(&wire);
        let mut got = Vec::new();
        while let Some(body) = f.take().expect("well formed") {
            got.push(FromClient::decode(&body).expect("well formed"));
        }
        assert_eq!(got, up(), "a burst in one read");

        // Now cut the wire mid-message so a partial tail is still buffered
        // when the next push compacts, and the message completes across it.
        for cut in [1usize, 7, wire.len() / 3, wire.len() - 2] {
            let mut f = Framer::new();
            f.push(&wire[..cut]);
            let mut got = Vec::new();
            while let Some(body) = f.take().expect("well formed") {
                got.push(FromClient::decode(&body).expect("well formed"));
            }
            // Whatever was left half-read is completed by the second push.
            f.push(&wire[cut..]);
            while let Some(body) = f.take().expect("well formed") {
                got.push(FromClient::decode(&body).expect("well formed"));
            }
            assert_eq!(got, up(), "split at {cut}");
        }
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
