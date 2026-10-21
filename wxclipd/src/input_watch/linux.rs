//! 监听"可能马上要粘贴"的输入动作：Ctrl 按下、鼠标右键按下。
//!
//! Wayland 没有给普通程序全局输入监听的接口，只能直接读 `/dev/input/event*`。
//! 因此需要 udev 的 `uaccess` 授权（见 `packaging/99-wxclipd-input.rules`），
//! 只在活跃会话里给当前用户临时 ACL，不把用户加进 `input` 组。
//!
//! 代码只判断按键码是不是 Ctrl / 右键，不记录、不区分其它按键内容。

use evdev::{Device, EventSummary, KeyCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 启动监听，返回成功打开的设备数。
pub fn spawn(events: mpsc::Sender<()>, debounce: Duration) -> usize {
    let last = Arc::new(Mutex::new(Instant::now() - debounce));
    let mut monitored = 0;

    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("event") {
            continue;
        }
        let Ok(device) = Device::open(entry.path()) else {
            continue;
        };
        let relevant = device
            .supported_keys()
            .map(|keys| {
                keys.contains(KeyCode::KEY_LEFTCTRL)
                    || keys.contains(KeyCode::KEY_RIGHTCTRL)
                    || keys.contains(KeyCode::BTN_RIGHT)
            })
            .unwrap_or(false);
        if !relevant {
            continue;
        }

        monitored += 1;
        let sender = events.clone();
        let last = last.clone();
        tokio::task::spawn_blocking(move || {
            let mut device = device;
            loop {
                let Ok(mut stream) = device.fetch_events() else {
                    return;
                };
                for event in stream.by_ref() {
                    let trigger = matches!(
                        event.destructure(),
                        EventSummary::Key(_, KeyCode::KEY_LEFTCTRL, 1)
                            | EventSummary::Key(_, KeyCode::KEY_RIGHTCTRL, 1)
                            | EventSummary::Key(_, KeyCode::BTN_RIGHT, 1)
                    );
                    if !trigger {
                        continue;
                    }
                    {
                        let mut last = last.lock().unwrap();
                        if last.elapsed() < debounce {
                            continue;
                        }
                        *last = Instant::now();
                    }
                    if sender.blocking_send(()).is_err() {
                        return;
                    }
                }
            }
        });
    }
    monitored
}
