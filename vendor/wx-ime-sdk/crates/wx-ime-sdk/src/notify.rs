//! 服务端推送长连接（WebSocket）。
//!
//! 官方实现：`CronetNetworkApi::OpenNotifyChannel` 连 `wss://<host>/`，子协议
//! `wxws_pb`；连上后先用 bootstrap 密钥在连接上重做一次 ECDH（`/oauth_pubkey_v2`），
//! 得到本连接专属的会话密钥，再发一条 cmd 8074 的订阅请求，之后服务端用
//! `websocket.WebSocketPbResponse`（`field3="notify"`）推送事件，正文用连接密钥
//! AES-256-ECB 加密，解密后是 `WebSocketPushInfo{event_type=1, timestamp=2, .., payload=4}`。
//!
//! 已实测：解绑事件（event_type=6）能收到并正确解密。剪贴板事件（4/7）在测试环境没
//! 有推送，因此上层把它当“有新数据”的唤醒信号，同时保留定时轮询兜底。

use crate::{Client, Error, Result, network};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use wx_ime_protocol::crypto::{ControlKey, md5, sha256};
use wx_ime_protocol::packet::{self, RequestContext};
use wx_ime_protocol::transport::{Endpoint, SIGN_SALT, VERSION};
use wx_ime_protocol::{websocket, wire};

/// 官方固定入口与子协议。
pub const NOTIFY_URL: &str = "wss://wetype.weixin.qq.com/";
pub const NOTIFY_SUBPROTOCOL: &str = "wxws_pb";
/// `OpenNotifyChannel` 使用的命令号（libime_net.so: 0x1f8a）。
pub const NOTIFY_CMD_ID: u32 = 0x1f8a;
/// 请求里的路径字段（官方任务参数里的全局常量）。
pub const API_PATH: &str = "/api_v2";
const HOST: &str = "wetype.weixin.qq.com";
/// 服务端约 65 秒空闲就断连，WS 层 ping 不算数；实测重发业务请求可续期，取 45 秒留余量。
const HEARTBEAT: Duration = Duration::from_secs(45);

pub enum NotifyEvent {
    /// 服务端推来一个事件；`event_type` 与原始 payload 都已解密。
    Push { event_type: u32, payload: Vec<u8> },
    /// 空闲心跳，连接仍然健康。
    Idle,
    /// 连接已断开，需要重连。
    Closed,
}

pub struct NotifyChannel {
    client: Client,
    stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    key: ControlKey,
    uin: u64,
    device_info: String,
    task: u32,
}

impl Client {
    /// 打开推送长连接并完成握手 + 订阅。失败时调用方应回退到轮询。
    pub async fn open_notify_channel(&self) -> Result<NotifyChannel> {
        tokio::time::timeout(Duration::from_secs(60), self.open_notify_channel_inner())
            .await
            .map_err(|_| Error::Timeout("notification channel setup"))?
    }

    async fn open_notify_channel_inner(&self) -> Result<NotifyChannel> {
        let (uin, device_info, public_key, previous_server_key, timestamp, task) = {
            let mut session = self.session().await?;
            (
                session.store.state.uin,
                session.device_info(),
                session.identity.public_hex()?,
                session.store.state.server_key.clone(),
                session.timestamp()?,
                session.next_task()?,
            )
        };

        let mut request = NOTIFY_URL
            .into_client_request()
            .map_err(|_| Error::Transport)?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(NOTIFY_SUBPROTOCOL),
        );
        let (mut stream, response) =
            network::connect_websocket(request, self.inner.proxy.as_ref()).await?;
        if response.status().as_u16() != 101 {
            return Err(Error::Transport);
        }

        // 1) 连接上重做 ECDH，拿到本连接专属的会话密钥。
        let bootstrap = ControlKey::bootstrap();
        let ecdh = wire::EcdhRequest {
            public_key: Some(public_key),
            previous_server_key: Some(previous_server_key),
            need_uin: Some(false),
            device_code: Some(self.device_code().await?),
            uin: Some(uin),
            uin_token: Some(String::new()),
            device_token: Some(String::new()),
            device_info: Some(device_info.clone()),
            device_name: Some(self.device_name().await?),
        };
        let (headers, body) = api_packet(
            Endpoint::Ecdh,
            &ecdh.encode_to_vec(),
            &bootstrap,
            uin,
            timestamp,
            task,
            None,
            0,
        )?;
        let frame = ws_frame(&headers, &body, "/oauth_pubkey_v2")?;
        send_frame(&mut stream, WsMessage::Binary(frame.into())).await?;
        let reply = read_frame(&mut stream).await?;
        let ecdh_response = {
            let (headers, body) = split_response(&reply)?;
            let _ = headers;
            let plain = bootstrap.open(&body)?;
            wire::EcdhResponse::decode(plain.as_slice()).map_err(wx_ime_protocol::Error::from)?
        };
        let server_key = ecdh_response
            .public_key
            .filter(|key| !key.is_empty())
            .ok_or(Error::Invalid("notify ECDH server key missing"))?;
        let key = {
            let session = self.session().await?;
            session.identity.agree(&server_key)?
        };

