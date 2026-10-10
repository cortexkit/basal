#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    #[cfg(windows)]
    {
        use std::io::Read;
        let mut buf = [0u8; 1];
        let _ = std::io::stdin().read(&mut buf);
    }
}
