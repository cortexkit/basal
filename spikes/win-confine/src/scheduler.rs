//! Native operations and capability analysis for SchedulerSharedData handles on Windows 11.
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
    fn NtQueryObject(
        handle: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;

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

/// Try all documented and observable operations on a SchedulerSharedData handle.
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
        4, // DACL_SECURITY_INFORMATION
        sec_buf.as_mut_ptr().cast(),
        sec_buf.len() as u32,
        &mut ret_len,
    );
    operations["query_security_object"] = json!({
        "status": hex(sec_st as u32),
        "permitted": sec_st == 0,
        "note": "denied (access 0x1 lacks READ_CONTROL 0x20000)",
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
    if dup_all_st == 0 && !dup_all.is_null() {
        CloseHandle(dup_all);
    }
    operations["duplicate_object"] = json!({
        "same_access": {"status": hex(dup_same_st as u32), "success": dup_same_ok},
        "generic_all": {"status": hex(dup_all_st as u32), "permitted": dup_all_st == 0},
    });

    // 6. WaitForSingleObject (synchronization check)
    SetLastError(0);
    let wait_res = WaitForSingleObject(h, 0);
    let wait_err = GetLastError();
    operations["wait_for_single_object"] = json!({
        "result": wait_res,
        "error": wait_err,
        "permitted": wait_res != WAIT_FAILED,
        "note": "denied (access 0x1 lacks SYNCHRONIZE 0x100000)",
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
        "note": "STATUS_OBJECT_TYPE_MISMATCH (0xc0000024)",
    });

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
        "note": "cannot be used as RootDirectory for file opens",
    });

    // 10. ALPC IPC escape test (can it be used as an ALPC communication port?)
    let alpc_st = NtAlpcSendWaitReceivePort(
        h,
        0,
        null_mut(),
        null_mut(),
        null_mut(),
        null_mut(),
        null_mut(),
    );
    operations["alpc_port_escape"] = json!({
        "status": hex(alpc_st as u32),
        "permitted": alpc_st == 0,
        "note": "cannot be used as an ALPC communication port",
    });

    // 11. ProcessSchedulerSharedData slot interface (class 112 / 0x70)
    let mut slot_ptr = null_mut();
    let mut assign_info = SchedulerSlotInfo {
        action: 0, // SchedulerSharedSlotAssign
        handle: h,
        slot: &mut slot_ptr as *mut *mut c_void as *mut c_void,
    };
    let assign_st = NtSetInformationProcess(
        GetCurrentProcess(),
        112,
        (&mut assign_info as *mut SchedulerSlotInfo).cast(),
        size_of::<SchedulerSlotInfo>() as u32,
    );

    let mut query_slot_ptr = null_mut();
    let mut query_info = SchedulerSlotInfo {
        action: 2, // SchedulerSharedSlotQuery
        handle: h,
        slot: &mut query_slot_ptr as *mut *mut c_void as *mut c_void,
    };
    let query_st = NtQueryInformationProcess(
        GetCurrentProcess(),
        112,
        (&mut query_info as *mut SchedulerSlotInfo).cast(),
        size_of::<SchedulerSlotInfo>() as u32,
        &mut ret_len,
    );

    let mut free_info = SchedulerSlotInfo {
        action: 1, // SchedulerSharedSlotFree
        handle: h,
        slot: slot_ptr,
    };
    let free_st = NtSetInformationProcess(
        GetCurrentProcess(),
        112,
        (&mut free_info as *mut SchedulerSlotInfo).cast(),
        size_of::<SchedulerSlotInfo>() as u32,
    );

    operations["process_scheduler_shared_data_class_112"] = json!({
        "class": 112,
        "assign_action_0": hex(assign_st as u32),
        "query_action_2": hex(query_st as u32),
        "free_action_1": hex(free_st as u32),
    });

    // 12. Related scheduling process information queries
    let mut sched_classes = json!({});
    for &(class_id, class_name) in &[
        (0u32, "ProcessBasicInformation"),
        (61, "ProcessDefaultCpuSetsInformation"),
        (62, "ProcessAllowedCpuSetsInformation"),
        (76, "ProcessThreadGovernor"),
    ] {
        let mut q_buf = [0usize; 8];
        let mut q_ret = 0u32;
        let q_st = NtQueryInformationProcess(
            GetCurrentProcess(),
            class_id,
            q_buf.as_mut_ptr().cast(),
            (q_buf.len() * 8) as u32,
            &mut q_ret,
        );
        sched_classes[class_name] = json!({
            "class": class_id,
            "status": hex(q_st as u32),
            "returned_bytes": q_ret,
        });
    }
    operations["related_process_classes"] = sched_classes;

    json!({
        "handle": h as usize,
        "granted_access": hex(granted),
        "operations": operations,
        "reaches_file": false,
        "reaches_network": false,
        "reaches_peer_process": false,
        "reaches_durable_state": false,
        "summary": "SchedulerSharedData access 0x1 is a private in-process scheduler slot handle with no file, network, IPC or persistent capability",
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
                "reason": "SchedulerSharedData handle absent (Windows Server 2022)",
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
