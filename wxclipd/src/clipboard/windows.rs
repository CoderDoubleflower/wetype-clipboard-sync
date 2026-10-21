use super::{Payload, Snapshot};
use crate::image::{ClipboardImage, MAX_DIMENSION, MAX_ENCODED_BYTES, MAX_RGBA_BYTES};
use anyhow::{bail, Context, Result};
use arboard::{Clipboard, Error as ClipboardError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

// Native access and cache changes are serialized on blocking workers. Image
// decoding never holds the Windows clipboard open or blocks the async runtime.
static CLIPBOARD_ACCESS: Mutex<ClipboardCache> = Mutex::new(ClipboardCache {
    snapshot: None,
    last_unsupported_warning: None,
});
static PNG_FORMAT: OnceLock<u32> = OnceLock::new();
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_DIBV5: u32 = 17;
const MAX_COMPARE_BYTES: usize = 32 * 1024 * 1024;
// Allow bounded headers, palettes and color profiles in addition to pixels.
const MAX_DIB_BYTES: usize = MAX_RGBA_BYTES as usize + 1024 * 1024;
const DIBV5_HEADER_BYTES: usize = 124;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Windows,
}

impl Backend {
    pub fn detect() -> Result<Self> {
        Ok(Self::Windows)
    }

    pub fn describe(&self) -> &'static str {
        "windows (Unicode text / PNG / DIB images)"
    }
}

struct ClipboardCache {
    snapshot: Option<Snapshot>,
    last_unsupported_warning: Option<Instant>,
}

impl ClipboardCache {
    fn get(&self, revision: u32) -> Option<Snapshot> {
        self.snapshot
            .as_ref()
            .filter(|snapshot| snapshot.revision == Some(revision))
            .cloned()
    }

    fn save(&mut self, snapshot: Snapshot) -> Snapshot {
        self.snapshot = Some(snapshot.clone());
        snapshot
    }

    fn unsupported(&mut self, revision: u32, reason: &str) -> Snapshot {
        if self
            .last_unsupported_warning
            .is_none_or(|previous| previous.elapsed() >= Duration::from_secs(30))
        {
            eprintln!("[wxclipd] 跳过不支持或超出限制的本地剪贴板内容：{reason}");
            self.last_unsupported_warning = Some(Instant::now());
        }
        self.save(Snapshot {
            content: None,
            revision: Some(revision),
        })
    }
}

fn retry_occupied<T>(mut operation: impl FnMut() -> Result<T, ClipboardError>) -> Result<T> {
    for attempt in 0..6 {
        match operation() {
            Err(ClipboardError::ClipboardOccupied) if attempt < 5 => {
                std::thread::sleep(Duration::from_millis(10 << attempt));
            }
            result => return result.context("访问 Windows 剪贴板失败"),
        }
    }
    unreachable!()
}

fn png_format() -> Result<u32, ClipboardError> {
    if let Some(&format) = PNG_FORMAT.get() {
        return Ok(format);
    }
    let name: Vec<u16> = "PNG\0".encode_utf16().collect();
    let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    if format == 0 {
        return Err(native_error("RegisterClipboardFormatW(PNG)"));
    }
    let _ = PNG_FORMAT.set(format);
    Ok(format)
}

