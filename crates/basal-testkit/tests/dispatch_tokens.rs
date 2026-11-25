//! Historical token-window reads after new grants are refused.

mod common;

use basal_core::tokens::window_start;
use basal_core::{Clock, Config};
use basal_testkit::harness::{World, test_manifest};
use common::config;
use serde_json::json;

fn manual(at_ms: i64) -> (Config, Clock) {
    let clock = Clock::manual(at_ms);
    (
        Config {
            clock: clock.clone(),
            ..config()
        },
        clock,
    )
}

/// An hour boundary in 2027, plus ten minutes.
const T0: i64 = 1_798_761_600_000 + 600_000;

struct StatementTrace<'a> {
    store: &'a basal_core::Store,
    count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl<'a> StatementTrace<'a> {
    fn start(store: &'a basal_core::Store) -> Self {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        unsafe extern "C" fn record(
            event: u32,
            context: *mut std::ffi::c_void,
            _: *mut std::ffi::c_void,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            if event == rusqlite::ffi::SQLITE_TRACE_STMT as u32 {
                // SAFETY: start registers the Arc's stable allocation; Drop
                // removes the callback before that allocation is released.
                let count = unsafe { &*context.cast::<std::sync::atomic::AtomicUsize>() };
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            0
        }
        store
            .read(|conn| {
                // SAFETY: Store::read holds the connection's lock throughout this
                // call. StatementTrace retains the Arc allocation passed as the
                // callback context, and unregisters it before dropping that Arc.
                let status = unsafe {
                    rusqlite::ffi::sqlite3_trace_v2(
                        conn.handle(),
                        rusqlite::ffi::SQLITE_TRACE_STMT as u32,
                        Some(record),
                        std::sync::Arc::as_ptr(&count).cast_mut().cast(),
                    )
                };
                assert_eq!(status, rusqlite::ffi::SQLITE_OK);
                Ok(())
            })
            .unwrap();
        Self { store, count }
    }
}

impl Drop for StatementTrace<'_> {
    fn drop(&mut self) {
        self.store
            .read(|conn| {
                // SAFETY: Store::read locks the connection that registered record,
                // excluding any concurrent callback while tracing is disabled.
                unsafe {
                    rusqlite::ffi::sqlite3_trace_v2(conn.handle(), 0, None, std::ptr::null_mut())
                };
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn batched_token_usage_keeps_current_windows_and_omits_unapproved_flows() {
    let world = World::new("tokens-batch");
    let (config, _clock) = manual(T0);
    let rt = world
        .runtime(std::sync::Arc::new(basal_core::NoHooks), config)
        .unwrap();
    for (id, window) in [
        ("hour", Some("1h")),
        ("day", Some("1d")),
        ("no-model", None),
        ("pending", Some("1h")),
    ] {
        let mut m = test_manifest();
        m["id"] = json!(id);
        world.spec_with(&rt, "return 1", &m).unwrap();
        if let Some(window) = window {
            // Seed a grant from an older approval without submitting it to intake.
            m["llm"] =
                json!({"iq":0,"token_cap":{"tokens":10_000,"window":window},"max_output":100});
            rt.store()
                .write(|tx| {
                    tx.execute(
                        "UPDATE installs SET manifest=?2 WHERE flow_id=?1",
                        rusqlite::params![id, m.to_string()],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
    }
    let hour = 3_600_000;
    let start = window_start(T0, hour);
    rt.store().write(|tx| {
        tx.execute("UPDATE flows SET approved_version=NULL WHERE flow_id='pending'", [])?;
        for (window_start, input) in [(start, 7), (start-hour, 999)] {
            tx.execute(
                "INSERT INTO token_windows (flow_id,window_ms,window_start,reserved,input_tokens,cache_write_tokens,output_tokens,cached_input_tokens,unreported_tokens) VALUES ('hour',?1,?2,3,?3,11,13,17,19)",
                rusqlite::params![hour, window_start, input],
            )?;
        }
        Ok(())
    }).unwrap();
    let trace = StatementTrace::start(rt.store());
    let got = rt
        .token_usage_many(
            &["hour", "day", "no-model", "pending", "missing", "hour"],
            T0,
        )
        .unwrap();
    let statements = trace.count.load(std::sync::atomic::Ordering::SeqCst);
    drop(trace);
    assert_eq!(
        statements, 1,
        "the batch must execute one SQL statement, not one per flow"
    );
    assert_eq!(
        got.keys().map(String::as_str).collect::<Vec<_>>(),
        ["day", "hour"]
    );
    let usage = &got["hour"];
    assert_eq!(
        (usage.window_ms, usage.window_start, usage.reserved),
        (hour, start, 3)
    );
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_write_tokens,
            usage.output_tokens,
            usage.cached_input_tokens,
            usage.unreported_tokens
        ),
        (7, 11, 13, 17, 19)
    );
    assert_eq!(got["day"].counted(), 0);
    assert_eq!(got["day"].reserved, 0);
    assert_eq!(got["day"].window_ms, 86_400_000);
    for id in ["hour", "day"] {
        assert_eq!(Some(&got[id]), rt.token_usage(id, T0).unwrap().as_ref());
    }
    assert!(rt.token_usage_many(&[], T0).unwrap().is_empty());
    assert_eq!(
        rt.token_usage_many(&["hour"], start - hour).unwrap()["hour"].input_tokens,
        999
    );
}
