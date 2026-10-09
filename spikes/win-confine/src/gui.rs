#![windows_subsystem = "windows"]

// Use the same worker code and explicit pipe handles while removing console
// initialization from the image, independently of process-creation flags.
include!("main.rs");
