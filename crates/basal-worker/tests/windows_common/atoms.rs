//! Cross-job atom visibility on a shared station, with live owner processes
//! and successful same-job lookups as positive controls.
use super::*;
use std::ffi::c_void;
use std::io::Write;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAddAtomW(name: *const u16) -> u16;
    fn GlobalFindAtomW(name: *const u16) -> u16;
    fn GlobalDeleteAtom(atom: u16) -> u16;
    fn GetLastError() -> u32;
    fn SetLastError(error: u32);
}
#[link(name = "user32")]
unsafe extern "system" {
    fn GetProcessWindowStation() -> *mut c_void;
    fn GetUserObjectInformationW(
        object: *mut c_void,
        index: i32,
        buffer: *mut c_void,
        bytes: u32,
        needed: *mut u32,
    ) -> i32;
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain([0]).collect()
}

fn line(child: &mut ConfinedProcess) -> String {
    let mut text = Vec::new();
    loop {
        let mut byte = [0u8];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut byte)
            .expect("worker atom reply");
        if byte[0] == b'\n' {
            break;
        }
        text.push(byte[0]);
        assert!(text.len() < 4096, "bounded atom reply");
    }
    String::from_utf8(text).unwrap().trim_end().to_owned()
}

fn request(child: &mut ConfinedProcess, operation: &str, name: &str) -> (u16, u32) {
    let input = child.stdin.as_mut().unwrap();
    writeln!(input, "{operation} {name}").unwrap();
    input.flush().unwrap();
    let response = line(child);
    println!("atom worker {} {operation}: {response}", child.id());
    let fields: Vec<_> = response.split(':').collect();
    assert_eq!(fields.len(), 3, "{response}");
    assert_eq!(fields[0], operation);
    (fields[1].parse().unwrap(), fields[2].parse().unwrap())
}

struct ParentAtom(u16);
impl Drop for ParentAtom {
    fn drop(&mut self) {
        unsafe { GlobalDeleteAtom(self.0) };
    }
}

#[test]
fn global_atoms_are_invisible_across_worker_job_boundaries() {
    let barrier = std::sync::Barrier::new(2);
    let mut children: Vec<_> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    launch(
                        &LaunchOptions::new(placed_worker(), FLOW_JOB_COMMIT_BYTES)
                            .arg("--confinement-probe")
                            .arg("--probe=atoms"),
                    )
                    .expect("fully confined atom worker")
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect()
    });
    for child in &mut children {
        assert_eq!(line(child), "atom-ready");
        let token = child.primary_token().unwrap();
        assert_eq!(token.integrity, "S-1-16-0");
        assert!(token.less_privileged);
        assert!(token.capabilities.is_empty());
        assert_eq!(token.restricting_sids, ["S-1-0-0"]);
        assert_eq!(token.privileges, 0);
        assert!(child.in_owned_job().unwrap());
        assert_eq!(child.job_limits().unwrap().ui_restrictions, 0xff);
    }
    let names: Vec<_> = children.iter().map(ConfinedProcess::desktop_name).collect();
    let (station_a, desktop_a) = names[0].split_once('\\').unwrap();
    let (station_b, desktop_b) = names[1].split_once('\\').unwrap();
    assert_eq!(station_a, station_b);
    assert_ne!(desktop_a, desktop_b);
    let unique = format!(
        "basal_atom_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let worker_name = format!("{unique}_worker_a");
    let other_worker_name = format!("{unique}_worker_b");
    let parent_name = format!("{unique}_parent");
    let added = request(&mut children[0], "add", &worker_name);
    assert_ne!(added.0, 0, "worker A can create its own atom");
    assert_eq!(added.1, 0);
    assert_eq!(request(&mut children[0], "find", &worker_name), added);
    let other_added = request(&mut children[1], "add", &other_worker_name);
    assert_ne!(other_added.0, 0);
    assert_eq!(other_added.1, 0);
    assert_eq!(
        request(&mut children[1], "find", &other_worker_name),
        other_added
    );
    assert_eq!(children[0].try_wait().unwrap(), None);
    assert_eq!(children[1].try_wait().unwrap(), None);
    // The parent process can use an alternate station. Select it
    // under the launcher's station-switch lock so the parent probes the same
    // station as the children, without racing another launch's desktop creation.
    ConfinedProcess::with_window_station(|| {
        let mut current_name = [0u16; 512];
        let mut needed = 0;
        let current = unsafe { GetProcessWindowStation() };
        assert!(!current.is_null());
        assert_ne!(
            unsafe {
                GetUserObjectInformationW(
                    current,
                    2,
                    current_name.as_mut_ptr().cast(),
                    1024,
                    &mut needed,
                )
            },
            0
        );
        let length = current_name.iter().position(|&unit| unit == 0).unwrap();
        let parent_station = String::from_utf16_lossy(&current_name[..length]);
        println!("atom shared station parent={parent_station}, worker desktops={names:?}");
        assert_eq!(station_a, parent_station);
        assert_eq!(station_b, parent_station);
        let parent_find = unsafe {
            SetLastError(0);
            let atom = GlobalFindAtomW(wide(&worker_name).as_ptr());
            (atom, GetLastError())
        };
        println!(
            "atom visibility worker-A -> parent: atom={} win32={}",
            parent_find.0, parent_find.1
        );
        let parent_add = unsafe {
            SetLastError(0);
            let atom = GlobalAddAtomW(wide(&parent_name).as_ptr());
            (atom, GetLastError())
        };
        let _cleanup = ParentAtom(parent_add.0);
        println!(
            "atom parent positive add: atom={} win32={}",
            parent_add.0, parent_add.1
        );
        assert_ne!(parent_add.0, 0);
        assert_eq!(parent_add.1, 0);
        let parent_own = unsafe {
            SetLastError(0);
            let atom = GlobalFindAtomW(wide(&parent_name).as_ptr());
            (atom, GetLastError())
        };
        assert_eq!(parent_own, parent_add);
        let worker_find_parent = request(&mut children[0], "find", &parent_name);
        println!(
            "atom visibility parent -> worker-A: atom={} win32={}",
            worker_find_parent.0, worker_find_parent.1
        );
        let other_worker_find = request(&mut children[1], "find", &worker_name);
        println!(
            "atom visibility worker-A -> worker-B: atom={} win32={}",
            other_worker_find.0, other_worker_find.1
        );
        assert_eq!(parent_find, (0, 2), "A's atom must not escape its job");
        assert_eq!(
            worker_find_parent,
            (0, 2),
            "the worker must not see parent atoms"
        );
        assert_eq!(
            other_worker_find,
            (0, 2),
            "workers must have job-private atom tables"
        );
    })
    .expect("select the workers' station for the parent probe");
    for child in &mut children {
        let input = child.stdin.as_mut().unwrap();
        writeln!(input, "done").unwrap();
        input.flush().unwrap();
        assert_eq!(line(child), "atom-done");
    }
    for child in children {
        let desktop_name = child.desktop_name();
        let ended = end(child);
        assert_eq!(ended.code, 0, "{}", ended.stderr);
        assert!(
            ended.stderr.contains(&format!(
                "startup desktop assignment read-back={:?}",
                Some(desktop_name)
            )),
            "{}",
            ended.stderr
        );
    }
}
