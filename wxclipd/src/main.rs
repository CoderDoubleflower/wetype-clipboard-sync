//! `wxclipd`: Windows clipboard sync for the WeType (微信输入法) cross-device
//! clipboard group.

mod clipboard;
mod config;
mod image;
mod input_watch;
mod ipc;
mod runtime_status;
mod sync;

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use wx_ime_sdk::{Client, ClientOptions, PairingCode, PairingUpdate, ProxyConfig};

const HELP: &str = "\
wxclipd - Windows 微信输入法跨设备剪贴板

用法:
  wxclipd [--state DIR] [--name NAME] [--proxy URL | --no-proxy] <命令>

命令:
  setup             交互设置代理并完成手机配对
  config            查看配置；带代理/轮询选项时保存到 config.json
  code              生成六位配对码，等待另一台设备输入并确认
  pair <CODE>       输入官方输入法显示的六位配对码
  devices           列出本机与已绑定设备
  unbind [--peer UIN]  解绑一台已关联设备
  status            显示剪贴板同步状态
  inspect           显示本地运行状态 JSON（不访问网络）
  run               按已保存的方向设置同步剪贴板（前台运行，Ctrl-C 退出）
  start             Windows 后台运行，日志写入状态目录
  stop              停止当前用户的后台同步
  sync-now          通知正在运行的 wxclipd 立即同步一次

选项:
  --state DIR       状态目录；默认 %LOCALAPPDATA%/wxclipd
  --name NAME       设备名，仅首次创建身份时使用
  --proxy URL       HTTP / SOCKS5 / SOCKS5h 代理，支持用户名密码
  --no-proxy        显式直连，不读取系统 HTTP_PROXY/HTTPS_PROXY
  --peer UIN        unbind 要解绑的设备 Uin（只有一台时可省略）
  --yes             配对时自动确认对方，不询问
  --interval MS     本地剪贴板轮询间隔，默认 400
  --poll-interval S 远端轮询秒数 (1..3600)，默认 10
  --no-paste-trigger        关闭「Ctrl/右键触发即时拉取」（默认开启）
  --paste-trigger-debounce-ms MS  粘贴触发防抖毫秒数，默认 800
  --send-current    启动时把当前剪贴板内容发送一次
  -h, --help        显示帮助
  -V, --version     显示版本

代理优先级：命令行 > WXCLIPD_PROXY > config.json > 直连。
临时命令行设置不会更改已经运行的后台进程；保存配置后请 stop/start。
";

struct Args {
    state: PathBuf,
    name: Option<String>,
    settings: config::Config,
    explicit_settings: bool,
    proxy_source: &'static str,
    command: Command,
}

enum Command {
    Setup,
    Config,
    Start,
    Stop,
    Code {
        yes: bool,
    },
    Pair {
        code: String,
    },
    Devices,
    Unbind {
        peer: Option<u64>,
        yes: bool,
    },
    Status,
    Inspect,
    SyncNow,
    Run {
        interval: Duration,
        send_current: bool,
        poll_interval: Duration,
        paste_trigger: bool,
        paste_debounce: Duration,
    },
}

fn main() -> Result<()> {
    let tokens: Vec<String> = std::env::args().skip(1).collect();
    let interactive = tokens.iter().any(|arg| arg == "setup") || tokens.is_empty();
    // Keep the setup presence guard through both successful and failed setup
    // prompts, including a setup window launched directly from a shortcut.
    let mut setup_guard = None;
    let result = (|| {
        let args = parse(tokens.into_iter())?;
        if matches!(args.command, Command::Setup) {
            setup_guard = Some(ipc::begin_setup(&args.state)?);
        }
        real_main(args)
    })();
    if interactive && result.is_err() {
        if let Err(error) = &result {
            eprintln!("设置失败：{error:#}");
        }
        let _ = prompt("按 Enter 关闭窗口：");
    }
    result
}

fn real_main(args: Args) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(dispatch(args))
}

