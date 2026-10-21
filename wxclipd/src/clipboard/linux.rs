//! System clipboard access through the desktop's own tools.
//!
//! Wayland and X11 both hand selection ownership to a process that must stay
//! alive, so writing goes through `wl-copy`/`xclip`, which fork and keep
//! serving the selection after the call returns.

use super::{Payload, Snapshot};
use crate::image::{ClipboardImage, MAX_ENCODED_BYTES};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

type ImageCache = Option<([u8; 32], Option<Arc<ClipboardImage>>)>;
static IMAGE_CACHE: Mutex<ImageCache> = Mutex::new(None);
static OVERSIZE_WARNING: Mutex<Option<Instant>> = Mutex::new(None);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const OVERSIZE_WARNING_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Wayland,
    X11,
}

impl Backend {
    fn reader(self) -> &'static OsStr {
        OsStr::new(match self {
            Self::Wayland => "wl-paste",
            Self::X11 => "xclip",
        })
    }

    fn writer(self) -> &'static OsStr {
        OsStr::new(match self {
            Self::Wayland => "wl-copy",
            Self::X11 => "xclip",
        })
    }

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
                vec!["--no-newline".to_string(), "--type".to_string(), mime],
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
    for preferred in [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
    ] {
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
        .kill_on_drop(true)
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

/// Prefer an image MIME over the alternate URL/HTML text many applications
/// provide alongside a copied picture. Reads are bounded before allocation.
pub async fn read_snapshot(backend: Backend) -> Result<Snapshot> {
    read_snapshot_using(backend, backend.reader()).await
}

async fn read_snapshot_using(backend: Backend, program: &OsStr) -> Result<Snapshot> {
    let offered = match backend {
        Backend::Wayland => capture(program, &["--list-types"], 64 * 1024).await,
        Backend::X11 => {
            capture(
                program,
                &["-selection", "clipboard", "-o", "-t", "TARGETS"],
                64 * 1024,
            )
            .await
        }
    };
    let offered = match offered {
        Ok(offered) => offered,
        Err(error) if is_empty_selection(backend, &error) => None,
        Err(error) => return Err(error),
    };
    let Some(offered) = offered else {
        return Ok(Snapshot {
            content: None,
            revision: None,
        });
    };
    let offered = String::from_utf8_lossy(&offered);
    let Some((mime, is_image)) = preferred_mime(&offered) else {
        return Ok(Snapshot {
            content: None,
            revision: None,
        });
    };
    let limit = if is_image {
        MAX_ENCODED_BYTES
    } else {
        1024 * 1024
    };
    let bytes = match backend {
        Backend::Wayland => capture(program, &["--no-newline", "--type", mime], limit).await?,
        Backend::X11 => {
            capture(
                program,
                &["-selection", "clipboard", "-o", "-t", mime],
                limit,
            )
            .await?
        }
    };
    let content = match bytes {
        None => None,
        Some(bytes) if is_image => {
            let image = tokio::task::spawn_blocking(move || decode_cached(&bytes))
                .await
                .context("图片解码任务失败")??;
            image.map(Payload::Image)
        }
        Some(bytes) => String::from_utf8(bytes).ok().map(Payload::Text),
    };
    Ok(Snapshot {
        content,
        revision: None,
    })
}

fn preferred_mime(offered: &str) -> Option<(&str, bool)> {
    for preferred in ["image/png", "image/jpeg", "image/jpg"] {
        if let Some(mime) = offered
            .lines()
            .map(str::trim)
            .find(|line| *line == preferred)
        {
            return Some((mime, true));
        }
    }
    for preferred in [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
        "STRING",
    ] {
        if let Some(mime) = offered
            .lines()
            .map(str::trim)
            .find(|line| *line == preferred)
        {
            return Some((mime, false));
        }
    }
    None
}

fn decode_cached(bytes: &[u8]) -> Result<Option<Arc<ClipboardImage>>> {
    let mut cache = IMAGE_CACHE
        .lock()
        .map_err(|_| anyhow::anyhow!("图片缓存锁不可用"))?;
    decode_with_cache(&mut cache, bytes, ClipboardImage::from_encoded)
}

fn decode_with_cache(
    cache: &mut ImageCache,
    bytes: &[u8],
    decode: impl FnOnce(&[u8]) -> Result<ClipboardImage>,
) -> Result<Option<Arc<ClipboardImage>>> {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    if let Some((previous, image)) = cache.as_ref() {
        if *previous == digest {
            return Ok(image.clone());
        }
    }
    let image = match decode(bytes) {
        Ok(image) => Some(Arc::new(image)),
        Err(error) if retryable_decode_error(&error) => return Err(error),
        Err(error) => {
            eprintln!("[wxclipd] 当前剪贴板图片无法同步: {error:#}");
            None
        }
    };
    *cache = Some((digest, image.clone()));
    Ok(image)
}

fn retryable_decode_error(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        cause.is::<std::collections::TryReserveError>()
            || cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::OutOfMemory)
    }) {
        return true;
    }
    // An in-memory decoder's UnexpectedEof means malformed input. Pure IO
    // errors from resources/encoders must remain retryable, without poisoning
    // the cache for this content after the resource becomes available again.
    !error.chain().any(|cause| cause.is::<::image::ImageError>())
        && error.chain().any(|cause| cause.is::<std::io::Error>())
}

