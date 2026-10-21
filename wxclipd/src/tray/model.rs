//! Presentation data shared by the UI and its local-control worker. No secrets
//! or clipboard content are copied into this view.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub const MAX_LAUNCH_BYTES: usize = 16 * 1024;

#[derive(Clone, Default)]
pub struct LaunchContext {
    // Some("") is an explicit environment override selecting a direct route.
    pub proxy_override: Option<String>,
    pub name: Option<String>,
    pub setup_complete: bool,
}

impl LaunchContext {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(
            &json!({"proxy": self.proxy_override, "name": self.name, "setup_complete": self.setup_complete}),
        )?;
        if bytes.len() > MAX_LAUNCH_BYTES {
            bail!("启动配置超过大小限制");
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_LAUNCH_BYTES {
            bail!("启动配置超过大小限制");
        }
        let value: Value = serde_json::from_slice(bytes).context("启动配置格式错误")?;
        let object = value.as_object().context("启动配置必须为对象")?;
        let string = |key: &str| -> Result<Option<String>> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(value)) if !value.contains('\0') => Ok(Some(value.clone())),
                _ => bail!("启动配置字段无效"),
            }
        };
        let proxy_override = string("proxy")?;
        if let Some(proxy) = &proxy_override {
            if !proxy.is_empty() {
                wx_ime_sdk::ProxyConfig::parse(proxy).context("启动代理配置无效")?;
            }
        }
        let setup_complete = match object.get("setup_complete") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => bail!("启动配置字段无效"),
        };
        Ok(Self {
            proxy_override,
            name: string("name")?,
            setup_complete,
        })
    }
}

#[derive(Clone)]
pub struct View {
    pub runtime: Option<Value>,
    pub busy: bool,
    pub operation: Option<String>,
    pub notice: Option<String>,
    pub saved_config: String,
    pub ready: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            runtime: None,
            busy: false,
            operation: None,
            notice: None,
            saved_config: "正在读取配置……".to_owned(),
            ready: false,
        }
    }
}

impl View {
    pub fn running(&self) -> bool {
        self.runtime.as_ref().is_some_and(|value| {
            matches!(
                value["phase"].as_str(),
                Some(
                    "initializing" | "running" | "retrying" | "stopping" | "legacy" | "unavailable"
                )
            )
        })
    }

    pub fn phase_label(&self) -> &'static str {
        if self.busy {
            return "正在处理";
        }
        let Some(value) = &self.runtime else {
            return if self.ready {
                "状态暂不可用"
            } else {
                "正在启动"
            };
        };
        if value["send_paused"].as_bool() == Some(true) {
            return "发送已暂停，等待手动处理";
        }
        match value["phase"].as_str() {
            Some("initializing") => "正在连接服务",
            Some("running") => "同步运行中",
            Some("retrying") => "连接或同步异常，正在重试",
            Some("stopping") => "正在停止",
            Some("stopped") => "同步已停止",
            Some("legacy") => "旧版后台正在运行，请重启同步",
            Some("unavailable") => "后台存在，详细状态暂不可用",
            _ => "状态暂不可用",
        }
    }

    pub fn status_text(&self, time: impl Fn(Option<u64>) -> String) -> String {
        let mut lines = vec![format!("状态：{}", self.phase_label())];
        if let Some(operation) = &self.operation {
            lines.push(operation.clone());
        }
        if let Some(value) = &self.runtime {
            if let Some(pid) = value["pid"].as_u64() {
                lines.push(format!("后台进程：{pid}"));
            }
            if value["phase"] != "stopped" {
                lines.push(format!(
                    "设备：{}",
                    value["device_name"].as_str().unwrap_or("正在读取")
                ));
                lines.push(format!("启动时间：{}", time(value["started_at"].as_u64())));
                lines.push(format!(
                    "最近成功操作：{}",
                    time(value["last_success_at"].as_u64())
                ));
                lines.push(format!(
                    "最近发送：{}    本次启动已发送 {} 条",
                    time(value["last_send_at"].as_u64()),
                    value["sent_count"].as_u64().unwrap_or(0)
                ));
                lines.push(format!(
                    "最近接收：{}    本次启动已接收 {} 条",
                    time(value["last_receive_at"].as_u64()),
                    value["received_count"].as_u64().unwrap_or(0)
                ));
                lines.push(format!(
                    "待发送：{}",
                    if value["pending_send"].as_bool() == Some(true) {
                        "有最新内容等待发送"
                    } else {
                        "无"
                    }
                ));
                if value["send_paused"].as_bool() == Some(true) {
                    lines.push(
                        "点击「立即同步」可重试；若提示发送结果不确定，重试可能重复发送。"
                            .to_owned(),
                    );
                }
                if let Some(error) = value["last_error"].as_str() {
                    lines.push(format!("最近异常：{error}"));
                }
            }
        }
        if let Some(notice) = &self.notice {
            lines.push(format!("提示：{notice}"));
        }
        lines.join("\r\n")
    }

    pub fn config_text(&self, state: &str) -> String {
        let mut lines = Vec::new();
        if let Some(value) = self.runtime.as_ref().filter(|v| v["proxy"].is_string()) {
            lines.push("【运行中的实际配置】".to_owned());
            // Redact again at the presentation boundary, including credentials
            // from an older/custom daemon. Invalid URLs are never echoed.
            lines.push(format!("代理：{}", safe_proxy(value["proxy"].as_str())));
            let source = match value["proxy_source"].as_str() {
                Some("command") => "命令行",
                Some("environment") => "环境变量 WXCLIPD_PROXY",
                Some("config") => "config.json",
                Some("direct") => "默认直连",
                _ => "后台配置",
            };
            lines.push(format!("代理来源：{source}"));
            lines.push(format!(
                "远端轮询：{} 秒；本地检查：{} 毫秒",
                value["poll_interval_seconds"].as_u64().unwrap_or(10),
                value["local_poll_interval_ms"].as_u64().unwrap_or(400)
            ));
            lines.push(format!(
                "Ctrl / 右键触发：{}",
                if value["paste_trigger"].as_bool() == Some(true) {
                    "开启"
                } else {
                    "关闭"
                }
            ));
            lines.push(String::new());
        }
        lines.push("【已保存的配置】".to_owned());
        lines.push(self.saved_config.clone());
        lines.push("保存后点击「重启同步」应用；环境变量可能覆盖文件内的代理。".to_owned());
        lines.push(String::new());
        lines.push(format!("状态目录：{state}"));
        lines.push("图片与文本同步：已启用；图片文件需使用「复制图片」或截图。".to_owned());
        lines.join("\r\n")
    }
}

