# 本地分析目录

这里放微信输入法 APK 和反编译产物，**不随仓库分发**：APK 是腾讯的版权作品，
产物体积也很大。仓库的 `.gitignore` 会忽略本目录除本文件以外的所有内容。

要复现分析：

1. 自备微信输入法 APK，放到本目录。
2. 用 jadx 反编译源码、apktool 解包资源与 manifest。
3. 用 hermes-dec 反编译 `assets/*.hbc`（React Native 设置页）。
4. 对照 `docs/reverse-notes.md`（结论与证据位置）、`docs/cmd-table.md`（业务命令表）。

详细步骤见 `DEVELOPMENT.md`。
