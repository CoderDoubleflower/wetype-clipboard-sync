//! `wxclipd`: Linux clipboard sync for the WeType (微信输入法) cross-device
//! clipboard group.

mod clipboard;
mod sync;

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use wx_ime_sdk::{Client, ClientOptions, PairingCode, PairingUpdate};

const HELP: &str = "\
wxclipd - Linux 端微信输入法跨设备剪贴板

用法:
  wxclipd [--state DIR] [--name NAME] <命令>

命令:
  code              生成六位配对码，等待另一台设备输入并确认
  pair <CODE>       输入官方输入法显示的六位配对码
  devices           列出本机与已绑定设备
  status            显示剪贴板同步状态
  run               启动剪贴板双向同步（前台运行，Ctrl-C 退出）

选项:
  --state DIR       身份与状态目录，默认 $WXCLIPD_STATE 或 ~/.local/state/wxclipd
  --name NAME       设备名，仅首次创建身份时使用
  --yes             配对时自动确认对方，不询问
  --interval MS     本地剪贴板轮询间隔，默认 400
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
    Status,
    Run {
        interval: Duration,
        send_current: bool,
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
    let name = resolve_name(&args.state, args.name)?;
    let client = Client::open(ClientOptions::new(&args.state, name)).await?;
    let result = match args.command {
        Command::Code { yes } => code(&client, yes).await,
        Command::Pair { code } => pair(&client, code).await,
        Command::Devices => devices(&client).await,
        Command::Status => status(&client).await,
        Command::Run {
            interval,
            send_current,
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
                    send_current,
                },
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
    let devices = client.devices().await?;
    if devices.is_empty() {
        println!("尚未配对，运行 `wxclipd pair CODE` 或 `wxclipd code`。");
        return Ok(());
    }
    for device in devices.iter().filter(|device| device.uin != local.uin) {
        println!(
            "已绑定: {} (Uin {}, 平台 {}{})",
            device.name,
            device.uin,
            device.platform,
            if device.supports_file_transfer {
                ", 支持文件传输"
            } else {
                ""
            }
        );
    }
    Ok(())
}

async fn status(client: &Client) -> Result<()> {
    let local = client.local_device().await?;
    println!("本机: {} (Uin {})", local.name, local.uin);
    let info = client.inspect_clipboard().await?;
    println!(
        "服务端剪贴板: 消息={} 版本={} 类型={} 来源Uin={} 已消费={} 过期={}",
        info.has_message,
        info.version,
        info.kind,
        info.from_uin,
        info.already_consumed,
        info.expired
    );
    Ok(())
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
    let mut interval = Duration::from_millis(400);
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
            "--yes" => yes = true,
            "--send-current" => send_current = true,
            "code" | "pair" | "devices" | "status" | "run" => {
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
        Some("status") => Command::Status,
        Some("run") => Command::Run {
            interval,
            send_current,
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