pub fn safe_proxy(proxy: Option<&str>) -> String {
    match proxy {
        None | Some("" | "直连" | "direct") => "直连".to_owned(),
        Some(value) => wx_ime_sdk::ProxyConfig::parse(value)
            .map(|proxy| proxy.to_string())
            .unwrap_or_else(|_| "配置无效，请检查代理设置（凭据已隐藏）".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_context_distinguishes_direct_environment_and_saved_config() {
        for proxy in [
            None,
            Some("".to_owned()),
            Some("socks5h://user:p%40ss@localhost:1080".to_owned()),
        ] {
            let context = LaunchContext {
                proxy_override: proxy.clone(),
                name: Some("电脑🙂".to_owned()),
                setup_complete: true,
            };
            let decoded = LaunchContext::decode(&context.encode().unwrap()).unwrap();
            assert_eq!(decoded.proxy_override, proxy);
            assert_eq!(decoded.name.as_deref(), Some("电脑🙂"));
            assert!(decoded.setup_complete);
        }
        assert!(LaunchContext::decode(br#"{"proxy":true}"#).is_err());
        assert!(LaunchContext::decode(br#"{"name":"x\u0000y"}"#).is_err());
        assert!(LaunchContext::decode(br#"{"proxy":"https://proxy.example:443"}"#).is_err());
        assert!(LaunchContext::decode(&vec![b' '; MAX_LAUNCH_BYTES + 1]).is_err());
    }

    #[test]
    fn configuration_view_never_displays_proxy_credentials() {
        let view = View {
            runtime: Some(
                json!({"phase":"running", "proxy":"socks5h://private-user:private-password@localhost:1080"}),
            ),
            ..View::default()
        };
        let text = view.config_text("C:\\Test");
        assert!(text.contains("socks5h://localhost:1080"));
        assert!(!text.contains("private-user"));
        assert!(!text.contains("private-password"));
        assert!(!safe_proxy(Some("invalid private-secret")).contains("private-secret"));
    }

    #[test]
    fn unavailable_or_retrying_status_is_never_reported_as_running() {
        let mut view = View {
            ready: true,
            ..View::default()
        };
        assert_eq!(view.phase_label(), "状态暂不可用");
        assert!(!view.running());
        view.runtime = Some(json!({"phase":"retrying", "pending_send":true}));
        assert!(view.running());
        assert_eq!(view.phase_label(), "连接或同步异常，正在重试");
        view.runtime = Some(json!({"phase":"running", "send_paused":true}));
        assert!(view.phase_label().contains("等待手动处理"));
        view.runtime = Some(json!({"phase":"legacy", "pid":123}));
        assert!(view.running());
        view.runtime = Some(json!({"phase":"stopped"}));
        assert!(!view.running());
        assert_eq!(view.phase_label(), "同步已停止");
    }
}
