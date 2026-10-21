//! Platform clipboard backends. Network work never runs while a native
//! clipboard is open.

use crate::image::ClipboardImage;
use std::sync::Arc;

/// A bounded clipboard value. Image buffers are shared by the observer,
/// latest-value outbox and network worker instead of copied on each poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    Text(String),
    Image(Arc<ClipboardImage>),
}

/// `revision` identifies every Windows clipboard change, including formats
/// that this application does not publish. Linux tools have no such token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub content: Option<Payload>,
    pub revision: Option<u32>,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{read_snapshot, read_text, write_if_unchanged, write_text, Backend};
#[cfg(windows)]
pub use windows::{read_snapshot, read_text, write_if_unchanged, write_text, Backend};

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("wxclipd currently supports Windows and Linux");
