//! Private, inherited-pipe connection to the WPF presentation process. The tray
//! remains the sole owner of synchronization, setup, and application lifetime.

use super::{
    SharedView, ID_FOLDER, ID_LOG, ID_RECEIVE_OFF, ID_RECEIVE_ON, ID_RESTART, ID_SEND_OFF,
    ID_SEND_ON, ID_SETUP, ID_START, ID_STOP, ID_SYNC,
};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, PostMessageW, WM_COMMAND,
};

#[derive(Default)]
struct Pending {
    snapshot: Vec<u8>,
    show: bool,
}

pub(super) struct Bridge {
    pending: Arc<Mutex<Pending>>,
    active: Arc<AtomicBool>,
    wake: SyncSender<()>,
    process: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    pub(super) fn new(controller: usize, view: SharedView) -> Result<Self> {
        let executable = std::env::current_exe()?
            .with_file_name("ui")
            .join("wxclipd-ui.exe");
        let pending = Arc::new(Mutex::new(Pending::default()));
        let active = Arc::new(AtomicBool::new(true));
        let process = Arc::new(Mutex::new(None::<Child>));
        let (wake, receiver) = sync_channel(1);
        let thread_pending = pending.clone();
        let thread_active = active.clone();
        let thread_process = process.clone();
        let thread = std::thread::Builder::new()
            .name("wxclipd-wpf-bridge".to_owned())
            .spawn(move || {
                let mut input = None;
                let mut reader: Option<JoinHandle<()>> = None;
                let mut last_snapshot = Vec::new();
                loop {
                    let _ = receiver.recv_timeout(Duration::from_millis(250));
                    if !thread_active.load(Ordering::Acquire) {
                        if let Some(mut input) = input.take() {
                            let _ = Write::write_all(&mut input, b"{\"type\":\"exit\"}\n");
                        }
                        break;
                    }
                    // Only the process handle is shared with shutdown. Never
                    // hold its mutex while writing a potentially blocked pipe.
                    let exited = thread_process
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                        .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))));
                    if exited {
                        input = None;
                        *thread_process.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        if let Some(reader) = reader.take() {
                            let _ = reader.join();
                        }
                        last_snapshot.clear();
                        show_failure(controller, &view);
                    }
                    let (snapshot, show) = {
                        let mut pending = thread_pending.lock().unwrap_or_else(|e| e.into_inner());
                        (pending.snapshot.clone(), std::mem::take(&mut pending.show))
                    };
                    if show && input.is_none() {
                        let spawned = (|| -> Result<_> {
                            let mut process =
                                thread_process.lock().unwrap_or_else(|e| e.into_inner());
                            // Prevent a late launch racing an installer shutdown.
                            if !thread_active.load(Ordering::Acquire) {
                                anyhow::bail!("托盘正在退出");
                            }
                            let mut child = Command::new(&executable)
                                .arg("--bridge")
                                .stdin(Stdio::piped())
                                .stdout(Stdio::piped())
                                .stderr(Stdio::null())
                                // The display needs no raw proxy credentials or
                                // startup overrides inherited from the launcher.
                                .env_remove("WXCLIPD_PROXY")
                                .env_remove("WXCLIPD_NAME")
                                .env_remove("WXCLIPD_SETUP_COMPLETE")
                                .spawn()
                                .context("启动 WPF 界面失败")?;
                            let stdin = child.stdin.take().context("WPF 输入管道不可用")?;
                            let stdout = child.stdout.take().context("WPF 输出管道不可用")?;
                            *process = Some(child);
                            Ok((stdin, stdout))
                        })();
                        match spawned {
                            Ok((stdin, stdout)) => {
                                input = Some(stdin);
                                let alive = thread_active.clone();
                                match std::thread::Builder::new()
                                    .name("wxclipd-wpf-actions".to_owned())
                                    .spawn(move || {
                                        let mut output = BufReader::new(stdout);
                                        while let Ok(Some(command)) = read_command(&mut output) {
                                            if !alive.load(Ordering::Acquire) {
                                                break;
                                            }
                                            if let Some(id) = action_id(&command) {
                                                unsafe {
                                                    PostMessageW(
                                                        controller as _,
                                                        WM_COMMAND,
                                                        id,
                                                        0,
                                                    );
                                                }
                                            }
                                        }
                                    }) {
                                    Ok(thread) => reader = Some(thread),
                                    Err(_) => {
                                        input = None;
                                        stop_child(&thread_process);
                                        show_failure(controller, &view);
                                        continue;
                                    }
                                }
                            }
                            Err(_) => {
                                show_failure(controller, &view);
                                continue;
                            }
                        }
                    }
                    if let Some(stdin) = &mut input {
                        let written = (|| -> std::io::Result<()> {
                            if !snapshot.is_empty() && snapshot != last_snapshot {
                                stdin.write_all(&snapshot)?;
                                stdin.write_all(b"\n")?;
                                last_snapshot = snapshot;
                            }
                            if show {
                                // The user's tray/shortcut click belongs to
                                // the controller process. Permit only its own
                                // display child to activate the existing window.
                                if let Some(child) = thread_process
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .as_ref()
                                {
                                    unsafe {
                                        AllowSetForegroundWindow(child.id());
                                    }
                                }
                                stdin.write_all(b"{\"type\":\"show\"}\n")?;
                            }
                            stdin.flush()
                        })();
                        if written.is_err() {
                            input = None;
                            stop_child(&thread_process);
                            if let Some(reader) = reader.take() {
                                let _ = reader.join();
                            }
                            last_snapshot.clear();
                            if show {
                                show_failure(controller, &view);
                            }
                        }
                    }
                }
                // stdin EOF also asks the UI to exit. The owner enforces a
                // bounded wait and kills this child if its dispatcher is stuck.
                drop(input);
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
            })
            .context("创建 WPF 通信线程失败")?;
        Ok(Self {
            pending,
            active,
            wake,
            process,
            thread: Some(thread),
        })
    }

    pub(super) fn update(&self, snapshot: Vec<u8>) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot = snapshot;
        let _ = self.wake.try_send(());
    }

    pub(super) fn show(&self) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).show = true;
        let _ = self.wake.try_send(());
    }

    pub(super) fn request_shutdown(&self) {
        self.active.store(false, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.request_shutdown();
        let deadline = Instant::now() + Duration::from_secs(2);
        while self
            .thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
        {
            if Instant::now() >= deadline {
                // Only our presentation child is terminated. Never kill a
                // daemon or a settings window containing unsaved user input.
                stop_child(&self.process);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        stop_child(&self.process);
    }
}

fn stop_child(process: &Mutex<Option<Child>>) {
    if let Some(mut child) = process.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

fn show_failure(controller: usize, view: &SharedView) {
    view.lock().unwrap_or_else(|e| e.into_inner()).notice = Some(
        "无法打开 WPF 界面。请重新安装完整安装包，或完整解压便携包（包含 ui 目录）。".to_owned(),
    );
    unsafe {
        PostMessageW(controller as _, super::WM_UI_ERROR, 0, 0);
    }
}

fn action_id(command: &str) -> Option<usize> {
    Some(match command {
        "start" => ID_START,
        "stop" => ID_STOP,
        "restart" => ID_RESTART,
        "sync" => ID_SYNC,
        "setup" => ID_SETUP,
        "log" => ID_LOG,
        "folder" => ID_FOLDER,
        "send-on" => ID_SEND_ON,
        "send-off" => ID_SEND_OFF,
        "receive-on" => ID_RECEIVE_ON,
        "receive-off" => ID_RECEIVE_OFF,
        _ => return None,
    })
}

fn read_command(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    // A malformed child must not grow the tray's memory without a bound. Do
    // not interpret an incomplete/truncated prefix as an accepted action.
    let count = reader.take(33).read_until(b'\n', &mut line)?;
    if count == 0 {
        return Ok(None);
    }
    if count > 32 || line.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "无效的界面命令",
        ));
    }
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_actions_are_explicit_and_allowlisted() {
        for (command, id) in [
            ("send-on", ID_SEND_ON),
            ("send-off", ID_SEND_OFF),
            ("receive-on", ID_RECEIVE_ON),
            ("receive-off", ID_RECEIVE_OFF),
        ] {
            assert_eq!(action_id(command), Some(id));
        }
        for command in ["toggle-send", "send-on true", "receive-off\nstart"] {
            assert_eq!(action_id(command), None);
        }
    }

    #[test]
    fn display_can_only_request_complete_allowlisted_actions() {
        let mut reader = std::io::Cursor::new(b"start\nstop\r\nsetup\n");
        for expected in [ID_START, ID_STOP, ID_SETUP] {
            assert_eq!(
                action_id(&read_command(&mut reader).unwrap().unwrap()),
                Some(expected)
            );
        }
        assert_eq!(read_command(&mut reader).unwrap(), None);
        for value in ["start --proxy secret", "exit", "stop\nstart", "", "START"] {
            assert_eq!(action_id(value), None);
        }
        assert!(read_command(&mut std::io::Cursor::new(b"start")).is_err());
        assert!(read_command(&mut std::io::Cursor::new(b"start\0\xff\n")).is_err());
        assert!(read_command(&mut std::io::Cursor::new(vec![b'x'; 100_000])).is_err());
    }
}
