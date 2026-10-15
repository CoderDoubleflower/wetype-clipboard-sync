//! 剪贴板同步主循环。
//!
//! 远端轮询默认 60 秒一次：请求会带上本地已有的剪贴板版本号，服务端据此省略
//! 未变化的内容，单次响应约 156 字节。需要立刻同步时，可以用 `wxclipd sync-now`
//! （走状态目录下的控制套接字）或给进程发 `SIGUSR1` 触发一次即时轮询。

use crate::clipboard::{self, Backend};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Notify;
use tokio::time::{interval, MissedTickBehavior};
use wx_ime_sdk::{Client, ClipboardContent, PairingCode};

pub struct SyncOptions {
    /// 本地剪贴板轮询间隔。
    pub local_interval: Duration,
    /// 远端（服务端）轮询间隔。
    pub poll_interval: Duration,
    pub send_current: bool,
}

pub fn control_socket_path(state: &Path) -> PathBuf {
    state.join("control.sock")
}

pub async fn run(
    client: Client,
    backend: Backend,
    options: SyncOptions,
    control: PathBuf,
) -> Result<()> {
    let notify = Arc::new(Notify::new());
    spawn_control(&control, notify.clone(), client.clone())?;
    println!(
        "[wxclipd] 控制套接字: {}（wxclipd sync-now 立即同步）",
        control.display()
    );
    let mut trigger = signal(SignalKind::user_defined1())?;

    let mut paired = is_paired(&client).await;
    let mut paired_checked_at = Instant::now();
    if !paired {
        println!(
            "[wxclipd] 尚未配对，先运行 `wxclipd pair CODE` 或 `wxclipd code`；本进程只接收不发送。"
        );
    }

    let mut last_known = clipboard::read_text(backend).await?;
    if options.send_current {
        if let Some(text) = last_known.clone() {
            send(&client, &text).await;
        }
    }

    let mut local_tick = interval(options.local_interval);
    local_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut poll_tick = interval(options.poll_interval);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut failures: u32 = 0;
    loop {
        tokio::select! {
            _ = local_tick.tick() => {
                match clipboard::read_text(backend).await {
                    Ok(Some(text)) if Some(&text) != last_known.as_ref() => {
                        last_known = Some(text.clone());
                        if paired {
                            send(&client, &text).await;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("[wxclipd] 读取剪贴板失败: {error:#}"),
                }
            }
            _ = poll_tick.tick() => {
                if !paired && paired_checked_at.elapsed() >= Duration::from_secs(30) {
                    paired_checked_at = Instant::now();
                    paired = is_paired(&client).await;
                    if paired {
                        println!("[wxclipd] 检测到已配对，开始同步本机剪贴板。");
                    }
                }
                failures = poll_remote(&client, backend, &mut last_known, failures).await;
            }
            _ = notify.notified() => {
                println!("[wxclipd] 手动触发同步");
                if let Ok(Some(text)) = clipboard::read_text(backend).await {
                    if Some(&text) != last_known.as_ref() && paired {
                        last_known = Some(text.clone());
                        send(&client, &text).await;
                    }
                }
                failures = poll_remote(&client, backend, &mut last_known, failures).await;
            }
            _ = trigger.recv() => {
                println!("[wxclipd] 收到 SIGUSR1，立即同步");
                failures = poll_remote(&client, backend, &mut last_known, failures).await;
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[wxclipd] 退出");
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&control);
    Ok(())
}

async fn poll_remote(
    client: &Client,
    backend: Backend,
    last_known: &mut Option<String>,
    failures: u32,
) -> u32 {
    match client.poll_clipboard_once().await {
        Ok(Some(received)) => {
            match received.content {
                ClipboardContent::Text(text) => {
                    if let Err(error) = clipboard::write_text(backend, &text).await {
                        eprintln!("[wxclipd] 写入剪贴板失败: {error:#}");
                    } else {
                        *last_known = Some(text.clone());
                        println!(
                            "[wxclipd] 已接收来自 {} 的文本（{} 字符）",
                            received.source.name,
                            text.chars().count()
                        );
                    }
                }
                ClipboardContent::Image(image) => {
                    println!(
                        "[wxclipd] 收到图片 {} 字节，当前版本仅同步文本，已跳过",
                        image.bytes
                    );
                }
            }
            if let Err(error) = client.acknowledge(received.id).await {
                eprintln!("[wxclipd] 确认接收失败: {error:#}");
            }
            0
        }
        Ok(None) => 0,
        Err(error) => {
            let failures = failures + 1;
            eprintln!("[wxclipd] 拉取远端剪贴板失败（第 {failures} 次）: {error:#}");
            if failures % 5 == 0 {
                eprintln!("[wxclipd] 重新建立会话");
                if let Err(error) = client.reconnect().await {
                    eprintln!("[wxclipd] 会话重建失败: {error:#}");
                }
            }
            failures
        }
    }
}

/// 控制套接字支持 `sync`、`devices`、`unbind <uin>`；
/// 让 `wxclipd` 子命令在服务运行时也能查询和解绑，不必先停服务。
fn spawn_control(path: &Path, notify: Arc<Notify>, client: Client) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("清理旧的 {} 失败", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("绑定控制套接字 {} 失败", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            let notify = notify.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 256];
                let read = stream.read(&mut buffer).await.unwrap_or(0);
                let command = String::from_utf8_lossy(&buffer[..read]);
                let reply = match command.trim() {
                    "sync" => {
                        notify.notify_one();
                        "ok\n".to_string()
                    }
                    "devices" => control_devices(&client).await,
                    "status" => control_status(&client).await,
                    other => match other.strip_prefix("unbind ") {
                        Some(value) => match value.trim().parse::<u64>() {
                            Ok(uin) => match client.unbind(uin).await {
                                Ok(()) => "ok\n".to_string(),
                                Err(error) => format!("err 解绑失败: {error}\n"),
                            },
                            Err(_) => "err --peer 需要是数字 Uin\n".to_string(),
                        },
                        None => match other.strip_prefix("pair ") {
                            Some(code) => control_pair(&client, code.trim()).await,
                            None => "err 未知命令\n".to_string(),
                        },
                    },
                };
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Ok(())
}

async fn control_devices(client: &Client) -> String {
    let (local, devices) = match (client.local_device().await, client.devices().await) {
        (Ok(local), Ok(devices)) => (local, devices),
        (Err(error), _) | (_, Err(error)) => return format!("err 查询设备失败: {error}\n"),
    };
    let peers: Vec<serde_json::Value> = devices
        .iter()
        .filter(|device| device.uin != local.uin)
        .map(|device| {
            serde_json::json!({
                "uin": device.uin,
                "name": device.name,
                "platform": device.platform,
                "supports_file_transfer": device.supports_file_transfer,
            })
        })
        .collect();
    let payload = serde_json::json!({
        "local": { "uin": local.uin, "name": local.name },
        "devices": peers,
    });
    format!("{payload}\n")
}

async fn control_status(client: &Client) -> String {
    let local = match client.local_device().await {
        Ok(local) => local,
        Err(error) => return format!("err 查询本机失败: {error}\n"),
    };
    let info = match client.inspect_clipboard().await {
        Ok(info) => info,
        Err(_) => {
            let payload = serde_json::json!({
                "local": { "uin": local.uin, "name": local.name },
                "paired": false,
            });
            return format!("{payload}\n");
        }
    };
    let payload = serde_json::json!({
        "local": { "uin": local.uin, "name": local.name },
        "paired": true,
        "clipboard": {
            "has_message": info.has_message,
            "version": info.version,
            "kind": info.kind,
            "from_uin": info.from_uin,
            "already_consumed": info.already_consumed,
            "expired": info.expired,
        },
    });
    format!("{payload}\n")
}

/// 让常驻进程替 CLI 加入配对码：六位码由官方输入法显示，对方确认后返回结果。
async fn control_pair(client: &Client, code: &str) -> String {
    let code = match PairingCode::parse(code.to_string()) {
        Ok(code) => code,
        Err(_) => return "err 配对码必须是六位数字\n".to_string(),
    };
    let mut session = match client.join(code).await {
        Ok(session) => session,
        Err(error) => return format!("err 提交配对码失败: {error}\n"),
    };
    match session.wait().await {
        Ok((_, peer)) => {
            let payload = serde_json::json!({
                "peer": { "uin": peer.uin.unwrap_or(0), "name": peer.name },
            });
            format!("{payload}\n")
        }
        Err(error) => format!("err 配对失败: {error}\n"),
    }
}

async fn is_paired(client: &Client) -> bool {
    match client.group().await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            eprintln!("[wxclipd] 查询设备组失败: {error:#}");
            false
        }
    }
}

async fn send(client: &Client, text: &str) {
    if text.is_empty() {
        return;
    }
    match client.send_text(text).await {
        Ok(receipt) => println!(
            "[wxclipd] 已发送 {} 字符（group {} version {}）",
            text.chars().count(),
            receipt.group,
            receipt.version
        ),
        Err(error) => eprintln!("[wxclipd] 发送剪贴板失败: {error:#}"),
    }
}