async fn dispatch(args: Args) -> Result<()> {
    if matches!(args.command, Command::Inspect) {
        match control_call(&args.state, "inspect").await? {
            Some(reply) => {
                let value: serde_json::Value = serde_json::from_str(
                    reply
                        .strip_prefix("ok ")
                        .context("后台返回了无效的运行状态")?,
                )?;
                println!("{value}");
            }
            None => println!(
                "{}",
                serde_json::json!({"schema": 1, "pid": null, "phase": "stopped"})
            ),
        }
        return Ok(());
    }
    if matches!(args.command, Command::Config) {
        if args.explicit_settings {
            args.settings.save(&args.state)?;
            println!(
                "已保存配置：{}。后台运行时请 stop 后重新 start。",
                config::path(&args.state).display()
            );
        }
        println!("状态目录：{}", args.state.display());
        match args.settings.proxy.as_deref() {
            Some(url) => println!("代理：{:?}", ProxyConfig::parse(url)?),
            None => println!("代理：直连"),
        }
        println!(
            "远端轮询：{} 秒；粘贴触发：{}",
            args.settings.poll_interval_seconds, args.settings.paste_trigger
        );
        println!(
            "发送到其他设备：{}；接收其他设备：{}",
            args.settings.send_enabled, args.settings.receive_enabled
        );
        return Ok(());
    }
    if matches!(args.command, Command::Setup) {
        return setup(args).await;
    }
    if matches!(args.command, Command::Stop) {
        match ipc::stop_and_wait(&args.state).await? {
            true => println!("同步进程已停止。"),
            false => println!("同步进程未运行。"),
        }
        return Ok(());
    }
    if matches!(args.command, Command::Start) {
        if args.explicit_settings {
            args.settings.save(&args.state)?;
        }
        // The resident tray attaches to an existing worker and deduplicates
        // repeated launches. Always invoke it so a CLI-started worker gains a
        // visible status/configuration entry point too.
        return start_background(
            &args.state,
            &args.settings,
            args.proxy_source,
            args.name.as_deref(),
            false,
        );
    }
    if matches!(args.command, Command::SyncNow) {
        return sync_now(&args.state).await;
    }
    if args.explicit_settings
        && !matches!(args.command, Command::Run { .. })
        && control_call(&args.state, "ping").await?.is_some()
    {
        bail!("后台进程正在运行，不能临时覆盖它的代理/轮询设置。请先 stop，或使用 config 保存后重新 start。");
    }
    // 设备列表和解绑优先走正在运行的服务，避免抢占身份锁。
    if matches!(args.command, Command::Devices) {
        if let Some(reply) = control_call(&args.state, "devices").await? {
            return print_devices(&reply);
        }
    }
    if let Command::Unbind { peer, yes } = &args.command {
        if let Some(reply) = control_call(&args.state, "devices").await? {
            return unbind_via_control(&args.state, &reply, *peer, *yes).await;
        }
    }
    if matches!(args.command, Command::Status) {
        if let Some(reply) = control_call(&args.state, "status").await? {
            return print_status(&reply);
        }
    }
    if let Command::Pair { code } = &args.command {
        if let Some(reply) = control_call(&args.state, &format!("pair {code}")).await? {
            return print_paired(&reply);
        }
    }
    ipc::prepare_state_dir(&args.state)?;
    let name = resolve_name(&args.state, args.name)?;
    let local_interval = match &args.command {
        Command::Run { interval, .. } => *interval,
        _ => Duration::from_millis(400),
    };
    let runtime_status = runtime_status::RuntimeStatus::new(
        &args.settings,
        args.proxy_source,
        name.clone(),
        local_interval,
    )?;
    let mut options = ClientOptions::new(&args.state, name);
    options.proxy = args
        .settings
        .proxy
        .as_deref()
        .map(ProxyConfig::parse)
        .transpose()?;
    let (client, listener) = if matches!(args.command, Command::Run { .. }) {
        let mut listener = ipc::listen(&ipc::endpoint(&args.state)?)?;
        let Some(client) = open_for_run(options, &mut listener, runtime_status.clone()).await?
        else {
            return Ok(());
        };
        (client, Some(listener))
    } else {
        (
            match Client::open(options).await {
                Ok(client) => client,
                Err(error) if error.to_string().contains("already in use") => {
                    bail!(
                        "wxclipd 服务正在运行并持有身份：{error}\n\
                 提示：先 `wxclipd stop`，执行完再重新启动同步。\n\
                 `devices` / `unbind` / `sync-now` / `status` / `pair` 支持在服务运行时直接使用。"
                    );
                }
                Err(error) => return Err(error.into()),
            },
            None,
        )
    };
    let result = match args.command {
        Command::Code { yes } => code(&client, yes).await,
        Command::Pair { code } => pair(&client, code).await,
        Command::Devices => devices(&client).await,
        Command::Unbind { peer, yes } => unbind_local(&client, peer, yes).await,
        Command::Status => status(&client).await,
        Command::SyncNow
        | Command::Setup
        | Command::Config
        | Command::Start
        | Command::Stop
        | Command::Inspect => {
            unreachable!("该命令不打开身份")
        }
        Command::Run {
            interval,
            send_current,
            poll_interval,
            paste_trigger,
            paste_debounce,
        } => {
            let local_name = client.local_device().await?.name;
            runtime_status.initialized(local_name.clone());
            println!(
                "[wxclipd] Windows 剪贴板（Unicode 文本 / PNG / DIB 图片），设备: {}",
                local_name
            );
            sync::run(
                client.clone(),
                sync::SyncOptions {
                    local_interval: interval,
                    poll_interval,
                    paste_trigger,
                    paste_debounce,
                    send_current,
                    send_enabled: args.settings.send_enabled,
                    receive_enabled: args.settings.receive_enabled,
                },
                ipc::endpoint(&args.state)?,
                listener,
                runtime_status,
            )
            .await
        }
    };
    let shutdown = client.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

/// Keep local controls available while a proxy/network is not ready at logon.
async fn open_for_run(
    options: ClientOptions,
    listener: &mut ipc::Listener,
    runtime_status: runtime_status::RuntimeStatus,
) -> Result<Option<Client>> {
    let initialize = async {
        let mut delay = 2;
        loop {
            match Client::open(options.clone()).await {
                Ok(client) => return Ok(client),
                Err(
                    error @ (wx_ime_sdk::Error::Transport
                    | wx_ime_sdk::Error::Proxy(_)
                    | wx_ime_sdk::Error::ProxyHttp(_)
                    | wx_ime_sdk::Error::Http(_)
                    | wx_ime_sdk::Error::Timeout(_)),
                ) => {
                    runtime_status.network_failed(&error);
                    eprintln!("[wxclipd] 初始化连接失败：{error}；{delay} 秒后重试，可用 wxclipd stop 停止。");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                    delay = (delay * 2).min(60);
                }
                Err(error) => return Err(anyhow::Error::from(error)),
            }
        }
    };
    tokio::pin!(initialize);
    loop {
        tokio::select! {
            result = &mut initialize => return result.map(Some),
            _ = tokio::signal::ctrl_c() => {
                runtime_status.stopping();
                return Ok(None);
            },
            stream = listener.accept() => {
                let mut reader = BufReader::new(stream?).take(256);
                let mut command = String::new();
                match tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut command)).await {
                    Ok(Ok(_)) if command.ends_with('\n') && command.len() <= 256 => {},
                    _ => continue,
                }
                let mut stream = reader.into_inner().into_inner();
                let stop = command.trim() == "stop";
                let reply = match command.trim() {
                    "ping" => format!("ok {}\n", std::process::id()),
                    "inspect" => runtime_status.reply(),
                    "stop" => "ok\n".to_owned(),
                    _ => "err 网络连接正在初始化，请稍后重试；可以 stop 停止。\n".to_owned(),
                };
                let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(reply.as_bytes())).await;
                let _ = stream.shutdown().await;
                if stop {
                    runtime_status.stopping();
                    return Ok(None);
                }
            }
        }
    }
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("输入已结束");
    }
    Ok(line.trim().to_string())
}

