//! Small Windows GUI-subsystem launcher: a detached worker with a user-owned log.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[allow(dead_code, unused_imports)]
mod ipc;

#[cfg(windows)]
fn main() {
    if let Err(error) = launch() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let text: Vec<u16> = format!("无法启动剪贴板同步：{error:#}")
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
}

#[cfg(windows)]
fn launch() -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use std::fs::OpenOptions;
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    let mut args = std::env::args_os().skip(1);
    let state = if let Some(flag) = args.next() {
        if flag != "--state" {
            bail!("只支持 --state DIR");
        }
        let state = PathBuf::from(args.next().context("--state 缺少目录")?);
        if args.next().is_some() {
            bail!("多余参数");
        }
        state
    } else if let Some(state) = std::env::var_os("WXCLIPD_STATE") {
        PathBuf::from(state)
    } else {
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA 未设置")?)
            .join("wxclipd")
    };
    if state.as_os_str().is_empty() {
        bail!("状态目录不能为空");
    }
    ipc::prepare_state_dir(&state)?;
    let log_path = state.join("wxclipd.log");
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let previous = state.join("wxclipd.previous.log");
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&log_path, previous);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let executable = std::env::current_exe()?.with_file_name("wxclipd.exe");
    let mut command = Command::new(executable);
    command.arg("--state").arg(&state);
    if let Some(name) = std::env::var_os("WXCLIPD_NAME") {
        command.arg("--name").arg(name);
    }
    command
        .arg("run")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("启动 wxclipd.exe 失败")?;
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("wxclipd-background is a Windows launcher; use wxclipd run on Linux.");
}
