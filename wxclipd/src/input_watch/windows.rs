use std::cell::RefCell;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LCONTROL, VK_RCONTROL};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KillTimer, SetTimer, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

struct WatchState {
    events: mpsc::Sender<()>,
    debounce: Duration,
    last: Option<Instant>,
    ctrl_down: u8,
}

thread_local! {
    static WATCH: RefCell<Option<WatchState>> = const { RefCell::new(None) };
}

/// Hooks live on a dedicated message-loop thread. Callbacks only inspect the
/// required key/button code and try to enqueue a hint; they never wait on I/O.
pub fn spawn(events: mpsc::Sender<()>, debounce: Duration) -> usize {
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("wxclipd-input".into())
        .spawn(move || {
            WATCH.with(|slot| {
                *slot.borrow_mut() = Some(WatchState {
                    events: events.clone(),
                    debounce,
                    last: None,
                    ctrl_down: 0,
                });
            });
            let module = unsafe { GetModuleHandleW(std::ptr::null()) };
            let keyboard =
                Hook(unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) });
            let mouse =
                Hook(unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) });
            let count = usize::from(!keyboard.0.is_null()) + usize::from(!mouse.0.is_null());
            if count < 2 {
                eprintln!(
                    "[wxclipd] Windows 输入监听只启用了 {count}/2 项；仍可使用定时同步和 sync-now"
                );
            }
            if ready_tx.send(count).is_err() || count == 0 {
                return;
            }
            // Wake periodically so a dropped receiver shuts down the thread
            // even when the user is not generating keyboard/mouse events.
            let timer = unsafe { SetTimer(std::ptr::null_mut(), 0, 1000, None) };
            let mut message: MSG = unsafe { std::mem::zeroed() };
            while !events.is_closed()
                && unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0
            {
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if timer != 0 {
                unsafe { KillTimer(std::ptr::null_mut(), timer) };
            }
            WATCH.with(|slot| *slot.borrow_mut() = None);
            // Hook guards are dropped on the same thread that installed them.
        });
    if let Err(error) = thread {
        eprintln!("[wxclipd] 无法启动 Windows 输入监听线程: {error}");
        return 0;
    }
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(0)
}

struct Hook(HHOOK);

impl Drop for Hook {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { UnhookWindowsHookEx(self.0) };
        }
    }
}

fn notify_paste_hint() {
    WATCH.with(|slot| {
        let Ok(mut state) = slot.try_borrow_mut() else {
            return;
        };
        let Some(state) = state.as_mut() else {
            return;
        };
        if state
            .last
            .is_some_and(|last| last.elapsed() < state.debounce)
        {
            return;
        }
        match state.events.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => state.last = Some(Instant::now()),
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    });
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let message = wparam as u32;
        if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
            let key = &*(lparam as *const KBDLLHOOKSTRUCT);
            let bit = match key.vkCode {
                value if value == u32::from(VK_LCONTROL) => 1,
                value if value == u32::from(VK_RCONTROL) => 2,
                value if value == u32::from(VK_CONTROL) => 4,
                _ => 0,
            };
            if bit != 0 {
                let pressed = WATCH.with(|slot| {
                    let Ok(mut state) = slot.try_borrow_mut() else {
                        return false;
                    };
                    let Some(state) = state.as_mut() else {
                        return false;
                    };
                    if matches!(message, WM_KEYUP | WM_SYSKEYUP) {
                        state.ctrl_down &= !bit;
                        false
                    } else {
                        let newly_pressed = state.ctrl_down & bit == 0;
                        state.ctrl_down |= bit;
                        newly_pressed
                    }
                });
                if pressed {
                    notify_paste_hint();
                }
            }
        }
    }
    // This feature observes input; it never swallows or delays an application key.
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam as u32 == WM_RBUTTONDOWN {
        notify_paste_hint();
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}
