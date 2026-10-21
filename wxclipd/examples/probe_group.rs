//! 诊断工具：对比 `GroupRequest.clipboard_version` 传 0 与传"本地已有版本"时，
//! 服务端返回的响应大小差异，用来确认该字段是否真的能省流量。
//!
//! 用法（在仓库根目录的 Windows 开发者 PowerShell 中运行；先停止同步，
//! 身份目录同一时间只能被一个进程占用）：
//!   cargo run --manifest-path wxclipd/Cargo.toml --features vendored-openssl --target i686-pc-windows-msvc --example probe_group -- "$env:LOCALAPPDATA\wxclipd" "测试文本"
//!
//! 给了测试文本时，会先把它发到设备组（手机上会看到这段剪贴板内容），再各发一次
//! `clipboard_version=0` 与 `clipboard_version=当前` 的请求，对比响应大小。
//!
//! 注意：为了让响应落盘，会开启 SDK 的 capture，明文会写到状态目录下的
//! `capture-*` 子目录，测完请自行删除。

use anyhow::{Context, Result};
use std::path::PathBuf;
use wx_ime_sdk::{Client, ClientOptions};

#[tokio::main]
async fn main() -> Result<()> {
    let state = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("用法: probe_group <状态目录>")?,
    );
    let raw = std::fs::read_to_string(state.join("identity.json"))
        .with_context(|| format!("读取 {} 失败", state.display()))?;
    let identity: serde_json::Value = serde_json::from_str(&raw)?;
    let device_name = identity
        .get("device_name")
        .and_then(|v| v.as_str())
        .context("identity.json 缺少 device_name")?
        .to_string();
    let mut known = identity
        .get("consumed")
        .and_then(|v| v.as_object())
        .map(|map| map.values().filter_map(|v| v.as_u64()).max().unwrap_or(0))
        .unwrap_or(0);
    if let Some(version) = identity
        .get("pending")
        .and_then(|pending| pending.get("version"))
        .and_then(|version| version.as_u64())
    {
        known = known.max(version);
    }

    let capture = state.join(format!(
        "capture-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs()
    ));
    let mut options = ClientOptions::new(&state, device_name);
    options.capture_dir = Some(capture.clone());
    let client = Client::open(options).await?;

    if let Some(text) = std::env::args().nth(2) {
        let receipt = client.send_text(&text).await?;
        known = receipt.version;
        println!("已发送测试文本 {} 字节，version={known}", text.len());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }

    client.group_with_clipboard_version(0).await?;
    client.group_with_clipboard_version(known).await?;

    let mut plains: Vec<PathBuf> = std::fs::read_dir(&capture)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(".plain.bin"))
        .collect();
    plains.sort();
    println!("本地已知剪贴板版本: {known}");
    println!("capture 目录: {}", capture.display());
    let labelled = [
        "clipboard_version=0（期望包含剪贴板）",
        "clipboard_version=当前（期望省略）",
    ];
    let mut last_two: Vec<&PathBuf> = plains.iter().rev().take(2).collect();
    last_two.reverse();
    for (index, path) in last_two.iter().enumerate() {
        let size = std::fs::metadata(path)?.len();
        println!(
            "{}: {} 字节 ({})",
            labelled[index],
            size,
            path.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    client.shutdown().await?;
    Ok(())
}