#[derive(Debug)]
struct CommandFailure {
    status: ExitStatus,
    diagnostic: String,
}

impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "剪贴板读取命令返回 {}: {}", self.status, self.diagnostic)
    }
}

impl std::error::Error for CommandFailure {}

fn is_empty_selection(backend: Backend, error: &anyhow::Error) -> bool {
    let Some(error) = error.downcast_ref::<CommandFailure>() else {
        return false;
    };
    let diagnostic = error.diagnostic.trim();
    match backend {
        Backend::Wayland => diagnostic == "Nothing is copied",
        Backend::X11 => diagnostic == "xclip: Error: There is no owner for the CLIPBOARD selection",
    }
}

fn should_warn_oversize(previous: &mut Option<Instant>, now: Instant) -> bool {
    if previous.is_some_and(|last| now.duration_since(last) < OVERSIZE_WARNING_INTERVAL) {
        return false;
    }
    *previous = Some(now);
    true
}

fn warn_oversized(limit: usize) {
    if OVERSIZE_WARNING
        .lock()
        .map(|mut previous| should_warn_oversize(&mut previous, Instant::now()))
        .unwrap_or(false)
    {
        eprintln!("[wxclipd] 当前剪贴板数据超过读取限制（{limit} 字节），已跳过并停止发送旧内容；请缩小内容后重新复制");
    }
}

async fn capture(program: &OsStr, args: &[&str], limit: usize) -> Result<Option<Vec<u8>>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("启动剪贴板读取命令失败")?;
    let stdout = child.stdout.take().context("剪贴板命令缺少 stdout")?;
    let stderr = child.stderr.take().context("剪贴板命令缺少 stderr")?;
    timeout(COMMAND_TIMEOUT, async {
        let output = async {
            let mut bytes = Vec::new();
            stdout
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > limit {
                child.kill().await?;
                return Ok::<_, anyhow::Error>((None, None));
            }
            let status = child.wait().await?;
            Ok((Some(bytes), Some(status)))
        };
        let diagnostic = async {
            let mut bytes = Vec::new();
            stderr.take(4096).read_to_end(&mut bytes).await?;
            Ok::<_, anyhow::Error>(String::from_utf8_lossy(&bytes).trim().to_string())
        };
        let ((bytes, status), diagnostic) = tokio::try_join!(output, diagnostic)?;
        let Some(bytes) = bytes else {
            // An unsupported local value still supersedes the previous one.
            // Do not retain or re-upload a stale text/image outbox.
            warn_oversized(limit);
            return Ok(None);
        };
        let status = status.expect("bounded output has an exit status");
        if !status.success() {
            return Err(CommandFailure { status, diagnostic }.into());
        }
        Ok(Some(bytes))
    })
    .await
    .context("读取剪贴板超时")?
}

pub async fn write_if_unchanged(
    backend: Backend,
    expected: &Snapshot,
    content: &Payload,
) -> Result<Option<Snapshot>> {
    write_if_unchanged_using(
        backend,
        expected,
        content,
        backend.reader(),
        backend.writer(),
    )
    .await
}

async fn prepare_written_snapshot(content: &Payload) -> Result<Snapshot> {
    let content = match content {
        Payload::Text(text) => Payload::Text(text.clone()),
        Payload::Image(image) => {
            // The transmitted JPEG may differ from from_rgba's native bitmap.
            // Prepare the actual encoded-image baseline before any side effect.
            let image = image.clone();
            let observed = tokio::task::spawn_blocking(move || decode_cached(image.encoded()))
                .await
                .context("准备图片写入基线失败")??
                .context("准备写入的图片编码无效")?;
            Payload::Image(observed)
        }
    };
    Ok(Snapshot {
        content: Some(content),
        revision: None,
    })
}

