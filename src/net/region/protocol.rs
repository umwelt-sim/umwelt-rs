//! The messages on the region-to-edge link.
//!
//! A message belongs to the protocol, not to whichever end happens to send it.
//! Each is named for what it carries.
//!
//! NATS delivers whole messages, so there is no framing here: a message is a
//! one-byte kind followed by its body. Which subject it arrived on says which
//! direction it traveled and, for a command, which edge sent it.

use crate::config::WorldConfig;
use crate::entity::{EntityId, EntityKind};
use crate::id::RegionId;
use crate::net::error::NetError;
use crate::net::version::{ProtocolVersion, ServerVersion};
use crate::net::wire::Cursor;
use crate::pos::Pos3;

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub(crate) const KIND_SPAWN_ENTITIES: u8 = 1;
pub(crate) const KIND_MOVE_ENTITIES: u8 = 2;
pub(crate) const KIND_DESPAWN_ENTITIES: u8 = 3;
pub(crate) const KIND_KEEPALIVE: u8 = 4;
pub(crate) const KIND_GAME_MESSAGE: u8 = 5;

/// Names a frame kind for an error message, without echoing the peer's bytes.
pub(crate) fn kind_name(kind: u8) -> &'static str {
    match kind {
        KIND_SPAWN_ENTITIES => "spawn entities",
        KIND_MOVE_ENTITIES => "move entities",
        KIND_DESPAWN_ENTITIES => "despawn entities",
        KIND_KEEPALIVE => "keepalive",
        KIND_GAME_MESSAGE => "game message",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/// What this build speaks. Exact match required, in both directions.
///
/// There is no negotiation and no compatibility window. Region and edge deploy
/// together, so a version skew is a deployment mistake to fail loudly on rather
/// than a condition to tolerate.
///
/// This versions the region-to-edge protocol only. The edge-to-game-client
/// protocol will carry its own, and the two are not required to move together.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::from_raw(1);

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
    /// The region's horizontal extent, in meters.
    pub region_size_m: i32,
    /// Its vertical extent, in meters.
    pub vertical_extent_m: i32,
    /// How far an observer sees, in meters.
    pub horizontal_view_radius_m: i32,
    /// The horizontal speed cap, in meters per second.
    pub max_horizontal_speed_m_per_sec: i32,
    /// Ticks per second.
    pub tick_hz: u32,
    /// [`WorldConfig::protocol_hash`] of the config these came from.
    pub protocol_hash: u64,
}

impl WorldParams {
    /// Their width on the wire.
    pub const BYTES: usize = 28;

    /// The five authored numbers, plus the digest of everything they imply.
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

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.region_size_m.to_le_bytes());
        out.extend_from_slice(&self.vertical_extent_m.to_le_bytes());
        out.extend_from_slice(&self.horizontal_view_radius_m.to_le_bytes());
        out.extend_from_slice(&self.max_horizontal_speed_m_per_sec.to_le_bytes());
        out.extend_from_slice(&self.tick_hz.to_le_bytes());
        out.extend_from_slice(&self.protocol_hash.to_le_bytes());
    }

    pub(crate) fn decode(c: &mut Cursor<'_>) -> Result<WorldParams, NetError> {
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

/// Describes the server and the region it owns. The reply on the info
/// subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    /// What this region speaks. There is no handshake to check it in, so it
    /// travels here and the edge refuses a region it cannot talk to.
    pub protocol: ProtocolVersion,
    /// The crate version the region runs. Informational.
    pub server: ServerVersion,
    /// Which region answered.
    pub region: RegionId,
    /// The world it runs, for the asker to rebuild and check.
    pub params: WorldParams,
}

impl ServerInfo {
    /// Its width on the wire.
    pub const BYTES: usize = 2 + 6 + 4 + WorldParams::BYTES;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.protocol.raw().to_le_bytes());
        out.extend_from_slice(&self.server.major.to_le_bytes());
        out.extend_from_slice(&self.server.minor.to_le_bytes());
        out.extend_from_slice(&self.server.patch.to_le_bytes());
        out.extend_from_slice(&self.region.raw().to_le_bytes());
        self.params.encode(out);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<ServerInfo, NetError> {
        let mut c = Cursor::new(body, "server info");
        let protocol = ProtocolVersion::from_raw(c.u16()?);
        let server = ServerVersion { major: c.u16()?, minor: c.u16()?, patch: c.u16()? };
        let region = RegionId::from_raw(c.u32()?);
        let params = WorldParams::decode(&mut c)?;
        c.finish()?;
        Ok(ServerInfo { protocol, server, region, params })
    }
}

// ---------------------------------------------------------------------------
// Session messages
// ---------------------------------------------------------------------------

