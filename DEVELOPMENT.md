# 开发说明

## 环境

```sh
sudo apt install protobuf-compiler libssl-dev pkg-config wl-clipboard
cargo --version   # 需要 1.92 或更新
```

OpenSSL 必须支持 `secp128r1`（Debian 13 自带 3.5.x 支持）。自带的
`openssl ecparam -list_curves | grep secp128r1` 能确认。

## 构建与测试

```sh
cd wxclipd
cargo build --release -j 3
cargo test -j 3
```

协议层测试在 vendor 目录里，另行执行：

```sh
cd vendor/wx-ime-sdk && cargo test --workspace -j 3
```

## 目录

| 路径 | 说明 |
| --- | --- |
| `wxclipd/src/main.rs` | 命令行：`code`/`pair`/`devices`/`status`/`run` |
| `wxclipd/src/clipboard.rs` | 桌面剪贴板后端（Wayland/X11） |
| `wxclipd/src/sync.rs` | 同步主循环与去重 |
| `vendor/wx-ime-sdk/` | 协议层；改动要记进 `VENDORED.md` |
| `docs/` | 逆向证据与协议速览 |
| `reverse/` | 样本与反编译产物，体积大，不要提交 |

## 协议调试

- 抓包与字段含义见 `docs/protocol.md`。
- SDK 支持把请求/响应落盘（`ClientOptions.capture_dir`），需要时在
  `wxclipd` 里打开；产物含明文剪贴板内容，注意清理。
- 请求头、签名 salt、端点等常量可以在 `reverse/work/so/libime_net.so`
  里用 `strings` 复核。

## 状态目录

默认 `~/.local/state/wxclipd`，包含 `identity.json`（私钥、uin、服务端公钥）、
`identity.lock`、已消费游标和图片缓存。测试时用 `--state /tmp/...` 隔离。
同一个状态目录同一时间只能有一个进程（SDK 会加锁）。

## 控制套接字

`wxclipd run` 会在状态目录下创建 `control.sock`（0600），接受单行命令：

| 命令 | 作用 |
| --- | --- |
| `sync` | 立即检查本地剪贴板并轮询一次远端 |
| `devices` | 返回本机与已绑定设备的 JSON |
| `status` | 返回本机与组内剪贴板状态的 JSON |
| `pair <code>` | 以加入方身份提交配对码，等对方确认后返回结果 |
| `unbind <uin>` | 解绑指定设备（命令 35） |

因此 `wxclipd devices` / `status` / `pair` / `unbind` / `sync-now` 在常驻服务运行时也能用，
不需要先停服务；这些子命令优先走套接字，连不上才自己打开身份文件。
`code` 需要边显示配对码边等用户确认，仍在本地执行，要先停服务。

## 轮询与带宽

- 本地剪贴板默认 400 ms 检查一次（`--interval`），只在内容变化时才发网络请求。
- 远端轮询自适应：在场（`loginctl` 的 `IdleHint`/`LockedHint` 都是 no）每 5 秒
  （`--active-poll-interval`），离开或锁屏 60 秒（`--poll-interval`）。请求携带本地已有版本号，
  服务端会省略未变化的剪贴板内容，实测单次响应约 156 字节；
  不携带版本号时响应会包含整段剪贴板文本。
- 拿到会话状态需要 `XDG_SESSION_ID` 与 `loginctl`；取不到时按“在场”处理（优先延迟）。
- `cargo run --example probe_group -- ~/.local/state/wxclipd [测试文本]` 可复现这个对比。
  它会在状态目录下写 `capture-*`（含明文响应），用完自行删除。
- 推送长连接实现见 `vendor/wx-ime-sdk/crates/wx-ime-sdk/src/notify.rs`；
  `cargo run -p wx-ime-sdk --example notify_probe -- <状态目录> <设备名> 60` 可单独观察事件。
  任何推送都会让守护进程立刻拉取一次剪贴板，轮询只是兜底。

## 发布

`packaging/install.sh` 把 release 二进制装到 `~/.local/bin/wxclipd` 并登记
systemd user unit。日志：`journalctl --user -u wxclipd -f`。

## 复现 APK 分析

APK 与反编译产物因版权和体积不随仓库分发，`reverse/` 目录被 `.gitignore` 忽略
（只保留说明文件）。复现步骤：

1. 自备微信输入法 APK（本项目基于 4.0.0、versionCode 57201）放到 `reverse/`。
2. 反编译源码与资源：jadx 1.5.6（`--no-res`）、apktool 2.12.0（`d -s`）。
3. 反编译 React Native 设置页：把 `assets/*.hbc` 交给 hermes-dec。
4. 用 `strings`、`readelf` 检查 native 库，重点 `libime_net.so`（HTTP 头、签名 salt）、
   `libwetap_business.so`（端点、取号）、`libcryptopp.so`（secp128r1）。
5. 结论与证据位置见 `docs/reverse-notes.md`、`docs/cmd-table.md`、`docs/protocol.md`。

## 持续集成

`.github/workflows/ci.yml` 在 Ubuntu 上安装 `protobuf-compiler`、`libssl-dev`、`pkg-config`，
构建 `wxclipd` 并运行协议层测试。本机可用同样的命令预先验证：

```sh
cargo build --release --manifest-path wxclipd/Cargo.toml -j 3
cargo test --workspace --manifest-path vendor/wx-ime-sdk/Cargo.toml -j 3
```
