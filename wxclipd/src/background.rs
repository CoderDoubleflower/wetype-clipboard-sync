//! Windows notification-area application and background worker supervisor.
#![windows_subsystem = "windows"]

#[allow(dead_code)]
mod config;
#[allow(dead_code, unused_imports)]
mod ipc;
mod tray;
#[path = "tray/model.rs"]
mod tray_model;

fn main() {
    if let Err(error) = tray::run() {
        // An installer must receive a failing exit code rather than wait for a
        // modal dialog when shutdown fails. Interactive failures remain visible.
        if !std::env::args_os().any(|arg| arg == "--quit") {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
            let text: Vec<u16> = format!("无法启动托盘程序：{error:#}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let title: Vec<u16> = "WeType 剪贴板同步".encode_utf16().chain(Some(0)).collect();
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    text.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}
