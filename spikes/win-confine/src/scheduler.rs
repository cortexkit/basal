//! Native operations and capability analysis for SchedulerSharedData handles on Windows 11.
#![allow(unsafe_op_in_unsafe_fn)]
use crate::native::*;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Threading::*;

const PAGE_READONLY: u32 = 0x02;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQueryInformationThread(
        thread: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtSetInformationThread(thread: HANDLE, class: u32, buffer: *mut c_void, length: u32) -> i32;
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;

    fn NtUnmapViewOfSection(process: HANDLE, base: *mut c_void) -> i32;
    fn NtMakeTemporaryObject(handle: HANDLE) -> i32;
    fn NtSetSecurityObject(handle: HANDLE, info: u32, desc: *mut c_void) -> i32;
    fn NtQuerySecurityObject(
        handle: HANDLE,
        info: u32,
        desc: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;

    fn NtDuplicateObject(
        source_process: HANDLE,
        source_handle: HANDLE,
        target_process: HANDLE,
        target_handle: *mut HANDLE,
        desired_access: u32,
        attributes: u32,
        options: u32,
    ) -> i32;

    fn NtMapViewOfSection(
        section: HANDLE,
        process: HANDLE,
        base_address: *mut *mut c_void,
        zero_bits: usize,
        commit_size: usize,
        section_offset: *mut i64,
        view_size: *mut usize,
        inherit_disposition: u32,
        allocation_type: u32,
        protect: u32,
    ) -> i32;

    fn NtOpenFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *mut c_void,
        io_status: *mut c_void,
        share_access: u32,
        open_options: u32,
    ) -> i32;

    fn NtAlpcSendWaitReceivePort(
        port_handle: HANDLE,
        flags: u32,
        send_msg: *mut c_void,
        send_attr: *mut c_void,
        recv_msg: *mut c_void,
        recv_attr: *mut c_void,
        timeout: *mut i64,
    ) -> i32;
}

unsafe extern "system" {
    fn DeviceIoControl(
        hdevice: HANDLE,
        dwiocontrolcode: u32,
        lpinbuffer: *const c_void,
        ninbuffersize: u32,
        lpoutbuffer: *mut c_void,
        noutbuffersize: u32,
        lpbytesreturned: *mut u32,
        lpoverlapped: *mut c_void,
    ) -> i32;
}

#[repr(C)]
struct SchedulerSlotInfo {
    action: u32,
    handle: HANDLE,
    slot: *mut c_void,
}

#[repr(C)]
struct ObjectBasicInfo {
    attributes: u32,
    granted_access: u32,
    handle_count: u32,
    pointer_count: u32,
    paged_pool: u32,
    non_paged_pool: u32,
    reserved: [u32; 3],
    name_info_size: u32,
    type_info_size: u32,
    security_size: u32,
    create_time: [u32; 2],
}

#[repr(C)]
struct ObjectTypeInfoLocal {
    name: UnicodeString,
    counters: [u32; 13],
    mapping: [u32; 4],
    access: u32,
    security: u8,
    maintain: u8,
    index: u8,
    reserved: u8,
    pool: [u32; 3],
}

unsafe fn duplicate_details(handle: HANDLE, status: i32) -> Value {
    let mut result =
        json!({"status":hex(status as u32),"success":status == 0 && !handle.is_null()});
    if status != 0 || handle.is_null() {
        return result;
    }
    let mut basic: ObjectBasicInfo = zeroed();
    let mut returned = 0;
    let query = NtQueryObject(
        handle,
        0,
        (&mut basic as *mut ObjectBasicInfo).cast(),
        size_of::<ObjectBasicInfo>() as u32,
        &mut returned,
    );
    result["basic_query_status"] = json!(hex(query as u32));
    if query == 0 {
        result["granted_access"] = json!(hex(basic.granted_access));
    }
    let mut security = vec![0u8; 4096];
    let read = NtQuerySecurityObject(
        handle,
        7,
        security.as_mut_ptr().cast(),
        security.len() as u32,
        &mut returned,
    );
    if read as u32 == 0xc0000023 {
        security.resize(returned as usize, 0);
    }
    let read = if read as u32 == 0xc0000023 {
        NtQuerySecurityObject(
            handle,
            7,
            security.as_mut_ptr().cast(),
            security.len() as u32,
            &mut returned,
        )
    } else {
        read
    };
    result["security_read_status"] = json!(hex(read as u32));
    if read == 0 {
        // Reapply the same descriptor: measure security-write authority without
        // making the scheduler object more accessible to any other principal.
        let write = NtSetSecurityObject(handle, 4, security.as_mut_ptr().cast());
        result["same_dacl_write_status"] = json!(hex(write as u32));
        let owner = NtSetSecurityObject(handle, 1, security.as_mut_ptr().cast());
        result["same_owner_write_status"] = json!(hex(owner as u32));
    }
    result["make_temporary_status"] = json!(hex(NtMakeTemporaryObject(handle) as u32));
    result
}

