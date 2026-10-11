//! At worker-requested barriers the test harness independently enumerates
//! threads through two APIs, then queries every thread's impersonation token.
//! Checking the complete live thread set rules out credentials on pool threads.
#![allow(unsafe_op_in_unsafe_fn)]
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        bytes: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtOpenThreadTokenEx(
        thread: HANDLE,
        access: u32,
        open_as_self: u8,
        attributes: u32,
        token: *mut HANDLE,
    ) -> i32;
}

unsafe fn toolhelp(pid: u32) -> BTreeSet<u32> {
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    assert_ne!(
        snapshot,
        INVALID_HANDLE_VALUE,
        "snapshot: {}",
        GetLastError()
    );
    let snapshot = super::Handle(snapshot);
    let mut entry: THREADENTRY32 = zeroed();
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    assert_ne!(
        Thread32First(snapshot.0, &mut entry),
        0,
        "first thread: {}",
        GetLastError()
    );
    let mut tids = BTreeSet::new();
    loop {
        if entry.th32OwnerProcessID == pid {
            assert!(tids.insert(entry.th32ThreadID));
        }
        entry.dwSize = size_of::<THREADENTRY32>() as u32;
        if Thread32Next(snapshot.0, &mut entry) == 0 {
            assert_eq!(GetLastError(), ERROR_NO_MORE_FILES);
            break;
        }
    }
    tids
}

/// Offsets and record sizes are the x64 SystemProcessInformation ABI. Validate
/// every record bound before reading its thread array; an error is not an empty set.
unsafe fn system(pid: u32) -> BTreeSet<u32> {
    assert_eq!(size_of::<usize>(), 8);
    let mut bytes = 2 * 1024 * 1024;
    loop {
        let mut buffer = vec![0usize; bytes / 8];
        let mut returned = 0;
        let status =
            NtQuerySystemInformation(5, buffer.as_mut_ptr().cast(), bytes as u32, &mut returned);
        if status as u32 == 0xc0000004 && bytes < 32 * 1024 * 1024 {
            bytes = (bytes * 2)
                .max(returned as usize + 8192)
                .next_multiple_of(8);
            continue;
        }
        assert_eq!(status, 0, "SystemProcessInformation: {:#x}", status as u32);
        let end = returned as usize;
        assert!(end <= bytes);
        let base = buffer.as_ptr().cast::<u8>();
        let mut offset = 0;
        while offset + 256 <= end {
            let next = base.add(offset).cast::<u32>().read_unaligned() as usize;
            let count = base.add(offset + 4).cast::<u32>().read_unaligned() as usize;
            let record_end = if next == 0 {
                end
            } else {
                offset.checked_add(next).unwrap()
            };
            assert!(record_end <= end && record_end >= offset + 256);
            if base.add(offset + 80).cast::<usize>().read_unaligned() == pid as usize {
                assert!(count <= (record_end - offset - 256) / 80);
                let mut tids = BTreeSet::new();
                for index in 0..count {
                    let thread = base.add(offset + 256 + index * 80);
                    assert_eq!(
                        thread.add(40).cast::<usize>().read_unaligned(),
                        pid as usize
                    );
                    let tid = thread.add(48).cast::<usize>().read_unaligned();
                    assert!(tid != 0 && tid <= u32::MAX as usize);
                    assert!(tids.insert(tid as u32));
                }
                return tids;
            }
            if next == 0 {
                break;
            }
            offset = record_end;
        }
        panic!("worker PID {pid} missing from SystemProcessInformation");
    }
}

pub fn assert_all_threads(pid: u32, stage: &str) -> BTreeSet<u32> {
    unsafe {
        let nt = system(pid);
        let toolhelp = toolhelp(pid);
        assert!(!nt.is_empty(), "{stage}: no threads");
        assert_eq!(nt, toolhelp, "{stage}: enumerations disagree");
        for tid in &nt {
            let thread = OpenThread(THREAD_QUERY_INFORMATION, 0, *tid);
            assert!(
                !thread.is_null(),
                "{stage}: OpenThread({tid}): {}",
                GetLastError()
            );
            let thread = super::Handle(thread);
            for open_as_self in [0, 1] {
                let mut token = null_mut();
                let status =
                    NtOpenThreadTokenEx(thread.0, TOKEN_QUERY, open_as_self, 0, &mut token);
                if status >= 0 {
                    drop(super::Handle(token));
                }
                assert_eq!(
                    status as u32, 0xc000007c,
                    "{stage}: TID {tid}, open_as_self={open_as_self}"
                );
                println!("{stage}: TID {tid} open_as_self={open_as_self} 0xc000007c");
            }
        }
        println!(
            "{stage}: SystemProcessInformation == Toolhelp ({} threads)",
            nt.len()
        );
        nt
    }
}