        // 2) 订阅通知事件。
        let timestamp = self.session().await?.timestamp()?;
        let task = self.session().await?.next_task()?;
        let plain = packet::envelope(uin, 0, &device_info, timestamp, Vec::new());
        let (headers, body) = api_packet(
            Endpoint::Api(NOTIFY_CMD_ID),
            &plain,
            &key,
            uin,
            timestamp,
            task,
            Some(NOTIFY_CMD_ID),
            0,
        )?;
        let frame = ws_frame(&headers, &body, API_PATH)?;
        send_frame(&mut stream, WsMessage::Binary(frame.into())).await?;
        let reply = read_frame(&mut stream).await?;
        let (status, _) = split_response(&reply)?;
        if status != 200 {
            return Err(Error::Http(status as u16));
        }

        Ok(NotifyChannel {
            client: self.clone(),
            stream,
            key,
            uin,
            device_info,
            task: task + 1,
        })
    }

    async fn device_name(&self) -> Result<String> {
        Ok(self.session().await?.store.state.device_name.clone())
    }

    async fn device_code(&self) -> Result<String> {
        Ok(self.session().await?.store.state.device_code.clone())
    }
}

impl NotifyChannel {
    /// 等待下一个事件；空闲时发 ping 保持连接。
    pub async fn next_event(&mut self) -> Result<NotifyEvent> {
        match tokio::time::timeout(HEARTBEAT, self.stream.next()).await {
            Err(_) => {
                self.heartbeat().await?;
                Ok(NotifyEvent::Idle)
            }
            Ok(None) | Ok(Some(Err(_))) => Ok(NotifyEvent::Closed),
            Ok(Some(Ok(WsMessage::Binary(data)))) => self.decode(&data),
            Ok(Some(Ok(WsMessage::Text(text)))) => self.decode(text.as_bytes()),
            Ok(Some(Ok(WsMessage::Ping(payload)))) => {
                send_frame(&mut self.stream, WsMessage::Pong(payload)).await?;
                Ok(NotifyEvent::Idle)
            }
            Ok(Some(Ok(_))) => Ok(NotifyEvent::Idle),
        }
    }

    fn decode(&self, raw: &[u8]) -> Result<NotifyEvent> {
        let (_, body) = split_response(raw)?;
        if body.is_empty() {
            return Ok(NotifyEvent::Idle);
        }
        let plain = self.key.open(&body)?;
        if plain.is_empty() {
            // 心跳/订阅请求的应答，没有事件。
            return Ok(NotifyEvent::Idle);
        }
        let push = websocket::WebSocketPushInfo::decode(plain.as_slice())
            .map_err(wx_ime_protocol::Error::from)?;
        if push.field1 == 0 {
            return Ok(NotifyEvent::Idle);
        }
        Ok(NotifyEvent::Push {
            event_type: push.field1,
            payload: push.field4,
        })
    }

    /// 业务心跳：重发一次订阅请求。WS 层 ping 不被服务端认可，实测必须发业务请求。
    async fn heartbeat(&mut self) -> Result<()> {
        let timestamp = self.client.session().await?.timestamp()?;
        let plain = packet::envelope(self.uin, 0, &self.device_info, timestamp, Vec::new());
        let (headers, body) = api_packet(
            Endpoint::Api(NOTIFY_CMD_ID),
            &plain,
            &self.key,
            self.uin,
            timestamp,
            self.task,
            Some(NOTIFY_CMD_ID),
            0,
        )?;
        self.task = self.task.wrapping_add(1);
        let frame = ws_frame(&headers, &body, API_PATH)?;
        send_frame(&mut self.stream, WsMessage::Binary(frame.into())).await
    }
}

/// 从 `WebSocketPbResponse` 里取出 HTTP 状态码与已加密的响应体。
fn split_response(raw: &[u8]) -> Result<(u32, Vec<u8>)> {
    let response =
        websocket::WebSocketPbResponse::decode(raw).map_err(wx_ime_protocol::Error::from)?;
    let status = response
        .http
        .as_ref()
        .map(|http| http.status as u32)
        .unwrap_or(0);
    let body = response.http.map(|http| http.body).unwrap_or_default();
    Ok((status, body))
}

