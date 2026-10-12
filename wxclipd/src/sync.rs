//! Bidirectional text clipboard loop.

use crate::clipboard::{self, Backend};
use anyhow::Result;
use std::time::{Duration, Instant};
use tokio::time::{interval, MissedTickBehavior};
use wx_ime_sdk::{Client, ClipboardContent};

pub struct SyncOptions {
    pub local_interval: Duration,
    pub send_current: bool,
}

pub async fn run(client: Client, backend: Backend, options: SyncOptions) -> Result<()> {
    let mut paired = is_paired(&client).await;
    let mut paired_checked_at = Instant::now();
    if !paired {
        println!("[wxclipd] 尚未配对，先运行 `wxclipd pair CODE` 或 `wxclipd code`；本进程只接收不发送。");
    }

    let mut last_known = clipboard::read_text(backend).await?;
    if options.send_current {
        if let Some(text) = last_known.clone() {
            send(&client, &text).await;
        }
    }

    let mut local_tick = interval(options.local_interval);
    local_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut remote_tick = interval(Duration::from_secs(2));
    remote_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
            _ = remote_tick.tick() => {
                if !paired && paired_checked_at.elapsed() >= Duration::from_secs(30) {
                    paired_checked_at = Instant::now();
                    paired = is_paired(&client).await;
                    if paired {
                        println!("[wxclipd] 检测到已配对，开始同步本机剪贴板。");
                    }
                }
                match client.poll_clipboard_once().await {
                    Ok(Some(received)) => {
                        failures = 0;
                        match received.content {
                            ClipboardContent::Text(text) => {
                                if let Err(error) = clipboard::write_text(backend, &text).await {
                                    eprintln!("[wxclipd] 写入剪贴板失败: {error:#}");
                                } else {
                                    last_known = Some(text.clone());
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
                    }
                    Ok(None) => {
                        failures = 0;
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("[wxclipd] 拉取远端剪贴板失败（第 {failures} 次）: {error:#}");
                        if failures % 5 == 0 {
                            eprintln!("[wxclipd] 重新建立会话");
                            if let Err(error) = client.reconnect().await {
                                eprintln!("[wxclipd] 会话重建失败: {error:#}");
                            }
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("[wxclipd] 退出");
                break;
            }
        }
    }
    Ok(())
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
