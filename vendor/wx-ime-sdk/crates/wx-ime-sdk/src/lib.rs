#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
// Core: identity, control session, persistence and pairing.
mod capture;
mod client;
mod device;
mod error;
mod network;
mod notify;
mod pairing;
mod session;
mod storage;
// Features built on the core.
mod clipboard;
mod file;

pub use client::{Client, ClientOptions};
pub use clipboard::{
    ClipboardContent, ClipboardInfo, DownloadedImage, Event, ReceivedClipboard, ReceivedId,
    SendReceipt,
};
pub use device::{Device, Group, LocalDevice};
pub use error::{Error, Result};
pub use file::{
    FileCode, FileError, FileInvite, FileReceipt, FileReceiveOptions, FileReceiver, FileTransfer,
    FileTransferProgress, ReceivedFile,
};
pub use network::ProxyConfig;
pub use notify::{NotifyChannel, NotifyEvent};
pub use pairing::{PairingPeer, PairingSession, PairingUpdate};
pub use wx_ime_protocol::pairing::PairingCode;
