//! The byte-level encoding used inside a frame.
//!
//! Every message is a tag byte followed by its fields in a fixed order.
//! Integers are big-endian; strings and byte strings carry a `u32` length
//! prefix; lists carry a `u32` element count. The decoder checks every length
//! against both a per-field maximum and the bytes actually left in the frame
//! before it copies or allocates anything, so a hostile length can neither
//! make it allocate a huge buffer nor read past the frame.

use std::fmt;

/// Why a frame payload could not be decoded into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The payload ended before a field was complete.
    Truncated {
        field: &'static str,
        needed: usize,
        remaining: usize,
    },
    /// A tag byte named no known variant.
    UnknownTag { field: &'static str, tag: u8 },
    /// A string field was not valid UTF-8.
    InvalidUtf8 { field: &'static str },
    /// A length-prefixed field declared more bytes or elements than allowed.
    TooLong {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// Bytes were left over after the message ended.
    TrailingBytes { count: usize },
    /// A field decoded but its value is not allowed.
    Invalid { field: &'static str, detail: String },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                field,
                needed,
                remaining,
            } => write!(
                f,
                "truncated {field}: needed {needed} bytes, {remaining} remaining"
            ),
            Self::UnknownTag { field, tag } => write!(f, "unknown tag {tag} for {field}"),
            Self::InvalidUtf8 { field } => write!(f, "{field} is not valid UTF-8"),
            Self::TooLong { field, len, max } => {
                write!(f, "{field} has length {len}, maximum is {max}")
            }
            Self::TrailingBytes { count } => write!(f, "{count} trailing bytes after message"),
            Self::Invalid { field, detail } => write!(f, "invalid {field}: {detail}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Appends fields to a message buffer.
#[derive(Debug, Default)]
pub(crate) struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub(crate) fn with_header_room() -> Self {
        // Four bytes are reserved for the frame length so a message can be
        // written with one `write_all` once its size is known.
        Self { buf: vec![0; 4] }
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub(crate) fn fixed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a length-prefixed byte string. Lengths above `u32::MAX` cannot
    /// be produced by any valid message (frames are far smaller), so they are
    /// saturated; the frame size check then refuses the message.
    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        self.u32(len);
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(crate) fn count(&mut self, len: usize) {
        self.u32(u32::try_from(len).unwrap_or(u32::MAX));
    }

    pub(crate) fn option<T>(&mut self, value: Option<&T>, mut each: impl FnMut(&mut Self, &T)) {
        match value {
            None => self.u8(0),
            Some(inner) => {
                self.u8(1);
                each(self, inner);
            }
        }
    }
}

/// Reads fields from a message payload, refusing anything malformed.
pub(crate) struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, field: &'static str, len: usize) -> Result<&'a [u8], DecodeError> {
        if len > self.remaining() {
            return Err(DecodeError::Truncated {
                field,
                needed: len,
                remaining: self.remaining(),
            });
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }

    pub(crate) fn finish(self) -> Result<(), DecodeError> {
        match self.remaining() {
            0 => Ok(()),
            count => Err(DecodeError::TrailingBytes { count }),
        }
    }

    pub(crate) fn u8(&mut self, field: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(field, 1)?[0])
    }

    pub(crate) fn u32(&mut self, field: &'static str) -> Result<u32, DecodeError> {
        let bytes = self.take(field, 4)?;
        let mut array = [0u8; 4];
        array.copy_from_slice(bytes);
        Ok(u32::from_be_bytes(array))
    }

    pub(crate) fn u64(&mut self, field: &'static str) -> Result<u64, DecodeError> {
        let bytes = self.take(field, 8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(array))
    }

    pub(crate) fn fixed32(&mut self, field: &'static str) -> Result<[u8; 32], DecodeError> {
        let bytes = self.take(field, 32)?;
        let mut array = [0u8; 32];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    /// Reads a length-prefixed byte string, checking the declared length
    /// against `max` before touching the bytes.
    pub(crate) fn bytes(
        &mut self,
        field: &'static str,
        max: usize,
    ) -> Result<&'a [u8], DecodeError> {
        let len = self.u32(field)? as usize;
        if len > max {
            return Err(DecodeError::TooLong { field, len, max });
        }
        self.take(field, len)
    }

    pub(crate) fn string(
        &mut self,
        field: &'static str,
        max: usize,
    ) -> Result<String, DecodeError> {
        let bytes = self.bytes(field, max)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| DecodeError::InvalidUtf8 { field })
    }

    /// Reads a list count. Every element takes at least one byte, so a count
    /// larger than the bytes left is refused before any allocation.
    pub(crate) fn count(&mut self, field: &'static str, max: usize) -> Result<usize, DecodeError> {
        let len = self.u32(field)? as usize;
        if len > max {
            return Err(DecodeError::TooLong { field, len, max });
        }
        if len > self.remaining() {
            return Err(DecodeError::Truncated {
                field,
                needed: len,
                remaining: self.remaining(),
            });
        }
        Ok(len)
    }

    pub(crate) fn option<T>(
        &mut self,
        field: &'static str,
        each: impl FnOnce(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Option<T>, DecodeError> {
        match self.u8(field)? {
            0 => Ok(None),
            1 => each(self).map(Some),
            tag => Err(DecodeError::UnknownTag { field, tag }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_length_beyond_maximum_is_refused_before_reading() {
        let mut enc = Encoder::default();
        enc.u32(10_000);
        let bytes = enc.into_inner();
        let mut dec = Decoder::new(&bytes);
        assert_eq!(
            dec.bytes("field", 100),
            Err(DecodeError::TooLong {
                field: "field",
                len: 10_000,
                max: 100
            })
        );
    }

    #[test]
    fn list_count_beyond_remaining_bytes_is_refused() {
        let mut enc = Encoder::default();
        enc.u32(1_000_000);
        let bytes = enc.into_inner();
        let mut dec = Decoder::new(&bytes);
        assert!(matches!(
            dec.count("list", usize::MAX),
            Err(DecodeError::Truncated { .. })
        ));
    }
}