/// Most bytes one message body may carry.
///
/// NATS enforces its own maximum payload, well above this. Keeping messages
/// small bounds the buffer a decoder allocates for a claimed count, and makes a
/// large spawn or move batch arrive as several messages rather than one.
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// Bytes one spawn request takes: three raw [`Fixed`](crate::Fixed) axes, the
/// role byte, the game-defined tag, and the caller's token.
const SPAWN_BYTES: usize = 23;

/// Bytes one move takes: an id, and three raw [`Fixed`](crate::Fixed) axes.
///
/// Raw fixed point rather than the reduced-precision wire record, because this is the
/// authoritative position going *into* the simulation. The precision reduction in
/// [`RecordCodec`](crate::RecordCodec) is a budget decision about what a client
/// needs to see. Applying it to an inbound command would add a small position
/// error on every round trip through the region.
const MOVE_BYTES: usize = 16;

/// Most bytes a game message body may carry. The five overhead bytes are the
/// kind and the entity id.
pub const MAX_GAME_MESSAGE_BODY: usize = MAX_MESSAGE_BYTES - 5;

/// Most entities one [`SpawnEntities`] may ask for. The five bytes are the kind
/// and the count.
///
/// An edge wanting more sends more messages. The cap bounds the buffer a
/// decoder allocates for a claimed count, so it does not move to suit a
/// caller.
pub const MAX_SPAWN_PER_MESSAGE: usize = (MAX_MESSAGE_BYTES - 5) / SPAWN_BYTES;

/// Most moves one [`MoveEntities`] may carry. Same bargain.
pub const MAX_MOVES_PER_MESSAGE: usize = (MAX_MESSAGE_BYTES - 5) / MOVE_BYTES;

/// Most entities one [`DespawnEntities`] may give up. Same bargain.
pub const MAX_DESPAWN_PER_MESSAGE: usize = (MAX_MESSAGE_BYTES - 5) / 4;

impl EntityKind {
    /// Three bytes on the wire: role, then tag as little-endian u16.
    pub(crate) fn encode_wire(self, out: &mut Vec<u8>) {
        out.push(self.role());
        out.extend_from_slice(&self.tag().to_le_bytes());
    }

    /// Reads three bytes: role, then tag as little-endian u16.
    pub(crate) fn decode_wire(c: &mut crate::net::wire::Cursor<'_>) -> Result<EntityKind, NetError> {
        let role = c.u8()?;
        let tag = c.u16()?;
        match role {
            0 => Ok(EntityKind::unattended(tag)),
            1 => Ok(EntityKind::observer(tag)),
            _ => Err(NetError::Malformed("entity kind role")),
        }
    }

}

/// One entity an edge is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spawn {
    /// Where to put it.
    pub position: Pos3,
    /// What is behind it, which decides whether it observes.
    pub kind: EntityKind,
    /// Opaque to the region, echoed back on the presence message that reports
    /// the entity. It exists because a presence subject says which edge owns
    /// an entity and nothing more: an edge that asked for three avatars in one
    /// message has no other way to tell which arrival belongs to which of its
    /// game clients. In practice it is the handle the edge already holds for
    /// that client.
    pub token: u64,
}

/// Asks the region to create entities this edge will manage.
///
/// The region allocates the ids, since entity identity is its to hand out, and
/// records the asking edge as their owner in the same step. The ids come back
/// as presence, not as a reply.
///
/// Positions travel with the request rather than the edge spawning at a default
/// and moving afterward. A crowd that appears at one point and scatters on the
/// next tick is a region-wide teleport, and the odometer and the subscription
/// update both assume entities move at most
/// [`max_horizontal_speed`](crate::WorldConfig::max_horizontal_speed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnEntities {
    /// What to create. At most [`MAX_SPAWN_PER_MESSAGE`].
    pub spawns: Vec<Spawn>,
}

impl SpawnEntities {
    /// Every entity an observer with tag 0, which is what a crowd of players
    /// is. Each is tokened with its index in `positions`, which is enough to
    /// match up one batch and no use across two.
    pub fn observers(positions: &[Pos3]) -> SpawnEntities {
        SpawnEntities {
            spawns: positions
                .iter()
                .enumerate()
                .map(|(n, p)| Spawn {
                    position: *p,
                    kind: EntityKind::observer(0),
                    token: n as u64,
                })
                .collect(),
        }
    }

