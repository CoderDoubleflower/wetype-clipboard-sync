## Windows x86 与 HTTP/SOCKS 代理

本版为 Windows 10 及以上提供 **32 位 x86** 安装包和便携版，继续支持 Linux。

### 下载

- `wetype-clipboard-sync-0.2.0-windows-x86-setup.exe`：当前用户安装，不需要管理员权限。包含配置/配对、启动、停止快捷方式，可选择登录自启。
- `wetype-clipboard-sync-0.2.0-windows-x86-portable.zip`：解压后运行 `wxclipd.exe setup`。
- `SHA256SUMS.txt`：上述文件的 SHA-256 校验值。

### 功能

- Windows 原生 Unicode 文本剪贴板、后台启动器、日志与当前用户 Named Pipe。
- HTTP CONNECT、SOCKS5/SOCKS5h，支持用户名密码认证；控制 API、COS 和 WSS 统一使用代理，失败不自动绕过代理。
- 代理和轮询配置向导，以及 `config`、`start`、`stop` 命令。
- 本地观察与网络请求解耦；最新待发值重试；先写入后确认远端 receipt；保护网络等待期间的本地新复制。
- 保留 Linux Wayland/X11 后端与 systemd 安装。

### 开始使用

安装后打开 **Setup and Pair**，填写代理地址（无需代理可留空），然后输入手机微信输入法「跨设备粘贴」显示的六位码并在手机确认。设置完成后启动后台同步。

常见地址：`http://127.0.0.1:7890` 或 `socks5h://127.0.0.1:7890`。修改配置后重新启动同步。完整说明随安装包提供，也可阅读仓库中的 [Windows 指南](https://github.com/CoderDoubleflower/wetype-clipboard-sync/blob/main/packaging/windows/README-Windows.md)。

### 验证与边界

发布流程必须通过 Linux/Windows 测试、Windows x86 release 构建、PE32 与动态依赖检查、安装/后台停止/卸载验证才会发布。代理测试使用本地模拟服务。Windows CI 在 x64 系统上运行 x86 程序；本次自动验证未连接用户手机完成真实配对。

当前同步纯文本，单次最多 1 MiB；不提供图片/文件同步或剪贴板历史。默认 Windows 远端轮询 10 秒；Ctrl/右键触发不会拦截按键等待网络，因此同一次粘贴不保证立即得到手机最新内容。出站待发值保存在内存；发送结果不确定时需 `sync-now` 或新复制触发重试。

安装包未做商业代码签名。软件为非官方兼容实现，协议行为仍可能受官方服务更新影响。
