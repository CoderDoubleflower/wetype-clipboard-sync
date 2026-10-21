# WeType Clipboard Sync

通过微信输入法（WeType）的「跨设备粘贴」，在手机与 **Windows / Linux** 电脑之间同步文本剪贴板。电脑以独立设备身份加入配对组，手机继续使用官方微信输入法。

0.2.0 增加 Windows 版、交互式配置与配对、后台启动，以及覆盖全部同步网络连接的 HTTP / SOCKS5 代理。项目基于 Linux 版扩展，是非官方实现，与腾讯无关联；官方协议变化可能影响兼容性。

## Windows 快速开始

支持 **Windows 10 及以上**。发布包中的 **x86 指 32 位程序**，也可在 64 位 Windows 上兼容运行。

从 [Releases](https://github.com/CoderDoubleflower/wetype-clipboard-sync/releases) 下载：

| 文件 | 用途 |
| --- | --- |
| `wetype-clipboard-sync-0.2.0-windows-x86-setup.exe` | 当前用户安装，可创建开始菜单入口并设置登录后自启 |
| `wetype-clipboard-sync-0.2.0-windows-x86-portable.zip` | 解压运行，无需安装；保留包内两个 `.exe` 在同一目录 |
| `SHA256SUMS.txt` | 安装包和压缩包的 SHA-256 校验值 |

1. 运行安装包，完成后勾选 **Configure proxy and pair with my phone**；也可从开始菜单打开 **WeType Clipboard Sync → Setup and Pair**。便携版解压后双击 `wxclipd.exe`。
2. 在设置窗口输入代理地址，或直接回车保留当前设置；首次使用默认直连，输入 `direct` 可清除已有代理。
3. 设置远端轮询间隔，默认 10 秒。打开手机微信输入法的「跨设备粘贴」，将手机显示的六位配对码填入窗口，并在手机上确认。
4. 设置完成后会启动后台同步。分别在电脑和手机复制一段新文本，留出网络同步时间后，在另一端粘贴验证。

开始菜单还提供 **Start Sync**、**Stop Sync**、**Sync Now**。当前版本通过控制台向导和后台进程工作，没有托盘设置界面。详细配置、日志与排错见 [Windows 使用说明](packaging/windows/README-Windows.md)。

## Linux 安装

需要 Rust **1.92 或更新版本**，以及正在登录的 Wayland / X11 桌面会话。以下以 Debian / Ubuntu 为例：

```sh
sudo apt install build-essential protobuf-compiler libssl-dev pkg-config
sudo apt install wl-clipboard        # Wayland；X11 改装 xclip

git clone https://github.com/CoderDoubleflower/wetype-clipboard-sync.git
cd wetype-clipboard-sync
cargo build --manifest-path wxclipd/Cargo.toml --release --locked --bin wxclipd
./packaging/install.sh

# 按提示设置代理、轮询间隔和手机配对码
"$HOME/.local/bin/wxclipd" setup
systemctl --user enable --now wxclipd
```

安装脚本把程序放到 `~/.local/bin`，并登记 systemd 用户服务。后续命令假定此目录已在 `PATH` 中；否则使用完整路径。Linux 的 `setup` 只完成设置与配对，常驻同步由 systemd 启动。

默认会安装 udev 规则，为当前活跃桌面会话授权读取输入事件，以便在按 Ctrl 或鼠标右键时提前拉取远端剪贴板。若只需要轮询和手动同步，可跳过该规则并关闭输入监听：

```sh
WXCLIPD_SKIP_UDEV=1 ./packaging/install.sh
wxclipd --no-paste-trigger config
systemctl --user restart wxclipd
```

Linux 日常管理：

```sh
systemctl --user status wxclipd          # 服务状态
journalctl --user -u wxclipd -f          # 实时日志
systemctl --user restart wxclipd         # 应用新配置
systemctl --user disable --now wxclipd   # 停止并取消自启
```

## 配置代理

Windows 请在程序目录打开 PowerShell，把下文的 `wxclipd` 写成 `.\wxclipd.exe`；Linux 直接使用 `wxclipd`。

| 类型 | 地址示例 | 目标域名解析 |
| --- | --- | --- |
| HTTP 代理 | `http://127.0.0.1:7890` | HTTPS / WSS 经 HTTP CONNECT 隧道，由代理连接目标 |
| SOCKS5 | `socks5://127.0.0.1:1080` | 本机解析目标域名 |
| SOCKS5h | `socks5h://127.0.0.1:1080` | 代理解析目标域名 |

HTTP Basic 和 SOCKS5 用户名密码认证均可使用 `scheme://user:password@host:port`。用户名或密码中的 `@`、`%` 等保留字符需进行 URL 百分号编码，例如密码 `p@ss` 写成 `p%40ss`。`socks5h` 只改变目标域名的解析位置；代理服务器自身的域名仍需本机可解析。

例如，保存 SOCKS5h 配置：

```sh
wxclipd --proxy "socks5h://127.0.0.1:1080" config
```

恢复直连使用 `wxclipd --no-proxy config`。查看当前有效设置使用 `wxclipd config`，显示时会隐藏代理凭据。

**保存后需要重启同步进程**：Windows 依次运行 `wxclipd stop`、`wxclipd start`；Linux 运行 `systemctl --user restart wxclipd`。单独给某个命令增加参数，不会修改已经运行的后台进程。

代理选择顺序为：**命令行 `--proxy` / `--no-proxy` > `WXCLIPD_PROXY` > 状态目录内的 `config.json` > 直连**。`WXCLIPD_PROXY` 为空时表示直连；如果设置过该环境变量，它仍会覆盖配置文件。程序不自动读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY` 或系统代理设置。

同一代理设置覆盖登录与配对等 Session 请求、较长文本的 COS 对象上传 / 下载，以及 WSS 通知长连接。代理连接或认证失败会报错并按同步策略重试，**不会静默改为直连**。当前支持 `http://`、`socks5://`、`socks5h://`；暂不支持 PAC、`https://` 代理端点或 NTLM / Kerberos 等企业代理认证。

## 配对与常用命令

`setup` 可以重新配置并配对。也可以直接使用命令：

```sh
wxclipd pair 123456        # 输入手机显示的六位配对码，在手机确认
wxclipd devices            # 查看已绑定设备
wxclipd status             # 查看同步状态
wxclipd sync-now           # 通知运行中的进程尽快同步一次
wxclipd unbind --peer UIN   # 解绑指定设备；只有一台时可省略 --peer
```

这些命令可以与后台同步进程配合使用。同一状态目录只允许一个活动身份进程。若要让电脑生成配对码，先停止后台服务，再运行 `wxclipd code`，在手机输入该码；配对结束后重新启动后台服务。

需要前台查看运行过程时，先停止后台服务，再运行 `wxclipd run`，按 Ctrl+C 退出。`--name` 只在首次创建身份时设置设备名。完整参数见 `wxclipd --help`。

## 同步时机与限制

| 行为 | Windows 默认值 | Linux 默认值 |
| --- | --- | --- |
| 检查本地剪贴板变化 | 400 毫秒 | 400 毫秒 |
| 主动拉取远端剪贴板 | 10 秒 | 90 秒 |
| Ctrl / 右键触发提前拉取 | 开启，防抖 800 毫秒 | 开启，需输入设备权限；防抖 800 毫秒 |

远端轮询可用 `wxclipd --poll-interval 15 config` 修改为 15 秒，允许范围为 1～3600 秒。后台同时保持通知连接，收到事件时会请求同步，但不依赖每次手机复制都产生通知。Ctrl / 右键触发只是提前发起网络请求，**不能保证紧接着的一次 Ctrl+V 已经拿到最新远端内容**。`sync-now` 返回表示请求已受理，也不是远端内容已经送达的确认。

当前只同步非空文本，单条上限 **1 MiB，按 UTF-8 字节计数**。图片、富文本格式和文件不在此版本支持范围内。启动时不会主动上传已有剪贴板；需要发送启动前内容时，可在前台使用 `wxclipd --send-current run`。

收取的入站内容有持久化记录，写入剪贴板失败会保留待处理状态并重试；期间新复制的本地内容优先。出站只在内存中保留最新待发送文本，新复制会替代旧值，退出或重启会丢失这部分待发状态，不构成剪贴板历史备份。

临时网络故障会退避重试。如果日志出现 **`OutcomeUnknown` / 发送结果不确定**，服务端可能已经接收，因此暂停该内容的自动重发。确认需要再次发送时运行 `wxclipd sync-now`；这可能造成重复发送，复制新的内容也可继续同步。

Windows 版本尚未使用真实手机账号完成端到端实测。首次安装后请验证两个方向的文本同步；构建和本地测试不能替代手机端、代理与桌面会话的实际互通验证。

## 配置与数据位置

| 平台 | 默认状态目录 | 后台日志 |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\wxclipd` | 状态目录内的 `wxclipd.log` |
| Linux | `${XDG_STATE_HOME:-~/.local/state}/wxclipd` | `journalctl --user -u wxclipd` |

配置文件为状态目录内的 `config.json`，可持久保存代理、远端轮询间隔与粘贴触发开关。`--state DIR` 可指定其他目录，优先于环境变量 `WXCLIPD_STATE`；所有管理命令需指向同一目录。便携版默认也使用上述状态目录，而不是可执行文件所在目录。

目录内包含设备身份、私钥、账号 secret、接收状态，以及可能带密码的代理配置。这些数据以明文存储；Windows 使用当前用户与系统可访问的目录 ACL 保护，Linux 使用用户目录权限保护。不要把整个状态目录发到问题反馈中。Windows 卸载程序保留该目录，以便重新安装后继续使用原身份。

## 目录与许可

| 路径 | 内容 |
| --- | --- |
| `wxclipd/` | Windows / Linux 剪贴板同步与命令行 |
| `vendor/wx-ime-sdk/` | 协议客户端及统一代理实现 |
| `packaging/windows/` | Windows 安装包、便携包脚本与使用说明 |
| `packaging/` | Linux systemd 用户服务、udev 规则与安装脚本 |
| `docs/` | 协议与逆向分析记录 |

原创代码按 MIT 发布。协议层来自 [wx-ime-sdk](https://github.com/mkdir700/wx-ime-sdk)（Apache-2.0），详见 [THIRD_PARTY.md](THIRD_PARTY.md)。
