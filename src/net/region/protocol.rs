//! The messages on the region-to-edge link.
//!
//! A message belongs to the protocol, not to whichever end happens to send it.
//! Each is named for what it carries: [`ClientIdentification`] identifies the
//! connecting client, [`ServerInfo`] describes the server, [`Rejection`] says
//! the connection will not be taken.
//!
//! The exchange, and why it is in this order:
//!
//! 1. The client sends [`ClientIdentification`]: the protocol version it
//!    speaks, and its credential.
//! 2. The server authorizes it. A peer that fails gets a bare [`Rejection`] and
//!    the connection closes.
//! 3. The server sends [`ServerInfo`]: its version, its region id, and the
//!    world parameters.
//! 4. The client accepts, or quits. Both are empty frames.
//!
//! **The identification comes first and the server info comes after it.** A
//! peer that cannot authorize never learns the region's size, its tick rate, or
//! its id. Putting the info first would have been friendlier to write and would
//! hand the shape of the world to anyone able to open a socket.
//!
//! The client accepts or quits rather than the server assuming it will stay. A
//! client that reads the parameters and finds a world it cannot render, or a
//! region it did not mean to reach, quits, so the server does not count an edge
//! that is about to leave.

use core::fmt;
use std::time::Duration;

use crate::config::WorldConfig;
use crate::entity::EntityId;
use crate::net::error::{NetError, RejectCode};
use crate::net::region::auth::MAX_CREDENTIAL_BYTES;
use crate::net::region::wire::{Cursor, MAX_FRAME_BYTES};
use crate::pos::Pos3;
use crate::sim::ViewerId;

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub(crate) const KIND_CLIENT_IDENTIFICATION: u8 = 1;
pub(crate) const KIND_SERVER_INFO: u8 = 2;
pub(crate) const KIND_REJECTION: u8 = 3;
pub(crate) const KIND_ACCEPTED: u8 = 4;
pub(crate) const KIND_QUIT: u8 = 5;
pub(crate) const KIND_SPAWN_ENTITIES: u8 = 6;
pub(crate) const KIND_ENTITIES_SPAWNED: u8 = 7;
pub(crate) const KIND_MOVE_ENTITIES: u8 = 8;
pub(crate) const KIND_POSITION_UPDATES: u8 = 9;
pub(crate) const KIND_DESPAWN_ENTITIES: u8 = 10;

/// Names a frame kind for an error message, without echoing the peer's bytes.
pub(crate) fn kind_name(kind: u8) -> &'static str {
    match kind {
        KIND_CLIENT_IDENTIFICATION => "client identification",
        KIND_SERVER_INFO => "server info",
        KIND_REJECTION => "rejection",
        KIND_ACCEPTED => "accepted",
        KIND_QUIT => "quit",
        KIND_SPAWN_ENTITIES => "spawn entities",
        KIND_ENTITIES_SPAWNED => "entities spawned",
        KIND_MOVE_ENTITIES => "move entities",
        KIND_POSITION_UPDATES => "position updates",
        KIND_DESPAWN_ENTITIES => "despawn entities",
        _ => "unknown",
    }
}

/// How long either end waits on the other during the handshake.
///
/// A peer that connects and then says nothing would otherwise hold a thread
/// for as long as it liked, which is a way in that does not need a credential.
/// The deadline is lifted once the link is up, since a link is expected to sit
/// idle between packets.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Identity and versions
// ---------------------------------------------------------------------------

/// Which region a simulation owns.
///
/// Assigned by the control plane, which is not built. Until then a consumer
/// picks one and passes it to
/// [`RegionServer::bind`](crate::net::RegionServer::bind).
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RegionId(u32);

impl RegionId {
    #[inline(always)]
    pub const fn from_raw(raw: u32) -> RegionId {
        RegionId(raw)
    }

    #[inline(always)]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "R{}", self.0)
    }
}

impl fmt::Display for RegionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "region {}", self.0)
    }
}

/// The version of this protocol itself.
///
/// Bumped when the messages below change shape. Distinct from
/// [`ServerVersion`], which is the crate build, and from
/// [`WorldConfig::protocol_hash`], which is the world's wire layout. All three
/// can move independently and all three are checked.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    #[inline(always)]
    pub const fn from_raw(raw: u16) -> ProtocolVersion {
        ProtocolVersion(raw)
    }

    #[inline(always)]
    pub const fn raw(self) -> u16 {
        self.0
    }
}

