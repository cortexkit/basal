//! Work, wait, timer and legacy callbacks block on a release event while the
//! test harness enumerates the worker's threads and queries their tokens. Holding
//! callbacks alive makes their TIDs observable in both thread snapshots.
#![allow(unsafe_op_in_unsafe_fn)]
use super::{Handle, byte, last, marker};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Threading::*;

struct Context {
    entered: AtomicUsize,
    departed: AtomicUsize,
    ready: HANDLE,
    release: HANDLE,
    done: HANDLE,
    records: Mutex<Vec<(&'static str, u32)>>,
}
unsafe fn record(param: *mut c_void, kind: &'static str) {
    let context = &*param.cast::<Context>();
    context
        .records
        .lock()
        .unwrap()
        .push((kind, GetCurrentThreadId()));
    if context.entered.fetch_add(1, Ordering::SeqCst) + 1 == 4 {
        SetEvent(context.ready);
    }
    WaitForSingleObject(context.release, INFINITE);
    if context.departed.fetch_add(1, Ordering::SeqCst) + 1 == 4 {
        SetEvent(context.done);
    }
}
unsafe extern "system" fn work(_: PTP_CALLBACK_INSTANCE, param: *mut c_void, _: PTP_WORK) {
    record(param, "work");
}
unsafe extern "system" fn wait(_: PTP_CALLBACK_INSTANCE, param: *mut c_void, _: PTP_WAIT, _: u32) {
    record(param, "wait");
}
unsafe extern "system" fn timer(_: PTP_CALLBACK_INSTANCE, param: *mut c_void, _: PTP_TIMER) {
    record(param, "timer");
}
unsafe extern "system" fn legacy(param: *mut c_void) -> u32 {
    record(param, "legacy");
    0
}

pub(super) unsafe fn run(barrier: bool) -> Result<(), String> {
    let ready = Handle(CreateEventW(null(), 1, 0, null()));
    let release = Handle(CreateEventW(null(), 1, 0, null()));
    let done = Handle(CreateEventW(null(), 1, 0, null()));
    let trigger = Handle(CreateEventW(null(), 1, 1, null()));
    if [ready.0, release.0, done.0, trigger.0]
        .iter()
        .any(|h| h.is_null())
    {
        return Err(format!("CreateEventW: {}", last()));
    }
    let context = Box::new(Context {
        entered: AtomicUsize::new(0),
        departed: AtomicUsize::new(0),
        ready: ready.0,
        release: release.0,
        done: done.0,
        records: Mutex::new(Vec::new()),
    });
    let param = (&*context as *const Context).cast_mut().cast();
    let work = CreateThreadpoolWork(Some(work), param, null());
    let wait = CreateThreadpoolWait(Some(wait), param, null());
    let timer = CreateThreadpoolTimer(Some(timer), param, null());
    if work == 0 || wait == 0 || timer == 0 {
        let error = last();
        if work != 0 {
            CloseThreadpoolWork(work);
        }
        if wait != 0 {
            CloseThreadpoolWait(wait);
        }
        if timer != 0 {
            CloseThreadpoolTimer(timer);
        }
        return Err(format!("CreateThreadpool work/wait/timer: {error}"));
    }
    SubmitThreadpoolWork(work);
    SetThreadpoolWait(wait, trigger.0, null());
    let relative = -10_000i64 as u64;
    let due = FILETIME {
        dwLowDateTime: relative as u32,
        dwHighDateTime: (relative >> 32) as u32,
    };
    SetThreadpoolTimer(timer, &due, 0, 0);
    let queued = QueueUserWorkItem(Some(legacy), param, 0) != 0;
    let queued_error = last();
    let held = queued && WaitForSingleObject(ready.0, 30_000) == WAIT_OBJECT_0;
    let checkpoint = (|| {
        if !held {
            return Err(format!(
                "four callbacks not ready (legacy queued={queued}, error={queued_error})"
            ));
        }
        if context.entered.load(Ordering::SeqCst) != 4
            || context.departed.load(Ordering::SeqCst) != 0
        {
            return Err("callbacks were not held".into());
        }
        for (kind, tid) in context.records.lock().unwrap().iter() {
            eprintln!("windows-pool-callback:{kind}:{tid}");
        }
        if barrier {
            marker("callbacks-held")?;
            byte()?;
        }
        Ok(())
    })();
    SetEvent(release.0);
    SetThreadpoolWait(wait, null_mut(), null());
    SetThreadpoolTimer(timer, null(), 0, 0);
    WaitForThreadpoolWorkCallbacks(work, 1);
    WaitForThreadpoolWaitCallbacks(wait, 1);
    WaitForThreadpoolTimerCallbacks(timer, 1);
    CloseThreadpoolWork(work);
    CloseThreadpoolWait(wait);
    CloseThreadpoolTimer(timer);
    if queued && WaitForSingleObject(done.0, 30_000) != WAIT_OBJECT_0 {
        // Legacy work has no cancellation API. Its storage and signaling handles
        // must outlive a late callback; the process exits on this error.
        std::mem::forget(context);
        std::mem::forget(ready);
        std::mem::forget(release);
        std::mem::forget(done);
        return Err("pool callbacks did not drain".into());
    }
    checkpoint?;
    if context.departed.load(Ordering::SeqCst) != 4 {
        return Err("pool did not complete all four callbacks".into());
    }
    eprintln!("windows-pool-complete:work,wait,timer,legacy");
    Ok(())
}
