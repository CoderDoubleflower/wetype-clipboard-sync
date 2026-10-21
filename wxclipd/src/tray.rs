//! Windows notification-area controller. WPF owns the presentation window;
//! separate workers own local IPC, its private bridge, and process lifecycle.

mod worker;
mod wpf;

use crate::ipc;
use crate::tray_model::{LaunchContext, View, MAX_LAUNCH_BYTES};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HWND, LPARAM, LRESULT, POINT, SYSTEMTIME,
    WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, ReleaseMutex, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CONTROLLER_CLASS: &str = "WeTypeClipboardSyncTray";
const WM_TRAY: u32 = WM_APP + 1;
const WM_FINISHED: u32 = WM_APP + 2;
// Kept stable for installer automation and regression tests.
const WM_QUIT_TRAY: u32 = WM_APP + 3;
const WM_SHOW_STATUS: u32 = WM_APP + 4;
const WM_START_SYNC: u32 = WM_APP + 5;
const WM_UI_ERROR: u32 = WM_APP + 6;
const COPY_LAUNCH: usize = 0x5743_4353;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;
const ID_SHOW: usize = 1001;
const ID_START: usize = 1002;
const ID_STOP: usize = 1003;
const ID_RESTART: usize = 1004;
const ID_SYNC: usize = 1005;
const ID_SETUP: usize = 1006;
const ID_LOG: usize = 1007;
const ID_FOLDER: usize = 1008;
const ID_EXIT: usize = 1009;

type SharedView = Arc<Mutex<View>>;
enum Action {
    Launch(LaunchContext),
    Start,
    Stop,
    Restart,
    Sync,
    Setup,
    Exit,
}

struct Args {
    state: PathBuf,
    quit: bool,
    show: bool,
}

impl Args {
    fn parse(tokens: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut args = tokens.into_iter();
        let mut state = None;
        let mut quit = false;
        let mut show = false;
        while let Some(arg) = args.next() {
            if arg == "--state" && state.is_none() {
                state = Some(PathBuf::from(args.next().context("--state 缺少目录")?));
            } else if arg == "--quit" && !quit {
                quit = true;
            } else if arg == "--show" && !show {
                show = true;
            } else {
                bail!("用法：wxclipd-background.exe [--state DIR] [--show | --quit]");
            }
        }
        if quit && show {
            bail!("--show 与 --quit 不能同时使用");
        }
        let state = state
            .or_else(|| std::env::var_os("WXCLIPD_STATE").map(PathBuf::from))
            .map(Ok)
            .unwrap_or_else(|| {
                std::env::var_os("LOCALAPPDATA")
                    .context("LOCALAPPDATA 未设置")
                    .map(|path| PathBuf::from(path).join("wxclipd"))
            })?;
        if state.as_os_str().is_empty() {
            bail!("状态目录不能为空");
        }
        Ok(Self {
            state: std::path::absolute(state)?,
            quit,
            show,
        })
    }
}

struct Instance {
    handle: OwnedHandle,
    owner: bool,
}
impl Drop for Instance {
    fn drop(&mut self) {
        if self.owner {
            unsafe {
                ReleaseMutex(self.handle.as_raw_handle());
            }
        }
    }
}

pub fn run() -> Result<()> {
    let args = Args::parse(std::env::args_os().skip(1))?;
    let endpoint = ipc::endpoint(&args.state)?;
    let digest = Sha256::digest(
        endpoint
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    let key = format!("WeTypeClipboardTray-{digest:x}");
    let mutex_name = wide(&format!("Local\\{key}"));
    let handle = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("创建托盘实例锁失败");
    }
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let _instance = Instance {
        handle: unsafe { OwnedHandle::from_raw_handle(handle) },
        owner: !exists,
    };
    if exists {
        let window = find_instance(&key)?;
        if args.quit {
            return request_exit(window);
        }
        let context = launch_context();
        let bytes = context.encode()?;
        let data = COPYDATASTRUCT {
            dwData: COPY_LAUNCH,
            cbData: bytes.len() as u32,
            lpData: bytes.as_ptr() as _,
        };
        let mut accepted = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                window,
                WM_COPYDATA,
                0,
                &data as *const _ as isize,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                3000,
                &mut accepted,
            )
        };
        if sent == 0 || accepted != 1 {
            bail!("现有托盘暂时无法接收启动请求，请稍后重试");
        }
        unsafe {
            // Transfer foreground eligibility from this user-launched process
            // to the existing tray, which will grant it to its WPF child.
            let mut owner = 0;
            GetWindowThreadProcessId(window, &mut owner);
            if owner != 0 {
                AllowSetForegroundWindow(owner);
            }
            PostMessageW(window, WM_SHOW_STATUS, 0, 0);
        }
        return Ok(());
    }
    if args.quit {
        if ipc::setup_in_progress(&args.state)? {
            bail!("请先完成或关闭设置与配对窗口，再退出或卸载");
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(ipc::stop_and_wait(&args.state))?;
        return Ok(());
    }
    ipc::prepare_state_dir(&args.state)?;
    run_ui(args, &key)
}

