# wx-ime-sdk（vendored）

本目录保留 Windows 剪贴板同步应用所依赖的协议层和异步客户端，来源于
[mkdir700/wx-ime-sdk](https://github.com/mkdir700/wx-ime-sdk)，固定上游提交为
`1ec70393cc9524c6c341cf768f090b0176586f72`。

| 路径 | 用途 |
| --- | --- |
| `crates/wx-ime-protocol/` | 包编解码、控制通道加密、配对码及 protobuf 消息 |
| `crates/wx-ime-sdk/` | 设备身份、配对、剪贴板收发、代理和通知连接 |

SDK 不读写宿主机剪贴板；Windows 剪贴板、托盘、配置和安装器由仓库中的
`wxclipd/` 与 `packaging/windows/` 提供。本地副本已移除上游独立 `wxc` CLI、
npm 分发脚本和嵌套发布工作流，使用与安装请从 [项目首页](../../README.md) 开始。

## 开发与接口

Windows MSVC x86 工具链、构建和测试命令见 [开发说明](../../DEVELOPMENT.md)。
产品使用 `wxclipd/Cargo.lock` 锁定实际交付依赖，协议库仍保留独立 Cargo workspace。

- [异步 SDK 接口说明](crates/wx-ime-sdk/README.md)
- [协议层说明](crates/wx-ime-protocol/README.md)
- [上游版本与本地补丁记录](VENDORED.md)

SDK 中的存储权限、符号链接检查和持久化保障沿用上游平台条件分支；产品仅构建和发布 Windows 版本。

## 许可与来源

本目录代码继续按 [Apache License 2.0](LICENSE) 分发。本地改动记录在
[VENDORED.md](VENDORED.md)，其余第三方声明见仓库根目录的
[THIRD_PARTY.md](../../THIRD_PARTY.md)。修改或重新分发时请保留这些许可与来源信息。

本项目是非官方协议兼容实现，与腾讯无关联，未获其授权、赞助或背书。
微信、WeChat、微信输入法（WeType）等名称和商标归其权利人所有。
