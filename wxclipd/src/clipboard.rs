//! Platform clipboard backends. Network work never runs while a native
//! clipboard is open.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{read_text, write_text, write_text_if_unchanged, Backend};
#[cfg(windows)]
pub use windows::{read_text, write_text, write_text_if_unchanged, Backend};

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("wxclipd currently supports Windows and Linux");