impl fmt::Debug for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// What this build speaks. Exact match required, in both directions.
///
/// There is no negotiation and no compatibility window. Region and edge deploy
/// together, so a version skew is a deployment mistake to fail loudly on rather
/// than a condition to tolerate.
///
/// This versions the region-to-edge protocol only. The edge-to-game-client
/// protocol will carry its own, and the two are not required to move together.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

/// The crate version a region server is running.
///
/// Informational: it is reported so an operator can see what a region is
/// running without asking it, and nothing rejects on it.
/// [`PROTOCOL_VERSION`] is what has to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ServerVersion {
    /// Taken from `Cargo.toml` at compile time, so it cannot drift from it.
    pub const CURRENT: ServerVersion = ServerVersion {
        major: parse_u16(env!("CARGO_PKG_VERSION_MAJOR")),
        minor: parse_u16(env!("CARGO_PKG_VERSION_MINOR")),
        patch: parse_u16(env!("CARGO_PKG_VERSION_PATCH")),
    };
}

impl fmt::Display for ServerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// # Panics
///
/// At compile time, if a cargo version component is not decimal digits. Cargo
/// will not produce one that is not.
const fn parse_u16(s: &str) -> u16 {
    let b = s.as_bytes();
    let mut n: u16 = 0;
    let mut i = 0;
    while i < b.len() {
        assert!(b[i] >= b'0' && b[i] <= b'9', "version component is not decimal");
        n = n * 10 + (b[i] - b'0') as u16;
        i += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// World parameters
// ---------------------------------------------------------------------------

/// A [`WorldConfig`] reduced to what the builder authors, plus its digest.
///
/// A `WorldConfig` is mostly derived values, so carrying the five authored ones
/// and letting the other end derive the rest keeps one derivation rather than
/// two. Carrying the derived fields would mean a peer could hold a config this
/// crate's builder would never produce.
///
/// The five are whole meters because
/// [`WorldConfigBuilder`](crate::WorldConfigBuilder) takes whole meters, so
/// nothing is lost in the round trip for a config built through it. A config
/// built some other way is caught by the digest rather than silently
/// approximated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldParams {
    pub region_size_m: i32,
    pub vertical_extent_m: i32,
    pub horizontal_view_radius_m: i32,
    pub max_horizontal_speed_m_per_sec: i32,
    pub tick_hz: u32,
    /// [`WorldConfig::protocol_hash`] of the config these came from.
    pub protocol_hash: u64,
}

impl WorldParams {
    pub const BYTES: usize = 28;

    pub fn from_config(cfg: &WorldConfig) -> WorldParams {
        WorldParams {
            region_size_m: cfg.region_size().floor_meters(),
            vertical_extent_m: cfg.vertical_extent().floor_meters(),
            horizontal_view_radius_m: cfg.horizontal_view_radius().floor_meters(),
            max_horizontal_speed_m_per_sec: cfg.max_horizontal_speed().floor_meters(),
            tick_hz: cfg.tick_hz(),
            protocol_hash: cfg.protocol_hash(),
        }
    }

    /// Rebuilds the config, then checks it decodes packets the same way.
    ///
    /// Two distinct failures. [`NetError::Config`] means the parameters do not
    /// describe a valid world at all. [`NetError::ConfigMismatch`] means they
    /// build, but into a world whose wire layout differs from the region's,
    /// which is what would turn its packets into garbage on this side.
    pub fn to_config(&self) -> Result<WorldConfig, NetError> {
        let cfg = WorldConfig::builder()
            .region_size_m(self.region_size_m)
            .vertical_extent_m(self.vertical_extent_m)
            .horizontal_view_radius_m(self.horizontal_view_radius_m)
            .max_horizontal_speed_m_per_sec(self.max_horizontal_speed_m_per_sec)
            .tick_hz(self.tick_hz)
            .build()?;

        if cfg.protocol_hash() != self.protocol_hash {
            return Err(NetError::ConfigMismatch {
                ours: cfg.protocol_hash(),
                theirs: self.protocol_hash,
            });
        }
        Ok(cfg)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.region_size_m.to_le_bytes());
        out.extend_from_slice(&self.vertical_extent_m.to_le_bytes());
        out.extend_from_slice(&self.horizontal_view_radius_m.to_le_bytes());
        out.extend_from_slice(&self.max_horizontal_speed_m_per_sec.to_le_bytes());
        out.extend_from_slice(&self.tick_hz.to_le_bytes());
        out.extend_from_slice(&self.protocol_hash.to_le_bytes());
    }

    fn decode(c: &mut Cursor<'_>) -> Result<WorldParams, NetError> {
        Ok(WorldParams {
            region_size_m: c.i32()?,
            vertical_extent_m: c.i32()?,
            horizontal_view_radius_m: c.i32()?,
            max_horizontal_speed_m_per_sec: c.i32()?,
            tick_hz: c.u32()?,
            protocol_hash: c.u64()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Identifies the connecting client. The first message on the link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentification {
    pub protocol: ProtocolVersion,
    /// Opaque bytes for the region's
    /// [`Authorizer`](crate::net::Authorizer). This crate never interprets
    /// them.
    pub credential: Vec<u8>,
}

impl ClientIdentification {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.protocol.raw().to_le_bytes());
        out.extend_from_slice(&(self.credential.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.credential);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<ClientIdentification, NetError> {
        let mut c = Cursor::new(body, "client identification");
        let protocol = ProtocolVersion::from_raw(c.u16()?);
        let len = c.u16()? as usize;
        if len > MAX_CREDENTIAL_BYTES {
            return Err(NetError::Malformed("client identification credential"));
        }
        let credential = c.bytes(len)?.to_vec();
        c.finish()?;
        Ok(ClientIdentification { protocol, credential })
    }
}

/// Describes the server and the region it owns. Sent once an identification
/// has authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub server: ServerVersion,
    pub region: RegionId,
    pub params: WorldParams,
}

impl ServerInfo {
    pub const BYTES: usize = 6 + 4 + WorldParams::BYTES;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.server.major.to_le_bytes());
        out.extend_from_slice(&self.server.minor.to_le_bytes());
        out.extend_from_slice(&self.server.patch.to_le_bytes());
        out.extend_from_slice(&self.region.raw().to_le_bytes());
        self.params.encode(out);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<ServerInfo, NetError> {
        let mut c = Cursor::new(body, "server info");
        let server = ServerVersion { major: c.u16()?, minor: c.u16()?, patch: c.u16()? };
        let region = RegionId::from_raw(c.u32()?);
        let params = WorldParams::decode(&mut c)?;
        c.finish()?;
        Ok(ServerInfo { server, region, params })
    }
}

/// Says the connection will not be taken, and nothing else.
///
/// One byte. Everything the server knows about why stays on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub code: RejectCode,
}

impl Rejection {
    pub const BYTES: usize = 1;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(self.code.as_u8());
    }

    pub(crate) fn decode(body: &[u8]) -> Result<Rejection, NetError> {
        let mut c = Cursor::new(body, "rejection");
        let raw = c.u8()?;
        c.finish()?;
        let code = RejectCode::from_u8(raw).ok_or(NetError::Malformed("rejection code"))?;
        Ok(Rejection { code })
    }
}