pub async fn read_snapshot(_backend: Backend) -> Result<Snapshot> {
    tokio::task::spawn_blocking(|| {
        let mut cache = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        read_snapshot_blocking(&mut cache)
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

enum ImageCandidate {
    Cached(Snapshot),
    Changed,
    Missing(u32),
    Bytes { revision: u32, bytes: Vec<u8> },
    Unsupported { revision: u32, reason: &'static str },
}

enum Blob {
    Bytes(Vec<u8>),
    Unsupported(&'static str),
}

fn read_image_candidate(
    format: u32,
    limit: usize,
    expected_revision: Option<u32>,
    cache: &ClipboardCache,
) -> Result<ImageCandidate> {
    retry_occupied(|| {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            return Err(ClipboardError::ClipboardOccupied);
        }
        let _open = OpenClipboardGuard;
        let revision = unsafe { GetClipboardSequenceNumber() };
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Ok(ImageCandidate::Changed);
        }
        if let Some(snapshot) = cache.get(revision) {
            return Ok(ImageCandidate::Cached(snapshot));
        }
        if unsafe { IsClipboardFormatAvailable(format) } == 0 {
            return Ok(ImageCandidate::Missing(revision));
        }
        let blob = unsafe { read_locked_blob(format, limit) }?;
        // GetClipboardData can materialize a delayed-rendered format. Associate
        // the copied bytes with the sequence AFTER rendering, while still open.
        let revision = unsafe { GetClipboardSequenceNumber() };
        Ok(match blob {
            Blob::Bytes(bytes) => ImageCandidate::Bytes { revision, bytes },
            Blob::Unsupported(reason) => ImageCandidate::Unsupported { revision, reason },
        })
    })
}

fn read_snapshot_blocking(cache: &mut ClipboardCache) -> Result<Snapshot> {
    let png = png_format()?;
    'snapshot: for _ in 0..3 {
        let revision = unsafe { GetClipboardSequenceNumber() };
        // Zero also denotes missing window-station permissions. Never trust an
        // unlocked zero as a cache hit; an actual OpenClipboard must succeed.
        if revision != 0 {
            if let Some(snapshot) = cache.get(revision) {
                return Ok(snapshot);
            }
        }
        let mut observed_revision = None;
        let mut image_errors = Vec::new();
        for (format, label, limit) in [
            (png, "PNG", MAX_ENCODED_BYTES as usize),
            (CF_DIBV5, "DIBV5", MAX_DIB_BYTES),
            (CF_DIB, "DIB", MAX_DIB_BYTES),
        ] {
            match read_image_candidate(format, limit, observed_revision, cache)? {
                ImageCandidate::Cached(snapshot) => return Ok(snapshot),
                ImageCandidate::Changed => continue 'snapshot,
                ImageCandidate::Missing(revision) => observed_revision = Some(revision),
                ImageCandidate::Unsupported { revision, reason } => {
                    observed_revision = Some(revision);
                    image_errors.push(format!("{label}: {reason}"));
                }
                ImageCandidate::Bytes { revision, bytes } => {
                    observed_revision = Some(revision);
                    #[cfg(test)]
                    tests::IMAGE_DECODES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The native clipboard is already closed here. A large
                    // bitmap cannot prevent another application from copying.
                    let image = if format == png {
                        ClipboardImage::from_encoded(&bytes)
                    } else {
                        ClipboardImage::from_dib(&bytes)
                    };
                    match image {
                        Ok(image) => {
                            return Ok(cache.save(Snapshot {
                                content: Some(Payload::Image(Arc::new(image))),
                                revision: Some(revision),
                            }));
                        }
                        Err(error)
                            if error
                                .chain()
                                .any(|cause| cause.is::<std::collections::TryReserveError>()) =>
                        {
                            return Err(error).context("分配 Windows 图片剪贴板缓冲区失败");
                        }
                        Err(error) => image_errors.push(format!("{label}: {error}")),
                    }
                }
            }
        }
        if !image_errors.is_empty() {
            // Unsupported images still have a usable revision for conflict
            // detection. Do not accidentally upload a browser's accompanying
            // URL/text, and do not repeat an expensive decode every poll.
            return Ok(cache.unsupported(observed_revision.unwrap(), &image_errors.join("; ")));
        }
        let text = retry_occupied(|| {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
                return Err(ClipboardError::ClipboardOccupied);
            }
            let _open = OpenClipboardGuard;
            if Some(unsafe { GetClipboardSequenceNumber() }) != observed_revision {
                return Ok(None);
            }
            let text = unsafe { read_locked_text() };
            let revision = unsafe { GetClipboardSequenceNumber() };
            Ok(Some((revision, text)))
        })?;
        match text {
            None => continue 'snapshot,
            Some((revision, Ok(text))) => {
                return Ok(cache.save(Snapshot {
                    content: text.map(Payload::Text),
                    revision: Some(revision),
                }));
            }
            Some((revision, Err(ClipboardError::ConversionFailure))) => {
                return Ok(cache.unsupported(revision, "Unicode 文本无效或超过大小限制"));
            }
            Some((_, Err(error))) => return Err(error).context("读取 Windows 文本剪贴板失败"),
        }
    }
    bail!("剪贴板在读取期间持续变化，请稍后重试")
}

/// Compare the revision and publish under one native lock. Never associate a
/// later local copy's revision with the content that we just wrote.
pub async fn write_if_unchanged(
    _backend: Backend,
    expected: &Snapshot,
    content: &Payload,
) -> Result<Option<Snapshot>> {
    let expected = expected.clone();
    let content = content.clone();
    tokio::task::spawn_blocking(move || {
        // Conversion and every global allocation precede EmptyClipboard.
        let mut prepared = PreparedPayload::new(&content)?;
        let owner = ClipboardOwner::new()?;
        let mut cache = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        let written = retry_occupied(|| {
            if unsafe { OpenClipboard(owner.0) } == 0 {
                return Err(ClipboardError::ClipboardOccupied);
            }
            let _open = OpenClipboardGuard;
            let revision = unsafe { GetClipboardSequenceNumber() };
            if expected.revision != Some(revision) {
                return Ok(None);
            }
            cache.snapshot = None;
            unsafe { prepared.publish() }?;
            let snapshot = Snapshot {
                content: Some(content.clone()),
                revision: Some(unsafe { GetClipboardSequenceNumber() }),
            };
            Ok(Some(cache.save(snapshot)))
        })?;
        // Closing can synthesize formats and advance the sequence. Dispose of
        // the owner too before observing the final clipboard revision.
        drop(owner);
        Ok(written.map(|snapshot| refresh_written_snapshot(&mut cache, snapshot)))
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

fn refresh_written_snapshot(cache: &mut ClipboardCache, written: Snapshot) -> Snapshot {
    // A successful write must stay successful even if another application
    // immediately occupies the clipboard. Keep the committed snapshot in that
    // case. If another application copied different content, retain its real
    // snapshot in the cache so the next observation sees the user's new value.
    if let Ok(current) = read_snapshot_blocking(cache) {
        if current.content == written.content {
            // Reuse the original image/encoding rather than a re-encoded DIB.
            return cache.save(Snapshot {
                content: written.content,
                revision: current.revision,
            });
        }
    }
    written
}

async fn on_clipboard<T: Send + 'static>(
    operation: impl FnMut(&mut Clipboard) -> Result<T, ClipboardError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let mut cache = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        // These compatibility calls do not produce a full image/text snapshot.
        cache.snapshot = None;
        let mut operation = operation;
        retry_occupied(|| {
            let mut clipboard = Clipboard::new()?;
            operation(&mut clipboard)
        })
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

pub async fn read_text(_backend: Backend) -> Result<Option<String>> {
    on_clipboard(|clipboard| match clipboard.get_text() {
        Ok(text) => Ok(Some(text)),
        Err(ClipboardError::ContentNotAvailable) => Ok(None),
        Err(error) => Err(error),
    })
    .await
}

pub async fn write_text(_backend: Backend, text: &str) -> Result<()> {
    if text.contains('\0') {
        bail!("Windows 文本剪贴板不能保存 NUL 字符");
    }
    let text = text.to_owned();
    on_clipboard(move |clipboard| clipboard.set_text(text.as_str())).await
}

pub async fn write_text_if_unchanged(
    _backend: Backend,
    expected: &Option<String>,
    text: &str,
) -> Result<bool> {
    if text.contains('\0') {
        bail!("Windows 文本剪贴板不能保存 NUL 字符");
    }
    let expected = expected.clone();
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        let content = Payload::Text(text);
        let mut prepared = PreparedPayload::new(&content)?;
        let owner = ClipboardOwner::new()?;
        let mut cache = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        let png = png_format()?;
        let written = retry_occupied(|| {
            if unsafe { OpenClipboard(owner.0) } == 0 {
                return Err(ClipboardError::ClipboardOccupied);
            }
            let _open = OpenClipboardGuard;
            // None means absent text, not permission to replace an image.
            if [png, CF_DIBV5, CF_DIB]
                .iter()
                .any(|&format| unsafe { IsClipboardFormatAvailable(format) } != 0)
            {
                return Ok(false);
            }
            if unsafe { read_locked_text() }? != expected {
                return Ok(false);
            }
            cache.snapshot = None;
            unsafe { prepared.publish() }?;
            cache.save(Snapshot {
                content: Some(content.clone()),
                revision: Some(unsafe { GetClipboardSequenceNumber() }),
            });
            Ok(true)
        })?;
        drop(owner);
        if written {
            if let Some(snapshot) = cache.snapshot.clone() {
                refresh_written_snapshot(&mut cache, snapshot);
            }
        }
        Ok(written)
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

struct ClipboardOwner(HWND);

impl ClipboardOwner {
    fn new() -> Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return Err(std::io::Error::last_os_error()).context("创建剪贴板窗口失败");
        }
        Ok(Self(window))
    }
}

impl Drop for ClipboardOwner {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

struct OpenClipboardGuard;
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

struct GlobalLockGuard(HGLOBAL);
impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        unsafe { GlobalUnlock(self.0) };
    }
}

struct GlobalMemory(HGLOBAL);
impl GlobalMemory {
    fn from_bytes(bytes: &[u8]) -> Result<Self, ClipboardError> {
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
        if memory.is_null() {
            return Err(native_error("GlobalAlloc"));
        }
        let allocation = Self(memory);
        let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
        if pointer.is_null() {
            return Err(native_error("GlobalLock"));
        }
        let _unlock = GlobalLockGuard(memory);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
        Ok(allocation)
    }

    unsafe fn publish(&mut self, format: u32) -> Result<(), ClipboardError> {
        if SetClipboardData(format, self.0).is_null() {
            return Err(native_error("SetClipboardData"));
        }
        // Windows owns this allocation after a successful SetClipboardData.
        self.0 = std::ptr::null_mut();
        Ok(())
    }
}

impl Drop for GlobalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { GlobalFree(self.0) };
        }
    }
}

