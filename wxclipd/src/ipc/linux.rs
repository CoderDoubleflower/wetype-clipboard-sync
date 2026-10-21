use super::Stream;
use anyhow::{bail, Context, Result};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};

pub fn endpoint(state: &Path) -> Result<PathBuf> {
    Ok(state.join("control.sock"))
}

pub fn prepare_state_dir(state: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(state)
        .with_context(|| format!("创建状态目录 {} 失败", state.display()))?;
    std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("保护状态目录 {} 失败", state.display()))
}

pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

pub fn listen(path: &Path) -> Result<Listener> {
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => bail!("wxclipd 已在运行（{}）", path.display()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                std::fs::remove_file(path)
                    .with_context(|| format!("清理失效的控制套接字 {} 失败", path.display()))?;
            }
            Err(error) => return Err(error).context("检查现有 wxclipd 控制套接字失败"),
        }
    }
    let inner = UnixListener::bind(path)
        .with_context(|| format!("绑定控制套接字 {} 失败", path.display()))?;
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(error).context("保护控制套接字失败");
    }
    Ok(Listener {
        inner,
        path: path.to_owned(),
    })
}

impl Listener {
    pub async fn accept(&mut self) -> Result<Stream> {
        let (stream, _) = self.inner.accept().await?;
        Ok(Box::new(stream))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) async fn connect(path: &Path) -> Result<Option<Stream>> {
    match UnixStream::connect(path).await {
        Ok(stream) => Ok(Some(Box::new(stream))),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("连接 wxclipd 控制套接字失败"),
    }
}

pub(super) struct ProcessWaiter;

pub(super) fn watch_process(_pid: u32) -> Result<Option<ProcessWaiter>> {
    Ok(Some(ProcessWaiter))
}

pub(super) async fn wait_for_exit(_process: ProcessWaiter, state: &Path) -> Result<()> {
    let path = endpoint(state)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if connect(&path).await?.is_none() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("等待 wxclipd 关闭控制端点超时（10 秒）")?
}
