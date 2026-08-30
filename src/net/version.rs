//! The version numbers both links and the control plane report.
//!
//! Two separate things. A [`ProtocolVersion`] says what shape the messages on
//! a link have, and has to match exactly. A [`ServerVersion`] says which build
//! is running, and nothing rejects on it. A world's wire layout is a third,
//! carried as
//! [`WorldConfig::protocol_hash`](crate::config::WorldConfig::protocol_hash),
//! and all three move independently.
//!
//! They live here rather than beside either protocol because each link carries
//! its own protocol version, and the control plane and
//! [`NetError`](crate::NetError) name these types without belonging to either
//! link.

use core::fmt;

/// The version of one protocol.
///
/// Bumped when that protocol's messages change shape. Each link owns its own
/// value: the region-to-edge constant is `PROTOCOL_VERSION`, and the
/// edge-to-game-client protocol will carry a separate one.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion(u16);

impl ProtocolVersion {
    /// From the raw value.
    #[inline]
    pub const fn from_raw(raw: u16) -> ProtocolVersion {
        ProtocolVersion(raw)
    }

    /// The raw value.
    #[inline]
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

/// The crate version a server is running.
///
/// Informational: it is reported so an operator can see what a region or an
/// edge is running without asking it, and nothing rejects on it. A protocol
/// version is what has to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServerVersion {
    /// Breaking changes.
    pub major: u16,
    /// Additions.
    pub minor: u16,
    /// Fixes.
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
