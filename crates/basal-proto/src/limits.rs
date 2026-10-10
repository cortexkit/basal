//! Protocol constants and hard size limits. The limits bound what either
//! side will read before it allocates, independent of any activation budget.

/// The current protocol version, exchanged in the handshake.
pub const PROTOCOL_VERSION: u32 = 5;

/// Linux codemode process ceiling, including the 64 MiB JS heap, native
/// stacks, executable mappings and allocator overhead.
pub const CODEMODE_ADDRESS_SPACE_BYTES: u64 = 512 * 1024 * 1024;

/// The largest flow memory budget QuickJS will accept.
pub const MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The JavaScript heap budget allocated to a codemode activation.
pub const CODEMODE_HEAP_BYTES: u64 = 64 * 1024 * 1024;

/// Windows job object commit limit for codemode workers.
pub const CODEMODE_JOB_COMMIT_BYTES: u64 = CODEMODE_ADDRESS_SPACE_BYTES;

/// Windows job object commit limit for flow workers, allowing for the maximum
/// memory budget plus the non-heap baseline overhead.
pub const FLOW_JOB_COMMIT_BYTES: u64 =
    MAX_MEMORY_BYTES + (CODEMODE_ADDRESS_SPACE_BYTES - CODEMODE_HEAP_BYTES);

/// The highest Landlock ABI supported by the worker's pinned `landlock`
/// crate, version `=0.4.7`. Update this together with the worker's crate pin
/// so both sides agree on the ABI that must be applied.
pub const LANDLOCK_ABI: u32 = 9;

/// The largest aggregate frame payload either side accepts. A run may have
/// 10,000 calls; their combined outcomes, script and metadata must still fit
/// this bound, independently of the per-value and protocol list limits.
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

/// A prefix within a byte cap without splitting a UTF-8 character.
pub fn utf8_prefix(text: &str, cap: usize) -> &str {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