// ---------------------------------------------------------------------------
// Session messages
// ---------------------------------------------------------------------------

/// Bytes one spawn request takes: three raw [`Fixed`](crate::Fixed) axes, and
/// what kind of thing is being spawned.
const SPAWN_BYTES: usize = 13;

/// Bytes one spawned entity takes in the reply: its id, and the viewer
/// watching it.
const SPAWNED_BYTES: usize = 8;

/// Bytes one move takes: an id, and three raw [`Fixed`](crate::Fixed) axes.
///
/// Raw fixed point rather than the quantized wire record, because this is the
/// authoritative position going *into* the simulation. The quantization in
/// [`RecordCodec`](crate::RecordCodec) is a budget decision about what a client
/// needs to see. Applying it to an inbound command would add a small position
/// error on every round trip through the region.
const MOVE_BYTES: usize = 16;

/// Most entities one [`SpawnEntities`] may ask for, so its reply fits a frame.
///
/// An edge wanting more sends more messages. The frame cap is what bounds an
/// unauthorized peer's allocation, so it does not move to suit a caller.
pub const MAX_SPAWN_PER_MESSAGE: usize = (MAX_FRAME_BYTES - 4) / SPAWN_BYTES;

/// Most moves one [`MoveEntities`] may carry. Same bargain.
pub const MAX_MOVES_PER_MESSAGE: usize = (MAX_FRAME_BYTES - 4) / MOVE_BYTES;