async fn read_frame(stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Result<Vec<u8>> {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), stream.next()).await {
            Ok(Some(Ok(WsMessage::Binary(data)))) => return Ok(data.to_vec()),
            Ok(Some(Ok(WsMessage::Text(text)))) => return Ok(text.as_bytes().to_vec()),
            Ok(Some(Ok(WsMessage::Ping(payload)))) => {
                send_frame(stream, WsMessage::Pong(payload)).await?;
            }
            Ok(Some(Ok(_))) => continue,
            _ => return Err(Error::Transport),
        }
    }
}

async fn send_frame(
    stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    message: WsMessage,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), stream.send(message))
        .await
        .map_err(|_| Error::Timeout("notification channel write"))?
        .map_err(|_| Error::Transport)
}

/// 构造一帧 `WebSocketPbRequest`：外层字段全零，内层 `HttpRequest` 带 Kb-* 头与密文正文。
fn ws_frame(headers: &BTreeMap<String, String>, body: &[u8], path: &str) -> Result<Vec<u8>> {
    let mut http = websocket::HttpRequest {
        method: "POST".to_string(),
        field2: String::new(),
        field3: path.to_string(),
        query: String::new(),
        headers: Vec::new(),
        body: body.to_vec(),
    };
    for (key, value) in headers {
        http.headers.push(websocket::HttpHeader {
            key: key.clone(),
            value: value.clone(),
        });
    }
    http.headers.push(websocket::HttpHeader {
        key: "HOST".to_string(),
        value: HOST.to_string(),
    });
    Ok(websocket::WebSocketPbRequest {
        field1: 0,
        field2: 0,
        field3: 0,
        field4: 0,
        http: Some(http),
    }
    .encode_to_vec())
}

/// 与 `packet::encode` 一致，但允许覆盖 `Kb-CmdId`（通知通道用 8074）。
#[allow(clippy::too_many_arguments)]
fn api_packet(
    endpoint: Endpoint,
    plain: &[u8],
    key: &ControlKey,
    uin: u64,
    timestamp_ms: i64,
    task: u32,
    cmd_override: Option<u32>,
    sub_cmd: u32,
) -> Result<(BTreeMap<String, String>, Vec<u8>)> {
    if cmd_override.is_none() {
        let trace = crate::session::random_hex(8)?;
        let packet = packet::encode(
            endpoint,
            plain,
            Some(key),
            RequestContext {
                timestamp_ms,
                trace_id: &trace,
                task,
                uin,
                registration_token: "",
            },
        )?;
        return Ok((packet.headers, packet.body));
    }

    let trace = crate::session::random_hex(8)?;
    let body = key.seal(plain)?;
    let cmd_id = cmd_override.expect("checked above");
    let mut headers = BTreeMap::new();
    for (name, value) in [
        ("Kb-OsType", "1".to_string()),
        ("Kb-Version", VERSION.to_string()),
        ("Kb-SystemVersion", "34".to_string()),
        ("Kb-PackageType", "2".to_string()),
        ("Kb-TimeStamp", timestamp_ms.to_string()),
        ("Kb-BodyMd5", md5(&body)?.to_uppercase()),
        ("Kb-TraceId", trace.clone()),
        ("Kb-TaskId", task.to_string()),
        ("Kb-Scene", "1".to_string()),
        ("Kb-CompressionType", "1".to_string()),
        ("Content-Type", "application/octet-stream".to_string()),
        ("Use_DebugNet", "0".to_string()),
        ("Kb-Uin", uin.to_string()),
        ("Kb-CmdId", cmd_id.to_string()),
        ("Kb-SubCmdId", sub_cmd.to_string()),
        ("Kb-SharedKeySuffix", key.suffix().to_string()),
    ] {
        headers.insert(name.to_string(), value);
    }
    let order = [
        "Kb-OsType",
        "Kb-Version",
        "Kb-Scene",
        "Kb-CmdId",
        "Kb-SubCmdId",
        "Kb-TimeStamp",
        "Kb-BodyMd5",
        "Kb-Uin",
        "Kb-TraceId",
        "Kb-TaskId",
    ];
    let sign_input = order
        .iter()
        .map(|name| headers[*name].as_str())
        .collect::<String>()
        + SIGN_SALT;
    headers.insert(
        "Kb-Sign".to_string(),
        sha256(sign_input.as_bytes()).to_uppercase(),
    );
    Ok((headers, body))
}
