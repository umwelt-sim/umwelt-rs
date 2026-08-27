//! Frames on the control channel.
//!
//! A frame is a one-byte kind, a `u32` length, then that many bytes of body.
//! Little-endian throughout, matching [`packet`](crate::packet).
//!
//! The length is bounded before anything is allocated. A handshake frame is
//! tens of bytes and the cap is [`MAX_FRAME_BYTES`], so a peer claiming more is
//! refused. The length is read from a peer that has not authorized yet, since
//! the credential is inside the frame being sized.
//!
//! This carries control messages only. The state payloads
//! [`PacketWriter`](crate::PacketWriter) assembles are latest-only and lossy,
//! and a reliable ordered stream is the wrong shape for them. Carrying those is
//! the next piece of work and is not built.

use std::io::{self, Read, Write};

use crate::net::error::NetError;

/// Most bytes a single control frame may carry.
///
/// A [`Welcome`](crate::net::Welcome) is 38 bytes and a
/// [`Hello`](crate::net::Hello) is 4 plus its credential. The cap is far above
/// both so the format has room, and far below anything that would matter as an
/// allocation.
pub const MAX_FRAME_BYTES: usize = 4096;

/// Kind, then a `u32` length.
const HEADER_BYTES: usize = 5;

/// **Does not flush.** A frame written to a `TcpStream` has been handed to the
/// kernel, since that flush is a no-op. A frame written to a buffered writer
/// waits for one. The bulk path depends on this: it writes many frames and
/// flushes once, because a syscall per frame held delivery to about 170,000
/// payloads a second. See §The smoke test.
pub(crate) fn write_frame(out: &mut impl Write, kind: u8, body: &[u8]) -> Result<(), NetError> {
    write_frame_parts(out, kind, &[body])
}

/// One frame from several pieces, without joining them first.
///
/// The bulk path has a viewer id and a payload that live in different places,
/// and copying them together to write them would be a copy per payload.
pub(crate) fn write_frame_parts(
    out: &mut impl Write,
    kind: u8,
    parts: &[&[u8]],
) -> Result<(), NetError> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    if len > MAX_FRAME_BYTES {
        return Err(NetError::FrameTooLarge { claimed: len, max: MAX_FRAME_BYTES });
    }
    let mut head = [0u8; HEADER_BYTES];
    head[0] = kind;
    head[1..].copy_from_slice(&(len as u32).to_le_bytes());
    out.write_all(&head)?;
    for part in parts {
        out.write_all(part)?;
    }
    Ok(())
}

/// Reads one frame into `body` and returns its kind.
///
/// `body` is reused across calls, so a caller holding one buffer does not
/// allocate per frame.
pub(crate) fn read_frame(src: &mut impl Read, body: &mut Vec<u8>) -> Result<u8, NetError> {
    let mut head = [0u8; HEADER_BYTES];
    read_exact(src, &mut head)?;
    let kind = head[0];
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(NetError::FrameTooLarge { claimed: len, max: MAX_FRAME_BYTES });
    }
    body.clear();
    body.resize(len, 0);
    read_exact(src, body)?;
    Ok(kind)
}

/// A peer that closes mid-frame is closed, not broken. Every other read failure
/// is the io error it was.
fn read_exact(src: &mut impl Read, into: &mut [u8]) -> Result<(), NetError> {
    match src.read_exact(into) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(NetError::Closed),
        Err(e) => Err(NetError::Io(e)),
    }
}

