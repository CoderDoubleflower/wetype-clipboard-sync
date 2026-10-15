//! `wxclipd`: Linux clipboard sync for the WeType (微信输入法) cross-device
//! clipboard group.

mod clipboard;
mod sync;

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wx_ime_sdk::{Client, ClientOptions, PairingCode, PairingUpdate};

const HELP: &str = "\
wxclipd - Linux 端微信输入法跨设备剪贴板

用法:
  wxclipd [--state DIR] [--name NAME] <命令>

命令:
  code              生成六位配对码，等待另一台设备输入并确认
  pair <CODE>       输入官方输入法显示的六位配对码
  devices           列出本机与已绑定设备
  unbind [--peer UIN]  解绑一台已关联设备
  status            显示剪贴板同步状态
  run               启动剪贴板双向同步（前台运行，Ctrl-C 退出）
  sync-now          通知正在运行的 wxclipd 立即同步一次

选项:
  --state DIR       身份与状态目录，默认 $WXCLIPD_STATE 或 ~/.local/state/wxclipd
  --name NAME       设备名，仅首次创建身份时使用
  --peer UIN        unbind 要解绑的设备 Uin（只有一台时可省略）
  --yes             配对时自动确认对方，不询问
  --interval MS     本地剪贴板轮询间隔，默认 400
  --poll-interval S 离开/锁屏时的远端轮询秒数，默认 60
  --active-poll-interval S  人在电脑前时的远端轮询秒数，默认 5
  --send-current    启动时把当前剪贴板内容发送一次
  -h, --help        显示帮助
";

struct Args {
    state: PathBuf,
    name: Option<String>,
    command: Command,
}

enum Command {
    Code { yes: bool },
    Pair { code: String },
    Devices,
    Unbind { peer: Option<u64>, yes: bool },
    Status,
    SyncNow,
    Run {
        interval: Duration,
        send_current: bool,
        poll_interval: Duration,
        active_poll_interval: Duration,
    },
}

fn main() -> Result<()> {
    let args = parse(std::env::args().skip(1))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(dispatch(args))
}

async fn dispatch(args: Args) -> Result<()> {
    if matches!(args.command, Command::SyncNow) {
        return sync_now(&args.state).await;
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
        if sync::control_socket_path(&args.state).exists() {
            println!("已提交配对码，等待另一台设备确认。");
            if let Some(reply) = control_call(&args.state, &format!("pair {code}")).await? {
                return print_paired(&reply);
            }
        }
    }
    let name = resolve_name(&args.state, args.name)?;
    let client = match Client::open(ClientOptions::new(&args.state, name)).await {
        Ok(client) => client,
        Err(error) if error.to_string().contains("already in use") => {
            bail!(
                "wxclipd 服务正在运行并持有身份：{error}\n\
                 提示：先 `systemctl --user stop wxclipd`，执行完再 `systemctl --user start wxclipd`。\n\
                 `devices` / `unbind` / `sync-now` / `status` / `pair` 支持在服务运行时直接使用。"
            );
        }
        Err(error) => return Err(error.into()),
    };
    let result = match args.command {
        Command::Code { yes } => code(&client, yes).await,
        Command::Pair { code } => pair(&client, code).await,
        Command::Devices => devices(&client).await,
        Command::Unbind { peer, yes } => unbind_local(&client, peer, yes).await,
        Command::Status => status(&client).await,
        Command::SyncNow => unreachable!("sync-now 不打开身份"),
        Command::Run {
            interval,
            send_current,
            poll_interval,
            active_poll_interval,
        } => {
            let backend = clipboard::Backend::detect()?;
            println!(
                "[wxclipd] 剪贴板后端: {}，设备: {}",
                backend.describe(),
                client.local_device().await?.name
            );
            sync::run(
                client.clone(),
                backend,
                sync::SyncOptions {
                    local_interval: interval,
                    poll_interval,
                    active_poll_interval,
                    send_current,
                },
                sync::control_socket_path(&args.state),
            )
            .await
        }
    };
    let shutdown = client.shutdown().await;
    result?;
    shutdown?;
    Ok(())
}

async fn code(client: &Client, yes: bool) -> Result<()> {
    let mut session = client.request_pairing_code().await?;
    let code = session.code().context("没有拿到配对码")?.expose().to_string();
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
                        peer.uin.map(|uin| format!(" (Uin {uin})")).unwrap_or_default()
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
                    peer.uin.map(|uin| format!(" (Uin {uin})")).unwrap_or_default()
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
                    peer.uin.map(|uin| format!(" (Uin {uin})")).unwrap_or_default(),
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
                supports_file_transfer: device["supports_file_transfer"]
                    .as_bool()
                    .unwrap_or(false),
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
    let value: serde_json::Value = serde_json::from_str(reply.trim()).context("解析配对结果失败")?;
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
    let path = sync::control_socket_path(state);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&path).await else {
        return Ok(None);
    };
    stream.write_all(command.as_bytes()).await?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await?;
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
            sync::control_socket_path(state).display()
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
    if let Ok(name) = std::fs::read_to_string("/etc/hostname") {
        let name = name.trim();
        if !name.is_empty() {
            return Ok(format!("{name}-linux"));
        }
    }
    Ok("wxclipd-linux".to_string())
}

fn stored_name(state: &Path) -> Result<Option<String>> {
    let identity = state.join("identity.json");
    if !identity.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&identity)
        .with_context(|| format!("读取 {} 失败", identity.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("解析 {} 失败", identity.display()))?;
    Ok(value
        .get("device_name")
        .and_then(|name| name.as_str())
        .map(str::to_string))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("WXCLIPD_STATE") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(value).join("wxclipd"));
    }
    let home = std::env::var_os("HOME").context("HOME 未设置，请用 --state 指定状态目录")?;
    Ok(PathBuf::from(home).join(".local/state/wxclipd"))
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args> {
    let tokens: Vec<String> = args.collect();
    let mut state = None;
    let mut name = None;
    let mut yes = false;
    let mut peer = None;
    let mut interval = Duration::from_millis(400);
    let mut poll_interval = Duration::from_secs(60);
    let mut active_poll_interval = Duration::from_secs(5);
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
            "--state" => state = Some(PathBuf::from(value_for("--state")?)),
            "--name" => name = Some(value_for("--name")?),
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
                poll_interval = Duration::from_secs(
                    value
                        .parse()
                        .with_context(|| format!("--poll-interval 需要秒数，得到 {value}"))?,
                );
            }
            "--active-poll-interval" => {
                let value = value_for("--active-poll-interval")?;
                active_poll_interval = Duration::from_secs(
                    value
                        .parse()
                        .with_context(|| format!("--active-poll-interval 需要秒数，得到 {value}"))?,
                );
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
            "code" | "pair" | "devices" | "unbind" | "status" | "sync-now" | "run" => {
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
    let command = match command {
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
        Some("sync-now") => Command::SyncNow,
        Some("run") => Command::Run {
            interval,
            send_current,
            poll_interval,
            active_poll_interval,
        },
        Some(other) => bail!("未知命令 {other}"),
        None => bail!("缺少命令，用 --help 查看用法"),
    };
    if !positional.is_empty() && !matches!(command, Command::Pair { .. }) {
        bail!("多余的参数: {}", positional.join(" "));
    }
    Ok(Args {
        state,
        name,
        command,
    })
}