/// A full spawn request's reply has to fit a frame too, or the region could
/// accept a request it cannot answer.
const _: () = assert!(
    4 + MAX_SPAWN_PER_MESSAGE * SPAWNED_BYTES <= MAX_FRAME_BYTES,
    "a reply to a full spawn request does not fit a frame"
);

/// Most entities one [`DespawnEntities`] may give up. Same bargain.
pub const MAX_DESPAWN_PER_MESSAGE: usize = (MAX_FRAME_BYTES - 4) / 4;

/// What is behind an entity, which decides whether it observes.
///
/// **An entity is a thing with a position; a viewer is a thing that receives.**
/// Every entity can be seen by whoever is near it. Only an observer is sent
/// what it can see, and only an observer costs the per-viewer pipeline: a
/// subscription, a gather, a score, a selection and a packet every tick it is
/// served, plus a [`GhostTable`](crate::GhostTable) of its own.
///
/// The difference is large enough that it cannot be implicit. A region holding
/// 8,192 unattended entities with one observer among them, and a region holding
/// 8,192 observers, are the same snapshot and nothing like the same tick.
///
/// **Static scenery is not a kind here. It is never spawned at all.** A rock
/// that never moves is in the client's content package already, and putting it
/// in the region would pay 12 bytes of snapshot and a gather-walk visit every
/// tick, forever, to tell clients something they were shipped. What belongs in
/// a region is what is authoritative and moves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum EntityKind {
    /// Nothing is behind it. Simulated and replicated to whoever can see it,
    /// observes nothing itself, and no viewer is registered. Projectiles,
    /// wildlife, NPCs, a vehicle with no driver.
    #[default]
    Unattended = 0,
    /// A game client is behind it. The region registers a viewer watching it,
    /// so it is sent a budgeted approximation of what it can see.
    Observer = 1,
}

impl EntityKind {
    #[inline(always)]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(raw: u8) -> Option<EntityKind> {
        match raw {
            0 => Some(EntityKind::Unattended),
            1 => Some(EntityKind::Observer),
            _ => None,
        }
    }

    #[inline(always)]
    pub const fn observes(self) -> bool {
        matches!(self, EntityKind::Observer)
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntityKind::Unattended => write!(f, "unattended"),
            EntityKind::Observer => write!(f, "observer"),
        }
    }
}

/// Asks the region to create entities this edge will manage, at these
/// positions.
///
/// The region allocates the ids, since entity identity is its to hand out, and
/// records the asking edge as their owner in the same step.
///
/// Positions travel with the request rather than the edge spawning at a default
/// and moving afterward. A crowd that appears at one point and scatters on the
/// next tick is a region-wide teleport, and the odometer and the subscription
/// update both assume entities move at most
/// [`max_horizontal_speed`](crate::WorldConfig::max_horizontal_speed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnEntities {
    pub spawns: Vec<(Pos3, EntityKind)>,
}

impl SpawnEntities {
    /// Every entity an observer, which is what a crowd of players is.
    pub fn observers(positions: &[Pos3]) -> SpawnEntities {
        SpawnEntities {
            spawns: positions.iter().map(|p| (*p, EntityKind::Observer)).collect(),
        }
    }

    /// Every entity unattended, which is what a flight of projectiles or a
    /// herd of wildlife is.
    pub fn unattended(positions: &[Pos3]) -> SpawnEntities {
        SpawnEntities {
            spawns: positions.iter().map(|p| (*p, EntityKind::Unattended)).collect(),
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&(self.spawns.len() as u32).to_le_bytes());
        for (pos, kind) in &self.spawns {
            out.extend_from_slice(&pos.x.raw().to_le_bytes());
            out.extend_from_slice(&pos.y.raw().to_le_bytes());
            out.extend_from_slice(&pos.z.raw().to_le_bytes());
            out.push(kind.as_u8());
        }
    }

