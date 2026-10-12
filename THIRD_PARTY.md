# 第三方组件与声明

## wx-ime-sdk（Apache License 2.0）

本仓库的协议层和客户端来自独立开源项目 wx-ime-sdk：

| 项 | 内容 |
| --- | --- |
| 来源 | https://github.com/mkdir700/wx-ime-sdk |
| 固定版本 | `1ec70393cc9524c6c341cf768f090b0176586f72` |
| 位置 | `vendor/wx-ime-sdk/` |
| 许可 | Apache-2.0，全文见 `vendor/wx-ime-sdk/LICENSE` |
| 本地改动 | 无，见 `vendor/wx-ime-sdk/VENDORED.md` |

Apache-2.0 是宽松许可，没有传染性：本仓库的原创代码（`wxclipd/`、`packaging/`、`docs/` 等）
按 MIT 发布，`vendor/` 下的代码仍按其 Apache-2.0 条款分发。分发本仓库时请一并保留
`LICENSE`、`THIRD_PARTY.md` 和 `vendor/wx-ime-sdk/LICENSE`。

Cargo 依赖从 crates.io 获取，各自保留其原始许可，本仓库不分发其源码。

## 商标与免责

微信、WeChat、微信输入法（WeType）等名称与商标归腾讯及其关联方所有，此处仅用于说明
兼容对象。本项目是非官方实现，与腾讯无关联，未获其授权、赞助或背书。

使用本项目会与官方服务通信，请自行确认符合相关服务条款，并仅在本人拥有或已获授权的
设备与数据上使用。
