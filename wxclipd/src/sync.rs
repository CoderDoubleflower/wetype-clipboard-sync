//! Clipboard synchronization with independently scheduled local observation.
//!
//! The network worker serializes protocol operations, while the event loop keeps
//! observing local copies during a slow proxy request. Incoming receipts stay in
//! memory until they are applied (or deliberately superseded) and acknowledged.

mod state;

use crate::clipboard::{self, Backend};
use anyhow::{anyhow, Context, Result};
use state::{PublishFailure, ReceiptPhase, Retry, State};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Notify};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, timeout, MissedTickBehavior};
use wx_ime_sdk::{
    Client, ClipboardContent, NotifyEvent, PairingCode, ReceivedClipboard, ReceivedId, SendReceipt,
};

pub struct SyncOptions {
    pub local_interval: Duration,
    pub poll_interval: Duration,
    pub paste_trigger: bool,
    pub paste_debounce: Duration,
    pub send_current: bool,
}

enum NetworkCommand {
    Send { generation: u64, text: String },
    Poll { generation: u64 },
    Acknowledge(ReceivedId),
}

enum NetworkResult {
    Sent {
        generation: u64,
        characters: usize,
        result: wx_ime_sdk::Result<SendReceipt>,
    },
    Polled {
        generation: u64,
        result: wx_ime_sdk::Result<Option<ReceivedClipboard>>,
    },
    Acknowledged {
        id: ReceivedId,
        result: wx_ime_sdk::Result<()>,
    },
}

