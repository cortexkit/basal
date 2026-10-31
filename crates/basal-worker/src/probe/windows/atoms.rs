//! A live atom-table probe. The parent keeps workers in separate jobs on the
//! same station, so cross-job lookups test isolation rather than name lifetime.
use std::io::{BufRead, Write};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAddAtomW(name: *const u16) -> u16;
    fn GlobalFindAtomW(name: *const u16) -> u16;
    fn GlobalDeleteAtom(atom: u16) -> u16;
    fn GetLastError() -> u32;
    fn SetLastError(error: u32);
}

pub(super) fn run() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut owned = Vec::new();
    writeln!(output, "atom-ready").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    loop {
        let mut line = String::new();
        if input
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            break;
        }
        let line = line.trim_end();
        if line == "done" {
            break;
        }
        let (operation, name) = line
            .split_once(' ')
            .ok_or("expected an atom operation and name")?;
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let (atom, error) = unsafe {
            SetLastError(0);
            let atom = match operation {
                "add" => GlobalAddAtomW(name.as_ptr()),
                "find" => GlobalFindAtomW(name.as_ptr()),
                _ => return Err("unexpected atom operation".into()),
            };
            (atom, GetLastError())
        };
        if operation == "add" && atom != 0 {
            owned.push(atom);
        }
        writeln!(output, "{operation}:{atom}:{error}").map_err(|error| error.to_string())?;
        output.flush().map_err(|error| error.to_string())?;
    }
    for atom in owned {
        unsafe { GlobalDeleteAtom(atom) };
    }
    writeln!(output, "atom-done").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    Ok(())
}
