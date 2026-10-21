# 开发说明

## 构建范围

项目支持 Windows 10 1607 及以上 / Windows 11，由 `wxclipd/` Rust 应用、`ui/WeTypeClipboard.Wpf/` WPF 界面和 `vendor/wx-ime-sdk/` 协议 workspace 组成。协议 workspace 仅保留 `wx-ime-sdk` 与 `wx-ime-protocol` 两个库；应用与协议 workspace 分别维护 `Cargo.lock`，固定各自的依赖解析。Rust 最低版本为 1.92，发布目标为 **i686-pc-windows-msvc（32 位 x86）**。WPF 使用 .NET 10，按 `win-x86` 自包含发布，用户无需另装运行时。

协议中的 `secp128r1` 运算继续使用 OpenSSL；HTTPS/WSS 的 TLS 使用 Rustls。Windows 使用 `vendored-openssl` 从源码静态构建 OpenSSL，并静态链接 MSVC CRT，用户无需另装 OpenSSL 或 VC++ Redistributable。不要把协议曲线替换成另一种算法。

## 构建 Windows x86

需要 Visual Studio 2022 C++ Build Tools、Windows SDK、Rustup、Perl、NASM、protoc 29.3、.NET 10 SDK 和 Inno Setup 6。在 **x86 Native Tools Command Prompt** 中启动 PowerShell，确保这些工具在 PATH 内。

使用 i686 主机工具链，使 OpenSSL 的构建脚本和目标程序都使用同一种 CPU 架构：

```powershell
rustup toolchain install 1.92.0-i686-pc-windows-msvc --profile minimal --component rustfmt --force-non-host
$env:CARGO_TARGET_I686_PC_WINDOWS_MSVC_RUSTFLAGS = "-C target-feature=+crt-static"

cargo +1.92.0-i686-pc-windows-msvc test --locked --release --manifest-path wxclipd/Cargo.toml -p wxclipd -p wx-ime-sdk -p wx-ime-protocol --features wxclipd/vendored-openssl --target i686-pc-windows-msvc -- --test-threads=1
cargo +1.92.0-i686-pc-windows-msvc build --locked --release --manifest-path wxclipd/Cargo.toml --features vendored-openssl --target i686-pc-windows-msvc

./packaging/windows/build-package.ps1 -Version 0.5.0
```

应用、SDK 和协议测试共用应用的 lockfile、release profile 和 target 目录，验证实际交付的依赖版本，并避免 MSVC 的串行 OpenSSL 构建重复执行。SDK 不再附带独立 CLI、npm 包及其分发脚本。

产物位于 `dist/`。打包脚本会运行 `dotnet publish`，将 WPF 应用与 x86 桌面运行时完整放入包内 `ui/` 子目录；不要删除运行时文件或仅复制 EXE。构建脚本要求 OpenSSL 源码许可仍位于 Cargo 缓存中，.NET runtime pack 的许可仍位于 NuGet 缓存中。CI 额外运行 `verify-package.ps1`，需要 `RUNNER_TEMP` 指向临时目录。该脚本验证：

- CLI、托盘、WPF 程序和安装器为 x86 PE32；各程序的 Windows 子系统设置正确。
- release 可执行文件可以运行，版本标识为 `windows-x86`。
- Rust 程序不依赖外置 OpenSSL 或 MSVC C++ 动态运行库；WPF 包包含 .NET Core 与 Windows Desktop 运行时。
- 可执行文件与安装器包含微信输入法图标，资源版本与 Cargo 版本一致。
- 当前用户模式安装、常驻托盘、状态窗口、单实例、后台启停、旧版本升级与运行中卸载能够完成。
- 实际 WPF 窗口渲染浅色与深色截图，验证主按钮生成文字的颜色与对比度；通过 UI Automation 读取配置和调用按钮，修改 Windows 应用颜色设置后同一个窗口实时切换。
- 升级基准为已发布且校验 SHA-256 的 0.4.1 安装包：启动其常驻托盘及同步进程后安装新版，确认旧进程退出，并保留配对身份与配置。

Windows 原生剪贴板测试操作执行测试账户的实际剪贴板，因此用单测试线程运行。CI 在 Windows Server 2022 的 x64 主机上执行 x86 程序；这不是对每种 32 位 Windows 设备的实机覆盖。

