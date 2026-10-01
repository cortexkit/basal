//! Freezing every intrinsic a script can reach.
//!
//! After the prelude has rebuilt the global environment, the worker freezes
//! every object reachable from the global object and from a few intrinsics
//! that only syntax exposes, following prototypes and property values,
//! getters and setters included. Walking this graph in JavaScript costs
//! several milliseconds per activation (thousands of descriptor objects in
//! an interpreter); walking it through the engine's C API costs a fraction
//! of that, so the walk lives here.

use std::collections::HashSet;
use std::mem::MaybeUninit;

use rquickjs::{Ctx, Value, qjs};

/// Takes ownership of a raw value returned by the engine with a reference
/// the caller must release.
///
/// # Safety
/// `raw` must be a live value of `ctx`'s runtime that the caller owns.
unsafe fn owned<'js>(ctx: &Ctx<'js>, raw: qjs::JSValue) -> Value<'js> {
    // SAFETY: guaranteed by the caller; `Value` releases the reference on
    // drop.
    unsafe { Value::from_raw(ctx.clone(), raw) }
}

/// Freezes everything reachable from `roots`. Returns how many objects it
/// froze.
pub fn harden<'js>(ctx: &Ctx<'js>, roots: Vec<Value<'js>>) -> rquickjs::Result<usize> {
    let raw_ctx = ctx.as_raw().as_ptr();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut work = roots;
    while let Some(value) = work.pop() {
        let raw = value.as_raw();
        if raw.tag != qjs::JS_TAG_OBJECT as i64 {
            continue;
        }
        // SAFETY: an object-tagged value stores its object pointer in `ptr`;
        // the pointer is only used as an identity key.
        let identity = unsafe { raw.u.ptr } as usize;
        if !seen.insert(identity) {
            continue;
        }
        // SAFETY: `raw` belongs to `value`, which stays alive for this whole
        // iteration; every value the calls below return is either owned by a
        // `Value` (and released on drop) or released explicitly.
        unsafe {
            if qjs::JS_FreezeObject(raw_ctx, raw) < 0 {
                return Err(rquickjs::Error::Exception);
            }
            let proto = qjs::JS_GetPrototype(raw_ctx, raw);
            if proto.tag == qjs::JS_TAG_EXCEPTION as i64 {
                return Err(rquickjs::Error::Exception);
            }
            work.push(owned(ctx, proto));

            let mut table: *mut qjs::JSPropertyEnum = std::ptr::null_mut();
            let mut len: u32 = 0;
            let flags = (qjs::JS_GPN_STRING_MASK | qjs::JS_GPN_SYMBOL_MASK) as i32;
            if qjs::JS_GetOwnPropertyNames(raw_ctx, &mut table, &mut len, raw, flags) < 0 {
                return Err(rquickjs::Error::Exception);
            }
            let mut failed = false;
            if !table.is_null() {
                for entry in std::slice::from_raw_parts(table, len as usize) {
                    let mut desc = MaybeUninit::<qjs::JSPropertyDescriptor>::uninit();
                    match qjs::JS_GetOwnProperty(raw_ctx, desc.as_mut_ptr(), raw, entry.atom) {
                        1 => {
                            // A found property fills every field and hands
                            // the caller one reference to each.
                            let desc = desc.assume_init();
                            work.push(owned(ctx, desc.value));
                            work.push(owned(ctx, desc.getter));
                            work.push(owned(ctx, desc.setter));
                        }
                        0 => {}
                        _ => {
                            failed = true;
                            break;
                        }
                    }
                }
            }
            qjs::JS_FreePropertyEnum(raw_ctx, table, len);
            if failed {
                return Err(rquickjs::Error::Exception);
            }
        }
    }
    Ok(seen.len())
}
