//! What can go wrong on the wire.

use core::fmt;
use std::io;

use crate::config::ConfigError;
use crate::net::region::protocol::{ProtocolVersion, kind_name};

/// Why a server refused a connection.
///
/// Deliberately coarse. A peer that fails the credential check is told that and
/// nothing else, so the reply cannot be used to learn whether a region is here,
/// what it expects, or how close a guess came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectCode {
    /// The credential did not authorize.
    Unauthorized = 1,
    /// The peer speaks a protocol version this server does not.
    ProtocolMismatch = 2,
    /// The peer's first frame did not decode.
    Malformed = 3,
}

impl RejectCode {
    #[inline(always)]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(raw: u8) -> Option<RejectCode> {
        match raw {
            1 => Some(RejectCode::Unauthorized),
            2 => Some(RejectCode::ProtocolMismatch),
            3 => Some(RejectCode::Malformed),
            _ => None,
        }
    }
}

impl fmt::Display for RejectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectCode::Unauthorized => write!(f, "unauthorized"),
            RejectCode::ProtocolMismatch => write!(f, "protocol version mismatch"),
            RejectCode::Malformed => write!(f, "malformed frame"),
        }
    }
}

/// A failure on the region-to-edge link.
#[derive(Debug)]
pub enum NetError {
    Io(io::Error),
    /// A frame arrived but did not decode. The string names what was being read
    /// rather than echoing any of the peer's bytes back.
    Malformed(&'static str),
    /// A well-formed frame of the wrong kind for this point in the exchange.
    Unexpected { expected: &'static str, got: u8 },
    /// A frame claimed a length past what this side will allocate for it.
    ///
    /// Refused before the allocation, which is the point: the length is read
    /// from an unauthorized peer.
    FrameTooLarge { claimed: usize, max: usize },
    /// The peer closed before the exchange finished.
    Closed,
    /// The server refused this client.
    Rejected(RejectCode),
    /// The two ends do not speak the same protocol version.
    ProtocolMismatch { ours: ProtocolVersion, theirs: ProtocolVersion },
    /// The advertised world parameters do not rebuild into a valid config.
    Config(ConfigError),
    /// The parameters rebuilt, but into a world that decodes packets
    /// differently from the one the server is running.
    ///
    /// The digest is [`WorldConfig::protocol_hash`](crate::WorldConfig::protocol_hash),
    /// which covers exactly the fields that affect wire decoding. A mismatch
    /// means the server's config was not reachable through the builder from the
    /// values it sent, so this side would decode its packets into garbage.
    ConfigMismatch { ours: u64, theirs: u64 },
    /// This client read the offer and chose not to take it.
    ///
    /// Not a failure of the link. It is an error only because a connect that
    /// quits has no client to return.
    Declined,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use NetError::*;
        match self {
            Io(e) => write!(f, "{e}"),
            Malformed(what) => write!(f, "malformed {what}"),
            Unexpected { expected, got } => {
                write!(f, "expected {expected}, got {} (frame kind {got})", kind_name(*got))
            }
            FrameTooLarge { claimed, max } => {
                write!(f, "frame claims {claimed} bytes, which is past the {max} byte cap")
            }
            Closed => write!(f, "peer closed the connection"),
            Rejected(code) => write!(f, "refused by the region: {code}"),
            ProtocolMismatch { ours, theirs } => {
                write!(f, "this end speaks protocol {ours}, the other speaks {theirs}")
            }
            Config(e) => write!(f, "advertised world parameters are not valid: {e}"),
            ConfigMismatch { ours, theirs } => write!(
                f,
                "world config digest {ours:#018x} does not match the region's {theirs:#018x}"
            ),
            Declined => write!(f, "this client declined the region's offer"),
        }
    }
}

impl std::error::Error for NetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NetError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for NetError {
    fn from(e: io::Error) -> NetError {
        NetError::Io(e)
    }
}

impl From<ConfigError> for NetError {
    fn from(e: ConfigError) -> NetError {
        NetError::Config(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reject_code_round_trips() {
        for code in [RejectCode::Unauthorized, RejectCode::ProtocolMismatch, RejectCode::Malformed]
        {
            assert_eq!(RejectCode::from_u8(code.as_u8()), Some(code));
        }
    }

    #[test]
    fn an_unknown_reject_code_is_not_invented() {
        assert_eq!(RejectCode::from_u8(0), None);
        assert_eq!(RejectCode::from_u8(200), None);
    }

    #[test]
    fn an_io_error_keeps_its_source() {
        use std::error::Error;
        let e = NetError::from(io::Error::new(io::ErrorKind::ConnectionRefused, "no listener"));
        assert!(e.source().is_some(), "a consumer chaining causes needs the inner error");
    }
}