async fn setup(mut args: Args) -> Result<()> {
    println!("WeType 剪贴板同步 — 配置与配对\n");
    if control_call(&args.state, "ping").await?.is_some() {
        println!("正在停止后台同步，以便更新设置……");
        ipc::stop_and_wait(&args.state).await?;
    }
    println!("支持 http://、socks5://、socks5h://；socks5h 由代理解析目标域名。");
    match &args.settings.proxy {
        Some(proxy) => println!("当前代理：{}", ProxyConfig::parse(proxy)?),
        None => println!("当前代理：直连"),
    }
    let proxy = prompt("代理地址（Enter 保持，输入 direct 改为直连）：")?;
    if proxy.eq_ignore_ascii_case("direct") {
        args.settings.proxy = None;
    } else if !proxy.is_empty() {
        ProxyConfig::parse(&proxy)?;
        args.settings.proxy = Some(proxy);
    }
    let seconds = prompt(&format!(
        "远端轮询秒数（Enter 保持 {}）：",
        args.settings.poll_interval_seconds
    ))?;
    if !seconds.is_empty() {
        let seconds: u64 = seconds.parse().context("轮询秒数必须为整数")?;
        if !(1..=3600).contains(&seconds) {
            bail!("轮询秒数必须是 1..3600");
        }
        args.settings.poll_interval_seconds = seconds;
    }
    args.settings.save(&args.state)?;
    println!("配置已保存。");
    if stored_name(&args.state)?.is_none() && args.name.is_none() {
        let name = prompt("设备名称（Enter 使用电脑名称）：")?;
        if !name.is_empty() {
            args.name = Some(name);
        }
    }
    println!("\n在手机微信输入法中打开「跨设备粘贴」，显示六位配对码。");
    let pairing_code = prompt("输入手机配对码（Enter 跳过配对）：")?;
    if !pairing_code.is_empty() {
        PairingCode::parse(pairing_code.clone())?;
        let name = resolve_name(&args.state, args.name.clone())?;
        let mut options = ClientOptions::new(&args.state, name);
        options.proxy = args
            .settings
            .proxy
            .as_deref()
            .map(ProxyConfig::parse)
            .transpose()?;
        let client = Client::open(options).await?;
        let result = pair(&client, pairing_code).await;
        let shutdown = client.shutdown().await;
        result?;
        shutdown?;
    }
    start_background(
        &args.state,
        &args.settings,
        "command",
        args.name.as_deref(),
        true,
    )?;
    let _ = prompt("按 Enter 关闭设置窗口：");
    Ok(())
}

fn start_background(
    state: &Path,
    settings: &config::Config,
    proxy_source: &str,
    name: Option<&str>,
    setup_complete: bool,
) -> Result<()> {
    let launcher = std::env::current_exe()?.with_file_name("wxclipd-background.exe");
    let mut command = std::process::Command::new(launcher);
    command.arg("--state").arg(state);
    if setup_complete {
        command.env("WXCLIPD_SETUP_COMPLETE", "1");
    } else {
        command.env_remove("WXCLIPD_SETUP_COMPLETE");
    }
    // Explicit start options and setup have already saved their choice.
    // Do not turn a saved value into a permanent environment override in
    // the resident tray: subsequent restarts must read the updated file.
    // A genuine environment override retains its documented precedence.
    if proxy_source == "environment" {
        command.env("WXCLIPD_PROXY", settings.proxy.as_deref().unwrap_or(""));
    } else {
        command.env_remove("WXCLIPD_PROXY");
    }
    if let Some(name) = name {
        command.env("WXCLIPD_NAME", name);
    } else {
        command.env_remove("WXCLIPD_NAME");
    }
    command
        .spawn()
        .context("启动后台程序失败，请确认 wxclipd-background.exe 在同一目录")?;
    println!(
        "已打开托盘同步程序。可通过右下角图标查看运行状态与配置。日志：{}",
        state.join("wxclipd.log").display()
    );
    Ok(())
}