struct PreparedPayload {
    primary_format: u32,
    primary: GlobalMemory,
    png: Option<(u32, GlobalMemory)>,
}

impl PreparedPayload {
    fn new(content: &Payload) -> Result<Self> {
        match content {
            Payload::Text(text) => {
                if text.contains('\0') {
                    bail!("Windows 文本剪贴板不能保存 NUL 字符");
                }
                let length = text
                    .encode_utf16()
                    .count()
                    .checked_add(1)
                    .and_then(|units| units.checked_mul(2))
                    .filter(|&bytes| bytes <= MAX_COMPARE_BYTES)
                    .context("Windows 文本剪贴板超过大小限制")?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(length)
                    .context("分配文本剪贴板内存失败")?;
                bytes.extend(
                    text.encode_utf16()
                        .chain(std::iter::once(0))
                        .flat_map(u16::to_le_bytes),
                );
                Ok(Self {
                    primary_format: CF_UNICODETEXT,
                    primary: GlobalMemory::from_bytes(&bytes)?,
                    png: None,
                })
            }
            Payload::Image(image) => {
                let dib = dibv5_bytes(image)?;
                let primary = GlobalMemory::from_bytes(&dib)?;
                drop(dib);
                let png = image
                    .png()
                    .map(|bytes| -> Result<_> {
                        if bytes.len() > MAX_ENCODED_BYTES as usize {
                            bail!("PNG 剪贴板内容超过大小限制");
                        }
                        Ok((png_format()?, GlobalMemory::from_bytes(bytes)?))
                    })
                    .transpose()?;
                Ok(Self {
                    primary_format: CF_DIBV5,
                    primary,
                    png,
                })
            }
        }
    }

