use super::Stream;
use anyhow::{bail, Context, Result};
use std::io::ErrorKind;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_INVALID_PARAMETER, ERROR_PIPE_BUSY, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

pub fn endpoint(state: &Path) -> Result<PathBuf> {
    let sid = current_user_sid()?;
    let absolute = std::path::absolute(state).context("解析状态目录绝对路径失败")?;
    // Fixed FNV-1a, independent of Rust's DefaultHasher and of pointer width.
    // The user SID separates users; the normalized path separates identities.
    let normalized = absolute.to_string_lossy().to_lowercase();
    let hash = normalized
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    Ok(PathBuf::from(format!(
        r"\\.\pipe\wxclipd-{sid}-{hash:016x}"
    )))
}

pub fn prepare_state_dir(state: &Path) -> Result<()> {
    std::fs::create_dir_all(state)
        .with_context(|| format!("创建状态目录 {} 失败", state.display()))?;
    let sid = current_user_sid()?;
    // Protect the DACL from broader inherited access. Child files/directories
    // inherit access only for this user and the operating system.
    let descriptor = SecurityDescriptor::new(&format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)"))?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted) }
        == 0
        || present == 0
        || dacl.is_null()
    {
        bail!("无法构建状态目录访问权限");
    }
    let path: Vec<u16> = state
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::from_raw_os_error(result as i32))
            .context("设置状态目录权限失败");
    }
    Ok(())
}

pub struct Listener {
    next: NamedPipeServer,
    path: PathBuf,
    sid: String,
}

pub fn listen(path: &Path) -> Result<Listener> {
    let sid = current_user_sid()?;
    let next = create_pipe(path, &sid, true)?;
    Ok(Listener {
        next,
        path: path.to_owned(),
        sid,
    })
}

impl Listener {
    pub async fn accept(&mut self) -> Result<Stream> {
        self.next.connect().await?;
        // Keep a server instance listening before handing this connection to
        // its task, so subsequent clients never see a gap in pipe ownership.
        let replacement = create_pipe(&self.path, &self.sid, false)?;
        Ok(Box::new(std::mem::replace(&mut self.next, replacement)))
    }
}

fn create_pipe(path: &Path, sid: &str, first: bool) -> Result<NamedPipeServer> {
    let descriptor = SecurityDescriptor::new(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let pipe = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
    .with_context(|| {
        format!(
            "创建用户控制管道 {} 失败（可能已有 wxclipd 运行）",
            path.display()
        )
    })?;
    Ok(pipe)
}

pub(super) async fn connect(path: &Path) -> Result<Option<Stream>> {
    loop {
        match ClientOptions::new().open(path) {
            Ok(stream) => return Ok(Some(Box::new(stream))),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // The caller imposes a two-second connection timeout.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error).context("连接用户控制管道失败"),
        }
    }
}

pub(super) struct ProcessWaiter(OwnedHandle);

pub(super) fn watch_process(pid: u32) -> Result<Option<ProcessWaiter>> {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(error).context("无法取得后台进程的退出通知句柄");
    }
    Ok(Some(ProcessWaiter(unsafe {
        OwnedHandle::from_raw_handle(handle)
    })))
}

pub(super) async fn wait_for_exit(process: ProcessWaiter, _state: &Path) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        match unsafe { WaitForSingleObject(process.0.as_raw_handle(), 10_000) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!("等待 wxclipd 完全退出超时（10 秒），程序文件可能仍被占用"),
            _ => Err(std::io::Error::last_os_error()).context("等待 wxclipd 退出失败"),
        }
    })
    .await
    .context("等待后台进程的工作线程异常")?
}

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn new(sddl: &str) -> Result<Self> {
        let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("构建用户访问权限失败");
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

fn current_user_sid() -> Result<String> {
    let mut handle = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) } == 0 {
        return Err(std::io::Error::last_os_error()).context("读取当前用户身份失败");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut bytes = 0u32;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
    }
    if bytes == 0 {
        return Err(std::io::Error::last_os_error()).context("读取用户 SID 长度失败");
    }
    // TOKEN_USER contains pointers. A u8 Vec would not guarantee its alignment,
    // especially when the same implementation is built for i686 and x86_64.
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("读取用户 SID 失败");
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut string = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut string) } == 0 {
        return Err(std::io::Error::last_os_error()).context("转换用户 SID 失败");
    }
    let mut length = 0;
    unsafe {
        while *string.add(length) != 0 {
            length += 1;
        }
    }
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) });
    unsafe { LocalFree(string.cast()) };
    sid.context("用户 SID 编码无效")
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{GetAce, ACCESS_ALLOWED_ACE, ACL, PSID};

    #[tokio::test]
    async fn native_pipe_dacl_has_only_current_user_and_system() {
        let state = std::env::temp_dir().join(format!("wxclipd-acl-test-{}", std::process::id()));
        let path = endpoint(&state).unwrap();
        let listener = listen(&path).unwrap();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let result = unsafe {
            GetSecurityInfo(
                listener.next.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(result, 0);
        let _descriptor = SecurityDescriptor(descriptor);
        assert!(!dacl.is_null());
        assert_eq!(unsafe { (*dacl).AceCount }, 2);
        let mut sids = Vec::new();
        for index in 0..2 {
            let mut ace = std::ptr::null_mut();
            assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
            let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(allowed.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
            let sid = &allowed.SidStart as *const u32 as PSID;
            let mut string = std::ptr::null_mut();
            assert_ne!(unsafe { ConvertSidToStringSidW(sid, &mut string) }, 0);
            let mut length = 0;
            unsafe {
                while *string.add(length) != 0 {
                    length += 1;
                }
            }
            sids.push(
                String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) }).unwrap(),
            );
            unsafe { LocalFree(string.cast()) };
        }
        assert!(sids.contains(&current_user_sid().unwrap()));
        assert!(sids.contains(&"S-1-5-18".to_owned()));
    }
}
