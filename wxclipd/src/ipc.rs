//! A local, per-user control channel shared by the foreground CLI and daemon.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use windows as platform;

pub use platform::{endpoint, listen, prepare_state_dir, Listener};
#[cfg(windows)]
pub use windows::{begin_setup, setup_in_progress};

pub trait ControlStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ControlStream for T {}
pub type Stream = Box<dyn ControlStream>;

const MAX_REPLY_BYTES: u64 = 1024 * 1024;

/// None means no daemon is listening. Permission errors and a busy listener
/// are errors, not permission to open the same identity in another process.
pub async fn call(state: &Path, command: &str) -> Result<Option<String>> {
    if command.is_empty()
        || command.len() > 255
        || command
            .bytes()
            .any(|byte| matches!(byte, b'\r' | b'\n' | 0))
    {
        bail!("控制命令必须是 1–255 字节的单行文本");
    }
    let path = endpoint(state)?;
    let connection = tokio::time::timeout(Duration::from_secs(2), platform::connect(&path))
        .await
        .context("连接正在运行的 wxclipd 超时")??;
    let Some(mut stream) = connection else {
        return Ok(None);
    };
    let reply = tokio::time::timeout(Duration::from_secs(130), async {
        stream.write_all(command.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        let mut reader = BufReader::new(stream).take(MAX_REPLY_BYTES + 1);
        let mut reply = String::new();
        reader.read_line(&mut reply).await?;
        if reply.len() as u64 > MAX_REPLY_BYTES {
            bail!("wxclipd 控制响应超过大小限制");
        }
        if !reply.ends_with('\n') {
            bail!("wxclipd 控制连接在返回完整响应前关闭");
        }
        Ok::<_, anyhow::Error>(reply)
    })
    .await
    .context("等待 wxclipd 控制响应超时")??;
    if let Some(error) = reply.trim().strip_prefix("err ") {
        bail!("{error}");
    }
    Ok(Some(reply.trim().to_owned()))
}

/// Stop the daemon and wait until it has released its executable. On Windows
/// an open process handle is acquired before sending stop, so an installer
/// cannot race cleanup or accidentally wait on a later process reusing its PID.
pub async fn stop_and_wait(state: &Path) -> Result<bool> {
    let reply = tokio::time::timeout(Duration::from_secs(2), call(state, "ping"))
        .await
        .context("查询 wxclipd 进程超时")??;
    let Some(reply) = reply else {
        return Ok(false);
    };
    let pid: u32 = reply
        .strip_prefix("ok ")
        .context("后台未返回有效进程 ID，不能确认它已退出")?
        .trim()
        .parse()
        .context("后台进程 ID 无效")?;
    if pid == 0 || pid == std::process::id() {
        bail!("后台返回了无效进程 ID");
    }
    let Some(process) = platform::watch_process(pid)? else {
        // The observed daemon exited between ping and acquiring the handle.
        return Ok(true);
    };
    let request = tokio::time::timeout(Duration::from_secs(2), call(state, "stop"))
        .await
        .context("发送停止命令超时")
        .and_then(|result| result);
    // Even if the response connection closed during shutdown, a signalled
    // process handle proves that stopping succeeded.
    match platform::wait_for_exit(process, state).await {
        Ok(()) => Ok(true),
        Err(wait_error) => {
            request.context("无法通知后台停止")?;
            Err(wait_error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir()
                .join(format!("wxclipd-ipc-test-{}-{stamp}", std::process::id()));
            prepare_state_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn local_control_roundtrip_and_duplicate_listener_rejected() {
        let directory = TestDirectory::new();
        assert!(call(&directory.0, "ping").await.unwrap().is_none());
        assert!(!stop_and_wait(&directory.0).await.unwrap());
        let path = endpoint(&directory.0).unwrap();
        let mut listener = listen(&path).unwrap();
        assert!(listen(&path).is_err());
        let server = tokio::spawn(async move {
            // On Unix the duplicate-listener probe opened a connection and
            // closed without sending a command. Ignore that empty connection.
            loop {
                let stream = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut command = String::new();
                if reader.read_line(&mut command).await.unwrap() == 0 {
                    continue;
                }
                assert_eq!(command, "ping\n");
                let mut stream = reader.into_inner();
                stream.write_all("ok 中文🙂\n".as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
                break;
            }
        });
        assert_eq!(
            call(&directory.0, "ping").await.unwrap().as_deref(),
            Some("ok 中文🙂")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn control_rejects_embedded_commands() {
        let directory = TestDirectory::new();
        assert!(call(&directory.0, "ping\nstop").await.is_err());
        assert!(call(&directory.0, "").await.is_err());
    }
}
