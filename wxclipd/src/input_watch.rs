//! Optional, nonblocking hints that the user may be about to paste.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::spawn;
#[cfg(windows)]
pub use windows::spawn;
