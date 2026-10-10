//! The built worker image: a GUI-subsystem executable that imports no DLL
//! which would load User32, GDI or COM into the confined process, and no C
//! runtime DLL.
//!
//! A static Userenv or Ole32 import once pulled User32, GDI and Win32u into
//! the confined child and left it with over a hundred handles before input;
//! those DLLs are loaded at run time in the parent only. The C runtime is
//! linked statically (`+crt-static` for this target in `.cargo/config.toml`),
//! so the worker never depends on a separately installed runtime DLL. The
//! console subsystem would start a console host the confined token cannot
//! initialise.
#![cfg(windows)]

use std::path::Path;

mod windows_common;

/// `IMAGE_SUBSYSTEM_WINDOWS_GUI`.
const GUI_SUBSYSTEM: u16 = 2;

/// DLLs whose import would load GUI or COM code into the worker.
const FORBIDDEN: [&str; 5] = [
    "userenv.dll",
    "ole32.dll",
    "user32.dll",
    "gdi32.dll",
    "win32u.dll",
];

/// What the import test reads from a PE image.
struct Image {
    subsystem: u16,
    imports: Vec<String>,
    delay_imports: Vec<String>,
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

/// Reads the subsystem and the import and delay-import DLL names of a
/// 64-bit PE image.
fn parse(bytes: &[u8]) -> Image {
    assert_eq!(&bytes[..2], b"MZ", "not a PE image");
    let pe = u32_at(bytes, 0x3c) as usize;
    assert_eq!(&bytes[pe..pe + 4], b"PE\0\0", "no PE signature");
    let coff = pe + 4;
    let sections = u16_at(bytes, coff + 2) as usize;
    let optional_size = u16_at(bytes, coff + 16) as usize;
    let optional = coff + 20;
    assert_eq!(u16_at(bytes, optional), 0x20b, "not a PE32+ image");
    let subsystem = u16_at(bytes, optional + 68);
    let image_base = u64_at(bytes, optional + 24);
    let directories = u32_at(bytes, optional + 108) as usize;
    let directory = |index: usize| -> (u32, u32) {
        if index >= directories {
            return (0, 0);
        }
        let entry = optional + 112 + index * 8;
        (u32_at(bytes, entry), u32_at(bytes, entry + 4))
    };

    // (virtual address, virtual size, raw size, raw offset) per section.
    let table = optional + optional_size;
    let sections: Vec<(u32, u32, u32, u32)> = (0..sections)
        .map(|index| {
            let header = table + index * 40;
            (
                u32_at(bytes, header + 12),
                u32_at(bytes, header + 8),
                u32_at(bytes, header + 16),
                u32_at(bytes, header + 20),
            )
        })
        .collect();
    let offset = |rva: u32| -> usize {
        let (address, _, _, raw) = sections
            .iter()
            .copied()
            .find(|&(address, virtual_size, raw_size, _)| {
                rva >= address && rva < address + virtual_size.max(raw_size)
            })
            .unwrap_or_else(|| panic!("RVA {rva:#x} is in no section"));
        (rva - address + raw) as usize
    };
    let name = |rva: u32| -> String {
        let start = offset(rva);
        let end = bytes[start..]
            .iter()
            .position(|&byte| byte == 0)
            .expect("a NUL-terminated name");
        String::from_utf8_lossy(&bytes[start..start + end]).into_owned()
    };

    let mut imports = Vec::new();
    let (import_rva, import_size) = directory(1);
    if import_rva != 0 && import_size != 0 {
        // IMAGE_IMPORT_DESCRIPTOR, 20 bytes, the name RVA at offset 12; the
        // list ends with an all-zero descriptor.
        let mut descriptor = offset(import_rva);
        while bytes[descriptor..descriptor + 20]
            .iter()
            .any(|&byte| byte != 0)
        {
            imports.push(name(u32_at(bytes, descriptor + 12)));
            descriptor += 20;
        }
    }
    let mut delay_imports = Vec::new();
    let (delay_rva, delay_size) = directory(13);
    if delay_rva != 0 && delay_size != 0 {
        // IMAGE_DELAYLOAD_DESCRIPTOR, 32 bytes: attributes, then the name.
        // Attribute bit 0 clear means the name is a virtual address.
        let mut descriptor = offset(delay_rva);
        while bytes[descriptor..descriptor + 32]
            .iter()
            .any(|&byte| byte != 0)
        {
            let attributes = u32_at(bytes, descriptor);
            let mut rva = u32_at(bytes, descriptor + 4);
            if attributes & 1 == 0 {
                rva = (u64::from(rva) - image_base) as u32;
            }
            delay_imports.push(name(rva));
            descriptor += 32;
        }
    }
    Image {
        subsystem,
        imports,
        delay_imports,
    }
}

fn is_c_runtime(name: &str) -> bool {
    (name.starts_with("vcruntime") && name.ends_with(".dll"))
        || (name.starts_with("msvcp") && name.ends_with(".dll"))
        || name == "ucrtbase.dll"
        || name.starts_with("api-ms-win-crt-")
}

#[test]
fn the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports() {
    let path = match std::env::var("CKDEV_WINDOWS_IMAGE") {
        Ok(source) => windows_common::dev_binary(&source),
        Err(_) => windows_common::dev_binary(env!("CARGO_BIN_EXE_ck-basal-worker")),
    };
    let image = parse(&std::fs::read(path).expect("read the worker image"));
    println!("subsystem: {}", image.subsystem);
    println!("imports: {:?}", image.imports);
    println!("delay imports: {:?}", image.delay_imports);
    assert_eq!(image.subsystem, GUI_SUBSYSTEM);

    let all: Vec<String> = image
        .imports
        .iter()
        .chain(&image.delay_imports)
        .map(|name| name.to_ascii_lowercase())
        .collect();
    // The parse found the import table: every Windows program imports
    // kernel32, so an empty or misread table cannot pass the checks below.
    assert!(all.iter().any(|name| name == "kernel32.dll"), "{all:?}");
    for name in &all {
        assert!(!FORBIDDEN.contains(&name.as_str()), "imports {name}");
        assert!(!is_c_runtime(name), "imports the C runtime DLL {name}");
    }

    if let Ok(directory) = std::env::var("CKDEV_WINDOWS_EVIDENCE") {
        let mut measured = all.clone();
        measured.sort();
        measured.dedup();
        std::fs::write(
            Path::new(&directory).join("windows-imports.txt"),
            format!("{}\n", measured.join("\n")),
        )
        .expect("record measured image imports");
    }
    assert!(
        !all.iter().any(|name| name == "ws2_32.dll"),
        "Winsock belongs only to dynamically resolved probe mode"
    );
    // Every import must be on the committed production inventory, so adding a
    // DLL dependency requires an explicit update. API-set names are names, not
    // filesystem paths; normal and delay imports use the same fixed allowlist.
    let listed = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/windows-imports.txt");
    let text = std::fs::read_to_string(&listed).expect("committed production import inventory");
    let allowed: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_ascii_lowercase)
        .collect();
    for name in &all {
        assert!(
            allowed.contains(name),
            "{name} is not in {}",
            listed.display()
        );
    }
}

#[test]
fn the_c_runtime_names_are_recognised() {
    for name in [
        "vcruntime140.dll",
        "vcruntime140_1.dll",
        "msvcp140.dll",
        "ucrtbase.dll",
        "api-ms-win-crt-runtime-l1-1-0.dll",
    ] {
        assert!(is_c_runtime(name), "{name}");
    }
    for name in [
        "kernel32.dll",
        "ntdll.dll",
        "advapi32.dll",
        "api-ms-win-core-synch-l1-2-0.dll",
    ] {
        assert!(!is_c_runtime(name), "{name}");
    }
}