fn launch_context() -> LaunchContext {
    LaunchContext {
        proxy_override: std::env::var("WXCLIPD_PROXY").ok(),
        name: std::env::var("WXCLIPD_NAME").ok(),
        setup_complete: std::env::var("WXCLIPD_SETUP_COMPLETE").as_deref() == Ok("1"),
    }
}

fn find_instance(key: &str) -> Result<HWND> {
    let class = wide(CONTROLLER_CLASS);
    let title = wide(key);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let window = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
        if !window.is_null() {
            return Ok(window);
        }
        if Instant::now() >= deadline {
            bail!("托盘实例正在启动或退出，请稍后再试");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn request_exit(window: HWND) -> Result<()> {
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut pid);
    }
    if pid == 0 || pid == std::process::id() {
        bail!("无法确认托盘进程");
    }
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error()).context("无法等待托盘退出");
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if unsafe { PostMessageW(window, WM_QUIT_TRAY, 0, 0) } == 0 {
        return Err(std::io::Error::last_os_error()).context("无法通知托盘退出");
    }
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 30_000) } != WAIT_OBJECT_0 {
        bail!("托盘未能在 30 秒内退出，后台可能仍在运行");
    }
    Ok(())
}

struct Ui {
    state: PathBuf,
    controller: Cell<HWND>,
    icon_small: HICON,
    icon_large: HICON,
    taskbar_created: u32,
    icon_added: Cell<bool>,
    view: SharedView,
    sender: SyncSender<Action>,
    display: RefCell<Option<wpf::Bridge>>,
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            if !self.icon_small.is_null() {
                DestroyIcon(self.icon_small);
            }
            if !self.icon_large.is_null() {
                DestroyIcon(self.icon_large);
            }
        }
    }
}