/// Probe the finite native interfaces relevant to the measured scheduler grant.
pub unsafe fn probe_scheduler_shared_data_handle(h: HANDLE, granted: u32) -> Value {
    let mut operations = json!({});

    // 1. NtQueryObject (ObjectBasicInformation = 0)
    let mut basic: ObjectBasicInfo = zeroed();
    let mut ret_len = 0u32;
    let basic_st = NtQueryObject(
        h,
        0,
        (&mut basic as *mut ObjectBasicInfo).cast(),
        size_of::<ObjectBasicInfo>() as u32,
        &mut ret_len,
    );
    operations["query_object_basic"] = if basic_st == 0 {
        json!({
            "status": "0x00000000",
            "attributes": hex(basic.attributes),
            "granted_access": hex(basic.granted_access),
            "handle_count": basic.handle_count,
            "pointer_count": basic.pointer_count,
        })
    } else {
        json!({"status": hex(basic_st as u32)})
    };

    // 2. NtQueryObject (ObjectTypeInformation = 2)
    let mut type_buf = vec![0u8; 4096];
    let type_st = NtQueryObject(
        h,
        2,
        type_buf.as_mut_ptr().cast(),
        type_buf.len() as u32,
        &mut ret_len,
    );
    operations["query_object_type"] = if type_st == 0 {
        let info = &*type_buf.as_ptr().cast::<ObjectTypeInfoLocal>();
        let name_str = if !info.name.buffer.is_null() && info.name.length > 0 {
            let slice =
                std::slice::from_raw_parts(info.name.buffer, (info.name.length / 2) as usize);
            String::from_utf16_lossy(slice)
        } else {
            String::new()
        };
        json!({
            "status": "0x00000000",
            "type_name": name_str,
            "valid_access_mask": hex(info.access),
            "generic_mapping": {
                "read": hex(info.mapping[0]),
                "write": hex(info.mapping[1]),
                "execute": hex(info.mapping[2]),
                "all": hex(info.mapping[3]),
            },
            "total_objects": info.counters[0],
            "total_handles": info.counters[1],
            "security_required": info.security != 0,
            "maintain_handle_count": info.maintain != 0,
            "type_index": info.index,
        })
    } else {
        json!({"status": hex(type_st as u32)})
    };

    // 3. NtQueryObject (ObjectNameInformation = 1)
    let mut name_buf = vec![0u8; 1024];
    let name_st = NtQueryObject(
        h,
        1,
        name_buf.as_mut_ptr().cast(),
        name_buf.len() as u32,
        &mut ret_len,
    );
    operations["query_object_name"] = if name_st == 0 {
        let us = &*name_buf.as_ptr().cast::<UnicodeString>();
        let s = if !us.buffer.is_null() && us.length > 0 {
            let slice = std::slice::from_raw_parts(us.buffer, (us.length / 2) as usize);
            String::from_utf16_lossy(slice)
        } else {
            String::new()
        };
        json!({
            "status": "0x00000000",
            "name": s,
            "is_unnamed": s.is_empty(),
        })
    } else {
        json!({"status": hex(name_st as u32)})
    };

    // 4. NtQuerySecurityObject (attempting to read security descriptor)
    let mut sec_buf = vec![0u8; 512];
    let sec_st = NtQuerySecurityObject(
        h,
        7, // OWNER | GROUP | DACL_SECURITY_INFORMATION
        sec_buf.as_mut_ptr().cast(),
        sec_buf.len() as u32,
        &mut ret_len,
    );
    operations["query_security_object"] = json!({
        "status": hex(sec_st as u32),
        "permitted": sec_st == 0,
        "returned_bytes": ret_len,
    });

    // 5. NtDuplicateObject
    let mut dup_same = null_mut();
    let dup_same_st = NtDuplicateObject(
        GetCurrentProcess(),
        h,
        GetCurrentProcess(),
        &mut dup_same,
        0,
        0,
        2, // DUPLICATE_SAME_ACCESS
    );
    let dup_same_ok = dup_same_st == 0 && !dup_same.is_null();
    let same_basic = duplicate_details(dup_same, dup_same_st);
    if dup_same_ok {
        CloseHandle(dup_same);
    }

    let mut dup_all = null_mut();
    let dup_all_st = NtDuplicateObject(
        GetCurrentProcess(),
        h,
        GetCurrentProcess(),
        &mut dup_all,
        0x10000000, // GENERIC_ALL
        0,
        0,
    );
    let all_basic = duplicate_details(dup_all, dup_all_st);
    if dup_all_st == 0 && !dup_all.is_null() {
        CloseHandle(dup_all);
    }
    operations["duplicate_object"] = json!({"same_access":same_basic,"generic_all":all_basic});
    let mut access_trials = Vec::new();
    for access in [
        0x80000000, 0x40000000, 0x20000000, 2, 0x02000000, 0x000e0000, 0x00100000,
    ] {
        let mut duplicate = null_mut();
        let status = NtDuplicateObject(
            GetCurrentProcess(),
            h,
            GetCurrentProcess(),
            &mut duplicate,
            access,
            0,
            0,
        );
        let mut details = duplicate_details(duplicate, status);
        details["requested_access"] = json!(hex(access));
        access_trials.push(details);
        if status == 0 && !duplicate.is_null() {
            CloseHandle(duplicate);
        }
    }
    operations["duplicate_access_trials"] = json!(access_trials);

    // 6. WaitForSingleObject (synchronization check)
    SetLastError(0);
    let wait_res = WaitForSingleObject(h, 0);
    let wait_err = GetLastError();
    operations["wait_for_single_object"] = json!({
        "result": wait_res,
        "error": if wait_res == WAIT_FAILED { wait_err } else { 0 },
        "permitted": wait_res != WAIT_FAILED,
    });

    // 7. File/Device I/O operations
    let mut io_buf = [0u8; 64];
    let mut bytes_rw = 0u32;
    SetLastError(0);
    let read_ok = ReadFile(
        h,
        io_buf.as_mut_ptr(),
        io_buf.len() as u32,
        &mut bytes_rw,
        null_mut(),
    ) != 0;
    let read_err = GetLastError();

    SetLastError(0);
    let write_ok = WriteFile(
        h,
        io_buf.as_ptr(),
        io_buf.len() as u32,
        &mut bytes_rw,
        null_mut(),
    ) != 0;
    let write_err = GetLastError();

    SetLastError(0);
    let ioctl_ok = DeviceIoControl(
        h,
        0x00220000,
        null(),
        0,
        null_mut(),
        0,
        &mut bytes_rw,
        null_mut(),
    ) != 0;
    let ioctl_err = GetLastError();

    operations["file_io"] = json!({
        "read_file": {"success": read_ok, "error": read_err},
        "write_file": {"success": write_ok, "error": write_err},
        "device_io_control": {"success": ioctl_ok, "error": ioctl_err},
        "permitted": read_ok || write_ok || ioctl_ok,
    });

    // 8. Section mapping check
    let mut base_addr = null_mut();
    let mut view_sz = 0usize;
    let mut sec_off = 0i64;
    let map_st = NtMapViewOfSection(
        h,
        GetCurrentProcess(),
        &mut base_addr,
        0,
        4096,
        &mut sec_off,
        &mut view_sz,
        1,
        0,
        PAGE_READONLY,
    );
    operations["section_map"] = json!({
        "status": hex(map_st as u32),
        "permitted": map_st == 0,
    });
    if map_st == 0 {
        NtUnmapViewOfSection(GetCurrentProcess(), base_addr);
    }

    // 9. RootDirectory escape test (can it be used to open filesystem files or devices?)
    let mut file_test_h = null_mut();
    let mut iosb = [0usize; 2];
    let mut test_name = wide("test");
    let mut us_name = UnicodeString {
        length: 8,
        maximum_length: 10,
        buffer: test_name.as_mut_ptr(),
    };
    #[repr(C)]
    struct ObjAttrs {
        len: u32,
        root_dir: HANDLE,
        name: *mut UnicodeString,
        attrs: u32,
        sd: *mut c_void,
        sqos: *mut c_void,
    }
    let mut oa_test = ObjAttrs {
        len: size_of::<ObjAttrs>() as u32,
        root_dir: h,
        name: &mut us_name,
        attrs: 0x40, // OBJ_CASE_INSENSITIVE
        sd: null_mut(),
        sqos: null_mut(),
    };
    let open_st = NtOpenFile(
        &mut file_test_h,
        FILE_READ_DATA,
        (&mut oa_test as *mut ObjAttrs).cast(),
        iosb.as_mut_ptr().cast(),
        FILE_SHARE_READ,
        0,
    );
    if open_st == 0 && !file_test_h.is_null() {
        CloseHandle(file_test_h);
    }
    operations["root_directory_escape"] = json!({
        "status": hex(open_st as u32),
        "permitted": open_st == 0,

    });

    // 10. ALPC IPC escape test (can it be used as an ALPC communication port?)
    let mut timeout = 0i64;
    let alpc_st = NtAlpcSendWaitReceivePort(
        h,
        0,
        null_mut(),
        null_mut(),
        null_mut(),
        null_mut(),
        &mut timeout,
    );
    operations["alpc_port_escape"] = json!({
        "status": hex(alpc_st as u32),
        "permitted": alpc_st == 0,

    });

    // Process class 112 takes only an eight-byte output handle. The hardware
    // syscall observer in connections.rs records the loader passing length 8 and
    // receiving the created handle in that buffer, unlike thread class 57 below.
    let mut process_query_handle = h;
    let query_status = NtQueryInformationProcess(
        GetCurrentProcess(),
        112,
        (&mut process_query_handle as *mut HANDLE).cast(),
        size_of::<HANDLE>() as u32,
        &mut ret_len,
    );
    operations["process_scheduler_shared_data_query_112"] = json!({"buffer_bytes":size_of::<HANDLE>(),"status":hex(query_status as u32),"output_handle":process_query_handle as usize});

    // Assign/free/query actions belong to THREAD class 57, not process class 112.
    // Slot is recorded as returned; no pointed-to bytes are read or overwritten.
    let mut slot_operations = Vec::new();
    for query_api in [true, false] {
        let mut assigned_slot = null_mut();
        for action in [2, 0, 1] {
            let mut info = SchedulerSlotInfo {
                action,
                handle: h,
                slot: if action == 1 {
                    assigned_slot
                } else {
                    null_mut()
                },
            };
            if action == 1 && assigned_slot.is_null() {
                slot_operations.push(json!({"api":if query_api {"query"} else {"set"},"action":action,"not_attempted":"no successful assignment to free"}));
                continue;
            }
            let status = if query_api {
                NtQueryInformationThread(
                    GetCurrentThread(),
                    57,
                    (&mut info as *mut SchedulerSlotInfo).cast(),
                    size_of::<SchedulerSlotInfo>() as u32,
                    &mut ret_len,
                )
            } else {
                NtSetInformationThread(
                    GetCurrentThread(),
                    57,
                    (&mut info as *mut SchedulerSlotInfo).cast(),
                    size_of::<SchedulerSlotInfo>() as u32,
                )
            };
            eprintln!(
                "scheduler-slot: query_api={query_api} action={action} status={} slot=0x{:x}",
                hex(status as u32),
                info.slot as usize
            );
            if action == 0 && status == 0 {
                assigned_slot = info.slot;
            }
            slot_operations.push(json!({"api":if query_api {"query"} else {"set"},"action":action,
                "status":hex(status as u32),"output_handle":info.handle as usize,"output_slot":format!("0x{:016x}",info.slot as usize)}));
        }
    }
    operations["thread_scheduler_shared_data_slot_class_57"] = json!({"class":57,"structure_bytes":size_of::<SchedulerSlotInfo>(),"trials":slot_operations});

    let mut allocated = null_mut();
    let set_status = NtSetInformationProcess(
        GetCurrentProcess(),
        112,
        (&mut allocated as *mut HANDLE).cast(),
        size_of::<HANDLE>() as u32,
    );
    eprintln!(
        "scheduler-allocation: set class112 bytes8 status={} handle={}",
        hex(set_status as u32),
        allocated as usize
    );
    let details = duplicate_details(allocated, set_status);
    let closed = if set_status == 0 && !allocated.is_null() && allocated != h {
        CloseHandle(allocated) != 0
    } else {
        false
    };
    operations["process_scheduler_shared_data_set_112"] = json!({"buffer_bytes":size_of::<HANDLE>(),"status":hex(set_status as u32),"output_handle":allocated as usize,"output_object":details,"new_handle_closed":closed});

    json!({
        "handle": h as usize,
        "granted_access": hex(granted),
        "operations": operations,
        "summary": "Finite operations measured under the worker primary; sharing and cached-context semantics are not inferred from object type.",
    })
}

