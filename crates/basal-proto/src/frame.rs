//! Length-prefixed frames over a byte stream.
//!
//! A frame is a 4-byte big-endian payload length followed by the payload.
//! The length is checked against [`MAX_FRAME_BYTES`] before the payload is
//! read, so a hostile peer cannot make the reader allocate more than that.

use std::fmt;
use std::io::{self, Read, Write};

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::limits::MAX_FRAME_BYTES;
use crate::types::{ParentMessage, WorkerMessage};
use crate::wire;

/// Why a frame could not be read or written.
#[derive(Debug)]
pub enum FrameError {
    /// The stream ended cleanly at a frame boundary.
    Closed,
    /// The stream ended inside a frame.
    Truncated {
        expected: usize,
        received: usize,
    },
    /// The declared length exceeds [`MAX_FRAME_BYTES`]. The payload was not
    /// read, so the stream can no longer be split into frames.
    Oversized {
        declared: u64,
        max: u64,
    },
    Io(io::Error),
    /// The whole frame was read but its payload is not a valid message. The
    /// stream is still in sync and the next frame can be read.
    Decode(DecodeError),
}

impl FrameError {
    /// Whether the byte stream is still usable after this error.
    pub fn stream_intact(&self) -> bool {
        matches!(self, Self::Decode(_))
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "stream closed"),
            Self::Truncated { expected, received } => {
                write!(
                    f,
                    "stream ended inside a frame: {received} of {expected} bytes"
                )
            }
            Self::Oversized { declared, max } => {
                write!(f, "frame of {declared} bytes exceeds the maximum of {max}")
            }
            Self::Io(e) => write!(f, "i/o error: {e}"),
            Self::Decode(e) => write!(f, "malformed frame: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Reads exactly `buf.len()` bytes, distinguishing a clean end of stream
/// before the first byte from an end in the middle.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize, io::Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Reads one frame's payload.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];
    match read_full(r, &mut header)? {
        0 => return Err(FrameError::Closed),
        4 => {}
        received => {
            return Err(FrameError::Truncated {
                expected: 4,
                received,
            });
        }
    }
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized {
            declared: len as u64,
            max: MAX_FRAME_BYTES as u64,
        });
    }
    let mut payload = vec![0u8; len];
    let received = read_full(r, &mut payload)?;
    if received != len {
        return Err(FrameError::Truncated {
            expected: len,
            received,
        });
    }
    Ok(payload)
}

/// Writes one frame with the given payload. Used by tests to send payloads
/// that are deliberately not valid messages.
pub fn write_raw_frame(w: &mut impl Write, payload: &[u8]) -> Result<(), FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized {
            declared: payload.len() as u64,
            max: MAX_FRAME_BYTES as u64,
        });
    }
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

/// Turns an encoder holding a header placeholder and a payload into a frame.
fn seal(enc: Encoder) -> Result<Vec<u8>, FrameError> {
    let mut frame = enc.into_inner();
    let len = frame.len() - 4;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized {
            declared: len as u64,
            max: MAX_FRAME_BYTES as u64,
        });
    }
    frame[..4].copy_from_slice(&(len as u32).to_be_bytes());
    Ok(frame)
}

/// Encodes a parent message as a complete frame, header included.
pub fn encode_parent_frame(m: &ParentMessage) -> Result<Vec<u8>, FrameError> {
    let mut enc = Encoder::with_header_room();
    wire::encode_parent(&mut enc, m);
    seal(enc)
}

/// Encodes a worker message as a complete frame, header included.
pub fn encode_worker_frame(m: &WorkerMessage) -> Result<Vec<u8>, FrameError> {
    let mut enc = Encoder::with_header_room();
    wire::encode_worker(&mut enc, m);
    seal(enc)
}

/// Decodes a parent message from a frame payload.
pub fn decode_parent_payload(payload: &[u8]) -> Result<ParentMessage, DecodeError> {
    let mut dec = Decoder::new(payload);
    let m = wire::decode_parent(&mut dec)?;
    dec.finish()?;
    Ok(m)
}

/// Decodes a worker message from a frame payload.
pub fn decode_worker_payload(payload: &[u8]) -> Result<WorkerMessage, DecodeError> {
    let mut dec = Decoder::new(payload);
    let m = wire::decode_worker(&mut dec)?;
    dec.finish()?;
    Ok(m)
}

pub fn write_parent_message(w: &mut impl Write, m: &ParentMessage) -> Result<(), FrameError> {
    let frame = encode_parent_frame(m)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn write_worker_message(w: &mut impl Write, m: &WorkerMessage) -> Result<(), FrameError> {
    let frame = encode_worker_frame(m)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn read_parent_message(r: &mut impl Read) -> Result<ParentMessage, FrameError> {
    decode_parent_payload(&read_frame(r)?).map_err(FrameError::Decode)
}

pub fn read_worker_message(r: &mut impl Read) -> Result<WorkerMessage, FrameError> {
    decode_worker_payload(&read_frame(r)?).map_err(FrameError::Decode)
}
