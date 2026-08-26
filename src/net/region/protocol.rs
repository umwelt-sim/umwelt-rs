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
//! The client accepts or quits rather than the server assuming: a client that
//! reads the parameters and finds a world it cannot render, or a region it did
//! not mean to reach, says so and leaves instead of holding a connection the
//! server counts as an edge.

use core::fmt;
use std::time::Duration;

use crate::config::WorldConfig;
use crate::net::error::{NetError, RejectCode};
use crate::net::region::auth::MAX_CREDENTIAL_BYTES;
use crate::net::region::wire::Cursor;

// ---------------------------------------------------------------------------
// Frame kinds
// ---------------------------------------------------------------------------

pub(crate) const KIND_CLIENT_IDENTIFICATION: u8 = 1;
pub(crate) const KIND_SERVER_INFO: u8 = 2;
pub(crate) const KIND_REJECTION: u8 = 3;
pub(crate) const KIND_ACCEPTED: u8 = 4;
pub(crate) const KIND_QUIT: u8 = 5;

/// Names a frame kind for an error message, without echoing the peer's bytes.
pub(crate) fn kind_name(kind: u8) -> &'static str {
    match kind {
        KIND_CLIENT_IDENTIFICATION => "client identification",
        KIND_SERVER_INFO => "server info",
        KIND_REJECTION => "rejection",
        KIND_ACCEPTED => "accepted",
        KIND_QUIT => "quit",
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn every_kind_has_a_name() {
        for kind in [
            KIND_CLIENT_IDENTIFICATION,
            KIND_SERVER_INFO,
            KIND_REJECTION,
            KIND_ACCEPTED,
            KIND_QUIT,
        ] {
            assert_ne!(kind_name(kind), "unknown", "kind {kind} needs a name");
        }
        assert_eq!(kind_name(200), "unknown");
    }
}
