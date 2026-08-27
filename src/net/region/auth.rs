//! Who is allowed to open a connection to a region.
//!
//! **What this is.** A bearer secret. The client presents bytes in its
//! [`Hello`](crate::net::Hello), the server compares them against bytes it
//! holds, and a peer that cannot produce them is refused before it is told
//! anything about the region.
//!
//! **What this is not.** Protection against anyone who can read the link. The
//! secret crosses the wire on every connect, so whoever sees the traffic has
//! it. The deployment assumption is that a region and its edges talk over a
//! network that is already private, and this check is the second lock rather
//! than the first: it stops a process that wandered onto the right port, a
//! misconfigured edge pointed at the wrong region, and a stale binary from a
//! previous deployment. It does not stop an attacker already on the wire.
//!
//! Raising that bar means challenge-response over a MAC, so the secret itself
//! never travels. That needs a vetted crypto implementation rather than a
//! hand-rolled one, which is a dependency this crate does not have and a
//! decision that has not been made. [`Authorizer`] is the seam it would arrive
//! through.

use core::fmt;

/// The credential did not authorize.
///
/// Carries no reason. The server tells the peer
/// [`RejectCode::Unauthorized`](crate::net::RejectCode::Unauthorized) and
/// nothing more. A type carrying a reason could be forwarded to the peer by
/// mistake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Denied;

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "credential denied")
    }
}

/// Decides whether a connecting peer may proceed.
///
/// Consulted once per connection, before the region says anything about
/// itself. Implementations are shared across accept threads, which is why this
/// is `Send + Sync` and takes `&self`.
///
/// `credential` is the opaque bytes the peer sent. This crate never interprets
/// them.
pub trait Authorizer: Send + Sync {
    fn authorize(&self, credential: &[u8]) -> Result<(), Denied>;
}

/// Longest credential a server will read.
///
/// Bounds the read before the credential is examined, since the length arrives
/// from a peer that has not authorized.
pub const MAX_CREDENTIAL_BYTES: usize = 256;

/// One secret, shared by every edge entitled to connect to this region.
///
/// Holds the secret for the process's lifetime. It is not zeroized on drop and
/// this crate makes no claim about where in memory it has been copied.
pub struct SharedSecret {
    secret: Box<[u8]>,
}

impl SharedSecret {
    /// # Panics
    ///
    /// If the secret is empty, or longer than [`MAX_CREDENTIAL_BYTES`]. An
    /// empty secret would authorize a peer that sent nothing, and one past the
    /// cap could never be presented.
    pub fn new(secret: impl Into<Vec<u8>>) -> SharedSecret {
        let secret = secret.into();
        assert!(!secret.is_empty(), "an empty shared secret authorizes everyone");
        assert!(
            secret.len() <= MAX_CREDENTIAL_BYTES,
            "a {} byte secret is past the {MAX_CREDENTIAL_BYTES} byte credential cap",
            secret.len()
        );
        SharedSecret { secret: secret.into_boxed_slice() }
    }
}

/// Redacted. A secret that prints itself ends up in a log.
impl fmt::Debug for SharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SharedSecret({} bytes, redacted)", self.secret.len())
    }
}

impl Authorizer for SharedSecret {
    fn authorize(&self, credential: &[u8]) -> Result<(), Denied> {
        if equal_without_short_circuit(&self.secret, credential) { Ok(()) } else { Err(Denied) }
    }
}

/// Compares every byte rather than stopping at the first difference, so the
/// time taken does not report how much of a guess was right.
///
/// **Not a hardened primitive.** Nothing in the language stops the optimizer
/// from rewriting this, and no timing measurement of the compiled output has
/// been taken. It is written this way because the naive `==` is measurably
/// worse and this costs nothing, not because the result is guaranteed. A
/// deployment that needs the guarantee wants a vetted constant-time compare.
///
/// Length is compared up front and therefore leaks, which is standard: the
/// length of a secret is not the secret.
fn equal_without_short_circuit(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut differing = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        differing |= x ^ y;
    }
    differing == 0
}

/// Authorizes everyone.
///
/// For tests, benchmarks, and a single-process example where there is no other
/// process to keep out. The name is explicit so that a deployment selecting it
/// is visible at the line that constructs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    #[inline]
    fn authorize(&self, _credential: &[u8]) -> Result<(), Denied> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_right_secret_authorizes() {
        let a = SharedSecret::new(*b"region-7-edge-key");
        assert_eq!(a.authorize(b"region-7-edge-key"), Ok(()));
    }

    #[test]
    fn a_wrong_secret_does_not() {
        let a = SharedSecret::new(*b"region-7-edge-key");
        assert_eq!(a.authorize(b"region-8-edge-key"), Err(Denied));
        assert_eq!(a.authorize(b""), Err(Denied));
        assert_eq!(a.authorize(b"region-7-edge-key-and-more"), Err(Denied));
        assert_eq!(a.authorize(b"region-7-edge-ke"), Err(Denied));
    }

    #[test]
    fn a_prefix_of_the_secret_does_not_authorize() {
        // The failure a short-circuiting compare would make cheap to find.
        let a = SharedSecret::new(*b"abcdefgh");
        for n in 0..8 {
            assert_eq!(a.authorize(&b"abcdefgh"[..n]), Err(Denied));
        }
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        let a = SharedSecret::new(*b"hunter2");
        let shown = format!("{a:?}");
        assert!(!shown.contains("hunter2"), "Debug leaked the secret: {shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    #[should_panic(expected = "authorizes everyone")]
    fn an_empty_secret_is_refused_at_construction() {
        SharedSecret::new(Vec::new());
    }

    #[test]
    #[should_panic(expected = "credential cap")]
    fn a_secret_past_the_credential_cap_is_refused() {
        SharedSecret::new(vec![b'x'; MAX_CREDENTIAL_BYTES + 1]);
    }

    #[test]
    fn allow_all_takes_anything() {
        assert_eq!(AllowAll.authorize(b""), Ok(()));
        assert_eq!(AllowAll.authorize(b"whatever"), Ok(()));
        assert_eq!(size_of::<AllowAll>(), 0, "the test default costs nothing to hold");
    }

    #[test]
    fn an_authorizer_can_be_held_behind_a_trait_object() {
        // The shape RegionServer stores it in.
        let held: std::sync::Arc<dyn Authorizer> = std::sync::Arc::new(SharedSecret::new(*b"k"));
        assert_eq!(held.authorize(b"k"), Ok(()));
    }
}
