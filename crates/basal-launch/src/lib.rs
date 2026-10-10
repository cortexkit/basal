//! Process launching, AppContainer profile creation, and confinement configuration for Windows.
//!
//! Off Windows this crate contains no code.

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::*;
