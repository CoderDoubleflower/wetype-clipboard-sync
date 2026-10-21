//! Native Windows notification-area UI. The GUI thread owns windows and icons;
//! a separate worker owns all blocking IPC and process lifecycle operations.

mod worker;

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
    GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME,
    WAIT_OBJECT_0, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, COLOR_BTNFACE, DEFAULT_CHARSET, FW_NORMAL, HFONT,
};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, ReleaseMutex, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETFOCUS, NIM_SETVERSION, NINF_KEY, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CONTROLLER_CLASS: &str = "WeTypeClipboardSyncTray";
const STATUS_CLASS: &str = "WeTypeClipboardSyncStatus";
const WM_TRAY: u32 = WM_APP + 1;
const WM_FINISHED: u32 = WM_APP + 2;
// Kept stable for installer automation and regression tests.
const WM_QUIT_TRAY: u32 = WM_APP + 3;
const WM_SHOW_STATUS: u32 = WM_APP + 4;
const WM_START_SYNC: u32 = WM_APP + 5;
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
const ID_HIDE: usize = 1010;
const STATUS_TEXT: i32 = 1101;
const CONFIG_TEXT: i32 = 1102;
const HEADING: i32 = 1103;
const SUBTITLE: i32 = 1104;
const FOOTER: i32 = 1105;

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
    status: Cell<HWND>,
    icon_small: HICON,
    icon_large: HICON,
    font: HFONT,
    heading_font: HFONT,
    taskbar_created: u32,
    icon_added: Cell<bool>,
    view: SharedView,
    sender: SyncSender<Action>,
    status_cache: RefCell<String>,
    config_cache: RefCell<String>,
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
            if !self.font.is_null() {
                DeleteObject(self.font);
            }
            if !self.heading_font.is_null() {
                DeleteObject(self.heading_font);
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
    let font_name = wide("Microsoft YaHei UI");
    let font = unsafe {
        CreateFontW(
            -16,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            5,
            0,
            font_name.as_ptr(),
        )
    };
    let heading_font = unsafe {
        CreateFontW(
            -23,
            0,
            0,
            0,
            600,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            5,
            0,
            font_name.as_ptr(),
        )
    };
    let (sender, receiver) = sync_channel(8);
    let mut ui = Box::new(Ui {
        state: args.state.clone(),
        controller: Cell::new(null_mut()),
        status: Cell::new(null_mut()),
        icon_small: small,
        icon_large: large,
        font,
        heading_font,
        taskbar_created: unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) },
        icon_added: Cell::new(false),
        view: Arc::new(Mutex::new(View::default())),
        sender,
        status_cache: RefCell::new(String::new()),
        config_cache: RefCell::new(String::new()),
    });
    let classes: [(&str, WNDPROC); 2] = [
        (CONTROLLER_CLASS, Some(controller_proc)),
        (STATUS_CLASS, Some(status_proc)),
    ];
    for (name, procedure) in classes {
        let name = wide(name);
        let class = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: procedure,
            hInstance: instance,
            hIcon: large,
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            hbrBackground: (COLOR_BTNFACE as usize + 1) as _,
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
        let status = ui.status.get();
        if status.is_null() || unsafe { IsDialogMessageW(status, &message) } == 0 {
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    delete_icon(&ui);
    unsafe {
        if !ui.status.get().is_null() {
            DestroyWindow(ui.status.get());
        }
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
    let window = ui.status.get();
    if window.is_null() {
        return;
    }
    let status = view.status_text(local_time);
    if *ui.status_cache.borrow() != status {
        *ui.status_cache.borrow_mut() = status.clone();
        unsafe {
            SetWindowTextW(GetDlgItem(window, STATUS_TEXT), wide(&status).as_ptr());
        }
    }
    let config = view.config_text(&ui.state.display().to_string());
    if *ui.config_cache.borrow() != config {
        *ui.config_cache.borrow_mut() = config.clone();
        unsafe {
            SetWindowTextW(GetDlgItem(window, CONFIG_TEXT), wide(&config).as_ptr());
        }
    }
    for (id, enabled) in [
        (ID_START, !view.running()),
        (ID_STOP, view.running()),
        (ID_RESTART, true),
        (ID_SYNC, view.running()),
        (ID_SETUP, true),
    ] {
        unsafe {
            EnableWindow(
                GetDlgItem(window, id as i32),
                i32::from(enabled && !view.busy),
            );
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
        ID_HIDE | 2 => unsafe {
            // IDCANCEL: Escape closes only the status window.
            ShowWindow(ui.status.get(), SW_HIDE);
        },
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
            let status = ui.status.replace(null_mut());
            if !status.is_null() {
                DestroyWindow(status);
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
    let mut window = ui.status.get();
    if window.is_null() {
        let height = unsafe { GetSystemMetrics(SM_CYSCREEN) }
            .saturating_sub(80)
            .clamp(560, 780);
        window = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,
                wide(STATUS_CLASS).as_ptr(),
                wide("WeType 剪贴板同步 — 状态与配置").as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                760,
                height,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                (ui as *const Ui).cast(),
            )
        };
        if window.is_null() {
            return;
        }
        ui.status.set(window);
        unsafe {
            SendMessageW(
                window,
                WM_SETICON,
                ICON_SMALL as usize,
                ui.icon_small as isize,
            );
            SendMessageW(
                window,
                WM_SETICON,
                ICON_BIG as usize,
                ui.icon_large as isize,
            );
        }
        create_control(ui, window, "STATIC", "WeType 剪贴板同步", HEADING, 0, false);
        create_control(
            ui,
            window,
            "STATIC",
            &format!(
                "文本与图片  ·  Windows x86  ·  v{}",
                env!("CARGO_PKG_VERSION")
            ),
            SUBTITLE,
            0,
            false,
        );
        for id in [STATUS_TEXT, CONFIG_TEXT] {
            create_control(
                ui,
                window,
                "EDIT",
                "",
                id,
                ES_MULTILINE as u32
                    | ES_READONLY as u32
                    | ES_AUTOVSCROLL as u32
                    | WS_VSCROLL
                    | WS_BORDER,
                true,
            );
        }
        for (id, label) in [
            (ID_START, "开始同步"),
            (ID_STOP, "停止同步"),
            (ID_SYNC, "立即同步"),
            (ID_RESTART, "重启同步"),
            (ID_SETUP, "设置与配对…"),
            (ID_LOG, "查看日志"),
            (ID_FOLDER, "打开配置目录"),
            (ID_HIDE, "关闭窗口"),
        ] {
            create_control(
                ui,
                window,
                "BUTTON",
                label,
                id as i32,
                BS_PUSHBUTTON as u32,
                true,
            );
        }
        create_control(
            ui,
            window,
            "STATIC",
            "状态每 2 秒刷新；关闭窗口后继续在托盘运行。退出请使用托盘菜单。",
            FOOTER,
            0,
            false,
        );
        unsafe {
            SendMessageW(
                GetDlgItem(window, HEADING),
                WM_SETFONT,
                ui.heading_font as usize,
                1,
            );
        }
        layout(window);
        ui.status_cache.borrow_mut().clear();
        ui.config_cache.borrow_mut().clear();
    }
    refresh(ui);
    unsafe {
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
        SetFocus(GetDlgItem(window, STATUS_TEXT));
    }
}

fn create_control(ui: &Ui, parent: HWND, class: &str, text: &str, id: i32, extra: u32, tab: bool) {
    let window = unsafe {
        CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | extra | if tab { WS_TABSTOP } else { 0 },
            0,
            0,
            1,
            1,
            parent,
            id as usize as _,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if !window.is_null() {
        unsafe {
            SendMessageW(window, WM_SETFONT, ui.font as usize, 1);
        }
    }
}

fn layout(window: HWND) {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(window, &mut rect);
    }
    let width = rect.right.max(400);
    let height = rect.bottom.max(400);
    let margin = 18;
    let inner = width - 2 * margin;
    let content = height - 212;
    let first = content / 2;
    let second = content - first;
    let place = |id, x, y, width, height| unsafe {
        MoveWindow(GetDlgItem(window, id), x, y, width, height, 1);
    };
    place(HEADING, margin, 15, inner, 30);
    place(SUBTITLE, margin, 49, inner, 25);
    place(STATUS_TEXT, margin, 83, inner, first);
    place(CONFIG_TEXT, margin, 95 + first, inner, second);
    let buttons_top = 106 + content;
    let button_width = (inner - 3 * 10) / 4;
    for (index, id) in [
        ID_START, ID_STOP, ID_SYNC, ID_RESTART, ID_SETUP, ID_LOG, ID_FOLDER, ID_HIDE,
    ]
    .into_iter()
    .enumerate()
    {
        place(
            id as i32,
            margin + (index % 4) as i32 * (button_width + 10),
            buttons_top + (index / 4) as i32 * 38,
            button_width,
            30,
        );
    }
    place(FOOTER, margin, height - 29, inner, 25);
}

unsafe extern "system" fn status_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as _);
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let pointer = GetWindowLongPtrW(window, GWLP_USERDATA) as *const Ui;
    if pointer.is_null() {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let ui = &*pointer;
    match message {
        WM_CLOSE => {
            ShowWindow(window, SW_HIDE);
            0
        }
        WM_COMMAND => {
            command(ui, wparam & 0xffff);
            0
        }
        WM_SIZE => {
            layout(window);
            0
        }
        WM_GETMINMAXINFO => {
            let size = &mut *(lparam as *mut MINMAXINFO);
            size.ptMinTrackSize = POINT { x: 600, y: 560 };
            0
        }
        WM_NCDESTROY => {
            if ui.status.get() == window {
                ui.status.set(null_mut());
            }
            SetWindowLongPtrW(window, GWLP_USERDATA, 0);
            DefWindowProcW(window, message, wparam, lparam)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_creation_preserves_titles_and_instance_discovery() {
        // Exercise both production window procedures without starting a worker
        // or touching the user's state. A null Ui pointer deliberately delegates
        // subsequent messages to DefWindowProc; WM_NCCREATE still takes exactly
        // the production path that used to discard the title.
        let procedures: [WNDPROC; 2] = [Some(controller_proc), Some(status_proc)];
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
