use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) const HEADER_LEN: u64 = 56;
const MAGIC: &[u8; 8] = b"BASALLOG";
const VERSION: u32 = 1;
pub(super) const LINK: u8 = 1;
pub(super) const SEAL: u8 = 2;
// Bounds allocation when a damaged length field is read during recovery.
pub(super) const MAX_RECORD: usize = 8 * 1024 * 1024;

pub(super) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0x82f63b78 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEnd {
    pub lsn: Lsn,
    pub crc: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Header {
    pub identity: Identity,
    pub lease: u64,
    pub seq: u64,
    pub crc: u32,
}

impl Header {
    pub fn write(file: &mut File, identity: Identity, lease: u64, seq: u64) -> Result<Self> {
        let mut bytes = Vec::with_capacity(HEADER_LEN as usize);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&identity.store_id);
        bytes.extend_from_slice(&identity.log_epoch.to_le_bytes());
        bytes.extend_from_slice(&lease.to_le_bytes());
        bytes.extend_from_slice(&seq.to_le_bytes());
        let crc = crc32c(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        observe_io!(
            super::measurement::Operation::Append(bytes.len() as u64),
            file.write_all(&bytes)
        )?;
        Ok(Self {
            identity,
            lease,
            seq,
            crc,
        })
    }

    pub fn read(file: &mut File) -> Result<Self> {
        let mut bytes = [0u8; HEADER_LEN as usize];
        file.seek(SeekFrom::Start(0))?;
        read_exact(file, &mut bytes)?;
        if &bytes[..8] != MAGIC
            || bytes[8..12] != VERSION.to_le_bytes()
            || crc32c(&bytes[..52]) != u32::from_le_bytes(bytes[52..56].try_into().unwrap())
        {
            return Err(Error::Corrupt("invalid segment header"));
        }
        Ok(Self {
            identity: Identity {
                store_id: bytes[12..28].try_into().unwrap(),
                log_epoch: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
            },
            lease: u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
            seq: u64::from_le_bytes(bytes[44..52].try_into().unwrap()),
            crc: u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
        })
    }
}

pub(super) fn frame<T: Serialize>(kind: u8, value: &T) -> Result<Vec<u8>> {
    let payload = postcard::to_allocvec(value).map_err(|_| Error::Corrupt("postcard encoding"))?;
    let len = payload.len() + 1;
    if len > MAX_RECORD {
        return Err(Error::Corrupt("record too large"));
    }
    let mut covered = (len as u32).to_le_bytes().to_vec();
    covered.push(kind);
    covered.extend_from_slice(&payload);
    let crc = crc32c(&covered);
    let mut bytes = covered[..4].to_vec();
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes.extend_from_slice(&covered[4..]);
    Ok(bytes)
}

pub(super) struct Frame {
    pub kind: u8,
    pub payload: Vec<u8>,
    pub crc: u32,
    pub end: u64,
}

fn read_exact(file: &mut File, bytes: &mut [u8]) -> Result<()> {
    match file.read_exact(bytes) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::Corrupt("torn record")),
        result => result.map_err(Error::from),
    }
}

pub(super) fn read_frame(file: &mut File) -> Result<Option<Frame>> {
    let start = file.stream_position()?;
    let mut len_bytes = [0u8; 4];
    if file.read(&mut len_bytes[..1])? == 0 {
        return Ok(None);
    }
    read_exact(file, &mut len_bytes[1..])?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len == 0 || len > MAX_RECORD {
        return Err(Error::Corrupt("invalid record length"));
    }
    let mut crc = [0u8; 4];
    read_exact(file, &mut crc)?;
    let mut data = vec![0; len];
    read_exact(file, &mut data)?;
    let mut covered = len_bytes.to_vec();
    covered.extend_from_slice(&data);
    let crc = u32::from_le_bytes(crc);
    if crc32c(&covered) != crc {
        return Err(Error::Corrupt("record CRC"));
    }
    Ok(Some(Frame {
        kind: data[0],
        payload: data[1..].to_vec(),
        crc,
        end: start + 8 + len as u64,
    }))
}

pub(super) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, rest) =
        postcard::take_from_bytes(bytes).map_err(|_| Error::Corrupt("invalid postcard payload"))?;
    if !rest.is_empty() {
        return Err(Error::Corrupt("trailing postcard bytes"));
    }
    Ok(value)
}