    pub(crate) fn decode(body: &[u8]) -> Result<SpawnEntities, NetError> {
        let mut c = Cursor::new(body, "spawn entities");
        let len = c.u32()? as usize;
        if len > MAX_SPAWN_PER_MESSAGE {
            return Err(NetError::Malformed("spawn entities count"));
        }
        let mut spawns = Vec::with_capacity(len);
        for _ in 0..len {
            let pos = Pos3::new(
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
            );
            let kind = EntityKind::from_u8(c.u8()?)
                .ok_or(NetError::Malformed("spawn entity kind"))?;
            spawns.push((pos, kind));
        }
        c.finish()?;
        Ok(SpawnEntities { spawns })
    }
}

/// The entities created, in the order they were asked for, and the viewer
/// watching each one where there is a viewer at all.
///
/// An unattended entity has no viewer, and its slot carries `None`. An
/// observer's viewer id
/// travels because it is what arrives on a [`PositionUpdates`], and the edge
/// has no way to work out that pairing for itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitiesSpawned {
    pub entities: Vec<(EntityId, Option<ViewerId>)>,
}

/// No viewer watches this entity. Viewer ids are dense from zero, so the top of
/// the range is free to reserve.
const NO_VIEWER: u32 = u32::MAX;

impl EntitiesSpawned {
    /// The observers among them, which is what an edge tracks for routing.
    pub fn observers(&self) -> impl Iterator<Item = (EntityId, ViewerId)> + '_ {
        self.entities.iter().filter_map(|(id, v)| v.map(|v| (*id, v)))
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&(self.entities.len() as u32).to_le_bytes());
        for (entity, viewer) in &self.entities {
            out.extend_from_slice(&entity.raw().to_le_bytes());
            out.extend_from_slice(&viewer.map_or(NO_VIEWER, ViewerId::raw).to_le_bytes());
        }
    }

    pub(crate) fn decode(body: &[u8]) -> Result<EntitiesSpawned, NetError> {
        let mut c = Cursor::new(body, "entities spawned");
        let len = c.u32()? as usize;
        if len > MAX_SPAWN_PER_MESSAGE {
            return Err(NetError::Malformed("entities spawned count"));
        }
        let mut entities = Vec::with_capacity(len);
        for _ in 0..len {
            let entity = EntityId::from_raw(c.u32()?);
            let raw = c.u32()?;
            entities.push((entity, (raw != NO_VIEWER).then(|| ViewerId::from_raw(raw))));
        }
        c.finish()?;
        Ok(EntitiesSpawned { entities })
    }
}

/// New absolute positions for entities this edge manages.
///
/// Absolute rather than a delta, so a lost or reordered message costs one tick
/// of staleness rather than corrupting a position permanently. Applying one is
/// idempotent for the same reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveEntities {
    pub moves: Vec<(EntityId, Pos3)>,
}

impl MoveEntities {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&(self.moves.len() as u32).to_le_bytes());
        for (id, pos) in &self.moves {
            out.extend_from_slice(&id.raw().to_le_bytes());
            out.extend_from_slice(&pos.x.raw().to_le_bytes());
            out.extend_from_slice(&pos.y.raw().to_le_bytes());
            out.extend_from_slice(&pos.z.raw().to_le_bytes());
        }
    }

    pub(crate) fn decode(body: &[u8]) -> Result<MoveEntities, NetError> {
        let mut c = Cursor::new(body, "move entities");
        let len = c.u32()? as usize;
        if len > MAX_MOVES_PER_MESSAGE {
            return Err(NetError::Malformed("move entities count"));
        }
        let mut moves = Vec::with_capacity(len);
        for _ in 0..len {
            let id = EntityId::from_raw(c.u32()?);
            let pos = Pos3::new(
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
            );
            moves.push((id, pos));
        }
        c.finish()?;
        Ok(MoveEntities { moves })
    }
}

/// Gives entities back, because the game clients behind them have gone.
///
/// An edge's population is not fixed. Clients connect and disconnect, so the
/// edge that spawned an entity is the one that has to say when it is finished
/// with it. The region despawns the entity, unregisters the viewer watching it,
/// and frees the ownership record.
///
/// An edge that detaches entirely gives up everything it held without sending
/// this: the region cannot leave entities alive with no connection behind them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DespawnEntities {
    pub ids: Vec<EntityId>,
}