## 代码结构

| 路径 | 职责 |
| --- | --- |
| `wxclipd/src/main.rs` | 命令行、交互配置/配对、离线启动重试 |
| `wxclipd/src/background.rs` | Windows GUI 子系统入口 |
| `wxclipd/src/tray.rs` | 常驻托盘、后台进程控制与日志入口 |
| `wxclipd/src/tray/wpf.rs` | WPF 子进程与有界标准输入输出通信、退出回收 |
| `ui/WeTypeClipboard.Wpf/` | WPF 窗口、主题资源、状态绑定和 UI 验证入口 |
| `wxclipd/src/runtime_status.rs` | 本地运行快照、收发计数与凭据安全的状态输出 |
| `wxclipd/src/config.rs` | 配置验证与原子保存 |
| `wxclipd/src/clipboard/` | Win32 剪贴板读写与原生测试 |
| `wxclipd/src/image.rs` | 有界图片解码、PNG/JPEG 编码、像素指纹与接收缓存校验 |
| `wxclipd/src/input_watch/` | Windows 键盘与鼠标输入 hooks |
| `wxclipd/src/ipc/` | Windows Named Pipe 与当前用户访问控制 |
| `wxclipd/src/sync.rs` | 本地观察、网络 worker、控制命令 |
| `wxclipd/src/sync/state.rs` | 最新值队列、receipt/ack 与冲突状态机 |
| `vendor/wx-ime-sdk/crates/wx-ime-sdk/src/network.rs` | HTTP/SOCKS 路由与 WebSocket 隧道 |
| `packaging/windows/` | 安装器、打包/验证脚本、用户文档 |
| `docs/` | 协议记录与逆向证据 |

## 配置和网络

`config.json` 保存 `proxy`、`poll_interval_seconds` 和 `paste_trigger`。代理优先级是命令行、`WXCLIPD_PROXY`、配置文件、直连。环境变量为空表示显式直连；系统 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和 `NO_PROXY` 不改变应用路由。

HTTP 代理使用 CONNECT，支持 Basic 认证；SOCKS5 支持用户名密码认证。`socks5://` 本地解析目标域名，`socks5h://` 把目标域名交给代理。控制 API、COS 图片/文本下载上传和通知 WSS 使用同一配置，不在代理失败时回退直连。图片沿用 SDK 的加密 COS 通道，不需要局域网文件通道；SDK 中应用未使用的文件传输通道没有扩展代理能力。

不要在错误或日志中输出原始代理 URL。认证信息留在受当前用户访问权限保护的配置文件中，并通过子进程环境传递最终代理设置，避免放进后台进程参数。

## 状态与本地控制

默认状态目录是 `%LOCALAPPDATA%\wxclipd`。测试使用独立 `--state` 目录。身份私钥和待确认 receipt 存在此处；同一个目录只能有一个同步进程。

状态目录与 Named Pipe 仅允许当前用户及 SYSTEM 访问，拒绝远程 Named Pipe 客户端。

控制协议每行一个请求，最多 255 字节，回应也按换行分帧：

| 请求 | 作用 |
| --- | --- |
| `ping` | 返回 daemon 进程 ID |
| `inspect` | 返回本地运行快照 JSON；不请求网络，也不等待 SDK 锁 |
| `stop` | 请求退出；CLI 等待进程结束 |
| `sync` | 检查本地剪贴板并拉取远端 |
| `devices` / `status` | 返回设备/剪贴板状态 JSON |
| `pair <code>` | 输入手机配对码并等待手机确认 |
| `unbind <uin>` | 解绑指定设备 |

初始化网络失败时仍可 `ping`、`inspect` 和 `stop`，其余命令返回初始化状态。`code` 需要前台交互，运行前先停 daemon。

Windows 的 `wxclipd-background.exe` 为常驻 GUI 子系统进程，负责 Shell 通知区域图标和进程控制；`wxclipd.exe run` 负责实际同步。第一次打开状态窗口时，托盘启动包内 `ui/wxclipd-ui.exe --bridge`，之后重复打开会激活同一个 WPF 窗口。托盘按状态目录去重，复用同一用户已有的同步进程。`stop` 仅停止同步，托盘保留；`wxclipd-background.exe --quit` 同时停止同步，等待 WPF 子进程及托盘退出后才返回，保证升级 / 卸载可替换运行时文件。升级兼容没有托盘、也不支持 `--quit` 的 0.3.0 启动器。

