//! The Windows confinement report has its own tag and five ordered booleans.
//! It is encoded on every platform so journals and protocol fixtures stay portable.

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::types::Confinement;

pub(super) const TAG: u8 = 3;

pub(super) fn encode(
    enc: &mut Encoder,
    lpac: bool,
    untrusted: bool,
    no_thread_token: bool,
    mitigations: bool,
    handle_table: bool,
) {
    enc.u8(TAG);
    enc.u8(u8::from(lpac));
    enc.u8(u8::from(untrusted));
    enc.u8(u8::from(no_thread_token));
    enc.u8(u8::from(mitigations));
    enc.u8(u8::from(handle_table));
}

pub(super) fn decode(dec: &mut Decoder<'_>) -> Result<Confinement, DecodeError> {
    Ok(Confinement::Windows {
        lpac: dec.bool("lpac")?,
        untrusted: dec.bool("untrusted")?,
        no_thread_token: dec.bool("no_thread_token")?,
        mitigations: dec.bool("mitigations")?,
        handle_table: dec.bool("handle_table")?,
    })
}
