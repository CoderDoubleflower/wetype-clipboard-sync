# wetype-clipboard-sync-linux

[![CI](https://github.com/YuleBest/wetype-clipboard-sync-linux/actions/workflows/ci.yml/badge.svg)](https://github.com/YuleBest/wetype-clipboard-sync-linux/actions/workflows/ci.yml)

在 Linux 上使用微信输入法（WeType）的跨设备剪贴板：手机复制，Linux 直接粘贴；
Linux 复制，手机也能收到。

微信输入法没有 Linux 版。本项目让 Linux 以一个全新设备身份加入官方的"跨设备粘贴"
设备组，再负责读写 Linux 桌面剪贴板，官方 App 不需要做任何改动。

已与官方 Android 版实测双向互通（2026-10-12）。

## 安装

先装依赖，再构建并安装为常驻服务：

```sh
sudo apt install protobuf-compiler libssl-dev pkg-config
sudo apt install wl-clipboard        # Wayland；X11 换成 xclip

git clone https://github.com/YuleBest/wetype-clipboard-sync-linux
cd wetype-clipboard-sync-linux/wxclipd
cargo build --release -j 3

cd .. && ./packaging/install.sh
systemctl --user enable --now wxclipd
```

## 配对

在手机的微信输入法里打开"跨设备粘贴"，然后二选一：

```sh
wxclipd pair 123456     # 手机显示配对码，Linux 输入
wxclipd code            # Linux 显示配对码，手机输入
```

手机上确认后即配对完成，服务会自动开始同步，可以用手机和 Linux 互发一段文字验证。

查看和解绑设备：

```sh
wxclipd devices             # 列出已绑定设备
wxclipd unbind              # 只有一台时直接解绑
wxclipd unbind --peer UIN   # 指定要解绑的 Uin
```

## 按需同步

服务端推送走 WebSocket 长连接（`wxws_pb` 子协议）：连接建立后先做一次连接级 ECDH，
再订阅通知事件；收到任何推送就立刻拉取一次剪贴板。推送不可用时自动退回 60 秒轮询，
轮询请求会带上本地已有的剪贴板版本号，服务端据此省略没变化的内容
（实测单次响应约 156 字节，不会重复下发整段剪贴板文字）。

想立刻把手机刚复制的内容拉过来：

```sh
wxclipd sync-now
```

在 KDE 里可以绑成全局快捷键：系统设置 → 快捷键 → 添加命令 → `~/.local/bin/wxclipd sync-now`。

## 日常管理

```sh
systemctl --user status wxclipd          # 运行状态
journalctl --user -u wxclipd -f          # 实时日志
systemctl --user restart wxclipd         # 重启
systemctl --user disable --now wxclipd   # 停用并取消自启
```

## 说明

- 目前只同步文本；图片与文件互传的协议已还原在 `docs/`，但未实现。
- 身份、私钥和消费游标保存在 `~/.local/state/wxclipd`，属于敏感数据，请勿外传。
- 同一个状态目录只允许一个进程；`devices`、`status`、`pair`、`unbind`、`sync-now`
  会通过控制套接字交给常驻进程执行，不需要先停服务。只有 `code`（要一边显示配对码
  一边等交互确认）需要先 `systemctl --user stop wxclipd`。
- 非官方项目，与腾讯无关联，协议可能随官方更新失效。

## 目录

| 路径 | 内容 |
| --- | --- |
| `wxclipd/` | Linux 剪贴板守护进程与配对命令行 |
| `vendor/wx-ime-sdk/` | 协议层与客户端（独立开源实现，Apache-2.0） |
| `docs/` | 逆向记录、协议速览、命令表 |
| `packaging/` | systemd 用户服务与安装脚本 |
| `reverse/` | 本地 APK 分析目录，不分发 |

## 许可

原创代码按 MIT 发布。协议层来自 [wx-ime-sdk](https://github.com/mkdir700/wx-ime-sdk)
（Apache-2.0），详见 [THIRD_PARTY.md](THIRD_PARTY.md)。
