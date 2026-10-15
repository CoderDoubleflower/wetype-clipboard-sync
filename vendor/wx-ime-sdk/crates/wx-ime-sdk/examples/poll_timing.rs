//! 实测一次远端拉取的往返耗时。
//! 用法：cargo run -p wx-ime-sdk --example poll_timing -- <状态目录> <设备名> [次数] [间隔毫秒]
use std::time::{Duration, Instant};
use wx_ime_sdk::{Client, ClientOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let state = args.next().expect("state dir");
    let name = args.next().expect("device name");
    let count: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(15);
    let gap_ms: u64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1000);

    let open_start = Instant::now();
    let client = Client::open(ClientOptions::new(&state, name)).await?;
    println!("建连+握手（timestamp+ECDH）: {} ms", open_start.elapsed().as_millis());

    let mut samples = Vec::new();
    for n in 0..count {
        let t = Instant::now();
        let result = client.poll_clipboard_once().await;
        let elapsed = t.elapsed();
        samples.push(elapsed);
        match result {
            Ok(Some(received)) => println!(
                "{n:2}: {:7.1} ms  收到 {} 字节内容",
                elapsed.as_secs_f64() * 1000.0,
                format!("{:?}", received.content).len()
            ),
            Ok(None) => println!("{n:2}: {:7.1} ms  无新内容", elapsed.as_secs_f64() * 1000.0),
            Err(error) => println!("{n:2}: {:7.1} ms  失败: {error:#}", elapsed.as_secs_f64() * 1000.0),
        }
        tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    }

    let mut sorted: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    println!(
        "\n{} 次拉取：最小 {:.1} ms / 中位 {:.1} ms / P95 {:.1} ms / 最大 {:.1} ms",
        sorted.len(),
        sorted[0],
        pick(0.5),
        pick(0.95),
        sorted[sorted.len() - 1]
    );
    client.shutdown().await?;
    Ok(())
}