impl DespawnEntities {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&(self.ids.len() as u32).to_le_bytes());
        for id in &self.ids {
            out.extend_from_slice(&id.raw().to_le_bytes());
        }
    }

    pub(crate) fn decode(body: &[u8]) -> Result<DespawnEntities, NetError> {
        let mut c = Cursor::new(body, "despawn entities");
        let len = c.u32()? as usize;
        if len > MAX_DESPAWN_PER_MESSAGE {
            return Err(NetError::Malformed("despawn entities count"));
        }
        let mut ids = Vec::with_capacity(len);
        for _ in 0..len {
            ids.push(EntityId::from_raw(c.u32()?));
        }
        c.finish()?;
        Ok(DespawnEntities { ids })
    }
}

/// One viewer's assembled payload, relayed to the edge that manages it.
///
/// The payload is exactly what [`PacketWriter`](crate::PacketWriter) built, so
/// an edge decodes it with [`PacketReader`](crate::PacketReader) and this
/// protocol does not need to know what is inside.
///
/// **A known deviation.** State is latest-only, lossy and unordered by design,
/// and this link is reliable and ordered. Carrying payloads here is what lets
/// the smoke test run end to end before the datagram path exists; it is not
/// what the architecture says should happen in the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionUpdates<'a> {
    pub viewer: ViewerId,
    pub payload: &'a [u8],
}

