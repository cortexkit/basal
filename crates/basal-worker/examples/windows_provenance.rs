//! One-off parent-side measurement of a production worker's startup handles.
//! This tool is not linked into the worker and is never run by ordinary tests.
#[cfg(windows)]
#[path = "windows_provenance/native.rs"]
mod native;

#[cfg(windows)]
fn main() -> Result<(), String> {
    native::run()
}
#[cfg(not(windows))]
fn main() {}