    unsafe fn publish(&mut self) -> Result<(), ClipboardError> {
        if EmptyClipboard() == 0 {
            return Err(native_error("EmptyClipboard"));
        }
        self.primary.publish(self.primary_format)?;
        if let Some((format, memory)) = &mut self.png {
            if let Err(error) = memory.publish(*format) {
                // The required DIBV5 is already valid. A missing optional PNG
                // flavor must not turn a successful image write into a retry.
                eprintln!("[wxclipd] PNG 剪贴板格式写入失败，已保留 DIBV5 图片：{error}");
            }
        }
        Ok(())
    }
}

fn dibv5_bytes(image: &ClipboardImage) -> Result<Vec<u8>> {
    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 || width > MAX_DIMENSION as u32 || height > MAX_DIMENSION as u32 {
        bail!("DIBV5 图片尺寸无效或超过限制");
    }
    let pixel_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&bytes| bytes <= MAX_RGBA_BYTES as usize)
        .context("DIBV5 图片超过解码大小限制")?;
    let rgba = image.rgba()?;
    if rgba.len() != pixel_bytes {
        bail!("DIBV5 图片像素长度不匹配");
    }
    let mut dib = Vec::new();
    dib.try_reserve_exact(DIBV5_HEADER_BYTES + pixel_bytes)
        .context("分配 DIBV5 图片内存失败")?;
    dib.resize(DIBV5_HEADER_BYTES, 0);
    dib[0..4].copy_from_slice(&(DIBV5_HEADER_BYTES as u32).to_le_bytes());
    dib[4..8].copy_from_slice(&(width as i32).to_le_bytes());
    dib[8..12].copy_from_slice(&(-(height as i32)).to_le_bytes()); // top-down
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    dib[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
    dib[20..24].copy_from_slice(&(pixel_bytes as u32).to_le_bytes());
    dib[40..44].copy_from_slice(&0x00ff_0000u32.to_le_bytes());
    dib[44..48].copy_from_slice(&0x0000_ff00u32.to_le_bytes());
    dib[48..52].copy_from_slice(&0x0000_00ffu32.to_le_bytes());
    dib[52..56].copy_from_slice(&0xff00_0000u32.to_le_bytes());
    dib[56..60].copy_from_slice(&0x7352_4742u32.to_le_bytes()); // LCS_sRGB
    dib[108..112].copy_from_slice(&4u32.to_le_bytes()); // LCS_GM_IMAGES
    for pixel in rgba.chunks_exact(4) {
        dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    Ok(dib)
}

fn native_error(operation: &str) -> ClipboardError {
    ClipboardError::Unknown {
        description: format!("{operation}: {}", std::io::Error::last_os_error()),
    }
}

unsafe fn read_locked_blob(format: u32, limit: usize) -> Result<Blob, ClipboardError> {
    let memory = GetClipboardData(format);
    if memory.is_null() {
        return Err(native_error("GetClipboardData"));
    }
    let length = GlobalSize(memory);
    if length == 0 || length > limit {
        return Ok(Blob::Unsupported("格式数据为空或超过大小限制"));
    }
    let pointer = GlobalLock(memory).cast::<u8>();
    if pointer.is_null() {
        return Err(native_error("GlobalLock"));
    }
    let _unlock = GlobalLockGuard(memory);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|error| ClipboardError::Unknown {
            description: format!("分配剪贴板读取缓冲区失败：{error}"),
        })?;
    bytes.extend_from_slice(std::slice::from_raw_parts(pointer, length));
    Ok(Blob::Bytes(bytes))
}

unsafe fn read_locked_text() -> Result<Option<String>, ClipboardError> {
    if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
        return Ok(None);
    }
    let memory = GetClipboardData(CF_UNICODETEXT);
    if memory.is_null() {
        return Err(native_error("GetClipboardData"));
    }
    let bytes = GlobalSize(memory);
    if bytes == 0 || bytes > MAX_COMPARE_BYTES || bytes % 2 != 0 {
        return Err(ClipboardError::ConversionFailure);
    }
    let pointer = GlobalLock(memory).cast::<u16>();
    if pointer.is_null() {
        return Err(native_error("GlobalLock"));
    }
    let _unlock = GlobalLockGuard(memory);
    let units = std::slice::from_raw_parts(pointer, bytes / 2);
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .ok_or(ClipboardError::ConversionFailure)?;
    String::from_utf16(&units[..end])
        .map(Some)
        .map_err(|_| ClipboardError::ConversionFailure)
}

#[cfg(test)]
#[path = "windows/tests.rs"]
mod tests;