/// Locate SchedulerSharedData in the current process and probe its capabilities.
pub unsafe fn probe_scheduler_shared_data_in_process() -> Value {
    let handles = match handle_table() {
        Ok(h) => h,
        Err(e) => return json!({"present": false, "error": e}),
    };

    let sched_entry = handles
        .iter()
        .find(|h| h["type"].as_str() == Some("SchedulerSharedData"));

    let entry = match sched_entry {
        Some(e) => e,
        None => {
            return json!({
                "present": false,
                "reason": "SchedulerSharedData absent in this process snapshot",
            });
        }
    };

    let handle_val = entry["handle"].as_u64().unwrap_or(0) as HANDLE;
    let access = entry["granted_access"]
        .as_str()
        .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(1);

    eprintln!(
        "scheduler-probe: probing SchedulerSharedData handle={} access=0x{:x}",
        handle_val as usize, access
    );

    let probe_results = probe_scheduler_shared_data_handle(handle_val, access);

    json!({
        "present": true,
        "handle": handle_val as usize,
        "probe": probe_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_slot_info_layout() {
        assert_eq!(size_of::<SchedulerSlotInfo>(), 24);
        assert_eq!(std::mem::offset_of!(SchedulerSlotInfo, handle), 8);
        assert_eq!(std::mem::offset_of!(SchedulerSlotInfo, slot), 16);
    }
}