/// Reads scalars out of a frame body, refusing to run off the end.
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
    what: &'static str,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(buf: &'a [u8], what: &'static str) -> Cursor<'a> {
        Cursor { buf, at: 0, what }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], NetError> {
        let end = self.at.checked_add(n).ok_or(NetError::Malformed(self.what))?;
        if end > self.buf.len() {
            return Err(NetError::Malformed(self.what));
        }
        let got = &self.buf[self.at..end];
        self.at = end;
        Ok(got)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, NetError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, NetError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, NetError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, NetError> {
        Ok(self.u32()? as i32)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, NetError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], NetError> {
        self.take(n)
    }

    /// Everything not yet read, for a message whose tail is an opaque blob.
    /// Consumes the cursor, since there is nothing left to read after it.
    pub(crate) fn rest(self) -> &'a [u8] {
        &self.buf[self.at..]
    }

    /// Trailing bytes mean the two ends disagree about the format, so this is a
    /// decode failure rather than something to ignore.
    pub(crate) fn finish(self) -> Result<(), NetError> {
        if self.at == self.buf.len() { Ok(()) } else { Err(NetError::Malformed(self.what)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 7, b"body bytes").expect("writes to a vec");

        let mut body = Vec::new();
        let kind = read_frame(&mut &wire[..], &mut body).expect("well formed");
        assert_eq!(kind, 7);
        assert_eq!(body, b"body bytes");
    }

    #[test]
    fn an_empty_body_is_a_frame() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 4, &[]).expect("writes to a vec");
        assert_eq!(wire.len(), HEADER_BYTES);

        let mut body = Vec::new();
        assert_eq!(read_frame(&mut &wire[..], &mut body).expect("well formed"), 4);
        assert!(body.is_empty());
    }

    #[test]
    fn a_length_past_the_cap_is_refused_before_it_allocates() {
        let mut wire = vec![1u8];
        wire.extend_from_slice(&(u32::MAX).to_le_bytes());
        let mut body = Vec::new();
        match read_frame(&mut &wire[..], &mut body) {
            Err(NetError::FrameTooLarge { claimed, max }) => {
                assert_eq!(claimed, u32::MAX as usize);
                assert_eq!(max, MAX_FRAME_BYTES);
            }
            other => panic!("a 4 GB claim must be refused, got {other:?}"),
        }
        assert!(body.capacity() < MAX_FRAME_BYTES + 1, "nothing was allocated for the claim");
    }

    #[test]
    fn a_truncated_frame_reads_as_closed() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 2, b"twenty four bytes of body").expect("writes to a vec");
        for cut in 0..wire.len() {
            let mut body = Vec::new();
            assert!(
                matches!(read_frame(&mut &wire[..cut], &mut body), Err(NetError::Closed)),
                "a frame {cut} bytes in must not parse"
            );
        }
    }

    #[test]
    fn the_buffer_is_reused_across_frames() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 1, &[9u8; 64]).expect("writes to a vec");
        write_frame(&mut wire, 1, &[9u8; 8]).expect("writes to a vec");

        let mut src = &wire[..];
        let mut body = Vec::new();
        read_frame(&mut src, &mut body).expect("well formed");
        let cap = body.capacity();
        read_frame(&mut src, &mut body).expect("well formed");
        assert_eq!(body.len(), 8, "the second frame is shorter");
        assert_eq!(body.capacity(), cap, "and it reused the first frame's allocation");
    }

    #[test]
    fn a_cursor_refuses_to_run_off_the_end() {
        let mut c = Cursor::new(&[1, 2, 3], "test frame");
        assert_eq!(c.u16().expect("two bytes are there"), 0x0201);
        assert!(matches!(c.u32(), Err(NetError::Malformed("test frame"))));
    }

    #[test]
    fn a_cursor_refuses_trailing_bytes() {
        let mut c = Cursor::new(&[1, 2, 3, 4], "test frame");
        c.u16().expect("two bytes are there");
        assert!(matches!(c.finish(), Err(NetError::Malformed("test frame"))));
    }

    #[test]
    fn a_cursor_reads_every_width() {
        let mut buf = Vec::new();
        buf.push(0xABu8);
        buf.extend_from_slice(&0x1234u16.to_le_bytes());
        buf.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        buf.extend_from_slice(&(-7i32).to_le_bytes());
        buf.extend_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes());
        buf.extend_from_slice(b"tail");

        let mut c = Cursor::new(&buf, "test frame");
        assert_eq!(c.u8().unwrap(), 0xAB);
        assert_eq!(c.u16().unwrap(), 0x1234);
        assert_eq!(c.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(c.i32().unwrap(), -7);
        assert_eq!(c.u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(c.bytes(4).unwrap(), b"tail");
        assert!(c.finish().is_ok());
    }
}
