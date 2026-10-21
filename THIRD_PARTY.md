# 第三方组件与声明

## wx-ime-sdk（Apache License 2.0）

本仓库的协议层和客户端来自独立开源项目 wx-ime-sdk：

| 项 | 内容 |
| --- | --- |
| 来源 | https://github.com/mkdir700/wx-ime-sdk |
| 固定版本 | `1ec70393cc9524c6c341cf768f090b0176586f72` |
| 位置 | `vendor/wx-ime-sdk/` |
| 许可 | Apache-2.0，全文见 `vendor/wx-ime-sdk/LICENSE` |
| 本地改动 | HTTP/SOCKS 代理、图片字节发送、接收确认与缓存处理、网络测试，见 `vendor/wx-ime-sdk/VENDORED.md` |

Apache-2.0 是宽松许可，没有传染性：本仓库的原创代码（`wxclipd/`、`packaging/`、`docs/` 等）
按 MIT 发布，`vendor/` 下的代码仍按其 Apache-2.0 条款分发。分发本仓库时请一并保留
`LICENSE`、`THIRD_PARTY.md` 和 `vendor/wx-ime-sdk/LICENSE`。

Cargo 依赖从 crates.io 获取，各自保留其原始许可，本仓库不分发其源码。微信输入法图标属于第三方品牌资源，不包含在本项目原创代码的 MIT 授权中；来源与处理方式见 [图标说明](wxclipd/assets/README.md)。

## Windows 二进制分发

Windows 发布包静态链接 OpenSSL（Apache-2.0）以及 Cargo.lock 中锁定的依赖，包含
`licenses/` 目录中的第三方许可文件。OpenSSL 用于兼容原协议的椭圆曲线运算，TLS
连接由 Rustls 实现。Windows 剪贴板使用 arboard（MIT/Apache-2.0），系统 API
绑定使用 windows-sys（MIT/Apache-2.0），SOCKS 连接使用 tokio-socks（MIT）。
图片编解码使用 image（MIT/Apache-2.0）及其 PNG/JPEG 依赖，像素指纹使用 sha2（MIT/Apache-2.0）。

安装包和便携包均保留项目 LICENSE、THIRD_PARTY.md、协议 SDK 许可和构建时取得的
依赖许可。修改或重新分发时请保留这些文件。

## 商标与免责

微信、WeChat、微信输入法（WeType）等名称、商标与图标归腾讯及其关联方所有。项目按维护者要求使用微信输入法图标识别其兼容对象，图标权利仍归原权利人所有，不随项目代码重新许可。本项目是非官方实现，与腾讯无关联，未获其授权、赞助或背书。

使用本项目会与官方服务通信，请自行确认符合相关服务条款，并仅在本人拥有或已获授权的
设备与数据上使用。