托盘 worker 定期调用本地 `inspect`，由独立通信线程通过继承的匿名管道向 WPF 传递经过字段筛选与脱敏的 NDJSON 快照；WPF 返回固定白名单操作名，由托盘执行原有控制流程。没有新增公开监听端口，不把代理凭据或启动覆盖环境变量交给界面。管道读写不能阻塞任一窗口消息线程；状态更新只保留最新快照。WPF 在管道断开时退出，托盘退出会等待并回收自己创建的窗口进程。

运行快照不包含剪贴板内容、代理凭据、设备密钥或完整远端错误。计数及最近收发时间仅属于当前同步进程，不持久化。窗口展示运行时配置与保存配置的差别，避免把磁盘修改误当成已经生效。配对与配置写入继续使用现有向导，避免在界面层复制凭据处理逻辑。

WPF 主题默认跟随 Windows 应用模式，响应 `WM_SETTINGCHANGE` / 系统偏好变化，以动态资源更新颜色并适配支持的标题栏。高对比度优先使用系统颜色。不要仅在启动时读取主题，也不要把硬编码浅色背景混入动态主题控件。窗口采用原生标题栏、可调整尺寸与滚动内容，保留键盘和 UI Automation 操作。`--capture-dir PATH` 是离线验证入口，渲染真实 XAML 的浅色 / 深色 PNG 并输出 `capture-result.json`；发布流程还对实际安装的桥接窗口验证系统主题切换，捕获模式不能替代此项。

图标采用原始品牌资源并派生多个 Windows ICO 尺寸，来源与权利说明随仓库保留；不能将其混入原创代码的 MIT 授权。构建时将图标和版本资源嵌入 Rust 与 WPF 可执行文件，安装器也使用同一图标。托盘处理 `TaskbarCreated` 以恢复资源管理器重启后的图标，退出时删除图标。

## 同步状态约定

本地默认每 400 ms 观察一次，独立于网络请求。远端默认每 10 秒轮询，并接受通知、Ctrl/右键和 `sync-now` 触发。输入触发不会阻塞或重放用户按键；不能保证同一次 Ctrl+V 等待网络完成。

出站只保留最新待发文本或图片，不是历史队列。明确网络失败及发布前的控制 HTTP 400 会退避重试；等待时间依次为 1、2、4、8、16、30 秒，此后保持 30 秒，不因重试次数耗尽而丢弃待发内容。请求耗时另计。发送期间新复制内容取代旧待发内容。SDK 返回 `OutcomeUnknown` 时保留待发内容但不自动重发，可由 `sync-now` 重试当前内容，或通过新内容恢复发送；同一内容仅序列号变化不会解除暂停。出站队列不跨进程重启持久化。

控制 HTTP 400 不足以证明具体服务端原因，但会使 SDK 放弃当前控制密钥。对可安全重试的控制请求，单次调用最多重新获取服务端时间、执行 ECDH 并重试一次；若仍失败，交回上层退避。未初始化或上次初始化被中断 / 失败时，后续控制请求会重新初始化，避免永久停留在 `session not initialized`。恢复沿用原设备身份和显式代理配置；通知 WSS 使用其自己的连接密钥。

最终 `SEND_CLIPBOARD` 发布请求不得因网络失败或会话恢复而盲目重放；无法确认结果时继续返回 `OutcomeUnknown`。`COS HTTP status 400` 不按控制会话错误分类，不能靠重新握手自动解除确定的上传拒绝。

入站持有完整 receipt，图片先在 blocking worker 中检查缓存长度/哈希并解码，再应用到剪贴板。写入成功后才确认；确认失败只重试确认，不重复覆盖。在同一次 Win32 剪贴板锁内比较序列号和写入新值，保护刚发生的本地复制，包括重新复制同一内容。

本地输入 hooks 只将 Ctrl/鼠标右键转换为提前拉取提示，不记录用户输入。