async fn code(client: &Client, yes: bool) -> Result<()> {
    let mut session = client.request_pairing_code().await?;
    let code = session
        .code()
        .context("没有拿到配对码")?
        .expose()
        .to_string();
    println!("配对码: {code}");
    println!("在另一台官方输入法里输入上面的配对码，有效期约 60 秒。");
    loop {
        match session.next_update().await? {
            PairingUpdate::AwaitingPeer => {}
            PairingUpdate::ConfirmationRequired { peer_name, .. } => {
                if yes || confirm(&format!("设备「{peer_name}」请求加入，允许吗？"))? {
                    let (_, peer) = session.confirm().await?;
                    println!(
                        "已配对: {}{}",
                        peer.name,
                        peer.uin
                            .map(|uin| format!(" (Uin {uin})"))
                            .unwrap_or_default()
                    );
                    return Ok(());
                }
                session.cancel().await?;
                bail!("已拒绝这次配对");
            }
            PairingUpdate::Paired { peer, .. } => {
                println!(
                    "已配对: {}{}",
                    peer.name,
                    peer.uin
                        .map(|uin| format!(" (Uin {uin})"))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            PairingUpdate::CodeUnavailable => bail!("配对码不可用，请重新生成"),
            PairingUpdate::Expired => bail!("配对码已过期，请重新生成"),
            PairingUpdate::Cancelled => bail!("配对已取消"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn pair(client: &Client, code: String) -> Result<()> {
    let mut session = client.join(PairingCode::parse(code)?).await?;
    println!("已提交配对码，等待另一台设备确认。");
    loop {
        match session.next_update().await? {
            PairingUpdate::AwaitingPeer => {}
            PairingUpdate::Paired { peer, group } => {
                println!(
                    "已配对: {}{}，当前组共 {} 台设备",
                    peer.name,
                    peer.uin
                        .map(|uin| format!(" (Uin {uin})"))
                        .unwrap_or_default(),
                    group.devices.len()
                );
                return Ok(());
            }
            PairingUpdate::ConfirmationRequired { .. } => {
                bail!("配对状态异常：本机不应承担确认方");
            }
            PairingUpdate::CodeUnavailable => bail!("配对码不可用"),
            PairingUpdate::Expired => bail!("配对超时，请重新生成配对码"),
            PairingUpdate::Cancelled => bail!("对方取消了配对"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn devices(client: &Client) -> Result<()> {
    let local = client.local_device().await?;
    println!("本机: {} (Uin {})", local.name, local.uin);
    print_peers(&local_peers(client, local.uin).await?);
    Ok(())
}

async fn local_peers(client: &Client, own_uin: u64) -> Result<Vec<Peer>> {
    Ok(client
        .devices()
        .await?
        .into_iter()
        .filter(|device| device.uin != own_uin)
        .map(|device| Peer {
            uin: device.uin,
            name: device.name,
            platform: device.platform as u64,
            supports_file_transfer: device.supports_file_transfer,
        })
        .collect())
}

struct Peer {
    uin: u64,
    name: String,
    platform: u64,
    supports_file_transfer: bool,
}

fn print_peers(peers: &[Peer]) {
    if peers.is_empty() {
        println!("尚未配对，运行 `wxclipd pair CODE` 或 `wxclipd code`。");
        return;
    }
    for peer in peers {
        println!(
            "已绑定: {} (Uin {}, 平台 {}{})",
            peer.name,
            peer.uin,
            peer.platform,
            if peer.supports_file_transfer {
                ", 支持文件传输"
            } else {
                ""
            }
        );
    }
}

/// 选定要解绑的设备；返回 `None` 表示没有设备或用户取消。
fn choose_peer(peers: &[Peer], requested: Option<u64>, yes: bool) -> Result<Option<(u64, String)>> {
    if peers.is_empty() {
        println!("当前没有已绑定设备。");
        return Ok(None);
    }
    let (uin, name) = match requested {
        Some(uin) => peers
            .iter()
            .find(|peer| peer.uin == uin)
            .map(|peer| (peer.uin, peer.name.clone()))
            .with_context(|| format!("--peer {uin} 不在当前设备组里"))?,
        None if peers.len() == 1 => (peers[0].uin, peers[0].name.clone()),
        None => {
            print_peers(peers);
            bail!("设备组里有多台设备，请用 --peer UIN 指定要解绑的设备");
        }
    };
    if !yes && !confirm(&format!("确认解绑 {name} (Uin {uin})？"))? {
        println!("已取消");
        return Ok(None);
    }
    Ok(Some((uin, name)))
}

async fn unbind_local(client: &Client, peer: Option<u64>, yes: bool) -> Result<()> {
    let local = client.local_device().await?;
    let peers = local_peers(client, local.uin).await?;
    let Some((uin, name)) = choose_peer(&peers, peer, yes)? else {
        return Ok(());
    };
    client.unbind(uin).await?;
    let remaining = client.devices().await.unwrap_or_default();
    if remaining.iter().any(|device| device.uin == uin) {
        bail!("服务端仍返回 {name} (Uin {uin})，解绑未生效");
    }
    println!("已解绑 {name} (Uin {uin})");
    Ok(())
}

async fn unbind_via_control(
    state: &Path,
    devices_reply: &str,
    peer: Option<u64>,
    yes: bool,
) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(devices_reply.trim()).context("解析设备列表失败")?;
    let peers = peers_from_json(&value);
    let Some((uin, name)) = choose_peer(&peers, peer, yes)? else {
        return Ok(());
    };
    control_call(state, &format!("unbind {uin}"))
        .await?
        .context("wxclipd 服务已退出，未执行解绑")?;
    println!("已解绑 {name} (Uin {uin})");
    Ok(())
}

fn peers_from_json(value: &serde_json::Value) -> Vec<Peer> {
    value["devices"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|device| {
            Some(Peer {
                uin: device["uin"].as_u64()?,
                name: device["name"].as_str().unwrap_or("?").to_string(),
                platform: device["platform"].as_u64().unwrap_or(0),
                supports_file_transfer: device["supports_file_transfer"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn print_devices(reply: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(reply.trim()).context("解析设备列表失败")?;
    println!(
        "本机: {} (Uin {})",
        value["local"]["name"].as_str().unwrap_or("?"),
        value["local"]["uin"].as_u64().unwrap_or(0)
    );
    print_peers(&peers_from_json(&value));
    Ok(())
}

async fn status(client: &Client) -> Result<()> {
    let local = client.local_device().await?;
    let info = client.inspect_clipboard().await?;
    print_status(
        &serde_json::json!({
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
        })
        .to_string(),
    )
}

fn print_status(reply: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(reply.trim()).context("解析状态失败")?;
    println!(
        "本机: {} (Uin {})",
        value["local"]["name"].as_str().unwrap_or("?"),
        value["local"]["uin"].as_u64().unwrap_or(0)
    );
    if !value["paired"].as_bool().unwrap_or(false) {
        println!("尚未配对，运行 `wxclipd pair CODE` 或 `wxclipd code`。");
        return Ok(());
    }
    let clipboard = &value["clipboard"];
    println!(
        "服务端剪贴板: 消息={} 版本={} 类型={} 来源Uin={} 已消费={} 过期={}",
        clipboard["has_message"].as_bool().unwrap_or(false),
        clipboard["version"].as_u64().unwrap_or(0),
        clipboard["kind"].as_u64().unwrap_or(0),
        clipboard["from_uin"].as_u64().unwrap_or(0),
        clipboard["already_consumed"].as_bool().unwrap_or(false),
        clipboard["expired"].as_bool().unwrap_or(false)
    );
    Ok(())
}

fn print_paired(reply: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(reply.trim()).context("解析配对结果失败")?;
    let peer = &value["peer"];
    let uin = peer["uin"].as_u64().unwrap_or(0);
    println!(
        "已配对: {}{}",
        peer["name"].as_str().unwrap_or("?"),
        if uin > 0 {
            format!(" (Uin {uin})")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// 向正在运行的 wxclipd 发一条控制命令；服务没在运行时返回 `None`。
/// 这些命令都不打开身份文件，避免和常驻进程抢锁。
async fn control_call(state: &Path, command: &str) -> Result<Option<String>> {
    let Some(reply) = ipc::call(state, command).await? else {
        return Ok(None);
    };
    if reply.trim_start().starts_with("err") {
        bail!("{}", reply.trim());
    }
    Ok(Some(reply))
}

async fn sync_now(state: &Path) -> Result<()> {
    match control_call(state, "sync").await? {
        Some(_) => {
            println!("已触发同步");
            Ok(())
        }
        None => bail!(
            "连接 {} 失败，wxclipd 是否在运行？",
            ipc::endpoint(state)?.display()
        ),
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

fn resolve_name(state: &Path, requested: Option<String>) -> Result<String> {
    if let Some(name) = requested {
        if name.trim().is_empty() {
            bail!("--name 不能为空");
        }
        return Ok(name);
    }
    if let Some(stored) = stored_name(state)? {
        return Ok(stored);
    }
    Ok(format!(
        "{}-windows",
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "wxclipd".into())
    ))
}

fn stored_name(state: &Path) -> Result<Option<String>> {
    let identity = state.join("identity.json");
    if !identity.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&identity)
        .with_context(|| format!("读取 {} 失败", identity.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("解析 {} 失败", identity.display()))?;
    Ok(value
        .get("device_name")
        .and_then(|name| name.as_str())
        .map(str::to_string))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("WXCLIPD_STATE") {
        return Ok(PathBuf::from(value));
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .context("LOCALAPPDATA 未设置，请用 --state 指定状态目录")?;
    Ok(PathBuf::from(local).join("wxclipd"))
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args> {
    parse_with_proxy_env(args, std::env::var("WXCLIPD_PROXY").ok())
}

// Inject the one relevant environment setting so parser regressions can be
// tested without mutating the process environment shared by parallel tests.
fn parse_with_proxy_env(
    args: impl Iterator<Item = String>,
    proxy_environment: Option<String>,
) -> Result<Args> {
    let tokens: Vec<String> = args.collect();
    let mut state = None;
    let mut name = None;
    let mut proxy_override: Option<Option<String>> = None;
    let mut yes = false;
    let mut peer = None;
    let mut interval = Duration::from_millis(400);
    let mut poll_seconds = None;
    let mut paste_trigger_override = None;
    let mut paste_debounce = Duration::from_millis(800);
    let mut send_current = false;
    let mut command: Option<&str> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        let mut value_for = |flag: &str| -> Result<String> {
            index += 1;
            tokens
                .get(index)
                .cloned()
                .with_context(|| format!("{flag} 需要一个值"))
        };
        match token {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!(
                    "wxclipd {} ({}-{})",
                    env!("CARGO_PKG_VERSION"),
                    std::env::consts::OS,
                    std::env::consts::ARCH
                );
                std::process::exit(0);
            }
            "--state" => state = Some(PathBuf::from(value_for("--state")?)),
            "--name" => name = Some(value_for("--name")?),
            "--proxy" => {
                if proxy_override.is_some() {
                    bail!("--proxy / --no-proxy 只能指定一次");
                }
                let url = value_for("--proxy")?;
                ProxyConfig::parse(&url)?;
                proxy_override = Some(Some(url));
            }
            "--no-proxy" => {
                if proxy_override.is_some() {
                    bail!("--proxy / --no-proxy 只能指定一次");
                }
                proxy_override = Some(None);
            }
            "--interval" => {
                let value = value_for("--interval")?;
                interval = Duration::from_millis(
                    value
                        .parse()
                        .with_context(|| format!("--interval 需要毫秒数，得到 {value}"))?,
                );
            }
            "--poll-interval" => {
                let value = value_for("--poll-interval")?;
                poll_seconds = Some(value.parse::<u64>().context("--poll-interval 需要秒数")?);
            }
            "--no-paste-trigger" => paste_trigger_override = Some(false),
            "--paste-trigger" => paste_trigger_override = Some(true),
            "--paste-trigger-debounce-ms" => {
                let value = value_for("--paste-trigger-debounce-ms")?;
                paste_debounce = Duration::from_millis(value.parse().with_context(|| {
                    format!("--paste-trigger-debounce-ms 需要毫秒数，得到 {value}")
                })?);
            }
            "--yes" => yes = true,
            "--peer" => {
                let value = value_for("--peer")?;
                peer = Some(
                    value
                        .parse()
                        .with_context(|| format!("--peer 需要数字 Uin，得到 {value}"))?,
                );
            }
            "--send-current" => send_current = true,
            "setup" | "config" | "start" | "stop" | "code" | "pair" | "devices" | "unbind"
            | "status" | "inspect" | "sync-now" | "run" => {
                if command.is_some() {
                    bail!("只能指定一个命令");
                }
                command = Some(token);
            }
            other if other.starts_with('-') => bail!("未知参数 {other}，用 --help 查看用法"),
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    let state = match state {
        Some(state) => state,
        None => default_state_dir()?,
    };
    if state.as_os_str().is_empty() {
        bail!("状态目录不能为空");
    }
    let explicit_settings =
        proxy_override.is_some() || poll_seconds.is_some() || paste_trigger_override.is_some();
    // These commands only address the already-running process. An edited or
    // damaged configuration must never prevent stopping it (including uninstall).
    if matches!(command, Some("stop" | "sync-now" | "inspect")) {
        if !positional.is_empty() {
            bail!("多余的参数: {}", positional.join(" "));
        }
        return Ok(Args {
            state,
            name,
            settings: config::Config::default(),
            explicit_settings,
            proxy_source: "direct",
            command: match command {
                Some("stop") => Command::Stop,
                Some("inspect") => Command::Inspect,
                _ => Command::SyncNow,
            },
        });
    }
    let setup = command == Some("setup") || command.is_none();
    let proxy_source = if proxy_override.is_some() {
        "command"
    } else if proxy_environment.is_some() {
        "environment"
    } else if config::path(&state).exists() {
        "config"
    } else {
        "direct"
    };
    let effective_proxy = proxy_override.or_else(|| {
        proxy_environment.map(|proxy| {
            if proxy.trim().is_empty() {
                None
            } else {
                Some(proxy)
            }
        })
    });
    let loaded = match effective_proxy.as_ref() {
        Some(proxy) => config::Config::load_with_proxy_override(&state, Some(proxy)),
        None => config::Config::load(&state),
    };
    let mut settings = match loaded {
        Ok(settings) => settings,
        Err(error) if setup => {
            // Only the interactive setup may recover with defaults. It will
            // ask the user to choose the proxy before saving or making requests.
            eprintln!("现有配置无法使用：{error:#}。将以默认值进入交互设置，请重新确认代理和轮询设置；当前配置文件尚未更改。");
            let mut defaults = config::Config::default();
            if let Some(proxy) = &effective_proxy {
                if proxy.as_deref().map(ProxyConfig::parse).transpose().is_ok() {
                    defaults.proxy = proxy.clone();
                }
            }
            defaults
        }
        Err(error) => return Err(error),
    };
    if let Some(seconds) = poll_seconds {
        settings.poll_interval_seconds = seconds;
    }
    if let Some(enabled) = paste_trigger_override {
        settings.paste_trigger = enabled;
    }
    if !(1..=3600).contains(&settings.poll_interval_seconds) {
        bail!("--poll-interval 必须是 1..3600 秒");
    }
    if !(50..=60_000).contains(&interval.as_millis()) {
        bail!("--interval 必须是 50..60000 毫秒");
    }
    if !(1..=60_000).contains(&paste_debounce.as_millis()) {
        bail!("粘贴触发防抖必须是 1..60000 毫秒");
    }
    let command = match command {
        Some("setup") => Command::Setup,
        Some("config") => Command::Config,
        Some("start") => Command::Start,
        Some("stop") => Command::Stop,
        Some("code") => Command::Code { yes },
        Some("pair") => Command::Pair {
            code: positional
                .first()
                .cloned()
                .context("pair 需要六位配对码，例如 `wxclipd pair 123456`")?,
        },
        Some("devices") => Command::Devices,
        Some("unbind") => Command::Unbind { peer, yes },
        Some("status") => Command::Status,
        Some("inspect") => Command::Inspect,
        Some("sync-now") => Command::SyncNow,
        Some("run") => Command::Run {
            interval,
            send_current,
            poll_interval: Duration::from_secs(settings.poll_interval_seconds),
            paste_trigger: settings.paste_trigger,
            paste_debounce,
        },
        Some(other) => bail!("未知命令 {other}"),
        None => Command::Setup,
    };
    if !positional.is_empty() && !matches!(command, Command::Pair { .. }) {
        bail!("多余的参数: {}", positional.join(" "));
    }
    if matches!(command, Command::Pair { .. }) && positional.len() != 1 {
        bail!("pair 只接受一个六位配对码");
    }
    Ok(Args {
        state,
        name,
        settings,
        explicit_settings,
        proxy_source,
        command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_in(state: &Path, arguments: &[&str]) -> Result<Args> {
        parse_in_with_env(state, arguments, None)
    }

    fn parse_in_with_env(
        state: &Path,
        arguments: &[&str],
        proxy_environment: Option<&str>,
    ) -> Result<Args> {
        let mut args = vec!["--state".into(), state.to_string_lossy().into_owned()];
        args.extend(arguments.iter().map(|s| s.to_string()));
        parse_with_proxy_env(args.into_iter(), proxy_environment.map(str::to_owned))
    }

    #[test]
    fn local_control_survives_broken_config_and_invalid_proxy_environment() {
        let directory = tempfile::tempdir().unwrap();
        let path = config::path(directory.path());
        std::fs::write(&path, b"{broken JSON").unwrap();
        for command in ["stop", "sync-now", "inspect"] {
            let args = parse_in_with_env(directory.path(), &[command], Some("file:///bad-proxy"))
                .expect("local IPC must not depend on network settings");
            assert!(matches!(
                args.command,
                Command::Stop | Command::SyncNow | Command::Inspect
            ));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken JSON");
    }

    #[test]
    fn explicit_proxy_can_repair_saved_proxy_without_resetting_other_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = config::path(directory.path());
        let broken =
            br#"{"proxy":"file:///bad-proxy","poll_interval_seconds":15,"paste_trigger":false}"#;
        std::fs::write(&path, broken).unwrap();
        assert!(parse_in(directory.path(), &["run"]).is_err());
        let args = parse_in(directory.path(), &["--no-proxy", "config"]).unwrap();
        assert!(args.settings.proxy.is_none());
        assert_eq!(args.settings.poll_interval_seconds, 15);
        assert!(!args.settings.paste_trigger);
        args.settings.save(directory.path()).unwrap();
        assert!(config::Config::load(directory.path())
            .unwrap()
            .proxy
            .is_none());

        std::fs::write(&path, broken).unwrap();
        let args = parse_in(
            directory.path(),
            &["--proxy", "socks5h://localhost:1080", "config"],
        )
        .unwrap();
        assert_eq!(
            args.settings.proxy.as_deref(),
            Some("socks5h://localhost:1080")
        );
        assert_eq!(args.settings.poll_interval_seconds, 15);
        assert!(!args.settings.paste_trigger);
    }

    #[test]
    fn only_interactive_setup_recovers_malformed_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = config::path(directory.path());
        std::fs::write(&path, b"{broken JSON").unwrap();
        let args = parse_in(directory.path(), &["setup"]).unwrap();
        assert!(matches!(args.command, Command::Setup));
        assert!(args.settings.proxy.is_none());
        for command in ["run", "start", "config", "devices"] {
            assert!(
                parse_in(directory.path(), &[command]).is_err(),
                "{command} must fail closed"
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken JSON");
    }

    #[test]
    fn invalid_environment_proxy_needs_setup_or_an_explicit_override() {
        let directory = tempfile::tempdir().unwrap();
        let invalid = Some("file:///bad-proxy");
        assert!(parse_in_with_env(directory.path(), &["run"], invalid).is_err());
        let setup = parse_in_with_env(directory.path(), &["setup"], invalid).unwrap();
        assert!(matches!(setup.command, Command::Setup));
        let direct = parse_in_with_env(directory.path(), &["--no-proxy", "run"], invalid).unwrap();
        assert!(direct.settings.proxy.is_none());
    }

    #[test]
    fn command_line_then_environment_then_file_proxy_precedence_is_exact() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            config::path(directory.path()),
            br#"{"proxy":"http://saved.example:8080"}"#,
        )
        .unwrap();
        let args = parse_in_with_env(
            directory.path(),
            &["--proxy", "socks5h://chosen.example:1080", "run"],
            Some("http://environment.example:8080"),
        )
        .unwrap();
        assert_eq!(
            args.settings.proxy.as_deref(),
            Some("socks5h://chosen.example:1080")
        );
        assert_eq!(args.proxy_source, "command");
        let args = parse_in_with_env(
            directory.path(),
            &["run"],
            Some("http://environment.example:8080"),
        )
        .unwrap();
        assert_eq!(
            args.settings.proxy.as_deref(),
            Some("http://environment.example:8080")
        );
        assert_eq!(args.proxy_source, "environment");
        let args = parse_in_with_env(directory.path(), &["run"], Some("")).unwrap();
        assert!(
            args.settings.proxy.is_none(),
            "empty inherited override explicitly selects direct mode"
        );
        let args = parse_in(directory.path(), &["run"]).unwrap();
        assert_eq!(
            args.settings.proxy.as_deref(),
            Some("http://saved.example:8080")
        );
        assert_eq!(args.proxy_source, "config");
    }

    #[test]
    fn proxy_override_and_saved_settings_are_consistent() {
        let directory = tempfile::tempdir().unwrap();
        let args = parse_in(
            directory.path(),
            &[
                "--proxy",
                "socks5h://127.0.0.1:1080",
                "--poll-interval",
                "15",
                "config",
            ],
        )
        .unwrap();
        args.settings.save(directory.path()).unwrap();
        let saved = config::Config::load(directory.path()).unwrap();
        assert_eq!(saved.proxy.as_deref(), Some("socks5h://127.0.0.1:1080"));
        assert_eq!(saved.poll_interval_seconds, 15);
        let args = parse_in(directory.path(), &["--no-proxy", "run"]).unwrap();
        assert!(args.settings.proxy.is_none());
        assert!(args.explicit_settings);
        assert!(config::Config::load(directory.path())
            .unwrap()
            .proxy
            .is_some());
    }

    #[test]
    fn invalid_cli_settings_do_not_reach_the_network() {
        let directory = tempfile::tempdir().unwrap();
        for args in [
            vec!["--interval", "0", "run"],
            vec!["--poll-interval", "0", "run"],
            vec!["--proxy", "http://localhost:7890", "--no-proxy", "run"],
            vec!["--proxy", "file:///tmp/proxy", "run"],
            vec!["pair", "123456", "654321"],
        ] {
            assert!(parse_in(directory.path(), &args).is_err());
        }
    }

    #[tokio::test]
    async fn inspect_and_stop_work_while_proxy_is_unavailable_and_config_changes() {
        let directory = tempfile::tempdir().unwrap();
        ipc::prepare_state_dir(directory.path()).unwrap();
        let endpoint = ipc::endpoint(directory.path()).unwrap();
        let mut listener = ipc::listen(&endpoint).unwrap();
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = unused.local_addr().unwrap();
        drop(unused);
        let proxy_url = format!("http://private-user:private-password@{address}");
        let mut options = ClientOptions::new(directory.path(), "offline-test");
        options.proxy = Some(ProxyConfig::parse(&proxy_url).unwrap());
        let settings = config::Config {
            proxy: Some(proxy_url),
            poll_interval_seconds: 17,
            paste_trigger: false,
            send_enabled: false,
            receive_enabled: true,
        };
        let runtime_status = runtime_status::RuntimeStatus::new(
            &settings,
            "config",
            "offline-test".into(),
            Duration::from_millis(700),
        )
        .unwrap();
        let task =
            tokio::spawn(async move { open_for_run(options, &mut listener, runtime_status).await });
        assert_eq!(
            ipc::call(directory.path(), "ping").await.unwrap(),
            Some(format!("ok {}", std::process::id()))
        );
        // Inspect is independent of both the pending connection attempt and
        // later edits to the saved configuration. It reports the running one.
        std::fs::write(config::path(directory.path()), b"{broken JSON").unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let reply = tokio::time::timeout(
                Duration::from_secs(2),
                ipc::call(directory.path(), "inspect"),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert!(!reply.contains("private-user"));
            assert!(!reply.contains("private-password"));
            let value: serde_json::Value =
                serde_json::from_str(reply.strip_prefix("ok ").unwrap()).unwrap();
            assert_eq!(value["poll_interval_seconds"], 17);
            assert_eq!(value["paste_trigger"], false);
            assert_eq!(value["send_enabled"], false);
            assert_eq!(value["receive_enabled"], true);
            assert_eq!(value["local_poll_interval_ms"], 700);
            assert!(value["last_success_at"].is_null());
            if value["phase"] == "retrying" {
                assert!(value["last_error"].is_string());
                break;
            }
            assert_eq!(value["phase"], "initializing");
            assert!(
                tokio::time::Instant::now() < deadline,
                "offline initialization must enter retry status"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert_eq!(
            ipc::call(directory.path(), "stop")
                .await
                .unwrap()
                .as_deref(),
            Some("ok")
        );
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(result.is_none());
    }
}
