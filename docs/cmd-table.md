# 业务命令表（cmdId 4814）

三个来源互相独立，结论一致：

1. **官方 App 自己的常量表**：Hermes 包 `assets/general.hbc` 反编译产物
   `reverse/work/hbc_out/general.js`（函数 4113 附近的 `CMDID_TRANSER` 表）。
2. **反编译的 Java 调用点**：`reverse/work/jadx/sources/com/tencent/wetype/...`。
3. **线上实测**：本仓库的 `wxclipd` 用这些命令完成了取号、配对、收发。

## 常量表（来自官方 JS）

| 值 | 官方常量名 | 说明 |
| --- | --- | --- |
| 1 | CMDID_BUSLOG | 日志上报 |
| 5/6 | CMDID_GETIMEDICTLISTCONFIG / GETIMENEWHOTDICTCONFIG | 词库 |
| 7 | CMDID_GETSEARCHRESULT | 搜索 |
| 8 | CMDID_GETDEVICEBENCHMARK | 设备基准 |
| 10/11 | CMDID_CHATLIST / CMDID_SENDCHAT | AI 对话 |
| 12 | CMDID_APPLYCOSTOKEN | 申请 token |
| 13 | CMDID_GETKBFEATURES | 功能开关（键盘） |
| 14 | CMDID_SYNCCHAT | 聊天同步 |
| 15/16/17/18/19 | CMDID_GETCOMMANDSBYUIN / UPDATECOMMANDSWITHUIN / SEARCH / BATCH... / UPDATEFULLCOMMANDS | 命令（快捷指令） |
| 20 | CMDID_DICTPULL | 词库拉取 |
| 21 | CMDID_REACTNATIVE_PATCH | RN 包更新 |
| 23 | CMDID_GET_TOKEN | 取 token |
| 25 | CMDID_GET_WXTOKEN | 取微信 token |
| 26 | CMDID_SAMPLE_INFO | 采样上报 |
| 27 | CMDID_AUTOREPLY | 自动回复 |
| 30 | CMDID_SYNC_GET_MATCH_CODE | 生成配对码 |
| 31 | CMDID_SYNC_BIND_MATCH_CODE | 提交配对码 |
| 32 | CMDID_SYNC_REAL_BIND_MATCH_CODE | 确认绑定 |
| 33 | CMDID_SYNC_RELEASE_MATCH_CODE_OR_BIND | 释放配对码 / 取消绑定 |
| 34 | CMDID_SYNC_GET_GROUP_INFO | 拉取设备组（含剪贴板） |
| 35 | CMDID_SYNC_UNBIND | 解绑设备，`UnbindReq{uin=1, platform=2, unbindUin=3}`，响应无字段（已实现） |
| 38 | CMDID_SYNC_FETCH_BIND_STATUS | 查询绑定状态 |
| 39 | CMDID_SYNC_SET_GROUP_FUNC_SWITCH | 设置组功能开关 |
| 40 | CMDID_SYNC_GET_MATCH_CODE_STATUS | 轮询配对码状态 |
| 79 | CMDID_SYNC_UPDATE_DEVICE_INFO | 更新设备信息 |
| 120/121 | CMDID_SYNC_GET_AI_MULTI_ROUNDS / GET_AI_SUGGEST_QUESTION | AI |

Java 侧另行确认了 4814/36 = `SendClipboardMsgReq`（`candidate/a0.java:1419`）、
4814/60 = COS 上传凭证（`cos/f.java:119`）、4814/73–80 = `WtP2PTransfer` 文件互传
（`ui/filetransfer/X.java`）、4814/95 = `GetUserToken`（`thirdparty/c.java:376`）。

## 配对码时序

```
出码端                        服务端                        加入端
  |-- 30 GetMatchCode --------->|                              |
  |<-- code=123456 -------------|                              |
  |                             |<--- 31 BindMatchCode(code) --|
  |                             |---- bind_id -------------->  |
  |-- 40 PollCodeStatus ------->|                              |
  |<-- state=1, bind_id --------|                              |
  |-- 32 ConfirmBind ---------->|                              |
  |<-- group_id, peer ----------|----- group 建立 ----------->|
  |-- 34 GetGroupInfo --------->|<--- 34 GetGroupInfo --------|
```

状态枚举（`pairing.rs` 与实测一致）：`state 0=等待`、`1=对方已提交待确认`、`2=配对码不可用/已释放`；
绑定状态 `0=待处理、1=完成、2=取消`。

## 组功能开关

官方 `FunctionSwitch`（`GetGroupInfo.java:1215`，与 JS 一致）：

| 值 | 名称 |
| --- | --- |
| 0 | FunctionSwitchClose |
| 1 | FunctionSwitchClipboard |
| 2 | FunctionSwitchUserHotWords |
| 4 | FunctionSwitchUserDict |
| 8 | FunctionSwitchVoiceRecap |

`SetGroupFuncSwitchReq{uin, platform, groupId, funcSwitch}` 由官方设置页“跨设备粘贴”开关触发。
官方发送剪贴板前会检查组里是否已开启 Clipboard 位；本项目客户端不做该检查，
但正式使用前建议在手机端把“跨设备粘贴/剪贴板同步”打开。

`GroupRequest.filter` 位（官方 JS 的 `Filter` 枚举）：`0=None`、`1=BoundList`、`2=Clipboard`、
`4=UserHotWords`、`8=UserDict`，另有 `64` 用于取待处理传输邀请。
