# 协议速览

下面内容只写“能被证据支撑”的部分：字段号来自 jadx 反编译的序列化器，常量来自 native 库字符串，
流程来自 Java 调用点与线上实测。标注 `推测` 的条目表示尚未直接验证。

## 传输

业务请求是 HTTPS POST，域名 `https://wetype.weixin.qq.com`：

| 端点 | 用途 |
| --- | --- |
| `/timestamp` | 取服务端毫秒时间戳，用于校正本机时钟 |
| `/oauth_pubkey_v2` | ECDH 握手（secp128r1），返回服务端公钥与 `gen_uin_token` |
| `/gen_uin_v2` | 用握手后的密钥提交 `UinRequest`，换取匿名 uin |
| `/api_v2` | 业务命令，`Kb-CmdId=4814` + `Kb-SubCmdId` |

### 请求头

```
Kb-OsType: 1              Kb-Version: 3.5.4.57423（4.0.0 客户端为 4.0.0.58111）
Kb-SystemVersion: 34      Kb-PackageType: 2
Kb-TimeStamp: <ms>        Kb-BodyMd5: <body 的 MD5 大写十六进制>
Kb-TraceId: <随机>        Kb-TaskId: <自增>
Kb-Scene: 1               Kb-CompressionType: 1（1=原文，2=snappy）
Content-Type: application/octet-stream
Use_DebugNet: 0
```

`/api_v2` 额外带 `Kb-Uin`、`Kb-CmdId=4814`、`Kb-SubCmdId`、`Kb-SharedKeySuffix`（会话密钥后 4 字节 ASCII）。
所有端点（除 `/timestamp`）都带 `Kb-Sign`：

```
Kb-Sign = SHA256(按固定顺序拼接的头值 + "aI0wC6qT1nB3nN5r").hex().upper()
```

固定顺序：

- `/oauth_pubkey_v2`：`Kb-OsType, Kb-Version, Kb-Scene, Kb-TimeStamp, Kb-BodyMd5, Kb-TraceId, Kb-TaskId`
- `/gen_uin_v2`：`Kb-OsType, Kb-Version, Kb-Scene, Kb-TimeStamp, Kb-BodyMd5, Kb-GenUinToken, Kb-TraceId, Kb-TaskId`
- `/api_v2`：`Kb-OsType, Kb-Version, Kb-Scene, Kb-CmdId, Kb-SubCmdId, Kb-TimeStamp, Kb-BodyMd5, Kb-Uin, Kb-TraceId, Kb-TaskId`

## 加密

- 握手请求体用固定 bootstrap 密钥做 AES-256-ECB，密钥为 ASCII `G7E1C8B0D7D7E4L3J2U9D6W0B2M8Q6O5`。
- ECDH：secp128r1，公钥为未压缩点（33 字节，`04` 前缀）的十六进制大写。
  共享密钥是共享点的 X 坐标（16 字节），取其十六进制大写（32 字节 ASCII）作为会话密钥。
- 握手之后的请求/响应体都用会话密钥 AES-256-ECB 加解密。
- 图片对象另有独立密钥：AES-128-CTR，key 与 IV 相同（16 字节随机 key）。

## 业务封装

`/api_v2` 的明文是 protobuf（字段号已用反编译的生成代码核对）：

```proto
message BaseRequest  { uint64 uin = 2; uint32 subcommand = 3; Header header = 4; bytes body = 5; }
message BaseResponse { int32 field2 = 2; bytes body = 6; }
message Header       { repeated HeaderItem items = 1; }
message HeaderItem   { string key = 1; string value = 2; }
```

`header` 里放 `vid`(uin)、`app_ver`、`platform=1`、`device`（形如
`UniClipboard;research;standalone;<设备名>;sdk 34;PHONE`）、`channelid`、`client_timestamp`，
`body` 是各命令自己的 protobuf。

## 命令

| SubCmdId | 名称 | 请求 / 响应 |
| --- | --- | --- |
| 30 | 申请配对码 | `CodeRequest` / `CodeResponse{code}` |
| 31 | 加入配对 | `BindRequest{code, device}` / `BindResponse{bind_id, state}` |
| 32 | 确认配对 | `FetchBindRequest{bind_id}` / `ConfirmBindResponse{group_id, peer}` |
| 33 | 释放配对码/取消绑定 | `ReleaseBindRequest` |
| 34 | 拉取设备组 | `GroupRequest{filter}` / `GroupResponse{group, status}` |
| 36 | 发送剪贴板 | `SendClipboardRequest{group_id, msg}` / `SendClipboardResponse{version}` |
| 38 | 查询绑定状态 | `FetchBindRequest` / `FetchBindResponse{state}` |
| 40 | 轮询配对码状态 | `PollCodeRequest` / `PollCodeResponse{state, bind_id, device}` |
| 60 | COS 上传凭证 | `CosRequest` / `CosResponse` |

`GroupRequest.filter` 位含义（实测）：bit0 取组成员，bit1 取剪贴板，bit6 取待处理传输邀请。

## 剪贴板消息

```proto
message ClipboardMsg {
  uint64 version = 1; uint32 type = 2; string data = 3;
  uint64 expire_at = 4; int64 expiration = 5; bytes binary_data = 6;
  uint64 from_uin = 7; bytes extra_info = 8; string trace_id = 9;
}
message SendClipboardRequest { uint64 uin = 1; uint32 platform = 2; ClipboardMsg msg = 3; uint64 group_id = 4; }
message SendClipboardResponse { int32 status = 1; uint64 version = 2; }
```

`type`：`1` 内联文本（`binary_data` 为 UTF-8 明文）、`5` 走 COS 的大文本（`binary_data` 为 URL）、
`2`/`6` 走 COS 的图片/文件（`binary_data` 为加密引用）。

接收条件：`version > 本组已消费版本`、`from_uin` 不是自己、`expire_at*1000 > 当前时间`。
消费后需要持久化游标，避免重启后重复投递。

## 推送

长连接（`libwcwss.so`，websocketpp + TLS）承载服务端推送：

- `Push.PushClipboardMsg{group_id=1, msg=2}`：组内出现新剪贴板消息。
- `Push.PushBindSuccess{bind_id, group_id, operate}` / `PushUnBind` / `PushOnBoundListChange`：绑定状态变化。

`推测`：推送只是“有新数据”的通知，正文仍以 4814/34 拉取为准；本实现用 2 秒轮询代替长连接。
