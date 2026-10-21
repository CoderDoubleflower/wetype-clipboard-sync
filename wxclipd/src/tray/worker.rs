//! Blocking operations and local IPC never execute on the Win32 message thread.

use super::{Action, SharedView, WM_FINISHED};
use crate::tray_model::{safe_proxy, LaunchContext};
use crate::{config, ipc};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

pub fn run(
    runtime: tokio::runtime::Runtime,
    state: PathBuf,
    mut context: LaunchContext,
    view: SharedView,
    receiver: Receiver<Action>,
    window: usize,
) {
    let mut child: Option<Child> = None;
    let mut setup: Option<Child> = None;
    let mut next_probe = Instant::now();
    loop {
        let action = match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(action) => Some(action),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                let _ = runtime.block_on(stop_owned(&state, &mut child));
                break;
            }
        };
        if let Some(action) = action {
            let operation = match &action {
                Action::Launch(_) | Action::Start => "正在启动同步……",
                Action::Stop => "正在停止同步……",
                Action::Restart => "正在重新加载配置并启动……",
                Action::Sync => "正在请求立即同步……",
                Action::Setup => "正在打开设置与配对……",
                Action::Exit => "正在停止同步并退出托盘……",
            };
            {
                let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
                view.busy = true;
                view.operation = Some(operation.to_owned());
                view.notice = None;
            }
            let exit = matches!(&action, Action::Exit);
            let owned_settings_open = setup
                .as_mut()
                .is_some_and(|process| process.try_wait().ok().flatten().is_none());
            let settings_query = ipc::setup_in_progress(&state);
            let settings_unknown = settings_query.is_err();
            let settings_open = owned_settings_open || settings_query.unwrap_or(true);
            let blocked_by_settings = settings_open
                && matches!(
                    &action,
                    Action::Start | Action::Stop | Action::Restart | Action::Exit
                );
            let result = runtime.block_on(async {
                // The interactive setup command can start a tray after saving.
                // Never report a clean exit/uninstall while that command is
                // still alive and could restart us. Do not discard user input.
                if blocked_by_settings {
                    bail!("请先关闭设置与配对窗口");
                }
                match action {
                    Action::Launch(mut new_context) => {
                        let setup_complete = new_context.setup_complete;
                        new_context.setup_complete = false;
                        context = new_context;
                        if settings_unknown {
                            bail!("无法确认设置窗口状态");
                        }
                        if settings_open && !setup_complete {
                            return Ok("设置与配对正在进行；完成后会自动启动同步。".to_owned());
                        }
                        ensure_started(&state, &context, &mut child).await
                    }
                    Action::Start => ensure_started(&state, &context, &mut child).await,
                    Action::Stop | Action::Exit => {
                        stop_owned(&state, &mut child).await?;
                        Ok("同步已停止。".to_owned())
                    }
                    Action::Restart => {
                        stop_owned(&state, &mut child).await?;
                        ensure_started(&state, &context, &mut child).await
                    }
                    Action::Sync => {
                        short_call(&state, "sync").await?.context("同步尚未启动")?;
                        Ok("已请求立即同步；实际收发结果会在状态中更新。".to_owned())
                    }
                    Action::Setup => {
                        if settings_unknown {
                            bail!("无法确认设置窗口状态");
                        }
                        if settings_open {
                            return Ok("设置窗口已经打开，请在该窗口继续。".to_owned());
                        }
                        stop_owned(&state, &mut child).await?;
                        let mut command = cli_command(&state, &context)?;
                        command.arg("setup").creation_flags(0x0000_0010); // CREATE_NEW_CONSOLE
                        setup = Some(command.spawn().context("打开设置窗口失败")?);
                        Ok("设置窗口已打开；完成配置与配对后会重新启动同步。".to_owned())
                    }
                }
            });
            if exit && result.is_ok() {
                unsafe {
                    PostMessageW(window as _, WM_FINISHED, 0, 0);
                }
                break;
            }
            {
                let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
                view.busy = false;
                view.operation = None;
                view.notice = Some(match result {
                    Ok(message) => message,
                    Err(_) if settings_unknown => {
                        "无法确认设置窗口状态。请稍后重试或检查当前用户的目录权限。".to_owned()
                    }
                    Err(_) if blocked_by_settings => {
                        "请先完成或关闭「设置与配对」窗口，再开始、停止、重启或退出同步。"
                            .to_owned()
                    }
                    Err(_) if exit => {
                        "未能确认后台已停止，托盘已保留。请稍后重试；仍失败时查看日志。".to_owned()
                    }
                    Err(_) => {
                        "操作未完成。请检查保存的配置与代理，或通过「查看日志」排查；可以重试。"
                            .to_owned()
                    }
                });
            }
            next_probe = Instant::now();
        }
        if Instant::now() >= next_probe {
            let snapshot = runtime.block_on(inspect(&state));
            let saved = saved_config(&state, &context);
            let mut view = view.lock().unwrap_or_else(|e| e.into_inner());
            view.runtime = snapshot.ok();
            view.saved_config = saved;
            view.ready = true;
            if let Some(process) = child.as_mut() {
                if let Ok(Some(status)) = process.try_wait() {
                    if !status.success() {
                        view.notice = Some(
                            "同步进程已退出。请检查代理或配置，查看日志后点击「开始同步」。"
                                .to_owned(),
                        );
                    }
                    child = None;
                }
            }
            next_probe = Instant::now() + Duration::from_secs(2);
        }
    }
}

