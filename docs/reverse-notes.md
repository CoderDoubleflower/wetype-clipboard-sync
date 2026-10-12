# 微信输入法跨设备剪贴板：APK 逆向记录

> 本文提到的 `reverse/...` 路径是本地分析产物，出于版权和体积原因不随仓库分发，
> 复现方法见 `DEVELOPMENT.md`。结论本身（常量、字段号、调用点）在文中都写了出处。

样本：`reverse/微信输入法_4.0.0.apk`

- 包名 `com.tencent.wetype`，versionName 4.0.0，versionCode 57201，仅含 arm64-v8a。
- 反编译：jadx 1.5.6（`reverse/work/jadx`）、apktool 2.12.0（`reverse/work/apktool`）。

## 1. 涉及的模块

| 位置 | 作用 |
| --- | --- |
| `com.tencent.wetype.plugin.hld.candidate.a0` | 剪贴板管理器（监听本机剪贴板、发送、接收） |
| `com.tencent.wetype.plugin.hld.group.{c,d}` | 设备组（Group）管理：组成员、绑定状态、剪贴板版本 |
| `com.tencent.wetype.plugin.hld.model.C2102q` | 业务网络层 `ImeBusinessMgr`，调用 `WetapBusinessApi.net_start_task(cmdId, subCmdId, ...)` |
| `com.tencent.wetype.pb.GetGroupInfo` | protobuf：`ClipboardMsg`、`SendClipboardMsgReq/Rsp`、`GroupInfo`、`BoundInfo` |
| `com.tencent.wetype.pb.Push` | protobuf：`PushClipboardMsg`、`PushBindSuccess`、`PushUnBind` 等推送 |
| `libime_net.so` | 网络传输层（Cronet + HTTP 头 `Kb-*`、签名 salt、`/api_v2` 等端点） |
| `libwetap_business.so` / `libwetap_business_jni.so` | wetap 业务 SDK：初始化、取号、任务调度、JNI 桥 |
| `libcryptopp.so` | Crypto++，包含 `secp128r1` 曲线（ECDH） |
| `libwcwss.so` | WebSocket+TLS（websocketpp），长连接/推送通道 |
| `libWXP2PTransfer.so` | 局域网文件互传（gRPC over TLS，独立于剪贴板文本通道） |

## 2. 业务命令（cmdId 4814）

从 Java 调用点直接读到（文件:行号为反编译产物内位置）：

| cmdId/subCmdId | 请求 → 响应 | 证据 |
| --- | --- | --- |
| 4814/34 | `GetGroupInfoReq` → `GetGroupInfoRsp` | `group/c.java:340` |
| 4814/36 | `SendClipboardMsgReq` → `SendClipboardMsgRsp` | `candidate/a0.java:1419` |
| 4814/60 | COS 上传凭证 | `cos/f.java:119` |
| 4814/79 | `UpdateDeviceInfoReq` → `UpdateDeviceInfoRsp` | `model/C2102q.java:1809` |
| 4814/73–80 | P2P 文件传输（`WtP2PTransfer`） | `ui/filetransfer/X.java` |
| 4814/25 | `GetWxToken` | `thirdparty/l.java:323` |
| 4814/95 | `GetUserToken` | `thirdparty/c.java:376` |

`SendClipboardMsgReq` 在 Java 侧构造为：
`setUin(本机 uin).setPlatform(1).setGroupId(组 id).setClipboardMsg(msg)`。
服务端记录该组的剪贴板版本号，其他设备通过 4814/34 拉取到最新 `ClipboardMsg`，
或由推送（`Push.PushClipboardMsg`）通知。

## 3. 剪贴板消息内容

`GetGroupInfo.ClipboardMsg` 字段（自生成代码的序列化器还原）：
`version=1, type=2, data=3, expire_at=4, expiration=5, binary_data=6, from_uin=7, extra_info=8, trace_id=9`。

发送路径（`candidate/a0.java`）：

- 文本：`type=1`，`binaryData = text.toString().getBytes(UTF_8)`，明文 UTF-8，无应用层加密；
  内容长度为 0 时跳过，超过阈值（`feature.a.u0()` KB）改走 COS：`type=5`，`binaryData` 为对象 URL。
- 图片：先做 AES-128-CTR（key 与 IV 同为 16 字节随机 key）加密后上传 COS，
  `type=2`（图片）或 `type=6`（大文件），`binaryData` 内是 COS 引用 + 密钥。

接收路径按 `type` 分流：1 直接 UTF-8 解码；5 下载 COS 后解码；2/6 下载并解密后写图片。
过期时间 `expire_at`（秒）用于判断是否仍然投递，`trace_id` 用于去重与上报。

## 4. 设备身份与配对

- wetap 初始化：`BusinessInitInfo{uin, uniqueId, deviceCode, scene, needGetUin, platformBaseMsg{appVer,device,platform,systemVersion,packageType}}`。
- 首次运行 `needGetUin=true`，SDK 自己走 `/oauth_pubkey_v2` + `/gen_uin_v2` 拿匿名 uin，
  不需要微信登录，也就没有“账号密码”这一步。
- 配对流程是服务端撮合：一端生成六位码（`CodeRequest`），另一端 `BindRequest` 提交码，
  生成方确认（`ConfirmBind`）后服务端建立 group，之后剪贴板按 group 广播。
- RN 设置页里对应的常量名：`CMDID_SYNC_BIND_MATCH_CODE`、`CMDID_SYNC_GET_MATCH_CODE_STATUS`、
  `CMDID_SYNC_UNBIND`、`CMDID_SYNC_FETCH_BIND_STATUS`、`CMDID_SYNC_UPDATE_DEVICE_INFO`、
  `GeneralDeviceSyncCodeInput`（见 `assets/*.hbc`）。

## 5. 与开源实现的交叉验证

独立开源实现 `mkdir700/wx-ime-sdk` 声称还原了同一协议。逐条比对后，以下关键点在本 APK 中
都能找到独立证据：

| 结论 | 本 APK 证据 |
| --- | --- |
| 业务域名为 `wetype.weixin.qq.com`，端点 `/timestamp`、`/oauth_pubkey_v2`、`/gen_uin_v2`、`/api_v2` | `libime_net.so`、`libwetap_business.so` 字符串 |
| HTTP 头 `Kb-Uin/Kb-CmdId/Kb-SubCmdId/Kb-Sign/Kb-SharedKeySuffix/Kb-BodyMd5/Kb-TraceId/Kb-TaskId/...` | `libime_net.so` 字符串 |
| 签名 salt `aI0wC6qT1nB3nN5r` | `libime_net.so` 字符串 |
| ECDH 曲线 secp128r1 | `libcryptopp.so` 符号 `CryptoPP::ASN1::secp128r1` |
| cmdId 4814 下的 34=Group、36=SendClipboard、60=COS | jadx 反编译调用点 |

实测（2026-10-12）：用该实现以全新设备身份完成 `/timestamp` → `/oauth_pubkey_v2` → `/gen_uin_v2`
握手并取得匿名 Uin，服务端接受 3.5.4 版本号；本地 22 项单元测试全部通过。

端到端联调（2026-10-12）：与官方 Android 版用手机显示的六位配对码配对成功并建立设备组；
Linux → 手机、手机 → Linux 两个方向的文本粘贴均实测通过。