fn run_ui(args: Args, key: &str) -> Result<()> {
    // Construct fallible runtime state before any window references Ui. A
    // runtime initialization failure must not leave an unclosable tray behind.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let instance = unsafe { GetModuleHandleW(null()) };
    let load_icon = |size: i32| unsafe {
        LoadImageW(
            instance,
            101usize as _,
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        ) as HICON
    };
    let small = load_icon(unsafe { GetSystemMetrics(SM_CXSMICON) });
    let large = load_icon(unsafe { GetSystemMetrics(SM_CXICON) });
    if small.is_null() || large.is_null() {
        unsafe {
            if !small.is_null() {
                DestroyIcon(small);
            }
            if !large.is_null() {
                DestroyIcon(large);
            }
        }
        bail!("程序图标资源缺失，请重新安装完整安装包");
    }
    let (sender, receiver) = sync_channel(8);
    let mut ui = Box::new(Ui {
        state: args.state.clone(),
        controller: Cell::new(null_mut()),
        icon_small: small,
        icon_large: large,
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        icon_added: Cell::new(false),
        view: Arc::new(Mutex::new(View::default())),
        sender,
        display: RefCell::new(None),
    });
    let classes: [(&str, WNDPROC); 1] = [(CONTROLLER_CLASS, Some(controller_proc))];
    for (name, procedure) in classes {
        let name = wide(name);
        let class = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: procedure,
            hInstance: instance,
            hIcon: large,
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            lpszClassName: name.as_ptr(),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(std::io::Error::last_os_error()).context("注册托盘窗口失败");
        }
    }
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            wide(CONTROLLER_CLASS).as_ptr(),
            wide(key).as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            (&mut *ui as *mut Ui).cast(),
        )
    };
    if window.is_null() {
        return Err(std::io::Error::last_os_error()).context("创建托盘窗口失败");
    }
    ui.controller.set(window);
    match wpf::Bridge::new(window as usize, ui.view.clone()) {
        Ok(bridge) => *ui.display.borrow_mut() = Some(bridge),
        Err(error) => {
            unsafe {
                DestroyWindow(window);
            }
            return Err(error);
        }
    }
    add_icon(&ui);
    unsafe {
        SetTimer(window, 1, 1000, None);
    }
    let state = args.state;
    let initial_context = launch_context();
    let context = initial_context.clone();
    let view = ui.view.clone();
    let worker_window = window as usize;
    let worker = match std::thread::Builder::new()
        .name("wxclipd-tray-control".to_owned())
        .spawn(move || {
            worker::run(runtime, state, context, view, receiver, worker_window);
        }) {
        Ok(worker) => worker,
        Err(error) => {
            // Destroy native windows before dropping the data referenced by
            // GWLP_USERDATA. The caller may show a modal error message.
            delete_icon(&ui);
            unsafe {
                KillTimer(window, 1);
                DestroyWindow(window);
            }
            return Err(error).context("创建托盘控制线程失败");
        }
    };
    send_action(&ui, Action::Launch(initial_context));
    if args.show || !ui.state.join("identity.json").exists() {
        show_status(&ui);
    }
    let mut message = MSG::default();
    let mut message_error = None;
    loop {
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == 0 {
            break;
        }
        if result == -1 {
            message_error = Some(std::io::Error::last_os_error());
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // Join the presentation bridge and close its owned WPF child before the
    // launcher returns to an installer or uninstaller.
    ui.display.borrow_mut().take();
    delete_icon(&ui);
    unsafe {
        if IsWindow(window) != 0 {
            DestroyWindow(window);
        }
    }
    drop(ui);
    let _ = worker.join();
    if let Some(error) = message_error {
        return Err(error).context("托盘消息循环失败");
    }
    Ok(())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn notification(ui: &Ui) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: ui.controller.get(),
        uID: 1,
        ..Default::default()
    }
}

fn add_icon(ui: &Ui) {
    let mut data = notification(ui);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    data.hIcon = ui.icon_small;
    data.uCallbackMessage = WM_TRAY;
    let mut tip = [0u16; 128];
    fill_tip(&mut tip, "WeType 剪贴板同步 — 正在启动");
    data.szTip = tip;
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } != 0
        || unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } != 0
    {
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
        ui.icon_added.set(true);
    }
}

fn delete_icon(ui: &Ui) {
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &notification(ui));
    }
    ui.icon_added.set(false);
}

fn fill_tip(buffer: &mut [u16; 128], text: &str) {
    let mut index = 0;
    for character in text.chars() {
        let mut encoded = [0u16; 2];
        let units = character.encode_utf16(&mut encoded);
        if index + units.len() >= buffer.len() {
            break;
        }
        buffer[index..index + units.len()].copy_from_slice(units);
        index += units.len();
    }
}

fn refresh(ui: &Ui) {
    if !ui.icon_added.get() {
        add_icon(ui);
    }
    let view = ui.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut data = notification(ui);
    let mut tip = [0u16; 128];
    fill_tip(
        &mut tip,
        &format!("WeType 剪贴板同步 — {}", view.phase_label()),
    );
    data.szTip = tip;
    data.uFlags = NIF_TIP | NIF_SHOWTIP;
    if ui.icon_added.get() && unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
        ui.icon_added.set(false);
    }
    if let Some(bridge) = ui.display.borrow().as_ref() {
        if let Ok(snapshot) = view.ui_snapshot(&ui.state.display().to_string(), local_time) {
            bridge.update(snapshot);
        }
    }
}