async fn short_call(state: &Path, command: &str) -> Result<Option<String>> {
    tokio::time::timeout(Duration::from_secs(3), ipc::call(state, command))
        .await
        .context("本地控制响应超时")?
}

async fn inspect(state: &Path) -> Result<Value> {
    match short_call(state, "inspect").await {
        Ok(Some(reply)) => {
            let value: Value =
                serde_json::from_str(reply.strip_prefix("ok ").context("状态响应格式无效")?)?;
            if value["schema"] != 1 || !value["phase"].is_string() {
                bail!("不支持的状态响应");
            }
            Ok(value)
        }
        Ok(None) => Ok(json!({"schema":1,"pid":null,"phase":"stopped"})),
        Err(error) => {
            // Attaching to a pre-tray daemon must not start a duplicate worker.
            if let Ok(Some(reply)) = short_call(state, "ping").await {
                if let Some(pid) = reply
                    .strip_prefix("ok ")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    return Ok(json!({"schema":1,"pid":pid,"phase":"unavailable"}));
                }
            }
            Err(error)
        }
    }
}

fn cli_command(state: &Path, context: &LaunchContext) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?.with_file_name("wxclipd.exe"));
    command.arg("--state").arg(state);
    command.env_remove("WXCLIPD_SETUP_COMPLETE");
    match &context.proxy_override {
        Some(value) => {
            command.env("WXCLIPD_PROXY", value);
        }
        None => {
            command.env_remove("WXCLIPD_PROXY");
        }
    }
    match &context.name {
        Some(name) => {
            command.env("WXCLIPD_NAME", name);
            command.arg("--name").arg(name);
        }
        None => {
            command.env_remove("WXCLIPD_NAME");
        }
    }
    Ok(command)
}

async fn ensure_started(
    state: &Path,
    context: &LaunchContext,
    child: &mut Option<Child>,
) -> Result<String> {
    if short_call(state, "ping").await?.is_some() {
        return Ok("同步已经运行。保存配置后可点击「重启同步」应用。".to_owned());
    }
    if let Some(process) = child.as_mut() {
        if process.try_wait()?.is_none() {
            return Ok("后台正在启动，请稍候。".to_owned());
        }
    }
    ipc::prepare_state_dir(state)?;
    let log_path = state.join("wxclipd.log");
    if std::fs::metadata(&log_path).is_ok_and(|metadata| metadata.len() > 2 * 1024 * 1024) {
        let previous = state.join("wxclipd.previous.log");
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::rename(&log_path, previous);
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let mut command = cli_command(state, context)?;
    *child = Some(
        command
            .arg("run")
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("启动同步进程失败")?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.as_mut().unwrap().try_wait()? {
            bail!("同步进程在初始化时退出：{status}");
        }
        if short_call(state, "ping").await?.is_some() {
            return Ok("后台已启动，正在连接服务。".to_owned());
        }
        if Instant::now() >= deadline {
            bail!("同步进程尚未提供控制接口");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn stop_owned(state: &Path, child: &mut Option<Child>) -> Result<()> {
    ipc::stop_and_wait(state).await?;
    // A newly launched child can still be parsing settings before its named pipe
    // exists. Do not report a successful exit while leaving that process behind.
    if let Some(process) = child.as_mut() {
        for _ in 0..20 {
            if process.try_wait()?.is_some() {
                *child = None;
                return Ok(());
            }
            if short_call(state, "ping").await?.is_some() {
                ipc::stop_and_wait(state).await?;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("后台仍在初始化，不能确认已停止");
    }
    Ok(())
}

fn saved_config(state: &Path, context: &LaunchContext) -> String {
    let mut text = match config::Config::load(state) {
        Ok(config) => format!(
            "代理：{}\r\n远端轮询：{} 秒；Ctrl / 右键触发：{}",
            safe_proxy(config.proxy.as_deref()),
            config.poll_interval_seconds,
            if config.paste_trigger {
                "开启"
            } else {
                "关闭"
            }
        ),
        Err(_) => "config.json 无效或无法读取；可通过设置向导修复，或打开配置目录检查。".to_owned(),
    };
    if let Some(proxy) = &context.proxy_override {
        text.push_str(&format!(
            "\r\n启动环境中的代理覆盖：{}",
            safe_proxy(Some(proxy))
        ));
    }
    text
}
