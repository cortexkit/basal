//! Exact native syscall tables. Argument failures have the same fatal action as unknown calls.
use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

pub const DEFAULT_ACTION: SeccompAction = SeccompAction::KillProcess;

// Numbers are from Linux's x86_64 and asm-generic (aarch64) syscall tables.
// A missing architecture entry is not an invitation to admit another call.
pub const TABLE: &[(&str, i64, i64)] = &[
    ("read", 0, 63),
    ("readv", 19, 65),
    ("write", 1, 64),
    ("writev", 20, 66),
    ("mmap", 9, 222),
    ("mprotect", 10, 226),
    ("munmap", 11, 215),
    ("brk", 12, 214),
    ("mremap", 25, 216),
    ("madvise", 28, 233),
    ("futex", 202, 98),
    ("clock_gettime", 228, 113),
    ("clock_nanosleep", 230, 115),
    ("nanosleep", 35, 101),
    ("gettimeofday", 96, 169),
    ("time", 201, -1),
    ("getcpu", 309, 168),
    ("getrandom", 318, 278),
    ("rt_sigreturn", 15, 139),
    ("rt_sigprocmask", 14, 135),
    ("rt_sigaction", 13, 134),
    ("sigaltstack", 131, 132),
    ("exit", 60, 93),
    ("exit_group", 231, 94),
    ("restart_syscall", 219, 128),
    ("sched_yield", 24, 124),
    ("getpid", 39, 172),
    ("gettid", 186, 178),
    ("kill", 62, 129),
    ("tgkill", 234, 131),
];

fn condition(index: u8, op: SeccompCmpOp, value: u64) -> SeccompCondition {
    // Full-width comparisons also reject malicious high bits in register arguments.
    SeccompCondition::new(index, SeccompCmpArgLen::Qword, op, value).expect("valid argument")
}
fn eq(index: u8, value: u64) -> SeccompCondition {
    condition(index, SeccompCmpOp::Eq, value)
}
fn choices(index: u8, values: &[u64]) -> Vec<SeccompRule> {
    values
        .iter()
        .map(|v| SeccompRule::new(vec![eq(index, *v)]).expect("nonempty rule"))
        .collect()
}
fn rules(name: &str, pid: u32, tid: u32) -> Vec<SeccompRule> {
    match name {
        "read" | "readv" => choices(0, &[0]),
        "write" | "writev" => choices(0, &[1, 2]),
        "mmap" => vec![
            SeccompRule::new(vec![
                condition(2, SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64), 0),
                eq(3, (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as u64),
            ])
            .expect("nonempty rule"),
        ],
        "mprotect" => vec![
            SeccompRule::new(vec![condition(
                2,
                SeccompCmpOp::MaskedEq(libc::PROT_EXEC as u64),
                0,
            )])
            .expect("nonempty rule"),
        ],
        "mremap" => choices(3, &[0, libc::MREMAP_MAYMOVE as u64]),
        "madvise" => choices(2, &[libc::MADV_DONTNEED as u64]),
        "futex" => choices(
            1,
            &[
                (libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG) as u64,
                (libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG) as u64,
            ],
        ),
        "clock_gettime" | "clock_nanosleep" => choices(
            0,
            &[
                libc::CLOCK_THREAD_CPUTIME_ID as u64,
                libc::CLOCK_MONOTONIC as u64,
                libc::CLOCK_REALTIME as u64,
            ],
        ),
        "kill" => choices(0, &[pid as u64]),
        "tgkill" => vec![
            SeccompRule::new(vec![eq(0, pid as u64), eq(1, tid as u64)]).expect("nonempty rule"),
        ],
        _ => vec![],
    }
}