pub async fn run(
    client: Client,
    backend: Backend,
    options: SyncOptions,
    control: PathBuf,
    listener: Option<crate::ipc::Listener>,
) -> Result<()> {
    let notify = Arc::new(Notify::new());
    let stop = Arc::new(Notify::new());
    let control_task = spawn_control(
        &control,
        listener,
        notify.clone(),
        stop.clone(),
        client.clone(),
    )?;
    println!(
        "[wxclipd] 控制端点: {}（sync-now 立即同步，stop 停止）",
        control.display()
    );

    // Unix signals are translated into the same explicit manual action as IPC.
    #[cfg(unix)]
    let signal_task = {
        use tokio::signal::unix::{signal, SignalKind};
        let mut trigger = signal(SignalKind::user_defined1())?;
        let notify = notify.clone();
        tokio::spawn(async move {
            while trigger.recv().await.is_some() {
                notify.notify_one();
            }
        })
    };

    let (commands, command_rx) = mpsc::channel(1);
    let (results, mut result_rx) = mpsc::channel(1);
    let network_task = tokio::spawn(network_worker(client.clone(), command_rx, results));
    let (push_tx, mut push_rx) = mpsc::channel::<u32>(8);
    let push_task = tokio::spawn(notify_loop(client.clone(), push_tx));
    let (paste_tx, mut paste_rx) = mpsc::channel::<()>(4);
    if options.paste_trigger {
        let monitored = crate::input_watch::spawn(paste_tx.clone(), options.paste_debounce);
        if monitored == 0 {
            eprintln!("[wxclipd] 粘贴触发未启用；仍可使用定时同步和 sync-now");
        } else {
            println!(
                "[wxclipd] 粘贴触发已启用，防抖 {} ms",
                options.paste_debounce.as_millis()
            );
        }
    }
    drop(paste_tx);

    let mut local_tick = interval(options.local_interval);
    local_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut poll_tick = interval(options.poll_interval);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Retry timing remains bounded even when the user chooses a long local interval.
    let mut maintenance_tick = interval(Duration::from_millis(250));
    maintenance_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    println!(
        "[wxclipd] 远端轮询间隔 {} 秒",
        options.poll_interval.as_secs()
    );
    let mut state = State::new(options.send_current);
    let mut read_failed = false;
    let mut poll_requested = true;
    let mut poll_retry = Retry::new(Instant::now());
    let mut in_flight = false;

    let outcome = loop {
        tokio::select! {
            biased;
            _ = stop.notified() => {
                println!("[wxclipd] 收到停止请求");
                break Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[wxclipd] 退出");
                break Ok(());
            }
            result = result_rx.recv() => {
                let Some(result) = result else {
                    break Err(anyhow!("同步网络任务已退出"));
                };
                in_flight = false;
                match result {
                    NetworkResult::Sent { generation, characters, result } => match result {
                        Ok(receipt) => {
                            state.sent(generation, receipt.group, receipt.version);
                            poll_requested = true;
                            println!("[wxclipd] 已发送 {characters} 字符（group {} version {}）", receipt.group, receipt.version);
                        }
                        Err(error) => {
                            let failure = publish_failure(&error);
                            let current = state.outgoing.as_ref().is_some_and(|pending| pending.generation == generation);
                            state.send_failed(generation, failure, Instant::now());
                            if current && failure == PublishFailure::OutcomeUnknown {
                                eprintln!("[wxclipd] 发送结果不确定，服务端可能已接收；已暂停自动重发。运行 wxclipd sync-now 可重试当前内容，或复制新内容继续同步。");
                            } else if current && failure == PublishFailure::Rejected {
                                eprintln!("[wxclipd] 发送失败且无法自动重试: {error}；修正问题后运行 wxclipd sync-now，或复制新内容。");
                            } else {
                                eprintln!("[wxclipd] 发送失败: {error}；将重试仍待发送的最新内容");
                            }
                        }
                    },
                    NetworkResult::Polled { generation, result } => match result {
                        Ok(Some(received)) => {
                            poll_retry.reset(Instant::now());
                            if let ClipboardContent::Image(image) = &received.content {
                                println!("[wxclipd] 收到图片 {} 字节，当前版本仅同步文本，已跳过", image.bytes);
                            }
                            if state.received(received, generation, Instant::now()) {
                                println!("[wxclipd] 本机已有更新的复制内容，已丢弃过时的远端值");
                            }
                        }
                        Ok(None) => poll_retry.reset(Instant::now()),
                        Err(error) => {
                            poll_retry.failed(Instant::now());
                            poll_requested = true;
                            eprintln!("[wxclipd] 拉取失败（第 {} 次，将退避重试）: {error}", poll_retry.failures);
                        }
                    },
                    NetworkResult::Acknowledged { id, result } => match result {
                        Ok(()) => {
                            state.acknowledged(&id);
                            poll_requested = true;
                        }
                        Err(error) => {
                            state.incoming_failed(Instant::now());
                            eprintln!("[wxclipd] 保存接收确认失败，将重试；不会重复写入剪贴板: {error}");
                        }
                    },
                }
            }
            _ = local_tick.tick() => {
                observe_clipboard(backend, &mut state, &mut read_failed).await;
            }
            _ = notify.notified() => {
                // Observe first so an explicit retry never republishes a value
                // that the user replaced since the last periodic observation.
                if observe_clipboard(backend, &mut state, &mut read_failed).await {
                    state.force_retry(Instant::now());
                }
                poll_requested = true;
                poll_retry.reset(Instant::now());
                println!("[wxclipd] 手动触发同步");
            }
            _ = poll_tick.tick() => poll_requested = true,
            Some(_) = push_rx.recv() => poll_requested = true,
            Some(_) = paste_rx.recv() => poll_requested = true,
            _ = maintenance_tick.tick() => {},
        }

        if state.apply_due(Instant::now()) {
            apply_incoming(backend, &mut state, &mut read_failed).await;
        }
        if in_flight || state.observed.is_none() {
            continue;
        }
        if state.incoming.is_none() && state.outgoing_due(Instant::now()).is_some() {
            // A retry may be due between local timer ticks, or after a long
            // clipboard read failure. Never resend an obsolete observation
            // without first checking what the user currently has copied.
            if !observe_clipboard(backend, &mut state, &mut read_failed).await {
                continue;
            }
        }
        let now = Instant::now();
        let command = if state.incoming.is_some() {
            // Finish or deliberately discard the SDK's durable receipt before
            // asking for another one. Acknowledgement errors never lose it.
            state.ack_due(now).map(NetworkCommand::Acknowledge)
        } else if state.outgoing.is_some() {
            state
                .outgoing_due(now)
                .map(|(generation, text)| NetworkCommand::Send { generation, text })
        } else if poll_requested && poll_retry.at <= now {
            poll_requested = false;
            Some(NetworkCommand::Poll {
                generation: state.generation,
            })
        } else {
            None
        };
        if let Some(command) = command {
            if commands.send(command).await.is_err() {
                break Err(anyhow!("同步网络任务已停止"));
            }
            in_flight = true;
        }
    };

    // Cancel all owners of the SDK session lock before main calls shutdown().
    // In particular, a status/pair request or an unresponsive proxy cannot hold
    // a Windows stop/uninstall request hostage until its network timeout.
    control_task.abort();
    network_task.abort();
    push_task.abort();
    let _ = control_task.await;
    let _ = network_task.await;
    let _ = push_task.await;
    #[cfg(unix)]
    {
        signal_task.abort();
        let _ = signal_task.await;
        let _ = std::fs::remove_file(&control);
    }
    outcome
}

