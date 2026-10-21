//! Clipboard synchronization with independently scheduled local observation.
//!
//! The network worker serializes protocol operations, while the event loop keeps
//! observing local copies during a slow proxy request. Incoming receipts stay in
//! memory until they are applied (or deliberately superseded) and acknowledged.

mod state;

use crate::clipboard::{self, Payload};
use crate::runtime_status::{LocalOperation, RuntimeStatus};
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
    Client, DownloadedImage, NotifyEvent, PairingCode, ReceivedClipboard, ReceivedId, SendReceipt,
};

pub struct SyncOptions {
    pub local_interval: Duration,
    pub poll_interval: Duration,
    pub paste_trigger: bool,
    pub paste_debounce: Duration,
    pub send_current: bool,
}

enum NetworkCommand {
    Send {
        generation: u64,
        payload: Payload,
    },
    Poll {
        generation: u64,
    },
    PrepareImage {
        id: ReceivedId,
        image: DownloadedImage,
    },
    Acknowledge(ReceivedId),
    Discard(ReceivedId),
}

enum NetworkResult {
    Sent {
        generation: u64,
        description: String,
        result: wx_ime_sdk::Result<SendReceipt>,
    },
    Polled {
        generation: u64,
        result: wx_ime_sdk::Result<Option<ReceivedClipboard>>,
    },
    Prepared {
        id: ReceivedId,
        result: std::result::Result<Payload, PrepareFailure>,
    },
    Acknowledged {
        id: ReceivedId,
        result: wx_ime_sdk::Result<()>,
    },
    Discarded {
        id: ReceivedId,
        result: wx_ime_sdk::Result<()>,
    },
}

#[derive(Debug)]
enum PrepareFailure {
    Retry(anyhow::Error),
    Invalid(anyhow::Error),
}

