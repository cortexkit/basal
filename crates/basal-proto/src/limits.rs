//! Hard size limits. They bound what either side will read before it
//! allocates, independent of any per-activation budget.

/// The current protocol version, exchanged in the handshake.
pub const PROTOCOL_VERSION: u32 = 2;

/// The largest frame payload either side accepts. A 1000-call prefix of
/// 16 KiB values is about 16 MiB, so this leaves room for twice that.
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// The largest single JSON value (an argument, an outcome, a trigger or a
/// result).
pub const MAX_VALUE_BYTES: usize = 1024 * 1024;

/// The largest script source.
pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

/// The largest module or op name.
pub const MAX_NAME_BYTES: usize = 128;

/// The largest human-readable detail string (error messages and the like).
pub const MAX_DETAIL_BYTES: usize = 16 * 1024;

/// The most entries in any list of positions or recorded calls.
pub const MAX_LIST_ENTRIES: usize = 1 << 20;
