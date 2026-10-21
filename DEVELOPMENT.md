# 开发说明

## 构建范围

项目由 `wxclipd/` 应用和 `vendor/wx-ime-sdk/` 独立 Cargo workspace 组成，各自使用自己的 `Cargo.lock`。Rust 最低版本为 1.92。Windows 发布目标是 **i686-pc-windows-msvc（32 位 x86）**，Linux 保留 Wayland/X11 支持。

协议中的 `secp128r1` 运算继续使用 OpenSSL；HTTPS/WSS 的 TLS 使用 Rustls。Windows 使用 `vendored-openssl` 从源码静态构建 OpenSSL，并静态链接 MSVC CRT，用户无需另装 OpenSSL 或 VC++ Redistributable。不要把协议曲线替换成另一种算法。

## Linux

```sh
sudo apt install protobuf-compiler libssl-dev pkg-config wl-clipboard
# X11 桌面用 xclip 代替 wl-clipboard。
cargo test --locked --manifest-path wxclipd/Cargo.toml
cargo test --locked --workspace --manifest-path vendor/wx-ime-sdk/Cargo.toml
cargo build --locked --release --manifest-path wxclipd/Cargo.toml --bin wxclipd
```

系统 OpenSSL 需要支持 `secp128r1`，可以用 `openssl ecparam -list_curves` 检查。也可启用 `--features vendored-openssl`，这需要 C 编译器、Perl 和 Make。

## Windows x86

需要 Visual Studio 2022 C++ Build Tools、Windows SDK、Rustup、Perl、NASM、protoc 29.3 和 Inno Setup 6。在 **x86 Native Tools Command Prompt** 中启动 PowerShell，确保这些工具在 PATH 内。

使用 i686 主机工具链，使 OpenSSL 的构建脚本和目标程序都使用同一种 CPU 架构：

```powershell
rustup toolchain install 1.92.0-i686-pc-windows-msvc --profile minimal --force-non-host
$env:CARGO_TARGET_I686_PC_WINDOWS_MSVC_RUSTFLAGS = "-C target-feature=+crt-static"

cargo +1.92.0-i686-pc-windows-msvc test --locked --release --manifest-path wxclipd/Cargo.toml -p wxclipd -p wx-ime-sdk -p wx-ime-protocol --features wxclipd/vendored-openssl --target i686-pc-windows-msvc -- --test-threads=1
cargo +1.92.0-i686-pc-windows-msvc build --locked --release --manifest-path wxclipd/Cargo.toml --features vendored-openssl --target i686-pc-windows-msvc

./packaging/windows/build-package.ps1 -Version 0.3.0
```

Windows 的应用与 SDK 测试共用应用的 lockfile、release profile 和 target 目录，验证实际交付的依赖版本，并避免 MSVC 的串行 OpenSSL 构建重复执行。Linux 仍额外验证独立 SDK workspace。

产物位于 `dist/`。构建脚本要求编译时下载的 OpenSSL 源码许可仍位于 Cargo 缓存中。CI 额外运行 `verify-package.ps1`，需要 `RUNNER_TEMP` 指向临时目录。该脚本验证：

- CLI、后台启动器和安装器为 x86 PE32；CLI/启动器的 Windows 子系统设置正确。
- release 可执行文件可以运行，版本标识为 `windows-x86`。
- 不依赖 OpenSSL 或 MSVC C++ 动态运行库。
- 当前用户模式安装、后台进程启动、停止和卸载能够完成。

Windows 原生剪贴板测试操作执行测试账户的实际剪贴板，因此用单测试线程运行。CI 在 Windows Server 2022 的 x64 主机上执行 x86 程序；这不是对每种 32 位 Windows 设备的实机覆盖。

## 代码结构

| 路径 | 职责 |
| --- | --- |
| `wxclipd/src/main.rs` | 命令行、交互配置/配对、离线启动重试 |
| `wxclipd/src/background.rs` | Windows 无控制台启动器、日志重定向 |
| `wxclipd/src/config.rs` | 配置验证与原子保存 |
| `wxclipd/src/clipboard/` | Win32 剪贴板和 Wayland/X11 后端 |
| `wxclipd/src/image.rs` | 有界图片解码、PNG/JPEG 编码、像素指纹与接收缓存校验 |
| `wxclipd/src/input_watch/` | Windows 输入 hooks、Linux evdev 触发 |
| `wxclipd/src/ipc/` | Windows Named Pipe、Linux Unix socket |
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

Windows 默认状态目录是 `%LOCALAPPDATA%\wxclipd`；Linux 是 `$XDG_STATE_HOME/wxclipd` 或 `~/.local/state/wxclipd`。测试使用独立 `--state` 目录。身份私钥和待确认 receipt 存在此处；同一个目录只能有一个同步进程。

Windows 状态目录与 Named Pipe 仅允许当前用户及 SYSTEM 访问，拒绝远程 Named Pipe 客户端。Linux 目录为 0700，socket 为 0600。

控制协议每行一个请求，最多 255 字节，回应也按换行分帧：

| 请求 | 作用 |
| --- | --- |
| `ping` | 返回 daemon 进程 ID |
| `stop` | 请求退出；CLI 等待进程结束 |
| `sync` | 检查本地剪贴板并拉取远端 |
| `devices` / `status` | 返回设备/剪贴板状态 JSON |
| `pair <code>` | 输入手机配对码并等待手机确认 |
| `unbind <uin>` | 解绑指定设备 |

