//! System clipboard access through the desktop's own tools.
//!
//! Wayland and X11 both hand selection ownership to a process that must stay
//! alive, so writing goes through `wl-copy`/`xclip`, which fork and keep
//! serving the selection after the call returns.

use anyhow::{bail, Context, Result};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Wayland,
    X11,
}

impl Backend {
    pub fn detect() -> Result<Self> {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if wayland && which("wl-paste") && which("wl-copy") {
            return Ok(Self::Wayland);
        }
        let x11 = std::env::var_os("DISPLAY").is_some();
        if x11 && which("xclip") {
            return Ok(Self::X11);
        }
        if wayland {
            bail!("需要 wl-clipboard（Debian: sudo apt install wl-clipboard）");
        }
        if x11 {
            bail!("需要 xclip（Debian: sudo apt install xclip）");
        }
        bail!("未检测到可用的剪贴板环境（既没有 WAYLAND_DISPLAY 也没有 DISPLAY）");
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Self::Wayland => "wayland (wl-clipboard)",
            Self::X11 => "x11 (xclip)",
        }
    }
}

fn which(binary: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file())
}

/// `Ok(None)` means the selection exists but carries no text.
pub async fn read_text(backend: Backend) -> Result<Option<String>> {
    let (program, args) = match backend {
        Backend::Wayland => {
            let Some(mime) = wayland_text_mime().await else {
                return Ok(None);
            };
            (
                "wl-paste",
                vec![
                    "--no-newline".to_string(),
                    "--type".to_string(),
                    mime,
                ],
            )
        }
        Backend::X11 => (
            "xclip",
            vec![
                "-selection".to_string(),
                "clipboard".to_string(),
                "-o".to_string(),
                "-t".to_string(),
                "UTF8_STRING".to_string(),
            ],
        ),
    };
    let output = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("启动剪贴板读取命令失败")?;
    if !output.status.success() {
        return Ok(None);
    }
    match String::from_utf8(output.stdout) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

/// Applications disagree on the exact text MIME type; ask what is on offer
/// instead of guessing so an image-only selection is skipped rather than
/// decoded as UTF-8.
async fn wayland_text_mime() -> Option<String> {
    let output = Command::new("wl-paste")
        .arg("--list-types")
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let offered = String::from_utf8_lossy(&output.stdout);
    for preferred in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT"] {
        if offered.lines().any(|line| line.trim() == preferred) {
            return Some(preferred.to_string());
        }
    }
    offered
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("text/"))
        .map(str::to_string)
}

pub async fn write_text(backend: Backend, text: &str) -> Result<()> {
    let mut command = match backend {
        Backend::Wayland => {
            let mut command = Command::new("wl-copy");
            command.arg("--type").arg("text/plain");
            command
        }
        Backend::X11 => {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard", "-i", "-t", "UTF8_STRING"]);
            command
        }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("启动剪贴板写入命令失败")?;
    let mut stdin = child.stdin.take().context("剪贴板写入命令缺少 stdin")?;
    stdin.write_all(text.as_bytes()).await?;
    drop(stdin);
    let status = child.wait().await?;
    if !status.success() {
        bail!("剪贴板写入命令返回 {}", status);
    }
    Ok(())
}
