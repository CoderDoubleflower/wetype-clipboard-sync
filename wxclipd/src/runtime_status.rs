//! Local, credential-free status for a tray or diagnostic client.
//!
//! Reading this snapshot never takes an SDK/session lock or makes a request.
//! Keep error text from a fixed vocabulary: SDK errors can contain addresses,
//! paths or protocol data and do not belong in a desktop tooltip.

use crate::config::Config;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wx_ime_sdk::{Error, ProxyConfig};

#[derive(Clone)]
pub struct RuntimeStatus(Arc<Mutex<Snapshot>>);

struct Snapshot {
    phase: &'static str,
    started_at: u64,
    device_name: String,
    proxy: String,
    proxy_source: &'static str,
    poll_interval_seconds: u64,
    paste_trigger: bool,
    local_poll_interval_ms: u64,
    last_success_at: Option<u64>,
    last_send_at: Option<u64>,
    last_receive_at: Option<u64>,
    sent_count: u64,
    received_count: u64,
    pending_send: bool,
    send_paused: bool,
    network_error: Option<&'static str>,
    local_errors: [Option<&'static str>; 5],
}

#[derive(Clone, Copy)]
pub enum LocalOperation {
    Read = 0,
    Write = 1,
    Prepare = 2,
    Acknowledge = 3,
    Discard = 4,
}

impl RuntimeStatus {
    pub fn new(
        config: &Config,
        proxy_source: &'static str,
        name: String,
        local_interval: Duration,
    ) -> Result<Self> {
        let proxy = config
            .proxy
            .as_deref()
            .map(ProxyConfig::parse)
            .transpose()?
            .map(|proxy| proxy.to_string())
            .unwrap_or_else(|| "直连".into());
        Ok(Self(Arc::new(Mutex::new(Snapshot {
            phase: "initializing",
            started_at: now(),
            device_name: name,
            proxy,
            proxy_source,
            poll_interval_seconds: config.poll_interval_seconds,
            paste_trigger: config.paste_trigger,
            local_poll_interval_ms: local_interval.as_millis().min(u64::MAX as u128) as u64,
            last_success_at: None,
            last_send_at: None,
            last_receive_at: None,
            sent_count: 0,
            received_count: 0,
            pending_send: false,
            send_paused: false,
            network_error: None,
            local_errors: [None; 5],
        }))))
    }

    fn lock(&self) -> MutexGuard<'_, Snapshot> {
        // A poisoned diagnostics mutex must not bring down clipboard delivery.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn value(&self) -> Value {
        let state = self.lock();
        let last_error = state
            .local_errors
            .iter()
            .flatten()
            .next()
            .copied()
            .or(state.network_error);
        let phase = if state.phase != "stopping" && last_error.is_some() {
            "retrying"
        } else {
            state.phase
        };
        json!({
            "schema": 1,
            "pid": std::process::id(),
            "phase": phase,
            "started_at": state.started_at,
            "device_name": state.device_name,
            "proxy": state.proxy,
            "proxy_source": state.proxy_source,
            "poll_interval_seconds": state.poll_interval_seconds,
            "paste_trigger": state.paste_trigger,
            "local_poll_interval_ms": state.local_poll_interval_ms,
            "last_success_at": state.last_success_at,
            "last_send_at": state.last_send_at,
            "last_receive_at": state.last_receive_at,
            "sent_count": state.sent_count,
            "received_count": state.received_count,
            "pending_send": state.pending_send,
            "send_paused": state.send_paused,
            "last_error": last_error,
        })
    }

    pub fn reply(&self) -> String {
        format!("ok {}\n", self.value())
    }

    pub fn initialized(&self, name: String) {
        let mut state = self.lock();
        state.device_name = name;
        state.phase = "running";
        state.network_error = None;
        // Running means initialization completed, not that a subsequent poll
        // or transfer has succeeded. Its timestamp remains empty until then.
    }

    pub fn stopping(&self) {
        self.lock().phase = "stopping";
    }

