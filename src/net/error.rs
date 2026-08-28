//! What can go wrong on either link.

use core::fmt;

use crate::config::ConfigError;
use crate::net::region::protocol::{ProtocolVersion, kind_name};

/// A failure on either link.
#[derive(Debug)]
pub enum NetError {
    /// The NATS client failed: connecting, publishing, subscribing or a request
    /// that went unanswered.
    Nats(Box<dyn std::error::Error + Send + Sync>),
    /// The client link failed: a connection lost, a stream that could not be
    /// written, or a datagram the peer will not take.
    Quic(Box<dyn std::error::Error + Send + Sync>),
    /// Named a client or an entity this edge does not hold.
    ///
    /// A race rather than a mistake, in most cases: a removal can arrive
    /// unprompted, so anything acting on a key may find it already gone.
    Unknown(&'static str),
    /// A message arrived but did not decode. The string names what was being
    /// read rather than echoing any of the sender's bytes back.
    Malformed(&'static str),
    /// A well-formed message of a kind that does not belong on that subject.
    Unexpected { expected: &'static str, got: u8 },
    /// A message body past what this side will allocate for it.
    MessageTooLarge { claimed: usize, max: usize },
    /// A subject carried an edge name this build will not accept.
    BadEdgeName(&'static str),
    /// A subject did not parse into the tokens it should have.
    BadSubject,
    /// The two ends do not speak the same protocol version.
    ProtocolMismatch { ours: ProtocolVersion, theirs: ProtocolVersion },
    /// The advertised world parameters do not rebuild into a valid config.
    Config(ConfigError),
    /// The parameters rebuilt, but into a world that decodes packets
    /// differently from the one the region is running.
    ///
    /// The digest is [`WorldConfig::protocol_hash`](crate::WorldConfig::protocol_hash),
    /// which covers exactly the fields that affect wire decoding.
    ConfigMismatch { ours: u64, theirs: u64 },
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use NetError::*;
        match self {
            Nats(e) => write!(f, "nats: {e}"),
            Quic(e) => write!(f, "quic: {e}"),
            Unknown(what) => write!(f, "no such {what}"),
            Malformed(what) => write!(f, "malformed {what}"),
            Unexpected { expected, got } => {
                write!(f, "expected {expected}, got {} (kind {got})", kind_name(*got))
            }
            MessageTooLarge { claimed, max } => {
                write!(f, "message claims {claimed} bytes, past the {max} byte cap")
            }
            BadEdgeName(why) => write!(f, "edge name: {why}"),
            BadSubject => write!(f, "subject does not parse"),
            ProtocolMismatch { ours, theirs } => {
                write!(f, "this end speaks protocol {ours}, the other speaks {theirs}")
            }
            Config(e) => write!(f, "advertised world parameters are not valid: {e}"),
            ConfigMismatch { ours, theirs } => write!(
                f,
                "world config digest {ours:#018x} does not match the region's {theirs:#018x}"
            ),
        }
    }
}

impl std::error::Error for NetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            NetError::Nats(e) | NetError::Quic(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl From<ConfigError> for NetError {
    fn from(e: ConfigError) -> NetError {
        NetError::Config(e)
    }
}

/// Every `async-nats` failure reaches here the same way, since a caller can do
/// nothing different about a publish that failed than about a subscribe that
/// did.
macro_rules! from_nats {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for NetError {
            fn from(e: $t) -> NetError {
                NetError::Nats(Box::new(e))
            }
        })*
    };
}

/// Every `quinn` failure reaches here the same way, for the same reason.
macro_rules! from_quic {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for NetError {
            fn from(e: $t) -> NetError {
                NetError::Quic(Box::new(e))
            }
        })*
    };
}

from_quic!(
    quinn::ConnectionError,
    quinn::WriteError,
    quinn::ReadError,
    quinn::ReadExactError,
    quinn::SendDatagramError,
    quinn::ClosedStream,
);

from_nats!(
    async_nats::client::FlushError,
    async_nats::ConnectError,
    async_nats::PublishError,
    async_nats::SubscribeError,
    async_nats::RequestError,
    std::io::Error,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_io_error_keeps_its_source() {
        use std::error::Error;
        let e = NetError::from(std::io::Error::other("no broker"));
        assert!(e.source().is_some(), "a consumer chaining causes needs the inner error");
    }

    #[test]
    fn a_malformed_message_names_what_was_read_and_not_the_bytes() {
        let shown = NetError::Malformed("move entities").to_string();
        assert_eq!(shown, "malformed move entities");
    }
}
