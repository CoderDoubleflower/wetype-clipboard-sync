# 项目信息

## 这是什么

Linux 端微信输入法跨设备剪贴板同步。目标：和手机上的官方微信输入法配对后，
两端剪贴板内容互通。

## 技术栈

- 语言：Rust（edition 2021，rustc 1.92+）
- 构建：Cargo；依赖系统 OpenSSL（需要 secp128r1 曲线）、protoc、pkg-config
- 协议层：`vendor/wx-ime-sdk`（第三方独立实现，Apache-2.0，已固定 commit）
- 剪贴板：Wayland 用 `wl-clipboard`，X11 用 `xclip`
- 服务：systemd user unit（`packaging/wxclipd.service`）

## 做什么

- 用配对码加入官方输入法的设备组（配对码可由任一端生成）。
- 双向同步文本剪贴板。
- 保持实现可直接运行、可诊断：未配对、网络失败、内容过期都要给出明确提示。

## 不做什么

- 不修改、不重打包官方 App，不使用微信账号密码或登录态。
- 不做云盘中转、不做剪贴板历史管理。
- 不在未获得授权的情况下使用他人设备或数据。

## 已知边界

- 只同步文本；图片、文件传输协议已还原但未接入。
- 官方协议不对外公开，可能随版本变化失效。
- 推送长连接未实现，改用 2 秒轮询拉取（实现简单，代价是延迟与流量）。

## 许可

- 原创代码：MIT，见 `LICENSE`。
- `vendor/wx-ime-sdk/`：Apache-2.0（非传染性），见 `THIRD_PARTY.md`。
