//! Reading a message body, shared by everything that has one.
//!
//! Both links and the control plane decode bytes somebody else wrote, and all
//! three need the same guarantee: never run off the end, and never accept
//! trailing bytes. That is one piece of machinery rather than three, and it
//! belongs to none of them, so it lives here instead of inside whichever module
//! happened to need it first.

use crate::net::error::NetError;

/// Reads scalars out of a message body, refusing to run off the end.
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

    /// A run of bytes, for a length-prefixed field.
    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], NetError> {
        self.take(n)
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

    /// Trailing bytes mean the two ends disagree about the format, so this is a
    /// decode failure rather than something to ignore.
    pub(crate) fn finish(self) -> Result<(), NetError> {
        if self.at == self.buf.len() { Ok(()) } else { Err(NetError::Malformed(self.what)) }
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn a_cursor_refuses_to_run_off_the_end() {
        let mut c = Cursor::new(&[1, 2, 3], "test");
        assert_eq!(c.u16().expect("two bytes are there"), 0x0201);
        assert!(matches!(c.u32(), Err(NetError::Malformed("test"))));
    }

    #[test]
    fn a_cursor_refuses_trailing_bytes() {
        let mut c = Cursor::new(&[1, 2, 3, 4], "test");
        c.u16().expect("two bytes are there");
        assert!(matches!(c.finish(), Err(NetError::Malformed("test"))));
    }

    #[test]
    fn a_cursor_reads_every_width() {
        let mut buf = vec![0xABu8];
        buf.extend_from_slice(&0x1234u16.to_le_bytes());
        buf.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        buf.extend_from_slice(&(-7i32).to_le_bytes());
        buf.extend_from_slice(&0x0123_4567_89AB_CDEFu64.to_le_bytes());
        buf.extend_from_slice(b"tail");

        let mut c = Cursor::new(&buf, "test");
        assert_eq!(c.u8().unwrap(), 0xAB);
        assert_eq!(c.u16().unwrap(), 0x1234);
        assert_eq!(c.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(c.i32().unwrap(), -7);
        assert_eq!(c.u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert!(matches!(c.finish(), Err(NetError::Malformed("test"))), "tail is left over");
    }
}