    pub fn network_failed(&self, error: &Error) {
        self.lock().network_error = Some(match error {
            Error::NotPaired => "尚未配对，请打开设置与配对。",
            Error::OutcomeUnknown => "发送结果不确定，已暂停重发；请查看日志后决定是否立即同步。",
            Error::Proxy(_) | Error::ProxyHttp(_) => "代理连接或认证失败，请检查当前代理设置。",
            Error::Transport | Error::CosTransport | Error::Timeout(_) => {
                "网络请求失败或超时，正在等待重试。"
            }
            Error::Http(_) | Error::CosHttp(_) => "服务器暂未接受请求，请检查网络并查看日志。",
            Error::InvalidClipboard { .. } => "收到的图片无效或超过限制，正在跳过此版本。",
            _ => "同步请求失败，请查看日志；必要时修正设置后立即同步。",
        });
    }

    pub fn network_succeeded(&self) {
        let mut state = self.lock();
        state.network_error = None;
        state.last_success_at = Some(now());
    }

    pub fn sent(&self) {
        let mut state = self.lock();
        let timestamp = now();
        state.network_error = None;
        state.last_success_at = Some(timestamp);
        state.last_send_at = Some(timestamp);
        state.sent_count = state.sent_count.saturating_add(1);
    }

    pub fn received(&self) {
        let mut state = self.lock();
        state.local_errors[LocalOperation::Write as usize] = None;
        state.last_receive_at = Some(now());
        state.received_count = state.received_count.saturating_add(1);
    }

    pub fn queue(&self, pending: bool, paused: bool) {
        let mut state = self.lock();
        state.pending_send = pending;
        state.send_paused = paused;
    }

    pub fn local_result(&self, operation: LocalOperation, success: bool) {
        self.lock().local_errors[operation as usize] = if success {
            None
        } else {
            Some(match operation {
                LocalOperation::Read => "暂时无法读取剪贴板，正在重试。",
                LocalOperation::Write => "剪贴板写入失败，保留收到的内容并等待重试。",
                LocalOperation::Prepare => "暂时无法解码已下载的图片，保留内容并等待重试。",
                LocalOperation::Acknowledge => "保存接收确认失败，正在重试；不会重复写入剪贴板。",
                LocalOperation::Discard => "保存拒收记录失败，正在重试此版本。",
            })
        };
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> RuntimeStatus {
        RuntimeStatus::new(
            &Config::default(),
            "direct",
            "test-pc".into(),
            Duration::from_millis(400),
        )
        .unwrap()
    }

    #[test]
    fn snapshot_never_exposes_proxy_credentials_or_raw_error_data() {
        let config = Config {
            proxy: Some("socks5h://private-user:p%40ssword@localhost:1080".into()),
            ..Config::default()
        };
        let status = RuntimeStatus::new(
            &config,
            "config",
            "test-pc".into(),
            Duration::from_millis(700),
        )
        .unwrap();
        status.network_failed(&Error::Proxy("secret-data-in-provider-error"));
        let reply = status.reply();
        for private in [
            "private-user",
            "p%40ssword",
            "p@ssword",
            "secret-data-in-provider-error",
        ] {
            assert!(!reply.contains(private), "status leaked {private}");
        }
        let value = status.value();
        assert_eq!(value["proxy"], "socks5h://localhost:1080");
        assert_eq!(value["local_poll_interval_ms"], 700);
        assert_eq!(value["phase"], "retrying");
    }

    #[test]
    fn status_tracks_real_success_local_failures_and_paused_outbox_separately() {
        let status = status();
        assert_eq!(status.value()["phase"], "initializing");
        status.network_failed(&Error::Transport);
        assert_eq!(status.value()["phase"], "retrying");
        status.initialized("registered-name".into());
        assert_eq!(status.value()["phase"], "running");
        assert!(status.value()["last_success_at"].is_null());
        status.local_result(LocalOperation::Write, false);
        status.network_succeeded();
        assert_eq!(
            status.value()["phase"],
            "retrying",
            "successful polling must not hide a pending local failure"
        );
        status.received();
        status.sent();
        assert_eq!(status.value()["phase"], "running");
        assert_eq!(status.value()["sent_count"], 1);
        assert_eq!(status.value()["received_count"], 1);
        status.network_failed(&Error::OutcomeUnknown);
        status.queue(true, true);
        assert_eq!(status.value()["send_paused"], true);
        assert_eq!(status.value()["pending_send"], true);
        status.stopping();
        assert_eq!(status.value()["phase"], "stopping");
    }
}