    /// Every entity unattended with tag 0, which is what a flight of
    /// projectiles or a herd of wildlife is. Tokened like
    /// [`observers`](Self::observers).
    pub fn unattended(positions: &[Pos3]) -> SpawnEntities {
        SpawnEntities {
            spawns: positions
                .iter()
                .enumerate()
                .map(|(n, p)| Spawn {
                    position: *p,
                    kind: EntityKind::unattended(0),
                    token: n as u64,
                })
                .collect(),
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_SPAWN_ENTITIES);
        out.extend_from_slice(&(self.spawns.len() as u32).to_le_bytes());
        for s in &self.spawns {
            out.extend_from_slice(&s.position.x.raw().to_le_bytes());
            out.extend_from_slice(&s.position.y.raw().to_le_bytes());
            out.extend_from_slice(&s.position.z.raw().to_le_bytes());
            s.kind.encode_wire(out);
            out.extend_from_slice(&s.token.to_le_bytes());
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
            let position = Pos3::new(
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
                crate::Fixed::from_raw(c.i32()?),
            );
            let kind = EntityKind::decode_wire(&mut c)?;
            spawns.push(Spawn { position, kind, token: c.u64()? });
        }
        c.finish()?;
        Ok(SpawnEntities { spawns })
    }
}

/// An entity appearing in or leaving a region.
///
/// Published on the presence subject of the edge that owns it, whatever caused
/// the change: an edge asking, the consumer's game despawning, or an edge
/// being expired and its entities orphaned. An edge that only ever hears about
/// what it asked for cannot know when something it owns has gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Now exists and is managed by the edge this was published for. `token`
    /// is what that edge sent on the [`Spawn`] that asked for it.
    Added {
        /// The id this region allocated.
        entity: EntityId,
        /// Echoed from the spawn that asked, so an edge can tell which of its
        /// requests this answers.
        token: u64,
    },
    /// No longer exists.
    Removed {
        /// The id that is gone.
        entity: EntityId,
    },
}

impl Presence {
    /// One presence report's width on the wire.
    pub const BYTES: usize = 13;

    const ADDED: u8 = 1;
    const REMOVED: u8 = 2;

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            Presence::Added { entity, token } => {
                out.push(Presence::ADDED);
                out.extend_from_slice(&entity.raw().to_le_bytes());
                out.extend_from_slice(&token.to_le_bytes());
            }
            Presence::Removed { entity } => {
                out.push(Presence::REMOVED);
                out.extend_from_slice(&entity.raw().to_le_bytes());
            }
        }
    }

    /// Reads one back.
    pub fn decode(body: &[u8]) -> Result<Presence, NetError> {
        let mut c = Cursor::new(body, "presence");
        let what = c.u8()?;
        let entity = EntityId::from_raw(c.u32()?);
        let got = match what {
            Presence::ADDED => Presence::Added { entity, token: c.u64()? },
            Presence::REMOVED => Presence::Removed { entity },
            _ => return Err(NetError::Malformed("presence kind")),
        };
        c.finish()?;
        Ok(got)
    }
}

/// New absolute positions for entities this edge manages.
///
/// Absolute rather than a delta, so a lost or reordered message costs one tick
/// of staleness rather than corrupting a position permanently. Applying one is
/// idempotent for the same reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveEntities {
    /// Where to put each. At most [`MAX_MOVES_PER_MESSAGE`].
    pub moves: Vec<(EntityId, Pos3)>,
}

impl MoveEntities {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_MOVE_ENTITIES);
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
    /// What to remove. At most [`MAX_DESPAWN_PER_MESSAGE`].
    pub ids: Vec<EntityId>,
}

impl DespawnEntities {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_DESPAWN_ENTITIES);
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

/// Opaque bytes from a game client, forwarded by an edge to the region.
///
/// The entity is the sender's entity in this region. The region validates that
/// the sending edge manages it before delivering the message to the game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameMessage {
    /// The sender's entity in this region.
    pub entity: EntityId,
    /// The game's own bytes. At most [`MAX_GAME_MESSAGE_BODY`].
    pub body: Vec<u8>,
}

