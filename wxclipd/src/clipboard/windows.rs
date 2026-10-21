use anyhow::{bail, Context, Result};
use arboard::{Clipboard, Error as ClipboardError};
use std::sync::Mutex;
use std::time::Duration;
use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

// Serialize this process's clipboard operations, including their bounded retry.
// The mutex is held on a blocking worker, never across an async await.
static CLIPBOARD_ACCESS: Mutex<()> = Mutex::new(());
const CF_UNICODETEXT: u32 = 13;
const MAX_COMPARE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Windows,
}

impl Backend {
    pub fn detect() -> Result<Self> {
        Ok(Self::Windows)
    }

    pub fn describe(&self) -> &'static str {
        "windows (Unicode clipboard)"
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

async fn on_clipboard<T: Send + 'static>(
    operation: impl FnMut(&mut Clipboard) -> Result<T, ClipboardError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || {
        let _guard = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        let mut operation = operation;
        retry_occupied(|| {
            let mut clipboard = Clipboard::new()?;
            operation(&mut clipboard)
        })
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

/// Missing text is distinct from an inaccessible clipboard: callers must not
/// overwrite local data when another application temporarily holds the lock.
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

/// Compare and write under one Windows clipboard lock. A local copy that took
/// place while the network request or lock retry was pending wins the race.
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
        let _guard = CLIPBOARD_ACCESS
            .lock()
            .map_err(|_| anyhow::anyhow!("剪贴板工作线程异常退出"))?;
        let owner = ClipboardOwner::new()?;
        retry_occupied(|| {
            // SAFETY: owner is a live window on this thread. Closing the
            // clipboard always precedes destruction of its owner window.
            if unsafe { OpenClipboard(owner.0) } == 0 {
                return Err(ClipboardError::ClipboardOccupied);
            }
            let _open = OpenClipboardGuard;
            let current = unsafe { read_locked_text() }?;
            if current != expected {
                return Ok(false);
            }
            unsafe { write_locked_text(&text) }?;
            Ok(true)
        })
    })
    .await
    .context("Windows 剪贴板工作线程异常")?
}

struct ClipboardOwner(HWND);

impl ClipboardOwner {
    fn new() -> Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // A message-only window supplies a real clipboard owner. Passing NULL
        // to OpenClipboard before EmptyClipboard is not a supported write path.
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

fn native_error(operation: &str) -> ClipboardError {
    ClipboardError::Unknown {
        description: format!("{operation}: {}", std::io::Error::last_os_error()),
    }
}

/// Caller must hold the native clipboard lock for this entire operation.
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

/// Allocate before EmptyClipboard so allocation failure leaves the old value.
/// Ownership of the global allocation passes to Windows only on success.
unsafe fn write_locked_text(text: &str) -> Result<(), ClipboardError> {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * std::mem::size_of::<u16>());
    if memory.is_null() {
        return Err(native_error("GlobalAlloc"));
    }
    let pointer = GlobalLock(memory).cast::<u16>();
    if pointer.is_null() {
        let error = native_error("GlobalLock");
        GlobalFree(memory);
        return Err(error);
    }
    std::ptr::copy_nonoverlapping(units.as_ptr(), pointer, units.len());
    GlobalUnlock(memory);
    if EmptyClipboard() == 0 {
        let error = native_error("EmptyClipboard");
        GlobalFree(memory);
        return Err(error);
    }
    if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
        let error = native_error("SetClipboardData");
        GlobalFree(memory);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_unicode_roundtrip_and_compare_exchange_preserve_local_copy() {
        let backend = Backend::Windows;
        let original = read_text(backend).await.unwrap();
        let first = "wxclipd Windows 测试 🙂\r\n第二行";
        write_text(backend, first).await.unwrap();
        assert_eq!(read_text(backend).await.unwrap().as_deref(), Some(first));
        assert!(
            write_text_if_unchanged(backend, &Some(first.to_owned()), "远端文本 ✅")
                .await
                .unwrap()
        );
        assert_eq!(
            read_text(backend).await.unwrap().as_deref(),
            Some("远端文本 ✅")
        );
        write_text(backend, "用户刚复制的内容").await.unwrap();
        assert!(
            !write_text_if_unchanged(backend, &Some(first.to_owned()), "过期远端值")
                .await
                .unwrap()
        );
        assert_eq!(
            read_text(backend).await.unwrap().as_deref(),
            Some("用户刚复制的内容")
        );
        write_text(backend, original.as_deref().unwrap_or(""))
            .await
            .unwrap();
    }

    #[test]
    fn clipboard_occupancy_is_retried_but_unavailable_content_is_not() {
        let mut attempts = 0;
        let value = retry_occupied(|| {
            attempts += 1;
            if attempts < 3 {
                Err(ClipboardError::ClipboardOccupied)
            } else {
                Ok("ready")
            }
        })
        .unwrap();
        assert_eq!((value, attempts), ("ready", 3));
        let mut attempts = 0;
        let error = retry_occupied::<()>(|| {
            attempts += 1;
            Err(ClipboardError::ContentNotAvailable)
        });
        assert!(error.is_err());
        assert_eq!(attempts, 1);
    }
}
