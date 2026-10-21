//! Windows clipboard access. Network work never runs while a native
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
/// that this application does not publish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub content: Option<Payload>,
    pub revision: Option<u32>,
}

mod windows;

pub use windows::{read_snapshot, write_if_unchanged};