async fn write_if_unchanged_using(
    backend: Backend,
    expected: &Snapshot,
    content: &Payload,
    reader: &OsStr,
    writer: &OsStr,
) -> Result<Option<Snapshot>> {
    let written = prepare_written_snapshot(content).await?;
    // Preparation may decode a large JPEG. Compare only after it is finished
    // so a local copy made during preparation wins before the write starts.
    if read_snapshot_using(backend, reader).await?.content != expected.content {
        return Ok(None);
    }
    write_payload_using(backend, content, writer).await?;
    // A committed write must not become an apparent failure because a later
    // decode/read fails, or the next observation would echo our own image.
    Ok(Some(written))
}

async fn write_payload_using(backend: Backend, content: &Payload, program: &OsStr) -> Result<()> {
    let (bytes, mime) = match content {
        Payload::Text(text) => (
            text.as_bytes(),
            match backend {
                Backend::Wayland => "text/plain",
                Backend::X11 => "UTF8_STRING",
            },
        ),
        Payload::Image(image) => (
            image.encoded(),
            if image.png().is_some() {
                "image/png"
            } else {
                "image/jpeg"
            },
        ),
    };
    let mut command = Command::new(program);
    match backend {
        Backend::Wayland => {
            command.args(["--type", mime]);
        }
        Backend::X11 => {
            command.args(["-selection", "clipboard", "-i", "-t", mime]);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("启动剪贴板写入命令失败")?;
    let mut stdin = child.stdin.take().context("剪贴板写入命令缺少 stdin")?;
    timeout(COMMAND_TIMEOUT, async {
        stdin.write_all(bytes).await?;
        drop(stdin);
        if !child.wait().await?.success() {
            bail!("剪贴板写入失败");
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("写入剪贴板超时")??;
    Ok(())
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn picture() -> ClipboardImage {
        ClipboardImage::from_rgba(1, 1, vec![12, 34, 56, 255]).unwrap()
    }

    #[test]
    fn images_take_priority_over_alternate_url_text() {
        assert_eq!(
            preferred_mime("UTF8_STRING\ntext/html\nimage/png\n"),
            Some(("image/png", true))
        );
        assert_eq!(
            preferred_mime("image/jpeg\ntext/plain\n"),
            Some(("image/jpeg", true))
        );
        assert_eq!(
            preferred_mime("text/plain;charset=utf-8\n"),
            Some(("text/plain;charset=utf-8", false))
        );
        assert_eq!(preferred_mime("application/octet-stream\n"), None);
    }

    #[tokio::test]
    async fn process_output_is_bounded() {
        assert_eq!(
            capture(OsStr::new("sh"), &["-c", "printf 12345"], 4)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            capture(OsStr::new("sh"), &["-c", "printf hello"], 5)
                .await
                .unwrap(),
            Some(b"hello".to_vec())
        );
    }

    #[tokio::test]
    async fn temporary_tool_failure_is_not_a_successful_empty_observation() {
        let error = capture(
            OsStr::new("sh"),
            &["-c", "printf 'connection failed' >&2; exit 9"],
            32,
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<CommandFailure>().is_some());
        assert!(!is_empty_selection(Backend::Wayland, &error));
        assert!(!is_empty_selection(Backend::X11, &error));
        // The sync loop observes only Ok snapshots; returning Err preserves
        // both its latest outbox and an OutcomeUnknown pause.
        let directory = tempfile::tempdir().unwrap();
        let reader = script(
            directory.path(),
            "reader",
            r#"
if [ "$1" = --list-types ]; then
    printf 'text/plain\n'
else
    printf 'selection owner temporarily unavailable\n' >&2
    exit 7
fi
"#,
        );
        assert!(read_snapshot_using(Backend::Wayland, reader.as_os_str())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn only_explicit_absence_is_an_empty_selection() {
        let directory = tempfile::tempdir().unwrap();
        let empty = script(
            directory.path(),
            "empty-wayland",
            "printf 'Nothing is copied\\n' >&2; exit 1",
        );
        assert_eq!(
            read_snapshot_using(Backend::Wayland, empty.as_os_str())
                .await
                .unwrap()
                .content,
            None
        );
        let absent = script(
            directory.path(),
            "empty-x11",
            "printf 'xclip: Error: There is no owner for the CLIPBOARD selection\\n' >&2; exit 1",
        );
        assert_eq!(
            read_snapshot_using(Backend::X11, absent.as_os_str())
                .await
                .unwrap()
                .content,
            None
        );
        let ambiguous = script(
            directory.path(),
            "ambiguous-x11",
            "printf 'Error: target TARGETS not available\\n' >&2; exit 1",
        );
        assert!(read_snapshot_using(Backend::X11, ambiguous.as_os_str())
            .await
            .is_err());
    }

    #[test]
    fn transient_image_resource_failure_does_not_poison_the_content_cache() {
        let image = picture();
        let mut cache = None;
        let reserve_error = Vec::<u8>::new().try_reserve(usize::MAX).unwrap_err();
        assert!(
            decode_with_cache(&mut cache, image.encoded(), |_| Err(reserve_error.into())).is_err()
        );
        assert!(cache.is_none());
        assert!(
            decode_with_cache(&mut cache, image.encoded(), |_| Err(std::io::Error::other(
                "temporary resource failure"
            )
            .into()))
            .is_err()
        );
        assert!(cache.is_none());
        assert!(
            decode_with_cache(&mut cache, image.encoded(), ClipboardImage::from_encoded)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn deterministic_decoder_eof_is_negatively_cached() {
        let mut cache = None;
        let truncated = b"\x89PNG\r\n\x1a\n";
        assert!(
            decode_with_cache(&mut cache, truncated, ClipboardImage::from_encoded)
                .unwrap()
                .is_none()
        );
        assert!(cache.is_some());
        assert!(decode_with_cache(&mut cache, truncated, |_| panic!(
            "invalid bytes should not be decoded again"
        ))
        .unwrap()
        .is_none());
    }

    #[test]
    fn oversized_content_diagnostics_are_rate_limited() {
        let now = Instant::now();
        let mut previous = None;
        assert!(should_warn_oversize(&mut previous, now));
        assert!(!should_warn_oversize(
            &mut previous,
            now + Duration::from_secs(1)
        ));
        assert!(!should_warn_oversize(
            &mut previous,
            now + Duration::from_secs(59)
        ));
        assert!(should_warn_oversize(
            &mut previous,
            now + Duration::from_secs(60)
        ));
    }

    #[tokio::test]
    async fn jpeg_write_returns_the_encoded_pixel_baseline_without_post_commit_reads() {
        let directory = tempfile::tempdir().unwrap();
        let reader = script(
            directory.path(),
            "reader",
            r#"
task_dir=$(dirname "$0")
if [ -f "$task_dir/written" ]; then
    printf 'reader fails after commit\n' >&2
    exit 7
fi
if [ "$1" = --list-types ]; then printf 'text/plain\n'; else printf before; fi
"#,
        );
        let writer = script(
            directory.path(),
            "writer",
            r#"cat >"$(dirname "$0")/written""#,
        );
        let mut jpeg = Vec::new();
        ::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(
                &[220, 80, 20, 10, 150, 30],
                2,
                1,
                ::image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        let payload = Payload::Image(Arc::new(ClipboardImage::from_encoded(&jpeg).unwrap()));
        let expected = Snapshot {
            content: Some(Payload::Text("before".into())),
            revision: None,
        };
        let written = write_if_unchanged_using(
            Backend::Wayland,
            &expected,
            &payload,
            reader.as_os_str(),
            writer.as_os_str(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("written")).unwrap(),
            jpeg
        );
        let actual = ClipboardImage::from_encoded(&jpeg).unwrap();
        assert_eq!(written.content, Some(Payload::Image(Arc::new(actual))));
        // Returning this snapshot makes a later observation of the encoded
        // image equal to the baseline, including a lossy JPEG representation.
        assert_eq!(prepare_written_snapshot(&payload).await.unwrap(), written);
    }

    #[tokio::test]
    async fn fresh_local_copy_prevents_the_prepared_remote_image_write() {
        let directory = tempfile::tempdir().unwrap();
        let reader = script(
            directory.path(),
            "reader",
            r#"
if [ "$1" = --list-types ]; then printf 'text/plain\n'; else printf newer; fi
"#,
        );
        let writer = script(
            directory.path(),
            "writer",
            r#"cat >"$(dirname "$0")/written""#,
        );
        let expected = Snapshot {
            content: Some(Payload::Text("before".into())),
            revision: None,
        };
        let payload = Payload::Image(Arc::new(picture()));
        assert!(write_if_unchanged_using(
            Backend::Wayland,
            &expected,
            &payload,
            reader.as_os_str(),
            writer.as_os_str()
        )
        .await
        .unwrap()
        .is_none());
        assert!(!directory.path().join("written").exists());
    }
}

/// Linux's clipboard tools do not provide an atomic compare-and-set operation.
/// Recheck immediately before writing; Windows implements this under one lock.
pub async fn write_text_if_unchanged(
    backend: Backend,
    expected: &Option<String>,
    text: &str,
) -> Result<bool> {
    if read_text(backend).await?.as_ref() != expected.as_ref() {
        return Ok(false);
    }
    write_text(backend, text).await?;
    Ok(true)
}
