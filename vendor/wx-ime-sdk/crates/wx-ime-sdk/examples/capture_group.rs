//! 抓一次 HTTP 组请求（cmd 4814/34），用于与手写实现逐字节对比。
use wx_ime_sdk::{Client, ClientOptions};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = std::env::args().nth(1).expect("state dir");
    let name = std::env::args().nth(2).expect("device name");
    let capture = std::env::args().nth(3).expect("capture dir");
    let mut options = ClientOptions::new(&state, name);
    options.capture_dir = Some(capture.into());
    let client = Client::open(options).await?;
    println!("devices: {:?}", client.devices().await?.len());
    client.shutdown().await?;
    Ok(())
}