fn local_time(seconds: Option<u64>) -> String {
    let Some(ticks) = seconds
        .and_then(|s| s.checked_add(11_644_473_600))
        .and_then(|s| s.checked_mul(10_000_000))
    else {
        return "暂无".to_owned();
    };
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    if unsafe { FileTimeToSystemTime(&file, &mut utc) } == 0
        || unsafe { SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) } == 0
    {
        return "暂无".to_owned();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

fn send_action(ui: &Ui, action: Action) {
    {
        let mut view = ui.view.lock().unwrap_or_else(|e| e.into_inner());
        if !view.busy || matches!(&action, Action::Exit) {
            // Set busy while holding the same mutex the worker uses. A fast
            // completed operation cannot be overwritten by a late GUI update.
            let previous_busy = view.busy;
            let previous_operation = view.operation.clone();
            view.busy = true;
            view.operation = Some("正在处理，请稍候……".to_owned());
            if ui.sender.try_send(action).is_err() {
                view.busy = previous_busy;
                view.operation = previous_operation;
                view.notice = Some("控制线程暂时不可用，请稍后重试。".to_owned());
            }
        }
    }
    refresh(ui);
}

fn command(ui: &Ui, id: usize) {
    match id {
        ID_SHOW => show_status(ui),
        ID_START => send_action(ui, Action::Start),
        ID_STOP => send_action(ui, Action::Stop),
        ID_RESTART => send_action(ui, Action::Restart),
        ID_SYNC => send_action(ui, Action::Sync),
        ID_SETUP => send_action(ui, Action::Setup),
        ID_EXIT => send_action(ui, Action::Exit),
        ID_LOG | ID_FOLDER => {
            let result = if id == ID_LOG {
                std::process::Command::new("notepad.exe")
                    .arg(ui.state.join("wxclipd.log"))
                    .spawn()
            } else {
                std::process::Command::new("explorer.exe")
                    .arg(&ui.state)
                    .spawn()
            };
            if result.is_err() {
                ui.view.lock().unwrap_or_else(|e| e.into_inner()).notice =
                    Some("无法打开文件或目录，请检查程序权限。".to_owned());
                show_status(ui);
            }
        }
        _ => {}
    }
}

fn show_menu(ui: &Ui) {
    let view = ui.view.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    unsafe {
        AppendMenuW(
            menu,
            MF_STRING | MF_DISABLED,
            0,
            wide(view.phase_label()).as_ptr(),
        );
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
    }
    for (id, label, enabled) in [
        (ID_SHOW, "状态与配置…", true),
        (ID_START, "开始同步", !view.running() && !view.busy),
        (ID_STOP, "停止同步", view.running() && !view.busy),
        (ID_RESTART, "重启同步（应用配置）", !view.busy),
        (ID_SYNC, "立即同步", view.running() && !view.busy),
        (ID_SETUP, "设置与配对…", !view.busy),
        (ID_LOG, "查看日志", true),
        (ID_FOLDER, "打开配置目录", true),
        (ID_EXIT, "退出（停止同步）", true),
    ] {
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING | if enabled { 0 } else { MF_GRAYED },
                id,
                wide(label).as_ptr(),
            );
        }
    }
    let mut position = POINT::default();
    unsafe {
        GetCursorPos(&mut position);
        SetForegroundWindow(ui.controller.get());
        SetMenuDefaultItem(menu, ID_SHOW as u32, 0);
        let selected = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            position.x,
            position.y,
            0,
            ui.controller.get(),
            null(),
        );
        DestroyMenu(menu);
        PostMessageW(ui.controller.get(), WM_NULL, 0, 0);
        Shell_NotifyIconW(NIM_SETFOCUS, &notification(ui));
        if selected != 0 {
            command(ui, selected as usize);
        }
    }
}