impl PositionUpdates<'_> {
    /// Only the round-trip test builds one of these. The live path writes the
    /// viewer and the payload straight into the edge's buffer rather than
    /// joining them into a body first, which is a copy per payload it does not
    /// need to pay.
    #[cfg(test)]
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.viewer.raw().to_le_bytes());
        out.extend_from_slice(self.payload);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<PositionUpdates<'_>, NetError> {
        let mut c = Cursor::new(body, "position updates");
        let viewer = ViewerId::from_raw(c.u32()?);
        let payload = c.rest();
        Ok(PositionUpdates { viewer, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixed::Fixed;

    #[test]
    fn a_region_id_round_trips() {
        assert_eq!(RegionId::from_raw(9001).raw(), 9001);
        assert_eq!(size_of::<RegionId>(), size_of::<u32>());
        assert_eq!(format!("{:?}", RegionId::from_raw(7)), "R7");
    }

    #[test]
    fn the_advertised_version_is_the_crate_version() {
        assert_eq!(ServerVersion::CURRENT.to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_identification_round_trips() {
        let m = ClientIdentification {
            protocol: PROTOCOL_VERSION,
            credential: b"region-7-edge-key".to_vec(),
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(ClientIdentification::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn an_identification_with_no_credential_round_trips() {
        // Well formed, and the authorizer is what refuses it.
        let m = ClientIdentification { protocol: PROTOCOL_VERSION, credential: Vec::new() };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(ClientIdentification::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn an_identification_claiming_a_credential_past_the_cap_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&PROTOCOL_VERSION.raw().to_le_bytes());
        buf.extend_from_slice(&((MAX_CREDENTIAL_BYTES + 1) as u16).to_le_bytes());
        buf.resize(buf.len() + MAX_CREDENTIAL_BYTES + 1, 0);
        assert!(matches!(
            ClientIdentification::decode(&buf),
            Err(NetError::Malformed("client identification credential"))
        ));
    }

    #[test]
    fn an_identification_shorter_than_it_claims_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&PROTOCOL_VERSION.raw().to_le_bytes());
        buf.extend_from_slice(&64u16.to_le_bytes());
        buf.extend_from_slice(b"only eight");
        assert!(matches!(
            ClientIdentification::decode(&buf),
            Err(NetError::Malformed("client identification"))
        ));
    }

    #[test]
    fn server_info_round_trips() {
        let cfg = WorldConfig::default();
        let m = ServerInfo {
            server: ServerVersion::CURRENT,
            region: RegionId::from_raw(42),
            params: WorldParams::from_config(&cfg),
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(buf.len(), ServerInfo::BYTES);
        assert_eq!(ServerInfo::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn truncated_server_info_is_refused() {
        let m = ServerInfo {
            server: ServerVersion::CURRENT,
            region: RegionId::from_raw(1),
            params: WorldParams::from_config(&WorldConfig::default()),
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(
                ServerInfo::decode(&buf[..cut]).is_err(),
                "server info {cut} bytes long must not parse"
            );
        }
    }

    #[test]
    fn params_rebuild_the_config_they_came_from() {
        let cfg = WorldConfig::default();
        assert_eq!(WorldParams::from_config(&cfg).to_config().expect("valid"), cfg);
    }

    #[test]
    fn params_rebuild_a_non_default_config() {
        let cfg = WorldConfig::builder()
            .region_size_m(1024)
            .vertical_extent_m(256)
            .horizontal_view_radius_m(64)
            .max_horizontal_speed_m_per_sec(30)
            .tick_hz(50)
            .build()
            .expect("valid");
        assert_eq!(WorldParams::from_config(&cfg).to_config().expect("valid"), cfg);
    }

    #[test]
    fn a_tampered_digest_is_caught() {
        let cfg = WorldConfig::default();
        let mut p = WorldParams::from_config(&cfg);
        p.protocol_hash ^= 1;
        match p.to_config() {
            Err(NetError::ConfigMismatch { ours, theirs }) => {
                assert_eq!(ours, cfg.protocol_hash());
                assert_eq!(theirs, cfg.protocol_hash() ^ 1);
            }
            other => panic!("a changed digest must be refused, got {other:?}"),
        }
    }

    #[test]
    fn parameters_that_describe_no_valid_world_are_refused() {
        let p = WorldParams {
            region_size_m: 1000, // not a power of two
            vertical_extent_m: 1024,
            horizontal_view_radius_m: 256,
            max_horizontal_speed_m_per_sec: 40,
            tick_hz: 20,
            protocol_hash: 0,
        };
        assert!(matches!(p.to_config(), Err(NetError::Config(_))));
    }

    #[test]
    fn a_config_the_builder_could_not_produce_is_caught_by_the_digest() {
        // with_cell_size_m overrides a derived field, so the authored five no
        // longer reproduce it. The digest covers cell size, so this is caught
        // rather than silently decoded against the wrong layout.
        let odd = WorldConfig::default().with_cell_size_m(64);
        let p = WorldParams::from_config(&odd);
        assert!(matches!(p.to_config(), Err(NetError::ConfigMismatch { .. })));
    }

    #[test]
    fn a_rejection_round_trips() {
        for code in [RejectCode::Unauthorized, RejectCode::ProtocolMismatch, RejectCode::Malformed]
        {
            let mut buf = Vec::new();
            Rejection { code }.encode(&mut buf);
            assert_eq!(buf.len(), Rejection::BYTES);
            assert_eq!(Rejection::decode(&buf).expect("well formed"), Rejection { code });
        }
    }

    #[test]
    fn an_unknown_rejection_code_is_refused() {
        assert!(matches!(Rejection::decode(&[99]), Err(NetError::Malformed("rejection code"))));
        assert!(matches!(Rejection::decode(&[]), Err(NetError::Malformed("rejection"))));
    }

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    #[test]
    fn a_spawn_request_round_trips() {
        let m = SpawnEntities {
            spawns: vec![
                (Pos3::from_meters(1, 2, 3), EntityKind::Observer),
                (Pos3::from_meters(4095, 4095, 1023), EntityKind::Unattended),
            ],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(SpawnEntities::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_spawn_request_says_what_observes() {
        let at = [Pos3::from_meters(1, 1, 0), Pos3::from_meters(2, 2, 0)];
        assert!(SpawnEntities::observers(&at).spawns.iter().all(|(_, k)| k.observes()));
        assert!(SpawnEntities::unattended(&at).spawns.iter().all(|(_, k)| !k.observes()));
    }

    #[test]
    fn an_unknown_entity_kind_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 12]);
        buf.push(9);
        assert!(matches!(
            SpawnEntities::decode(&buf),
            Err(NetError::Malformed("spawn entity kind"))
        ));
    }

    #[test]
    fn every_entity_kind_round_trips() {
        for kind in [EntityKind::Unattended, EntityKind::Observer] {
            assert_eq!(EntityKind::from_u8(kind.as_u8()), Some(kind));
        }
        assert_eq!(EntityKind::from_u8(2), None);
    }

    #[test]
    fn a_spawn_request_past_the_cap_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_SPAWN_PER_MESSAGE + 1) as u32).to_le_bytes());
        assert!(matches!(
            SpawnEntities::decode(&buf),
            Err(NetError::Malformed("spawn entities count"))
        ));
    }

    #[test]
    fn a_full_spawn_request_fits_a_frame() {
        let m = SpawnEntities {
            spawns: vec![(Pos3::ZERO, EntityKind::Observer); MAX_SPAWN_PER_MESSAGE],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_FRAME_BYTES, "{} bytes does not fit a frame", buf.len());
    }

    #[test]
    fn a_spawn_reply_round_trips() {
        let m = EntitiesSpawned {
            entities: vec![
                (ent(0), Some(ViewerId::from_raw(0))),
                (ent(41), None),
                (ent(42), Some(ViewerId::from_raw(7))),
            ],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(EntitiesSpawned::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_spawn_reply_names_only_the_observers_as_watchable() {
        let m = EntitiesSpawned {
            entities: vec![
                (ent(0), Some(ViewerId::from_raw(0))),
                (ent(41), None),
                (ent(42), Some(ViewerId::from_raw(7))),
            ],
        };
        assert_eq!(
            m.observers().collect::<Vec<_>>(),
            vec![(ent(0), ViewerId::from_raw(0)), (ent(42), ViewerId::from_raw(7))],
            "an unattended entity has no viewer to route to"
        );
    }

    #[test]
    fn a_move_round_trips_losslessly() {
        // Raw fixed point, so a position survives the wire exactly. A round trip
        // that moved an entity would walk it every tick.
        let m = MoveEntities {
            moves: vec![
                (ent(1), Pos3::new(Fixed::from_millis(7, 500), Fixed::ZERO, Fixed::from_raw(1))),
                (ent(2), Pos3::from_meters(4095, 0, 1023)),
            ],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(MoveEntities::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_full_move_message_fits_a_frame() {
        let m = MoveEntities { moves: vec![(ent(0), Pos3::ZERO); MAX_MOVES_PER_MESSAGE] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_FRAME_BYTES, "{} bytes does not fit a frame", buf.len());
    }

    #[test]
    fn a_move_message_past_the_cap_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_MOVES_PER_MESSAGE + 1) as u32).to_le_bytes());
        assert!(matches!(
            MoveEntities::decode(&buf),
            Err(NetError::Malformed("move entities count"))
        ));
    }

    #[test]
    fn a_truncated_move_message_is_refused() {
        let m = MoveEntities { moves: vec![(ent(1), Pos3::from_meters(1, 2, 3))] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(MoveEntities::decode(&buf[..cut]).is_err(), "{cut} bytes must not parse");
        }
    }

    #[test]
    fn a_despawn_round_trips() {
        let m = DespawnEntities { ids: vec![ent(3), ent(9), ent(1000)] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(DespawnEntities::decode(&buf).expect("well formed"), m);
    }

    #[test]
    fn a_full_despawn_message_fits_a_frame() {
        let m = DespawnEntities { ids: vec![ent(0); MAX_DESPAWN_PER_MESSAGE] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_FRAME_BYTES, "{} bytes does not fit a frame", buf.len());
    }

    #[test]
    fn a_despawn_message_past_the_cap_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_DESPAWN_PER_MESSAGE + 1) as u32).to_le_bytes());
        assert!(matches!(
            DespawnEntities::decode(&buf),
            Err(NetError::Malformed("despawn entities count"))
        ));
    }

    #[test]
    fn position_updates_carry_the_payload_untouched() {
        let payload = b"a payload PacketWriter built";
        let m = PositionUpdates { viewer: ViewerId::from_raw(9), payload };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        let back = PositionUpdates::decode(&buf).expect("well formed");
        assert_eq!(back.viewer, ViewerId::from_raw(9));
        assert_eq!(back.payload, payload);
    }

    #[test]
    fn position_updates_carry_an_empty_payload() {
        let m = PositionUpdates { viewer: ViewerId::from_raw(0), payload: &[] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(PositionUpdates::decode(&buf).expect("well formed").payload, b"");
    }

    #[test]
    fn every_kind_has_a_name() {
        for kind in [
            KIND_CLIENT_IDENTIFICATION,
            KIND_SERVER_INFO,
            KIND_REJECTION,
            KIND_ACCEPTED,
            KIND_QUIT,
            KIND_SPAWN_ENTITIES,
            KIND_ENTITIES_SPAWNED,
            KIND_MOVE_ENTITIES,
            KIND_POSITION_UPDATES,
            KIND_DESPAWN_ENTITIES,
        ] {
            assert_ne!(kind_name(kind), "unknown", "kind {kind} needs a name");
        }
        assert_eq!(kind_name(200), "unknown");
    }
}
