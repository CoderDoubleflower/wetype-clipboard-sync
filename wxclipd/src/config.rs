//! Persistent user settings. Credentials are never included in diagnostics.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Config {
    pub proxy: Option<String>,
    pub poll_interval_seconds: u64,
    pub paste_trigger: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: None,
            poll_interval_seconds: if cfg!(windows) { 10 } else { 90 },
            paste_trigger: true,
        }
    }
}

pub fn path(state: &Path) -> PathBuf {
    state.join("config.json")
}

impl Config {
    pub fn load(state: &Path) -> Result<Self> {
        Self::load_with_proxy_override(state, None)
    }

    /// Resolve the selected proxy before validating the saved value, so a valid
    /// command-line override can repair an invalid proxy without resetting the
    /// user's unrelated settings. An explicit None selects a direct connection.
    pub fn load_with_proxy_override(
        state: &Path,
        proxy_override: Option<&Option<String>>,
    ) -> Result<Self> {
        let mut value = match std::fs::read(path(state)) {
            Ok(bytes) => {
                if bytes.len() > 16 * 1024 {
                    bail!("config.json 超过大小限制");
                }
                serde_json::from_slice::<Value>(&bytes).context("config.json 不是有效的 JSON")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                json!({})
            }
            Err(error) => return Err(error).context("读取 config.json 失败"),
        };
        if let Some(proxy) = proxy_override {
            value
                .as_object_mut()
                .context("config.json 必须是 JSON 对象")?
                .insert("proxy".to_owned(), json!(proxy));
        }
        Self::from_value(&value)
    }

    fn from_value(value: &Value) -> Result<Self> {
        let object = value.as_object().context("config.json 必须是 JSON 对象")?;
        let mut config = Self::default();
        if let Some(value) = object.get("proxy") {
            config.proxy = match value {
                Value::Null => None,
                Value::String(value) if value.trim().is_empty() => None,
                Value::String(value) => Some(value.trim().to_owned()),
                _ => bail!("config.json 的 proxy 必须是字符串或 null"),
            };
        }
        if let Some(value) = object.get("poll_interval_seconds") {
            config.poll_interval_seconds = value
                .as_u64()
                .filter(|n| (1..=3600).contains(n))
                .context("poll_interval_seconds 必须是 1..3600 的整数")?;
        }
        if let Some(value) = object.get("paste_trigger") {
            config.paste_trigger = value
                .as_bool()
                .context("paste_trigger 必须为 true 或 false")?;
        }
        if let Some(proxy) = &config.proxy {
            wx_ime_sdk::ProxyConfig::parse(proxy).context("config.json 的代理配置无效")?;
        }
        Ok(config)
    }

    pub fn save(&self, state: &Path) -> Result<()> {
        crate::ipc::prepare_state_dir(state)?;
        let mut file = tempfile::NamedTempFile::new_in(state).context("创建临时配置失败")?;
        let value = json!({
            "proxy": self.proxy,
            "poll_interval_seconds": self.poll_interval_seconds,
            "paste_trigger": self.paste_trigger,
        });
        serde_json::to_writer_pretty(&mut file, &value)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(path(state))
            .map_err(|error| error.error)
            .context("保存 config.json 失败")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_intervals_and_types() {
        for value in [
            json!({"poll_interval_seconds": 0}),
            json!({"poll_interval_seconds": 3601}),
            json!({"poll_interval_seconds": "10"}),
            json!({"paste_trigger": "false"}),
            json!({"proxy": true}),
            json!([]),
        ] {
            assert!(Config::from_value(&value).is_err());
        }
    }

    #[test]
    fn accepts_explicit_direct_and_proxy_settings() {
        assert!(Config::from_value(&json!({"proxy": null}))
            .unwrap()
            .proxy
            .is_none());
        let config = Config::from_value(&json!({
            "proxy": "socks5h://127.0.0.1:1080",
            "poll_interval_seconds": 15,
            "paste_trigger": false
        }))
        .unwrap();
        assert_eq!(config.poll_interval_seconds, 15);
        assert!(!config.paste_trigger);
        assert_eq!(config.proxy.as_deref(), Some("socks5h://127.0.0.1:1080"));
    }
}