unsafe extern "system" fn controller_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as _);
        // DefWindowProc stores CREATESTRUCT.lpszName as the window title. Merely
        // returning TRUE leaves it empty and prevents a second process from
        // finding this controller by its per-state instance title.
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Ui;
    if pointer.is_null() {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    // Shared references plus Cells permit synchronous Win32 callbacks without
    // holding an exclusive Rust borrow across a reentrant window message.
    let ui = &*pointer;
    if message == ui.taskbar_created && message != 0 {
        ui.icon_added.set(false);
        add_icon(ui);
        return 0;
    }
    match message {
        WM_TIMER => {
            refresh(ui);
            0
        }
        WM_TRAY => {
            match lparam as u32 & 0xffff {
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(ui),
                WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT => show_status(ui),
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            command(ui, wparam & 0xffff);
            0
        }
        WM_SHOW_STATUS => {
            show_status(ui);
            0
        }
        WM_START_SYNC => {
            send_action(ui, Action::Start);
            0
        }
        WM_UI_ERROR => {
            // A missing/broken companion must be visible even when no WPF
            // window could be created; the tray and synchronization stay alive.
            MessageBoxW(
                window,
                wide("无法打开 WPF 界面。请重新安装完整安装包，或完整解压便携包（包含 ui 目录）。")
                    .as_ptr(),
                wide("WeType 剪贴板同步").as_ptr(),
                MB_OK | MB_ICONERROR,
            );
            0
        }
        WM_QUIT_TRAY | WM_CLOSE => {
            send_action(ui, Action::Exit);
            0
        }
        WM_COPYDATA => {
            if lparam == 0 {
                return 0;
            }
            let data = &*(lparam as *const COPYDATASTRUCT);
            if data.dwData != COPY_LAUNCH
                || data.cbData == 0
                || data.cbData as usize > MAX_LAUNCH_BYTES
                || data.lpData.is_null()
            {
                return 0;
            }
            // Copy/parse before returning: Windows owns this transient buffer.
            let bytes = std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize);
            match LaunchContext::decode(bytes) {
                Ok(context) => isize::from(ui.sender.try_send(Action::Launch(context)).is_ok()),
                Err(_) => 0,
            }
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                delete_icon(ui);
                send_action(ui, Action::Exit);
            }
            0
        }
        WM_FINISHED => {
            delete_icon(ui);
            if let Some(bridge) = ui.display.borrow().as_ref() {
                bridge.request_shutdown();
            }
            DestroyWindow(window);
            0
        }
        WM_DESTROY => {
            KillTimer(window, 1);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            DefWindowProcW(window, message, wparam, lparam)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

fn show_status(ui: &Ui) {
    refresh(ui);
    if let Some(bridge) = ui.display.borrow().as_ref() {
        bridge.show();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_creation_preserves_titles_and_instance_discovery() {
        // Exercise the production window procedure without starting a worker
        // or touching the user's state. A null Ui pointer deliberately delegates
        // subsequent messages to DefWindowProc; WM_NCCREATE still takes exactly
        // the production path that used to discard the title.
        let procedures: [WNDPROC; 1] = [Some(controller_proc)];
        for (index, procedure) in procedures.into_iter().enumerate() {
            let instance = unsafe { GetModuleHandleW(null()) };
            let class_name = wide(&format!(
                "WxclipdTrayTitleTest-{}-{index}",
                std::process::id()
            ));
            let class = WNDCLASSW {
                lpfnWndProc: procedure,
                hInstance: instance,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };
            assert_ne!(unsafe { RegisterClassW(&class) }, 0);
            struct TestWindow {
                window: HWND,
                class_name: Vec<u16>,
            }
            impl Drop for TestWindow {
                fn drop(&mut self) {
                    unsafe {
                        if !self.window.is_null() {
                            DestroyWindow(self.window);
                        }
                        UnregisterClassW(self.class_name.as_ptr(), GetModuleHandleW(null()));
                    }
                }
            }
            let title = "WeType 状态与配置 — 单实例测试🙂";
            let expected = wide(title);
            let window = TestWindow {
                window: unsafe {
                    CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        class_name.as_ptr(),
                        expected.as_ptr(),
                        WS_OVERLAPPEDWINDOW,
                        0,
                        0,
                        400,
                        300,
                        null_mut(),
                        null_mut(),
                        instance,
                        null(),
                    )
                },
                class_name,
            };
            assert!(!window.window.is_null());
            let mut actual = [0u16; 128];
            let count =
                unsafe { GetWindowTextW(window.window, actual.as_mut_ptr(), actual.len() as i32) };
            assert_eq!(
                String::from_utf16(&actual[..count as usize]).unwrap(),
                title
            );
            assert_eq!(
                unsafe { FindWindowW(window.class_name.as_ptr(), expected.as_ptr()) },
                window.window
            );
        }
    }

    #[test]
    fn tray_arguments_and_utf16_tooltip_have_strict_boundaries() {
        assert!(Args::parse([OsString::from("--quit"), OsString::from("--show")]).is_err());
        assert!(Args::parse([OsString::from("--state")]).is_err());
        assert!(Args::parse([OsString::from("--state"), OsString::new()]).is_err());
        let mut tip = [0; 128];
        fill_tip(&mut tip, &"🙂".repeat(100));
        let end = tip.iter().position(|unit| *unit == 0).unwrap();
        assert_eq!(end, 126);
        assert!(String::from_utf16(&tip[..end]).is_ok());
    }
}