impl GameMessage {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_GAME_MESSAGE);
        out.extend_from_slice(&self.entity.raw().to_le_bytes());
        out.extend_from_slice(&self.body);
    }

    pub(crate) fn decode(body: &[u8]) -> Result<GameMessage, NetError> {
        let mut c = Cursor::new(body, "game message");
        let entity = EntityId::from_raw(c.u32()?);
        let rest = c.rest();
        if rest.len() > MAX_GAME_MESSAGE_BODY {
            return Err(NetError::Malformed("game message body too large"));
        }
        Ok(GameMessage { entity, body: rest.to_vec() })
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
    fn server_info_round_trips() {
        let cfg = WorldConfig::default();
        let m = ServerInfo {
            protocol: PROTOCOL_VERSION,
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
            protocol: PROTOCOL_VERSION,
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

    fn ent(n: u32) -> EntityId {
        EntityId::from_raw(n)
    }

    fn want(x: i32, kind: EntityKind, token: u64) -> Spawn {
        Spawn { position: Pos3::from_meters(x, 2, 3), kind, token }
    }

    #[test]
    fn a_spawn_request_round_trips() {
        let m = SpawnEntities {
            spawns: vec![
                want(1, EntityKind::observer(0), 0xDEAD_BEEF_CAFE_F00D),
                want(4095, EntityKind::unattended(0), 0),
            ],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(SpawnEntities::decode(&buf[1..]).expect("well formed"), m);
    }

    #[test]
    fn a_spawn_request_says_what_observes() {
        let at = [Pos3::from_meters(1, 1, 0), Pos3::from_meters(2, 2, 0)];
        assert!(SpawnEntities::observers(&at).spawns.iter().all(|s| s.kind.observes()));
        assert!(SpawnEntities::unattended(&at).spawns.iter().all(|s| !s.kind.observes()));
    }

    #[test]
    fn an_unknown_entity_kind_role_is_refused() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 12]); // position
        buf.push(9); // invalid role
        buf.extend_from_slice(&0u16.to_le_bytes()); // tag
        buf.extend_from_slice(&0u64.to_le_bytes()); // token
        assert!(matches!(
            SpawnEntities::decode(&buf),
            Err(NetError::Malformed("entity kind role"))
        ));
    }

    #[test]
    fn every_entity_kind_round_trips() {
        for kind in [
            EntityKind::unattended(0),
            EntityKind::observer(0),
            EntityKind::unattended(42),
            EntityKind::observer(65535),
        ] {
            let m = SpawnEntities {
                spawns: vec![want(1, kind, 0)],
            };
            let mut buf = Vec::new();
            m.encode(&mut buf);
            let back = SpawnEntities::decode(&buf[1..]).expect("well formed");
            assert_eq!(back.spawns[0].kind, kind);
        }
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
    fn a_full_spawn_request_fits_one_message() {
        let m = SpawnEntities {
            spawns: vec![want(0, EntityKind::observer(0), 7); MAX_SPAWN_PER_MESSAGE],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_MESSAGE_BYTES, "{} bytes is past the cap", buf.len());
    }

    #[test]
    fn a_move_round_trips_losslessly() {
        // Raw fixed point, so a position survives the wire exactly. A round trip
        // that moved an entity would walk it every tick.
        let m = MoveEntities {
            moves: vec![
                (
                    ent(1),
                    Pos3::new(
                        Fixed::from_millimeters(7, 500),
                        Fixed::ZERO,
                        Fixed::from_raw(1),
                    ),
                ),
                (ent(2), Pos3::from_meters(4095, 0, 1023)),
            ],
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(MoveEntities::decode(&buf[1..]).expect("well formed"), m);
    }

    #[test]
    fn a_full_move_message_fits_one_message() {
        let m = MoveEntities { moves: vec![(ent(0), Pos3::ZERO); MAX_MOVES_PER_MESSAGE] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_MESSAGE_BYTES, "{} bytes is past the cap", buf.len());
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
        for cut in 1..buf.len() {
            assert!(
                MoveEntities::decode(&buf[1..cut]).is_err(),
                "{cut} bytes must not parse"
            );
        }
    }

    #[test]
    fn a_despawn_round_trips() {
        let m = DespawnEntities { ids: vec![ent(3), ent(9), ent(1000)] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(DespawnEntities::decode(&buf[1..]).expect("well formed"), m);
    }

    #[test]
    fn a_full_despawn_message_fits_one_message() {
        let m = DespawnEntities { ids: vec![ent(0); MAX_DESPAWN_PER_MESSAGE] };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert!(buf.len() <= MAX_MESSAGE_BYTES, "{} bytes is past the cap", buf.len());
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
    fn presence_round_trips_in_both_directions() {
        for m in [
            Presence::Added { entity: ent(41), token: 0x0123_4567_89AB_CDEF },
            Presence::Added { entity: ent(41), token: 0 },
            Presence::Removed { entity: ent(9) },
        ] {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            assert!(buf.len() <= Presence::BYTES);
            assert_eq!(Presence::decode(&buf).expect("well formed"), m);
        }
    }

    #[test]
    fn an_unknown_presence_kind_is_refused() {
        let mut buf = vec![9u8];
        buf.extend_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            Presence::decode(&buf),
            Err(NetError::Malformed("presence kind"))
        ));
    }

    #[test]
    fn a_truncated_presence_is_refused() {
        let mut buf = Vec::new();
        Presence::Added { entity: ent(1), token: 2 }.encode(&mut buf);
        for cut in 0..buf.len() {
            assert!(Presence::decode(&buf[..cut]).is_err(), "{cut} bytes must not parse");
        }
    }

    #[test]
    fn every_kind_has_a_name() {
        for kind in [
            KIND_SPAWN_ENTITIES,
            KIND_MOVE_ENTITIES,
            KIND_DESPAWN_ENTITIES,
            KIND_KEEPALIVE,
            KIND_GAME_MESSAGE,
        ] {
            assert_ne!(kind_name(kind), "unknown", "kind {kind} needs a name");
        }
        assert_eq!(kind_name(200), "unknown");
    }
}