pub async fn run(
    client: Client,
    options: SyncOptions,
    control: PathBuf,
    listener: Option<crate::ipc::Listener>,
    runtime_status: RuntimeStatus,
) -> Result<()> {
    let notify = Arc::new(Notify::new());
    let stop = Arc::new(Notify::new());
    let control_task = spawn_control(
        &control,
        listener,
        notify.clone(),
        stop.clone(),
        client.clone(),
        runtime_status.clone(),
    )?;
    println!(
        "[wxclipd] 控制端点: {}（sync-now 立即同步，stop 停止）",
        control.display()
    );

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
                runtime_status.stopping();
                println!("[wxclipd] 收到停止请求");
                break Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                runtime_status.stopping();
                println!("[wxclipd] 退出");
                break Ok(());
            }
            result = result_rx.recv() => {
                let Some(result) = result else {
                    break Err(anyhow!("同步网络任务已退出"));
                };
                in_flight = false;
                match result {
                    NetworkResult::Sent { generation, description, result } => match result {
                        Ok(receipt) => {
                            runtime_status.sent();
                            state.sent(generation, receipt.group, receipt.version);
                            poll_requested = true;
                            println!("[wxclipd] 已发送{description}（group {} version {}）", receipt.group, receipt.version);
                        }
                        Err(error) => {
                            runtime_status.network_failed(&error);
                            let failure = publish_failure(&error);
                            let current = state.outgoing.as_ref().is_some_and(|pending| pending.generation == generation);
                            let failed_at = Instant::now();
                            state.send_failed(generation, failure, failed_at);
                            if current && failure == PublishFailure::OutcomeUnknown {
                                eprintln!("[wxclipd] 发送结果不确定，服务端可能已接收；已暂停自动重发。运行 wxclipd sync-now 可重试当前内容，或复制新内容继续同步。");
                            } else if current && failure == PublishFailure::Rejected {
                                eprintln!("[wxclipd] 发送失败且无法自动重试: {error}；修正问题后运行 wxclipd sync-now，或复制新内容。");
                            } else if current {
                                let pending = state.outgoing.as_ref().expect("current outgoing value");
                                eprintln!("[wxclipd] 发送失败（第 {} 次，{} 秒后重试最新内容）: {error}", pending.retry.failures, pending.retry.at.duration_since(failed_at).as_secs());
                            } else {
                                eprintln!("[wxclipd] 旧内容发送失败: {error}；继续处理最新复制的内容");
                            }
                        }
                    },
                    NetworkResult::Polled { generation, result } => match result {
                        Ok(Some(received)) => {
                            runtime_status.network_succeeded();
                            poll_retry.reset(Instant::now());
                            if state.received(received, generation, Instant::now()) {
                                println!("[wxclipd] 本机已有更新的复制内容，已丢弃过时的远端值");
                            }
                        }
                        Ok(None) => {
                            runtime_status.network_succeeded();
                            poll_retry.reset(Instant::now());
                        },
                        Err(wx_ime_sdk::Error::InvalidClipboard { id, reason }) => {
                            poll_retry.reset(Instant::now());
                            eprintln!("[wxclipd] 远端图片无效或超过限制（{reason}），将拒收此版本后继续同步");
                            state.received_invalid(id, Instant::now());
                        }
                        Err(error) => {
                            runtime_status.network_failed(&error);
                            let failed_at = Instant::now();
                            poll_retry.failed(failed_at);
                            poll_requested = true;
                            eprintln!("[wxclipd] 拉取失败（第 {} 次，{} 秒后重试）: {error}", poll_retry.failures, poll_retry.at.duration_since(failed_at).as_secs());
                        }
                    },
                    NetworkResult::Prepared { id, result } => match result {
                        Ok(payload) => {
                            runtime_status.local_result(LocalOperation::Prepare, true);
                            // A newer local copy may have deliberately ended
                            // this receipt while the image was being decoded.
                            state.prepared(&id, payload, Instant::now());
                        }
                        Err(PrepareFailure::Retry(error)) => {
                            if state.preparation_failed(&id, Instant::now()) {
                                runtime_status.local_result(LocalOperation::Prepare, false);
                                eprintln!("[wxclipd] 暂时无法准备已下载的图片，保留消息稍后重试: {error:#}");
                            }
                        }
                        Err(PrepareFailure::Invalid(error)) => {
                            if state.invalid_image(&id, Instant::now()) {
                                runtime_status.local_result(LocalOperation::Prepare, true);
                                eprintln!("[wxclipd] 远端图片损坏、格式不支持或超过限制，已跳过此版本: {error:#}");
                            }
                        }
                    },
                    NetworkResult::Acknowledged { id, result } => match result {
                        Ok(()) => {
                            runtime_status.local_result(LocalOperation::Acknowledge, true);
                            state.acknowledged(&id);
                            poll_requested = true;
                        }
                        Err(error) => {
                            runtime_status.local_result(LocalOperation::Acknowledge, false);
                            state.incoming_failed(Instant::now());
                            eprintln!("[wxclipd] 保存接收确认失败，将重试；不会重复写入剪贴板: {error}");
                        }
                    },
                    NetworkResult::Discarded { id, result } => match result {
                        Ok(()) => {
                            runtime_status.local_result(LocalOperation::Discard, true);
                            state.discarded(&id);
                            poll_requested = true;
                        }
                        Err(error) => {
                            runtime_status.local_result(LocalOperation::Discard, false);
                            state.discard_failed(&id, Instant::now());
                            eprintln!("[wxclipd] 保存无效图片的拒收记录失败，将重试同一版本: {error}");
                        }
                    },
                }
            }
            _ = local_tick.tick() => {
                observe_clipboard(&mut state, &mut read_failed, &runtime_status).await;
            }
            _ = notify.notified() => {
                // Observe first so an explicit retry never republishes a value
                // that the user replaced since the last periodic observation.
                if observe_clipboard(&mut state, &mut read_failed, &runtime_status).await {
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
            apply_incoming(&mut state, &mut read_failed, &runtime_status).await;
        }
        update_local_status(&runtime_status, &state);
        if in_flight || state.observed.is_none() {
            continue;
        }
        if state.incoming.is_none()
            && state.invalid.is_none()
            && state.outgoing_due(Instant::now()).is_some()
        {
            // A retry may be due between local timer ticks, or after a long
            // clipboard read failure. Never resend an obsolete observation
            // without first checking what the user currently has copied.
            if !observe_clipboard(&mut state, &mut read_failed, &runtime_status).await {
                continue;
            }
        }
        let now = Instant::now();
        let command = if state.invalid.is_some() {
            state.discard_due(now).map(NetworkCommand::Discard)
        } else if state.incoming.is_some() {
            // Finish or deliberately discard the SDK's durable receipt before
            // asking for another one. Acknowledgement errors never lose it.
            if let Some((id, image)) = state.prepare_due(now) {
                Some(NetworkCommand::PrepareImage { id, image })
            } else {
                state.ack_due(now).map(NetworkCommand::Acknowledge)
            }
        } else if state.outgoing.is_some() {
            state
                .outgoing_due(now)
                .map(|(generation, payload)| NetworkCommand::Send {
                    generation,
                    payload,
                })
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
    outcome
}

fn update_local_status(runtime_status: &RuntimeStatus, state: &State) {
    runtime_status.queue(
        state.outgoing.is_some(),
        state
            .outgoing
            .as_ref()
            .is_some_and(|pending| pending.paused),
    );
    // Superseding a receipt also ends any local retry associated with it.
    // Do not leave a stale write/decode error after a new local copy wins.
    let phase = state.incoming.as_ref().map(|pending| pending.phase);
    for (operation, expected) in [
        (LocalOperation::Prepare, ReceiptPhase::Prepare),
        (LocalOperation::Write, ReceiptPhase::Apply),
        (LocalOperation::Acknowledge, ReceiptPhase::Acknowledge),
    ] {
        if phase != Some(expected) {
            runtime_status.local_result(operation, true);
        }
    }
    if state.invalid.is_none() {
        runtime_status.local_result(LocalOperation::Discard, true);
    }
}

async fn observe_clipboard(
    state: &mut State,
    read_failed: &mut bool,
    runtime_status: &RuntimeStatus,
) -> bool {
    match clipboard::read_snapshot().await {
        Ok(snapshot) => {
            runtime_status.local_result(LocalOperation::Read, true);
            *read_failed = false;
            let applying = state
                .incoming
                .as_ref()
                .is_some_and(|pending| pending.phase != ReceiptPhase::Acknowledge);
            if state.observe(snapshot, Instant::now()) && applying {
                println!("[wxclipd] 已保留新复制的本机内容，结束旧远端消息的应用");
            }
            true
        }
        Err(error) => {
            runtime_status.local_result(LocalOperation::Read, false);
            if !*read_failed {
                eprintln!("[wxclipd] 暂时无法读取剪贴板，稍后重试: {error:#}");
            }
            *read_failed = true;
            false
        }
    }
}

async fn apply_incoming(state: &mut State, read_failed: &mut bool, runtime_status: &RuntimeStatus) {
    // A fresh read catches copies made after the last timer tick. On Windows the
    // conditional write rechecks under the same clipboard lock as the write.
    if !observe_clipboard(state, read_failed, runtime_status).await {
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
    let payload = pending
        .payload
        .as_ref()
        .expect("apply requires prepared content")
        .clone();
    let source = pending.message.source.name.clone();
    let expected = state
        .observed
        .as_ref()
        .expect("clipboard was observed")
        .clone();
    match clipboard::write_if_unchanged(&expected, &payload).await {
        Ok(Some(written)) => {
            runtime_status.received();
            state.applied(written, Instant::now());
            println!(
                "[wxclipd] 已接收来自 {source} 的{}",
                describe_payload(&payload)
            );
        }
        Ok(None) => {
            state.discard_incoming(Instant::now());
            observe_clipboard(state, read_failed, runtime_status).await;
            println!("[wxclipd] 写入前发现本机剪贴板已更新，已保留本机内容");
        }
        Err(error) => {
            runtime_status.local_result(LocalOperation::Write, false);
            state.incoming_failed(Instant::now());
            eprintln!("[wxclipd] 写入剪贴板失败，保留消息稍后重试: {error:#}");
        }
    }
}

fn describe_payload(payload: &Payload) -> String {
    match payload {
        Payload::Text(text) => format!("文本（{} 字符）", text.chars().count()),
        Payload::Image(image) => format!(
            "图片（{}×{}，{} 字节）",
            image.width(),
            image.height(),
            image.encoded().len()
        ),
    }
}

/// File reads and image decoding must not block the local observation loop.
/// Only definite invalid data is consumed; a temporary file/worker failure
/// keeps the already-emitted SDK receipt so preparation can be retried.
async fn prepare_image(image: DownloadedImage) -> std::result::Result<Payload, PrepareFailure> {
    match tokio::task::spawn_blocking(move || crate::image::ClipboardImage::from_downloaded(&image))
        .await
    {
        Ok(Ok(image)) => Ok(Payload::Image(Arc::new(image))),
        Ok(Err(error))
            if error
                .chain()
                .any(|cause| cause.is::<std::collections::TryReserveError>()) =>
        {
            Err(PrepareFailure::Retry(error))
        }
        // Decoding from a bounded in-memory buffer can itself return an IO
        // UnexpectedEof. That is malformed content, not a transient cache read.
        Ok(Err(error)) if error.chain().any(|cause| cause.is::<::image::ImageError>()) => {
            Err(PrepareFailure::Invalid(error))
        }
        Ok(Err(error)) if error.chain().any(|cause| cause.is::<std::io::Error>()) => {
            Err(PrepareFailure::Retry(error))
        }
        Ok(Err(error)) => Err(PrepareFailure::Invalid(error)),
        Err(error) => Err(PrepareFailure::Retry(
            anyhow!(error).context("图片解码任务退出"),
        )),
    }
}

fn publish_failure(error: &wx_ime_sdk::Error) -> PublishFailure {
    match error {
        wx_ime_sdk::Error::OutcomeUnknown => PublishFailure::OutcomeUnknown,
        // NotPaired is safely checked before publication. Keep the latest copy
        // so pairing can complete without losing what was copied meanwhile.
        wx_ime_sdk::Error::NotPaired => PublishFailure::Retry,
        // Control HTTP 400 can require a fresh timestamp/key exchange. The SDK
        // invalidates that session and bounds recovery for safe queries. Keep
        // the outbox alive if recovery also fails. Actual publication errors
        // are still OutcomeUnknown; COS 400 does not imply a stale session.
        wx_ime_sdk::Error::Http(400) => PublishFailure::Retry,
        error if transient_network_error(error) => PublishFailure::Retry,
        _ => PublishFailure::Rejected,
    }
}

fn transient_network_error(error: &wx_ime_sdk::Error) -> bool {
    // Control 400 recovery is handled by the SDK per request, so it must not
    // also trigger the worker's five-failure transport reconnect below.
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
            NetworkCommand::Send {
                generation,
                payload,
            } => {
                let description = describe_payload(&payload);
                let result = match &payload {
                    Payload::Text(text) => client.send_text(text).await,
                    Payload::Image(image) => client.send_image_bytes(image.encoded()).await,
                };
                let failed = result.as_ref().err().is_some_and(transient_network_error);
                (
                    NetworkResult::Sent {
                        generation,
                        description,
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
            NetworkCommand::PrepareImage { id, image } => {
                let result = prepare_image(image).await;
                (NetworkResult::Prepared { id, result }, false)
            }
            NetworkCommand::Acknowledge(id) => {
                let result = client.acknowledge(id.clone()).await;
                (NetworkResult::Acknowledged { id, result }, false)
            }
            NetworkCommand::Discard(id) => {
                let result = client.discard_clipboard(id.clone()).await;
                (NetworkResult::Discarded { id, result }, false)
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
    runtime_status: RuntimeStatus,
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
                        let runtime_status = runtime_status.clone();
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
                                "inspect" => runtime_status.reply(),
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

#[cfg(test)]
mod image_tests {
    use super::*;
    use crate::clipboard::Snapshot;
    use sha2::{Digest, Sha256};

    fn receipt(content: serde_json::Value, version: u64) -> ReceivedClipboard {
        serde_json::from_value(serde_json::json!({
            "id": { "group": 10, "version": version, "from_uin": 2, "trace_id": format!("image-test-{version}") },
            "source": { "uin": 2, "name": "phone", "platform": 1, "supports_file_transfer": false },
            "content": content,
            "expire_at": 4_000_000_000u64
        })).unwrap()
    }

    fn downloaded(path: PathBuf, bytes: &[u8]) -> DownloadedImage {
        DownloadedImage {
            path,
            bytes: bytes.len(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
        }
    }

    #[test]
    fn control_400_retries_text_and_images_with_capped_backoff_until_success() {
        let image =
            Arc::new(crate::image::ClipboardImage::from_rgba(1, 1, vec![12, 34, 56, 255]).unwrap());
        for payload in [
            Payload::Text("keep this copy".into()),
            Payload::Image(image),
        ] {
            let mut now = Instant::now();
            let mut state = State::new(true);
            let status = RuntimeStatus::new(
                &crate::config::Config::default(),
                "direct",
                "test-pc".into(),
                Duration::from_millis(400),
            )
            .unwrap();
            status.initialized("test-pc".into());
            state.observe(
                Snapshot {
                    content: Some(payload.clone()),
                    revision: Some(1),
                },
                now,
            );
            for seconds in [1, 2, 4, 8, 16, 30, 30, 30] {
                let (generation, pending) = state.outgoing_due(now).unwrap();
                assert_eq!(pending, payload);
                let error = wx_ime_sdk::Error::Http(400);
                status.network_failed(&error);
                state.send_failed(generation, publish_failure(&error), now);
                update_local_status(&status, &state);
                assert_eq!(status.value()["phase"], "retrying");
                assert_eq!(status.value()["pending_send"], true);
                assert_eq!(status.value()["send_paused"], false);
                assert!(status.value()["last_error"]
                    .as_str()
                    .unwrap()
                    .contains("自动恢复会话"));
                now += Duration::from_secs(seconds);
                assert!(state.outgoing_due(now - Duration::from_millis(1)).is_none());
                assert_eq!(state.outgoing_due(now).unwrap().1, payload);
            }
            let (generation, _) = state.outgoing_due(now).unwrap();
            state.sent(generation, 10, 21);
            status.sent();
            update_local_status(&status, &state);
            assert!(state.outgoing.is_none());
            assert_eq!(state.last_published, Some((10, 21)));
            assert_eq!(status.value()["phase"], "running");
            assert_eq!(status.value()["pending_send"], false);
            assert_eq!(status.value()["sent_count"], 1);
            assert!(status.value()["last_error"].is_null());
        }
    }

    #[test]
    fn new_copy_supersedes_a_control_400_retry_without_manual_intervention() {
        let now = Instant::now();
        let mut state = State::new(true);
        state.observe(
            Snapshot {
                content: Some(Payload::Text("old copy".into())),
                revision: Some(1),
            },
            now,
        );
        let old = state.outgoing_due(now).unwrap().0;
        state.send_failed(old, publish_failure(&wx_ime_sdk::Error::Http(400)), now);
        let latest = Payload::Image(Arc::new(
            crate::image::ClipboardImage::from_rgba(1, 1, vec![34, 56, 78, 255]).unwrap(),
        ));
        state.observe(
            Snapshot {
                content: Some(latest.clone()),
                revision: Some(2),
            },
            now,
        );
        // A completion from an older operation cannot pause or remove this copy.
        state.send_failed(
            old,
            publish_failure(&wx_ime_sdk::Error::OutcomeUnknown),
            now,
        );
        state.sent(old, 10, 20);
        let (generation, payload) = state.outgoing_due(now).unwrap();
        assert_ne!(generation, old);
        assert_eq!(payload, latest);
        state.sent(generation, 10, 21);
        assert!(state.outgoing.is_none());
    }

    #[test]
    fn control_recovery_does_not_replay_uncertain_publications_or_invalid_objects() {
        for (error, expected) in [
            (
                wx_ime_sdk::Error::OutcomeUnknown,
                PublishFailure::OutcomeUnknown,
            ),
            (wx_ime_sdk::Error::CosHttp(400), PublishFailure::Rejected),
            (wx_ime_sdk::Error::Http(403), PublishFailure::Rejected),
            (
                wx_ime_sdk::Error::Invalid("object exceeds server COS limit"),
                PublishFailure::Rejected,
            ),
        ] {
            let now = Instant::now();
            let mut state = State::new(true);
            state.observe(
                Snapshot {
                    content: Some(Payload::Text("pending copy".into())),
                    revision: Some(1),
                },
                now,
            );
            let generation = state.outgoing_due(now).unwrap().0;
            assert_eq!(publish_failure(&error), expected);
            state.send_failed(generation, publish_failure(&error), now);
            assert!(state.outgoing.as_ref().unwrap().paused);
            assert!(state
                .outgoing_due(now + Duration::from_secs(3600))
                .is_none());
            state.force_retry(now);
            assert!(state.outgoing_due(now).is_some());
        }
    }

    #[test]
    fn status_clears_superseded_local_errors_and_keeps_paused_send_visible() {
        let now = Instant::now();
        let mut state = State::new(false);
        let runtime_status = RuntimeStatus::new(
            &crate::config::Config::default(),
            "direct",
            "test-pc".into(),
            Duration::from_millis(400),
        )
        .unwrap();
        runtime_status.initialized("test-pc".into());
        state.observe(
            Snapshot {
                content: None,
                revision: None,
            },
            now,
        );
        state.received(
            receipt(serde_json::json!({"Text": "incoming"}), 11),
            state.generation,
            now,
        );
        runtime_status.local_result(LocalOperation::Write, false);
        update_local_status(&runtime_status, &state);
        assert_eq!(runtime_status.value()["phase"], "retrying");
        state.observe(
            Snapshot {
                content: Some(Payload::Text("new local copy".into())),
                revision: None,
            },
            now,
        );
        let generation = state.outgoing.as_ref().unwrap().generation;
        state.send_failed(generation, PublishFailure::OutcomeUnknown, now);
        update_local_status(&runtime_status, &state);
        assert!(
            runtime_status.value()["last_error"].is_null(),
            "discarded receipt no longer has a write failure"
        );
        assert_eq!(runtime_status.value()["pending_send"], true);
        assert_eq!(runtime_status.value()["send_paused"], true);
        state.force_retry(now);
        update_local_status(&runtime_status, &state);
        assert_eq!(runtime_status.value()["send_paused"], false);
    }

    #[tokio::test]
    async fn preparation_verifies_and_decodes_a_real_download_without_a_clipboard() {
        let directory = tempfile::tempdir().unwrap();
        let original =
            crate::image::ClipboardImage::from_rgba(1, 1, vec![12, 34, 56, 255]).unwrap();
        let image = downloaded(directory.path().join("image"), original.encoded());
        std::fs::write(&image.path, original.encoded()).unwrap();
        let prepared = prepare_image(image).await.unwrap();
        assert_eq!(prepared, Payload::Image(Arc::new(original)));
    }

    #[tokio::test]
    async fn a_missing_cache_file_remains_retryable_until_the_file_is_readable() {
        let directory = tempfile::tempdir().unwrap();
        let original = crate::image::ClipboardImage::from_rgba(1, 1, vec![1, 2, 3, 255]).unwrap();
        let image = downloaded(directory.path().join("image"), original.encoded());
        assert!(matches!(
            prepare_image(image.clone()).await,
            Err(PrepareFailure::Retry(_))
        ));
        std::fs::write(&image.path, original.encoded()).unwrap();
        assert!(matches!(prepare_image(image).await, Ok(Payload::Image(_))));
    }

    #[tokio::test]
    async fn truncated_png_is_consumed_and_does_not_block_the_next_message() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\n";
        let image = downloaded(directory.path().join("truncated"), bytes);
        std::fs::write(&image.path, bytes).unwrap();
        let now = Instant::now();
        let mut state = State::new(false);
        state.observe(
            Snapshot {
                content: None,
                revision: None,
            },
            now,
        );
        state.received(
            receipt(serde_json::json!({ "Image": image }), 20),
            state.generation,
            now,
        );
        let (id, pending) = state.prepare_due(now).unwrap();
        let result = prepare_image(pending).await;
        assert!(
            matches!(result, Err(PrepareFailure::Invalid(_))),
            "{result:?}"
        );
        assert!(state.invalid_image(&id, now));
        assert!(!state.apply_due(now));
        assert_eq!(state.ack_due(now), Some(id.clone()));
        state.acknowledged(&id);
        state.received(
            receipt(serde_json::json!({ "Text": "after invalid image" }), 21),
            state.generation,
            now,
        );
        assert!(state.apply_due(now));
    }

    #[tokio::test]
    async fn oversized_image_is_consumed_before_opening_or_allocating_its_payload() {
        let directory = tempfile::tempdir().unwrap();
        let image = DownloadedImage {
            path: directory.path().join("not-created"),
            bytes: crate::image::MAX_ENCODED_BYTES + 1,
            sha256: String::new(),
        };
        let now = Instant::now();
        let mut state = State::new(false);
        state.observe(
            Snapshot {
                content: None,
                revision: None,
            },
            now,
        );
        state.received(
            receipt(serde_json::json!({ "Image": image }), 20),
            state.generation,
            now,
        );
        let (id, pending) = state.prepare_due(now).unwrap();
        assert!(matches!(
            prepare_image(pending).await,
            Err(PrepareFailure::Invalid(_))
        ));
        assert!(state.invalid_image(&id, now));
        state.acknowledged(&state.ack_due(now).unwrap());
        state.received(
            receipt(serde_json::json!({ "Text": "after oversized image" }), 21),
            state.generation,
            now,
        );
        assert!(state.apply_due(now));
    }
}