async fn observe_clipboard(backend: Backend, state: &mut State, read_failed: &mut bool) -> bool {
    match clipboard::read_text(backend).await {
        Ok(text) => {
            *read_failed = false;
            let applying = state
                .incoming
                .as_ref()
                .is_some_and(|pending| pending.phase == ReceiptPhase::Apply);
            if state.observe(text, Instant::now()) && applying {
                println!("[wxclipd] 已保留新复制的本机内容，结束旧远端消息的应用");
            }
            true
        }
        Err(error) => {
            if !*read_failed {
                eprintln!("[wxclipd] 暂时无法读取剪贴板，稍后重试: {error:#}");
            }
            *read_failed = true;
            false
        }
    }
}

async fn apply_incoming(backend: Backend, state: &mut State, read_failed: &mut bool) {
    // A fresh read catches copies made after the last timer tick. On Windows the
    // conditional write rechecks under the same clipboard lock as the write.
    if !observe_clipboard(backend, state, read_failed).await {
        state.incoming_failed(Instant::now());
        return;
    }
    if !state.apply_due(Instant::now()) {
        return;
    }
    let pending = state
        .incoming
        .as_ref()
        .expect("apply_due requires a receipt");
    let ClipboardContent::Text(text) = &pending.message.content else {
        return;
    };
    let text = text.clone();
    let source = pending.message.source.name.clone();
    let expected = state
        .observed
        .as_ref()
        .expect("clipboard was observed")
        .clone();
    match clipboard::write_text_if_unchanged(backend, &expected, &text).await {
        Ok(true) => {
            state.applied(Instant::now());
            println!(
                "[wxclipd] 已接收来自 {source} 的文本（{} 字符）",
                text.chars().count()
            );
        }
        Ok(false) => {
            state.discard_incoming(Instant::now());
            observe_clipboard(backend, state, read_failed).await;
            println!("[wxclipd] 写入前发现本机剪贴板已更新，已保留本机内容");
        }
        Err(error) => {
            state.incoming_failed(Instant::now());
            eprintln!("[wxclipd] 写入剪贴板失败，保留消息稍后重试: {error:#}");
        }
    }
}

fn publish_failure(error: &wx_ime_sdk::Error) -> PublishFailure {
    match error {
        wx_ime_sdk::Error::OutcomeUnknown => PublishFailure::OutcomeUnknown,
        // NotPaired is safely checked before publication. Keep the latest copy
        // so pairing can complete without losing what was copied meanwhile.
        wx_ime_sdk::Error::NotPaired => PublishFailure::Retry,
        error if transient_network_error(error) => PublishFailure::Retry,
        _ => PublishFailure::Rejected,
    }
}

fn transient_network_error(error: &wx_ime_sdk::Error) -> bool {
    match error {
        wx_ime_sdk::Error::Transport
        | wx_ime_sdk::Error::CosTransport
        | wx_ime_sdk::Error::Timeout(_) => true,
        wx_ime_sdk::Error::Http(status) | wx_ime_sdk::Error::CosHttp(status) => {
            *status == 408 || *status == 429 || *status >= 500
        }
        _ => false,
    }
}

async fn network_worker(
    client: Client,
    mut commands: mpsc::Receiver<NetworkCommand>,
    results: mpsc::Sender<NetworkResult>,
) {
    let mut failures = 0u32;
    while let Some(command) = commands.recv().await {
        let (result, failed) = match command {
            NetworkCommand::Send { generation, text } => {
                let result = client.send_text(&text).await;
                let failed = result.as_ref().err().is_some_and(transient_network_error);
                (
                    NetworkResult::Sent {
                        generation,
                        characters: text.chars().count(),
                        result,
                    },
                    failed,
                )
            }
            NetworkCommand::Poll { generation } => {
                let result = client.poll_clipboard_once().await;
                let failed = result.as_ref().err().is_some_and(transient_network_error);
                (NetworkResult::Polled { generation, result }, failed)
            }
            NetworkCommand::Acknowledge(id) => {
                let result = client.acknowledge(id.clone()).await;
                (NetworkResult::Acknowledged { id, result }, false)
            }
        };
        failures = if failed {
            failures.saturating_add(1)
        } else {
            0
        };
        if failures >= 5 {
            failures = 0;
            if let Err(error) = client.reconnect().await {
                eprintln!("[wxclipd] 会话重建失败，将继续退避重试: {error}");
            }
        }
        // The completion also means the worker is idle. Otherwise an outgoing
        // value could sit behind a slow reconnect and become stale before send.
        if results.send(result).await.is_err() {
            return;
        }
    }
}

