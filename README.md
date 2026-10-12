# wetype-clipboard-sync-linux

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
- 同一个状态目录只允许一个进程；服务在跑时直接执行 `wxclipd devices` 会提示身份被占用。
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
