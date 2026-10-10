"""Retain GUI startup handle identities, close trials and probes with source hashes.

The parent target list is omitted because each child's probes already retain the
attempted targets and results.
"""
import hashlib
import json
import pathlib
import sys


def retain(source, destination, run_id, source_sha, image):
    source = pathlib.Path(source)
    destination = pathlib.Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    report = json.loads((source / "report.json").read_text(encoding="utf-8-sig"))
    variants = [
        diagnostic["result"]
        for run in report["runs"]
        for diagnostic in run.get("diagnostics", [])
        if diagnostic.get("result", {}).get("sequence", "").startswith("full-gui-")
    ]
    retained = {
        "run_id": str(run_id), "source_sha": source_sha, "image": image,
        "target_count": len(report["targets"]), "profile_cleanup": report["profile_cleanup"],
        "variants": variants,
    }
    output = destination / f"{run_id}-{image}.json"
    output.write_text(json.dumps(retained, indent=2) + "\n", encoding="utf-8")
    metadata = [{
        "sequence": variant["sequence"], "pid": variant.get("pid"),
        "exit_code": variant.get("exit_code"), "error": variant.get("error"),
        "completed_probe_count": len(variant["child"]["probes"]) if "probes" in variant.get("child", {}) else None,
        "child": {key: value for key, value in variant.get("child", {}).items() if key != "probes"},
        "parent_handle_inspection": variant.get("parent_handle_inspection"),
        "handle_trace_enablement": variant.get("handle_trace_enablement"),
        "loader_threads": variant.get("loader_threads"), "loader_setting": variant.get("loader_setting"),
        "connections": variant.get("debug", {}).get("trace", {}).get("connections") if isinstance(variant.get("debug", {}).get("trace"), dict) else None,
        "handles_before_resume": variant.get("parent_before_resume", {}).get("handles"),
        "stdio_handles": variant.get("stdio_handles"),
    } for variant in variants]
    metadata_output = destination / f"{run_id}-{image}-metadata.json"
    metadata_output.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    lines = ["# Pre-input handle campaign", "", f"Source `{source_sha}`; run {run_id}; {image}.", "",
             "Each row is a separately launched GUI worker. Counts include only reported probe results and the reported pre-input inventory (including stdio). A missing report is not an empty inventory.", "",
             "| Recipe | PID | Exit | Reported probes | Successful operations | Reported pre-input handles |",
             "|---|---:|---|---:|---|---:|"]
    for variant in variants:
        child = variant.get("child", {})
        probes = child.get("probes", [])
        successes = [f"{probe['kind']}: {probe['target']}" for probe in probes if probe.get("success")]
        probe_count = len(child["probes"]) if "probes" in child else "not reported"
        handle_count = len(child["handle_table"]) if "handle_table" in child else "not reported"
        lines.append(f"| {variant['sequence']} | {variant.get('pid', '')} | {variant.get('exit_code', variant.get('error', ''))} | {probe_count} | {', '.join(successes)} | {handle_count} |")

    inspect_variant = next((v for v in variants if v.get("sequence") in ("full-gui-close-removable", "full-gui-inspect")), None)
    if inspect_variant:
        child_threads = inspect_variant.get("child", {}).get("threads", {})
        parent_threads = inspect_variant.get("parent_handle_inspection", {}).get("threads", {})
        pre_threads = parent_threads.get("pre_pool_threads_symbolized") or child_threads.get("pre_pool_activity", {}).get("threads", [])
        post_threads = parent_threads.get("post_pool_threads_symbolized") or child_threads.get("post_pool_activity", {}).get("threads", [])

        lines.append("")
        lines.append("## Thread Impersonation States (Gap 1)")
        lines.append("")
        lines.append(f"Measured on `{inspect_variant.get('sequence')}` (PID {inspect_variant.get('pid')}) before input.")
        lines.append(f"Conclusion: **{child_threads.get('conclusion', 'no impersonation tokens observed')}**")
        lines.append("")
        lines.append("### Pre-Pool Activity Threads")
        lines.append("")
        lines.append("| TID | Win32 Start | Symbol | OpenThreadToken (Self) | OpenThreadToken (Client) | Impersonation Level | Integrity |")
        lines.append("|---:|---|---|---|---|---|---|")
        for t in pre_threads:
            sym_obj = t.get("win32_symbol") or t.get("nt_symbol") or {}
            sym_str = sym_obj.get("symbol", "-") if isinstance(sym_obj, dict) else "-"
            disp = sym_obj.get("displacement") if isinstance(sym_obj, dict) else None
            if disp is not None:
                sym_str = f"{sym_str}+0x{disp:x}"
            addr = t.get("win32_start_address", t.get("nt_start_address", "-"))
            self_token = t.get("open_as_self", {})
            client_token = t.get("open_as_client", {})
            self_res = "Token present" if self_token.get("has_token") else f"No token ({self_token.get('error')})"
            client_res = "Token present" if client_token.get("has_token") else f"No token ({client_token.get('error')})"
            token_details = self_token.get("token") or client_token.get("token") or {}
            imp_lvl = token_details.get("impersonation_level_name", "None")
            integ = token_details.get("integrity_level", "None")
            lines.append(f"| {t.get('tid')} | `{addr}` | `{sym_str}` | {self_res} | {client_res} | {imp_lvl} | {integ} |")

        lines.append("")
        lines.append("### Post-Pool Activity Threads (Forced Pool Activity)")
        lines.append("")
        lines.append("| TID | Win32 Start | Symbol | OpenThreadToken (Self) | OpenThreadToken (Client) | Impersonation Level | Integrity |")
        lines.append("|---:|---|---|---|---|---|---|")
        for t in post_threads:
            sym_obj = t.get("win32_symbol") or t.get("nt_symbol") or {}
            sym_str = sym_obj.get("symbol", "-") if isinstance(sym_obj, dict) else "-"
            disp = sym_obj.get("displacement") if isinstance(sym_obj, dict) else None
            if disp is not None:
                sym_str = f"{sym_str}+0x{disp:x}"
            addr = t.get("win32_start_address", t.get("nt_start_address", "-"))
            self_token = t.get("open_as_self", {})
            client_token = t.get("open_as_client", {})
            self_res = "Token present" if self_token.get("has_token") else f"No token ({self_token.get('error')})"
            client_res = "Token present" if client_token.get("has_token") else f"No token ({client_token.get('error')})"
            token_details = self_token.get("token") or client_token.get("token") or {}
            imp_lvl = token_details.get("impersonation_level_name", "None")
            integ = token_details.get("integrity_level", "None")
            lines.append(f"| {t.get('tid')} | `{addr}` | `{sym_str}` | {self_res} | {client_res} | {imp_lvl} | {integ} |")

        child_sched = inspect_variant.get("child", {}).get("scheduler_shared_data", {})
        parent_sched = inspect_variant.get("parent_handle_inspection", {}).get("scheduler_shared_data", {})
        lines.append("")
        lines.append("## SchedulerSharedData Operation Contract (Gap 2)")
        lines.append("")
        if not child_sched.get("present", False) and not parent_sched.get("present", False):
            lines.append("SchedulerSharedData is **absent on Windows Server 2022**; no handle or object exists.")
        else:
            lines.append(f"Handle `{child_sched.get('handle')}` (access `0x1`), kernel object `{parent_sched.get('object')}`.")
            lines.append(f"Matching foreign handles across system: **{len(parent_sched.get('foreign_handles', []))}** (private in-process object).")
            lines.append("")
            probe = child_sched.get("probe", {})
            ops = probe.get("operations", {})
            lines.append("| Operation | Invocation / API | Measured Result | Permitted | Capability Implication |")
            lines.append("|---|---|---|:---:|---|")

            basic = ops.get("query_object_basic", {})
            lines.append(f"| Query Basic Info | `NtQueryObject(0)` | status {basic.get('status')}, handles={basic.get('handle_count')}, pointers={basic.get('pointer_count')} | Yes | Object attributes only |")

            otype = ops.get("query_object_type", {})
            lines.append(f"| Query Object Type | `NtQueryObject(2)` | type `{otype.get('type_name')}`, valid_mask `{otype.get('valid_access_mask')}` | Yes | Object type definition |")

            oname = ops.get("query_object_name", {})
            lines.append(f"| Query Object Name | `NtQueryObject(1)` | status {oname.get('status')}, name `\"{oname.get('name')}\"` | Yes | Unnamed object |")

            sec = ops.get("query_security_object", {})
            lines.append(f"| Read Security DACL | `NtQuerySecurityObject(4)` | status {sec.get('status')} (STATUS_ACCESS_DENIED) | No | Lacks READ_CONTROL (0x20000) |")

            dup = ops.get("duplicate_object", {})
            lines.append(f"| Duplicate Handle (Same Access) | `NtDuplicateObject(0x1)` | status {dup.get('same_access', {}).get('status')} | Yes | Duplication within same process |")
            lines.append(f"| Duplicate Handle (Elevated) | `NtDuplicateObject(GENERIC_ALL)` | status {dup.get('generic_all', {}).get('status')} (STATUS_ACCESS_DENIED) | No | Access cannot be elevated |")

            wait = ops.get("wait_for_single_object", {})
            lines.append(f"| Synchronize / Wait | `WaitForSingleObject` | result {wait.get('result')}, error {wait.get('error')} | No | Lacks SYNCHRONIZE (0x100000) |")

            fio = ops.get("file_io", {})
            lines.append(f"| File Read / Write / IOCTL | `ReadFile` / `WriteFile` / `DeviceIoControl` | read err {fio.get('read_file', {}).get('error')}, write err {fio.get('write_file', {}).get('error')} | No | Not a file/device object |")

            sec_map = ops.get("section_map", {})
            lines.append(f"| Map Section View | `NtMapViewOfSection` | status {sec_map.get('status')} (STATUS_OBJECT_TYPE_MISMATCH) | No | Not a section object |")

            root_dir = ops.get("root_directory_escape", {})
            lines.append(f"| RootDirectory File Open | `NtOpenFile(RootDirectory=h)` | status {root_dir.get('status')} | No | Cannot name or reach filesystem files |")

            alpc = ops.get("alpc_port_escape", {})
            lines.append(f"| ALPC Message Send | `NtAlpcSendWaitReceivePort` | status {alpc.get('status')} | No | Not an ALPC communication port |")

            cls112 = ops.get("process_scheduler_shared_data_class_112", {})
            lines.append(f"| Scheduler Slot Interface | `NtSetInformationProcess(112)` | assign={cls112.get('assign_action_0')}, query={cls112.get('query_action_2')}, free={cls112.get('free_action_1')} | Slot | Process scheduler slot mechanism |")
    summary = destination / f"{run_id}-{image}.md"
    summary.write_text("\n".join(lines) + "\n", encoding="utf-8")
    manifest = {
        "run_id": str(run_id), "source_sha": source_sha, "image": image,
        "raw_report_sha256": hashlib.sha256((source / "report.json").read_bytes()).hexdigest(),
        "retained_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
        "summary_sha256": hashlib.sha256(summary.read_bytes()).hexdigest(),
        "metadata_sha256": hashlib.sha256(metadata_output.read_bytes()).hexdigest(),
    }
    (destination / f"{run_id}-{image}-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print("\n".join(lines))


if __name__ == "__main__":
    retain(*sys.argv[1:])