图片快照通过 `Arc` 共享，比较尺寸及 RGBA 的 SHA-256；Windows 按剪贴板序列号缓存解码结果。识别顺序为 PNG、DIBV5、DIB、文本。网络图片上限 16 MiB，解码像素上限 64 MiB RGBA、每边 16384；decoder 另有分配限制。保留 JPEG 编码，位图默认 PNG，超限时仅不透明图片可降为 JPEG 95/90。有限长度 writer 必须检查编码器返回值和 writer 的溢出、分配失败状态：PNG 的最终 IEND 写入可发生在 Drop 中。

读取 Windows packed DIB 时，适配层计算并校验像素起点，补上具有显式 `bfOffBits` 的 BMP 文件头，再交给解码器。`image` 0.25.10 的无文件头 BMP 路径会对 V4/V5 的 `BI_BITFIELDS` 额外跳过 12 字节，原生测试已覆盖由此导致的解码失败与透明度丢失。V4/V5 的掩码已经包含在结构中；仅旧 40 字节 `BITMAPINFOHEADER` 使用外置掩码。参见 [Microsoft BITMAPV5HEADER](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-bitmapv5header) 的 `bV5Compression` 与 packed DIB 说明，以及 [V4 结构布局](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-wmf/071b0c0d-c2df-4f1c-9828-d03c26002c61)。

确定无效的图片不阻塞后续内容：完整 receipt 的解码错误进入确认阶段；SDK 在 receipt 创建前发现坏引用、超限或完整性错误时返回 `InvalidClipboard`，应用使用精确 ID 的 `discard_clipboard`。临时网络、文件 I/O 和可识别的分配错误保留并重试；游标持久化失败恢复旧状态。缓存清理保留待确认图片，过滤符号链接和 Windows reparse point。

## 测试与发布

`.github/workflows/ci.yml` 在 feature 分支、PR 和 main 上执行 Windows 格式检查、构建与测试。测试包含真实文本/图片剪贴板、透明度、占用与序列冲突、本地 IPC；代理测试用本地模拟 HTTP/SOCKS 服务器验证认证、DNS 模式、拒绝/超时、WSS 握手，以及受信本地 TLS 下的加密 COS 图片 PUT/GET 逐字节往返，不依赖用户手机号或配对状态。

控制会话回归测试使用本地模拟服务覆盖 HTTP 400 后重建、持续拒绝的有界恢复、初始化失败后的再次恢复，以及有副作用请求不被自动重放；应用测试覆盖控制 / COS 错误分类、最新值重试和发送结果不确定时的暂停。测试无需真实手机身份。

main 的 Windows 构建、测试和安装验收通过后，在 Windows 发布任务中以 `wxclipd/Cargo.toml` 的版本创建 GitHub Release，上传安装器、便携 ZIP 和 SHA256SUMS。已存在的版本不覆盖；后续发布请递增版本并更新 `packaging/windows/RELEASE-NOTES.md`。

格式检查：

```powershell
cargo +1.92.0-i686-pc-windows-msvc fmt --manifest-path wxclipd/Cargo.toml --check
cargo +1.92.0-i686-pc-windows-msvc fmt --manifest-path vendor/wx-ime-sdk/Cargo.toml --all --check
git diff --check
```

代码测试和安装测试不能代替真实手机验收。验收时配对官方 App，分别测试两端复制中文/emoji/多行文本、超过 256 KiB 的 COS 文本路径、截图、透明 PNG、JPEG、文本/图片交替、代理断开与恢复、图片下载时再次复制、锁屏/睡眠恢复和重新登录自启。确认接收图片没有自动回传，并在支持图片粘贴的应用中检查方向和透明度。

## 协议与 APK 分析

调试材料见 `docs/protocol.md`、`docs/cmd-table.md`、`docs/reverse-notes.md`。SDK 的 `ClientOptions.capture_dir` 可保存请求/响应；它们含明文剪贴板数据，勿提交。

APK 和反编译产物不随仓库分发。原始分析基于微信输入法 4.0.0（versionCode 57201），复现可自备 APK 放入 `reverse/`，使用 jadx、apktool 和 hermes-dec 分析，再按文档定位 native 库中的协议常量。协议层本地修改记在 `vendor/wx-ime-sdk/VENDORED.md`。