初始化网络失败时仍可 `ping` 和 `stop`，其余命令返回初始化状态。`code` 需要前台交互，运行前先停 daemon。

## 同步状态约定

本地默认每 400 ms 观察一次，独立于网络请求。远端默认 Windows 10 秒、Linux 90 秒轮询，并接受通知、Ctrl/右键和 `sync-now` 触发。输入触发不会阻塞或重放用户按键；不能保证同一次 Ctrl+V 等待网络完成。

出站只保留最新待发文本或图片，不是历史队列。明确网络失败会退避重试；发送期间新复制内容取代旧待发内容。SDK 返回 `OutcomeUnknown` 时保留待发内容但不自动重发，可由 `sync-now` 重试当前内容，或通过新内容恢复发送；同一内容仅序列号变化不会解除暂停。出站队列不跨进程重启持久化。

入站持有完整 receipt，图片先在 blocking worker 中检查缓存长度/哈希并解码，再应用到剪贴板。写入成功后才确认；确认失败只重试确认，不重复覆盖。Windows 在同一次剪贴板锁内比较序列号和写入新值，保护刚发生的本地复制，包括重新复制同一内容。Linux 外部剪贴板命令只能做最后一次重新读取检查，不能提供同样的原子保证。

本地输入监听代码只用 Ctrl/鼠标右键触发拉取。Linux 的 `60-wxclipd-input.rules` 需早于 systemd 的 `73-seat-late.rules` 生效，以便 `uaccess` 授予活跃会话权限。

图片快照通过 `Arc` 共享，比较尺寸及 RGBA 的 SHA-256；Windows 按剪贴板序列号缓存解码结果。识别顺序为 PNG、DIBV5、DIB、文本。网络图片上限 16 MiB，解码像素上限 64 MiB RGBA、每边 16384；decoder 另有分配限制。保留 JPEG 编码，位图默认 PNG，超限时仅不透明图片可降为 JPEG 95/90。有限长度 writer 必须检查编码器返回值和 writer 的溢出、分配失败状态：PNG 的最终 IEND 写入可发生在 Drop 中。

读取 Windows packed DIB 时，适配层计算并校验像素起点，补上具有显式 `bfOffBits` 的 BMP 文件头，再交给解码器。`image` 0.25.10 的无文件头 BMP 路径会对 V4/V5 的 `BI_BITFIELDS` 额外跳过 12 字节，原生测试已覆盖由此导致的解码失败与透明度丢失。V4/V5 的掩码已经包含在结构中；仅旧 40 字节 `BITMAPINFOHEADER` 使用外置掩码。参见 [Microsoft BITMAPV5HEADER](https://learn.microsoft.com/en-us/windows/win32/api/wingdi/ns-wingdi-bitmapv5header) 的 `bV5Compression` 与 packed DIB 说明，以及 [V4 结构布局](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-wmf/071b0c0d-c2df-4f1c-9828-d03c26002c61)。

确定无效的图片不阻塞后续内容：完整 receipt 的解码错误进入确认阶段；SDK 在 receipt 创建前发现坏引用、超限或完整性错误时返回 `InvalidClipboard`，应用使用精确 ID 的 `discard_clipboard`。临时网络、文件 I/O 和可识别的分配错误保留并重试；游标持久化失败恢复旧状态。缓存清理保留待确认图片，过滤符号链接和 Windows reparse point。

## 测试与发布

`.github/workflows/ci.yml` 在 feature 分支、PR 和 main 上构建并测试 Linux/Windows。Windows 测试包含真实文本/图片剪贴板、透明度、占用与序列冲突、本地 IPC；代理测试用本地模拟 HTTP/SOCKS 服务器验证认证、DNS 模式、拒绝/超时、WSS 握手，以及受信本地 TLS 下的加密 COS 图片 PUT/GET 逐字节往返，不依赖用户手机号或配对状态。

main 的 Linux 与 Windows 任务都通过后，以 `wxclipd/Cargo.toml` 的版本创建 GitHub Release，上传安装器、便携 ZIP 和 SHA256SUMS。已存在的版本不覆盖；后续发布请递增版本并更新 `RELEASE-NOTES.md`。

格式检查：

```sh
cargo fmt --manifest-path wxclipd/Cargo.toml --check
cargo fmt --manifest-path vendor/wx-ime-sdk/Cargo.toml --all --check
git diff --check
```

代码测试和安装测试不能代替真实手机验收。验收时配对官方 App，分别测试两端复制中文/emoji/多行文本、超过 256 KiB 的 COS 文本路径、截图、透明 PNG、JPEG、文本/图片交替、代理断开与恢复、图片下载时再次复制、锁屏/睡眠恢复和重新登录自启。确认接收图片没有自动回传，并在支持图片粘贴的应用中检查方向和透明度。

## 协议与 APK 分析

调试材料见 `docs/protocol.md`、`docs/cmd-table.md`、`docs/reverse-notes.md`。SDK 的 `ClientOptions.capture_dir` 可保存请求/响应；它们含明文剪贴板数据，勿提交。

APK 和反编译产物不随仓库分发。原始分析基于微信输入法 4.0.0（versionCode 57201），复现可自备 APK 放入 `reverse/`，使用 jadx、apktool 和 hermes-dec 分析，再按文档定位 native 库中的协议常量。协议层本地修改记在 `vendor/wx-ime-sdk/VENDORED.md`。