/// Maintain an independent notification channel. Events are only wake-up hints.
async fn notify_loop(client: Client, events: mpsc::Sender<u32>) {
    let mut backoff = Duration::from_secs(5);
    loop {
        match client.open_notify_channel().await {
            Ok(mut channel) => {
                println!("[wxclipd] 推送通道已连接");
                backoff = Duration::from_secs(5);
                loop {
                    match channel.next_event().await {
                        Ok(NotifyEvent::Push { event_type, .. }) => {
                            if events.send(event_type).await.is_err() {
                                return;
                            }
                        }
                        Ok(NotifyEvent::Idle) => {}
                        Ok(NotifyEvent::Closed) => break,
                        Err(error) => {
                            eprintln!("[wxclipd] 推送通道错误: {error}");
                            break;
                        }
                    }
                }
            }
            Err(error) => eprintln!("[wxclipd] 推送通道连接失败: {error}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(120));
    }
}

fn spawn_control(
    path: &Path,
    listener: Option<crate::ipc::Listener>,
    notify: Arc<Notify>,
    stop: Arc<Notify>,
    client: Client,
) -> Result<JoinHandle<()>> {
    let mut listener = match listener {
        Some(listener) => listener,
        None => crate::ipc::listen(path)
            .with_context(|| format!("创建控制端点 {} 失败", path.display()))?,
    };
    Ok(tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(mut stream) => {
                        let notify = notify.clone();
                        let stop = stop.clone();
                        let client = client.clone();
                        connections.spawn(async move {
                            let mut buffer = Vec::with_capacity(256);
                            let read = {
                                let mut reader = BufReader::new((&mut stream).take(257));
                                timeout(Duration::from_secs(5), reader.read_until(b'\n', &mut buffer)).await
                            };
                            let valid = matches!(read, Ok(Ok(_))) && buffer.len() <= 256 && buffer.last() == Some(&b'\n');
                            if !valid {
                                let _ = stream.write_all(b"err invalid or oversized control command\n").await;
                                let _ = stream.shutdown().await;
                                return;
                            }
                            let Ok(command) = std::str::from_utf8(&buffer) else { return };
                            let command = command.trim();
                            let stopping = command == "stop";
                            let reply = match command {
                                "sync" => { notify.notify_one(); "ok\n".to_string() },
                                "ping" => format!("ok {}\n", std::process::id()),
                                "stop" => "ok\n".to_string(),
                                command => match timeout(Duration::from_secs(120), control_command(&client, command)).await {
                                    Ok(reply) => reply,
                                    Err(_) => "err 控制请求超时\n".to_string(),
                                },
                            };
                            let _ = stream.write_all(reply.as_bytes()).await;
                            let _ = stream.shutdown().await;
                            if stopping { stop.notify_one(); }
                        });
                    }
                    Err(error) => {
                        eprintln!("[wxclipd] 控制连接失败: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
            }
        }
    }))
}

async fn control_command(client: &Client, command: &str) -> String {
    match command {
        "devices" => control_devices(client).await,
        "status" => control_status(client).await,
        other => {
            if let Some(value) = other.strip_prefix("unbind ") {
                return match value.trim().parse::<u64>() {
                    Ok(uin) => match client.unbind(uin).await {
                        Ok(()) => "ok\n".to_string(),
                        Err(error) => format!("err 解绑失败: {error}\n"),
                    },
                    Err(_) => "err --peer 需要是数字 Uin\n".to_string(),
                };
            }
            if let Some(code) = other.strip_prefix("pair ") {
                return control_pair(client, code.trim()).await;
            }
            "err 未知命令\n".to_string()
        }
    }
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
