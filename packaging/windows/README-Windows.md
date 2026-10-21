# WeType Clipboard Sync 0.2.0 · Windows 使用说明

本程序通过手机微信输入法的「跨设备粘贴」，同步手机与 Windows 电脑的文本剪贴板。支持 **Windows 10 及以上**；发布包中的 **x86 是 32 位程序**，可在 64 位 Windows 上兼容运行。

## 安装与首次配对

从 [项目 Releases](https://github.com/CoderDoubleflower/wetype-clipboard-sync/releases) 下载以下一种包：

| 文件 | 使用方式 |
| --- | --- |
| `wetype-clipboard-sync-0.2.0-windows-x86-setup.exe` | 运行安装向导，安装到当前用户，不需要管理员权限 |
| `wetype-clipboard-sync-0.2.0-windows-x86-portable.zip` | 完整解压到固定目录，再双击 `wxclipd.exe` |

安装版默认位置为 `%LOCALAPPDATA%\Programs\WeTypeClipboardSync`。安装时的 **Start clipboard sync when I sign in** 控制登录后自启；完成页的 **Configure proxy and pair with my phone** 会打开设置向导。

设置向导依次询问：

1. **代理地址**：首次使用默认直连。回车保留当前设置，输入 `direct` 改为直连，也可输入下文列出的代理 URL。
2. **远端轮询秒数**：回车使用当前值，Windows 默认 10 秒，可设为 1～3600 秒。
3. **设备名称**：首次创建身份时询问，回车使用电脑名称。
4. **手机配对码**：在手机微信输入法中打开「跨设备粘贴」，把手机显示的六位码填入窗口，并在手机端确认。已有配对时可回车跳过。

完成后程序启动后台同步，按 Enter 关闭设置窗口即可。重新打开设置向导时，会先停止正在运行的同步进程，保存设置后再启动。

首次使用请在电脑和手机分别复制一段新的纯文本，在另一端等待同步后粘贴验证。手机复制到电脑的默认主动拉取间隔是 10 秒，实际时间还取决于网络和代理。**按下 Ctrl+V 本身不保证远端内容已经到达。**

Windows 版本尚未使用真实手机账号完成端到端实测，需要在你的手机、代理和桌面会话中验证首次配对及双向同步。

## 日常启动、停止与日志

安装版开始菜单中的 **WeType Clipboard Sync** 提供：

| 入口 | 作用 |
| --- | --- |
| **Setup and Pair** | 修改代理、轮询间隔并完成配对 |
| **Start Sync** | 启动后台同步 |
| **Stop Sync** | 停止后台同步 |
| **Sync Now** | 请求正在运行的进程尽快同步一次 |
| **Windows Guide** | 打开本说明 |
| **Uninstall** | 卸载程序 |

程序没有托盘设置界面。便携版可通过双击 `wxclipd.exe` 进入设置向导，或双击 `wxclipd-background.exe` 启动已有配置；**这两个文件必须在同一目录**。便携包不自动登记登录自启。

也可以在程序目录打开 PowerShell，运行：

```powershell
.\wxclipd.exe start
.\wxclipd.exe status
.\wxclipd.exe sync-now
.\wxclipd.exe stop
```

默认日志位于 `%LOCALAPPDATA%\wxclipd\wxclipd.log`。查看最近日志并持续跟踪：

```powershell
Get-Content "$env:LOCALAPPDATA\wxclipd\wxclipd.log" -Tail 50 -Wait
```

`start` 返回只表示启动了后台进程，网络初始化仍可能在重试；有问题时查看日志。`sync-now` 返回只表示同步请求已受理，并不是本轮收发完成的确认。

前台排错时先停止后台进程，再运行：

```powershell
.\wxclipd.exe stop
.\wxclipd.exe run
```

前台按 Ctrl+C 退出，需要恢复后台时再执行 `.\wxclipd.exe start`。

## HTTP / SOCKS5 代理

推荐通过 **Setup and Pair** 填写代理，或者使用 `config` 命令保存：

| 地址示例 | 行为 |
| --- | --- |
| `http://127.0.0.1:7890` | HTTP 代理；HTTPS 和 WSS 使用 CONNECT 隧道 |
| `socks5://127.0.0.1:1080` | SOCKS5，由本机解析目标域名 |
| `socks5h://127.0.0.1:1080` | SOCKS5，由代理解析目标域名 |
| `http://user:password@127.0.0.1:7890` | 带 HTTP Basic 用户名密码认证 |
| `socks5h://user:password@127.0.0.1:1080` | 带 SOCKS5 用户名密码认证 |

端口应填写你的代理软件实际监听端口。用户名或密码中的 `@`、`%` 等保留字符需要 URL 百分号编码，例如密码 `p@ss` 应写为 `p%40ss`。`socks5h` 仅将目标域名交给代理解析，代理服务器自身的域名仍需本机可解析。

例如，保存 SOCKS5h 代理并应用：

```powershell
.\wxclipd.exe --proxy "socks5h://127.0.0.1:1080" config
.\wxclipd.exe stop
.\wxclipd.exe start
```

恢复直连：

```powershell
.\wxclipd.exe --no-proxy config
.\wxclipd.exe stop
.\wxclipd.exe start
```

查看当前有效配置使用 `.\wxclipd.exe config`。显示时会隐藏用户名密码，但保存的代理 URL 保留原始凭据。

配置优先级是 **命令行 `--proxy` / `--no-proxy` > 环境变量 `WXCLIPD_PROXY` > `config.json` > 直连**。空的 `WXCLIPD_PROXY` 表示直连；若该变量仍存在，它会覆盖文件内的代理。普通临时命令不会更改已运行的后台进程，**保存后需要 `stop` 再 `start`**。

程序忽略 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY` 和 Windows 系统代理自动配置。代理统一用于 Session 登录 / 配对请求、COS 较长文本上传 / 下载、WSS 通知连接；代理不可用或认证失败时不会自动改为直连。暂不支持 PAC、`https://` 代理端点和 NTLM / Kerberos 等企业代理认证。

## 配置文件与状态目录

默认状态目录是 `%LOCALAPPDATA%\wxclipd`。可把此路径粘贴到资源管理器地址栏打开。配置文件为其中的 `config.json`，Windows 默认内容为：

```json
{
  "proxy": null,
  "poll_interval_seconds": 10,
  "paste_trigger": true
}
```

`proxy` 为 `null` 表示直连，也可改成代理 URL 字符串。直接编辑时必须使用有效 JSON，保存后重启同步进程。包内的 `config.example.json` 只是示例，放在程序旁边不会自动生效。

通常使用命令更方便，例如把远端轮询改成 15 秒、关闭 Ctrl / 右键触发：

```powershell
.\wxclipd.exe --poll-interval 15 --no-paste-trigger config
.\wxclipd.exe stop
.\wxclipd.exe start
```

`--paste-trigger` 可重新开启触发。本地剪贴板默认每 400 毫秒检查一次；`--interval` 和 `--paste-trigger-debounce-ms` 是前台 `run` 参数，不写入 `config.json`。

便携版默认也使用 `%LOCALAPPDATA%\wxclipd`，不会把身份和日志放到程序旁边。需要独立目录时，所有命令使用相同的 `--state`，例如：

```powershell
.\wxclipd.exe --state "D:\WeTypeData" setup
.\wxclipd.exe --state "D:\WeTypeData" stop
.\wxclipd.exe --state "D:\WeTypeData" start
```

也可以设置 `WXCLIPD_STATE`，显式 `--state` 的优先级更高。同一状态目录只能运行一个同步进程。

状态目录包含设备身份、私钥、账号 secret、接收记录，以及可能带密码的代理 URL，均以明文存储；程序通过 Windows ACL 将目录访问限制为当前用户和系统。不要分享整个目录。卸载会停止同步并删除安装文件、自启项，**保留状态目录和配对身份**。

## 配对、重试与常见问题

**如何查看设备、添加配对或解绑？**

```powershell
.\wxclipd.exe devices
.\wxclipd.exe pair 123456
.\wxclipd.exe unbind --peer UIN
```

`pair` 后在手机确认；只有一个可解绑设备时可省略 `--peer`。若让电脑显示六位码，先 `stop`，再运行 `.\wxclipd.exe code`，在手机输入该码，结束后重新 `start`。

**手机复制后电脑还没有更新。**

先确认手机和电脑已配对，检查日志中的网络或代理错误，等待一次远端轮询，或运行 `.\wxclipd.exe sync-now` 请求同步。Ctrl / 右键触发会提前拉取，但网络请求可能晚于粘贴操作完成。修改代理后要重启；如果配置显示的代理与文件不同，检查 `WXCLIPD_PROXY`。

**断网时复制的内容如何处理？**

临时网络故障会退避重试。出站只在内存中保存最新待发送文本，后续复制会替代旧内容；退出或重启会丢失待发状态。入站内容先持久化，剪贴板暂时被占用时保留并重试；此时用户新复制的本地内容优先。

如果日志显示 **`OutcomeUnknown` / 发送结果不确定**，服务端可能已收到该内容，程序暂停自动重发。需要重试时执行 `.\wxclipd.exe sync-now`，这可能重复发送；也可复制新内容继续同步。

**哪些内容会同步？**

仅非空文本，单条不超过 **1 MiB（UTF-8 字节）**。图片、文件和富文本格式暂不支持。程序默认不会上传启动前已有的剪贴板；首次验证请复制新文本。若确实要发送启动前内容，停止后台后前台运行 `.\wxclipd.exe --send-current run`。

**如何核对下载文件？**

发布页同时提供 `SHA256SUMS.txt`。PowerShell 可计算下载文件的校验值，与对应行比较：

```powershell
Get-FileHash .\wetype-clipboard-sync-0.2.0-windows-x86-setup.exe -Algorithm SHA256
```

本项目是非官方协议实现，与腾讯无关联。更多源码、Linux 使用说明和许可证见 [项目仓库](https://github.com/CoderDoubleflower/wetype-clipboard-sync) 与包内 `THIRD_PARTY.md`。