pub fn build(arch: TargetArch, pid: u32, tid: u32) -> Result<BpfProgram, String> {
    let mut table = BTreeMap::new();
    for &(name, x86, arm) in TABLE {
        let number = match arch {
            TargetArch::x86_64 => x86,
            TargetArch::aarch64 => arm,
            _ => return Err("unsupported architecture".into()),
        };
        if number >= 0 {
            table.insert(number, rules(name, pid, tid));
        }
    }
    SeccompFilter::new(table, DEFAULT_ACTION, SeccompAction::Allow, arch)
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|e: seccompiler::BackendError| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_action_is_kill_process() {
        assert_eq!(DEFAULT_ACTION, SeccompAction::KillProcess);
    }

    #[test]
    fn admission_document_matches_each_architecture_table() {
        let text = include_str!("../../../sandbox/linux/syscall-admission.md");
        for (arch, column) in [("x86_64", 1), ("aarch64", 2)] {
            let documented: std::collections::BTreeSet<_> = text
                .lines()
                .filter_map(|line| {
                    let fields: Vec<_> = line.split('|').map(str::trim).collect();
                    if fields.len() == 6 && fields[column + 1] == "yes" {
                        Some(fields[1])
                    } else {
                        None
                    }
                })
                .collect();
            let expected: std::collections::BTreeSet<_> = TABLE
                .iter()
                .filter_map(|&(name, x86, arm)| {
                    if (if arch == "x86_64" { x86 } else { arm }) >= 0 {
                        Some(name)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(documented, expected, "{arch}");
        }
    }

    // Interpret the emitted classic BPF rather than reimplementing the rule predicates.
    fn run(program: &BpfProgram, arch: u32, number: u32, args: [u64; 6]) -> u32 {
        let mut data = [0u8; 64];
        data[..4].copy_from_slice(&number.to_ne_bytes());
        data[4..8].copy_from_slice(&arch.to_ne_bytes());
        for (i, arg) in args.iter().enumerate() {
            data[16 + i * 8..24 + i * 8].copy_from_slice(&arg.to_ne_bytes());
        }
        let (mut pc, mut a) = (0usize, 0u32);
        loop {
            let ins = &program[pc];
            match ins.code {
                0x20 => {
                    a = u32::from_ne_bytes(
                        data[ins.k as usize..ins.k as usize + 4].try_into().unwrap(),
                    )
                }
                0x54 => a &= ins.k,
                0x15 | 0x25 | 0x35 | 0x45 => {
                    let yes = match ins.code {
                        0x15 => a == ins.k,
                        0x25 => a > ins.k,
                        0x35 => a >= ins.k,
                        _ => a & ins.k != 0,
                    };
                    pc += if yes { ins.jt } else { ins.jf } as usize;
                }
                0x05 => pc += ins.k as usize,
                0x06 => return ins.k,
                code => panic!("unhandled BPF instruction {code:x}"),
            }
            pc += 1;
        }
    }

    #[test]
    fn both_filters_kill_unknown_architectures_syscalls_and_rejected_arguments() {
        for (arch, audit, arm) in [
            (TargetArch::x86_64, 0xc000003e, false),
            (TargetArch::aarch64, 0xc00000b7, true),
        ] {
            let program = build(arch, 123, 124).unwrap();
            let nr = |name| {
                let row = TABLE.iter().find(|row| row.0 == name).unwrap();
                (if arm { row.2 } else { row.1 }) as u32
            };
            let allow = |name, args| {
                assert_eq!(
                    run(&program, audit, nr(name), args),
                    libc::SECCOMP_RET_ALLOW,
                    "{name}"
                )
            };
            let deny = |name, args| {
                assert_eq!(
                    run(&program, audit, nr(name), args),
                    libc::SECCOMP_RET_KILL_PROCESS,
                    "{name}"
                )
            };
            assert_eq!(
                run(&program, 0, nr("read"), [0; 6]),
                libc::SECCOMP_RET_KILL_PROCESS
            );
            assert_eq!(
                run(&program, audit, 0x40000000 | nr("read"), [0; 6]),
                libc::SECCOMP_RET_KILL_PROCESS
            );
            assert_eq!(
                run(&program, audit, 9999, [0; 6]),
                libc::SECCOMP_RET_KILL_PROCESS
            );
            allow("read", [0; 6]);
            deny("read", [1, 0, 0, 0, 0, 0]);
            for fd in [1, 2] {
                allow("write", [fd, 0, 0, 0, 0, 0]);
            }
            deny("write", [0; 6]);
            deny("write", [0x100000001, 0, 0, 0, 0, 0]);
            allow("mmap", [0, 4096, 3, 0x22, 0, 0]);
            for flags in [0x21, 0x32, 0x23, 0x1022] {
                deny("mmap", [0, 4096, 3, flags, 0, 0]);
            }
            deny("mmap", [0, 4096, 7, 0x22, 0, 0]);
            allow("mprotect", [0, 4096, 3, 0, 0, 0]);
            deny("mprotect", [0, 4096, 5, 0, 0, 0]);
            for flags in [0, 1] {
                allow("mremap", [0, 0, 0, flags, 0, 0]);
            }
            for flags in [2, 3, 4] {
                deny("mremap", [0, 0, 0, flags, 0, 0]);
            }
            allow("madvise", [0, 0, 4, 0, 0, 0]);
            deny("madvise", [0; 6]);
            for op in [128, 129] {
                allow("futex", [0, op, 0, 0, 0, 0]);
            }
            deny("futex", [0; 6]);
            deny("futex", [0, 130, 0, 0, 0, 0]);
            for clock in [0, 1, 3] {
                allow("clock_gettime", [clock, 0, 0, 0, 0, 0]);
            }
            for clock in [2, 4, 0xfffffffa] {
                deny("clock_gettime", [clock, 0, 0, 0, 0, 0]);
            }
            allow("kill", [123, 6, 0, 0, 0, 0]);
            for pid in [0, 122, u64::MAX] {
                deny("kill", [pid, 6, 0, 0, 0, 0]);
            }
            allow("tgkill", [123, 124, 6, 0, 0, 0]);
            deny("tgkill", [123, 125, 6, 0, 0, 0]);
        }
    }
}
