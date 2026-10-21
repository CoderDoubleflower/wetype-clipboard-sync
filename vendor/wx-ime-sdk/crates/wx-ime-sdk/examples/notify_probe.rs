//! 推送长连接探针：连接 → 握手 → 订阅 → 打印收到的事件。
//!
//! 用法：cargo run -p wx-ime-sdk --example notify_probe -- <状态目录> <设备名> [秒数]
use std::time::Duration;
use wx_ime_sdk::{Client, ClientOptions, NotifyEvent};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let state = args
        .next()
        .expect("用法: notify_probe <状态目录> <设备名> [秒数]");
    let name = args
        .next()
        .expect("用法: notify_probe <状态目录> <设备名> [秒数]");
    let seconds: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(120);
    let client = Client::open(ClientOptions::new(&state, name)).await?;
    let local = client.local_device().await?;
    println!("本机: {} (Uin {})", local.name, local.uin);
    let mut channel = client.open_notify_channel().await?;
    println!("推送通道就绪，等待 {seconds} 秒…");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    while tokio::time::Instant::now() < deadline {
        match channel.next_event().await? {
            NotifyEvent::Push {
                event_type,
                payload,
            } => {
                println!(
                    "事件 event_type={event_type} payload={}B {}",
                    payload.len(),
                    payload
                        .iter()
                        .take(24)
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
            }
            NotifyEvent::Idle => {}
            NotifyEvent::Closed => {
                println!("连接已关闭");
                break;
            }
        }
    }
    client.shutdown().await?;
    Ok(())
}
