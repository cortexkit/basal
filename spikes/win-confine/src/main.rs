#[cfg(windows)]
mod child;
#[cfg(windows)]
mod native;
#[cfg(windows)]
mod network;
#[cfg(windows)]
mod observe;
#[cfg(windows)]
mod parent;
#[cfg(windows)]
mod profile;
#[cfg(windows)]
mod trace;

#[cfg(windows)]
fn main() {
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode,
        };
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
    let result = if std::env::args().any(|a| a == "--child") {
        child::run()
    } else if std::env::args().any(|a| a == "--observe") {
        observe::run()
    } else {
        parent::run()
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("win-confine requires native Windows; no emulated measurements are accepted");
    std::process::exit(1);
}
