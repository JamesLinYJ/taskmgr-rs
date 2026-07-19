//! 应用主控模块。
//! 这里负责 Win32 启动、主窗口生命周期、消息循环、菜单与托盘状态，
//! 并统一协调各个页面的初始化、激活和定时刷新。

use std::env;
use std::mem::{size_of, zeroed};
use std::ptr::{NonNull, null, null_mut};
use std::sync::OnceLock;

use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::{IShellDispatch, Shell};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_GEN_FAILURE, ERROR_INVALID_PARAMETER, ERROR_TIMEOUT,
    HANDLE, HINSTANCE, HWND, LPARAM, POINT, RECT, TRUE, WAIT_ABANDONED, WAIT_OBJECT_0,
    WAIT_TIMEOUT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_3DFACE, CreateRectRgn, DCX_CACHE, DCX_CLIPSIBLINGS, DCX_INTERSECTRGN, DeleteObject,
    FillRect, GetDC, GetDCEx, GetDeviceCaps, GetSysColorBrush, GetUpdateRgn, LOGPIXELSX,
    MapWindowPoints, ReleaseDC,
};
use windows_sys::Win32::System::Diagnostics::Debug::MessageBeep;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, CreateMutexW, GetActiveProcessorCount, ReleaseMutex,
    SetProcessShutdownParameters, WaitForSingleObject,
};
use windows_sys::Win32::UI::Controls::{
    ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_TAB_CLASSES, INITCOMMONCONTROLSEX,
    InitCommonControlsEx, NMHDR, SB_SETPARTS, SB_SETTEXTW, SB_SIMPLE, SB_SIMPLEID, SBARS_SIZEGRIP,
    SBT_NOBORDERS, STATUSCLASSNAMEW, TCM_ADJUSTRECT, TCM_GETCURSEL, TCM_INSERTITEMW, TCM_SETCURSEL,
    TCN_SELCHANGE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SetCapture, VK_CONTROL,
};
use windows_sys::Win32::UI::Shell::{NIM_ADD, NIM_DELETE, ShellAboutW, ShellExecuteW, WinHelpW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CheckMenuItem, CheckMenuRadioItem, CreateWindowExW, DefWindowProcW, DeleteMenu,
    DestroyAcceleratorTable, DestroyWindow, DispatchMessageW, DrawMenuBar, EnableMenuItem,
    FindWindowW, GWL_STYLE, GetClassInfoW, GetClientRect, GetCursorPos, GetDlgItem,
    GetForegroundWindow, GetMenu, GetMenuItemInfoW, GetMessageW, GetShellWindow, GetWindowLongW,
    GetWindowPlacement, GetWindowRect, HACCEL, HELP_FINDER, HICON, HMENU, HTCAPTION, HTCLIENT,
    HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IDCANCEL, IsDialogMessageW, IsIconic, IsWindowVisible,
    IsZoomed, KillTimer, LR_DEFAULTCOLOR, LR_DEFAULTSIZE, MB_ICONSTOP, MB_OK, MENUITEMINFOW,
    MF_BYCOMMAND, MF_CHECKED, MF_ENABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_SYSMENU,
    MF_UNCHECKED, MIIM_ID, MINMAXINFO, MSG, MessageBoxW, OpenIcon, PostMessageW, PostQuitMessage,
    RegisterClassW, SIZE_MINIMIZED, SMTO_ABORTIFHUNG, SW_HIDE, SW_MINIMIZE, SW_SHOW,
    SW_SHOWMAXIMIZED, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SW_SHOWNORMAL, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOREDRAW, SWP_NOSIZE, SWP_NOZORDER, SendMessageTimeoutW,
    SendMessageW, SetForegroundWindow, SetMenu, SetMenuDefaultItem, SetTimer, SetWindowLongW,
    SetWindowPos, SetWindowTextW, ShowWindow, TPM_RETURNCMD, TrackPopupMenuEx,
    TranslateAcceleratorW, TranslateMessage, WINDOWPLACEMENT, WM_CLOSE, WM_COMMAND, WM_CREATE,
    WM_DESTROY, WM_ENDSESSION, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_INITDIALOG, WM_INITMENU,
    WM_LBUTTONDBLCLK, WM_MENUSELECT, WM_MOVE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCRBUTTONDOWN,
    WM_NCRBUTTONUP, WM_NOTIFY, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETICON, WM_SETREDRAW, WM_SIZE,
    WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_CLIPSIBLINGS, WS_DLGFRAME, WS_MAXIMIZEBOX,
    WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_TILEDWINDOW, WS_VISIBLE,
};

use crate::app_controllers::{
    MenuController, RuntimeStatsController, TrayController, WindowModeController,
};
use crate::assets::{MAIN_ICON_RESOURCE, create_accelerator_table, load_icon_resource};
use crate::cpu_topology::CpuTopologyError;
use crate::dialog_templates::create_dialog;
use crate::language::{TextKey, localize_dialog, menu_status_help, text};
use crate::menus::build_popup_menu;
use crate::options::{Options, update_speed_timer_interval};
use crate::pages::{DialogPage, RefreshReason, default_pages};
use crate::process_identity::ProcIdentity;
use crate::resource::*;
use crate::runtime_menu::PopupMenu;
use crate::system_sampler::{SystemSample, SystemSampleError, SystemSampler};
use crate::winutil::{
    call_window_proc, destroy_icon_handle, enable_debug_privilege, format_resource_string, height,
    hiword, loword, process_is_elevated, record_hresult_error, record_ntstatus_error,
    record_startup_timing, record_win32_error, sanitize_task_manager_menu, set_dialog_msg_result,
    set_style, set_window_userdata_ptr, to_wide_null, width, window_userdata_non_null,
};

const STARTUP_MUTEX_NAME: &str = "NTShell Taskman Startup Mutex";
const FINDME_TIMEOUT: u32 = 10_000;
static FRAME_BASE_WNDPROC: OnceLock<
    Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize>,
> = OnceLock::new();

const PERF_FRAME_CLASS_NAME: &str = "TaskManagerFrame";
const BUTTON_CLASS: &str = "Button";

// `RedrawWindow` 标志位只用到其中一部分，因此在这里保留最小子集。
const RDW_INVALIDATE: u32 = 0x0001;
const RDW_ERASE: u32 = 0x0004;
const RDW_UPDATENOW: u32 = 0x0100;
const RDW_FRAME: u32 = 0x0400;

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, i32> {
        // The main window thread owns this apartment for the duration of the system Run request.
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            Ok(Self)
        } else {
            Err(result.0)
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // Every successful CoInitializeEx call, including S_FALSE, requires a matching call.
        unsafe { CoUninitialize() };
    }
}

unsafe extern "system" {
    fn RedrawWindow(hwnd: HWND, lprcupdate: *const RECT, hrgnupdate: HANDLE, flags: u32) -> i32;
}

#[derive(Default)]
struct GlobalStrings {
    // 全局字符串缓存，主要供状态栏、标题栏和提示框复用。
    app_title: String,
    fmt_procs: String,
    fmt_cpu: String,
    fmt_mem: String,
}

pub struct App {
    // 主应用状态对象统一持有主窗口、菜单、页面和托盘/定时器相关状态。
    hinstance: HINSTANCE,
    main_hwnd: HWND,
    status_hwnd: HWND,
    timer_id: usize,
    startup_mutex: HANDLE,
    startup_mutex_owned: bool,
    accelerator_table: HACCEL,
    main_icon: HICON,
    menu: MenuController,
    tray: TrayController,
    strings: GlobalStrings,
    options: Options,
    pages: [DialogPage; NUM_PAGES],
    stats: RuntimeStatsController,
    system_sampler: SystemSampler,
    last_system_sample_error: Option<SystemSampleError>,
    last_cpu_topology_error: Option<CpuTopologyError>,
    last_system_worker_error: Option<u32>,
    window_mode: WindowModeController,
    min_width: i32,
    min_height: i32,
    already_applied_initial_position: bool,
    initialization_error: Option<u32>,
}

pub fn run() -> i32 {
    // 主应用对象的生命周期由 `run()` 栈帧直接持有，
    // 主窗口过程通过窗口 user data 回到这份状态，而不是依赖可变全局单例。
    // 安全性: 进程启动阶段尚未暴露任何窗口回调，`App` 只在当前线程初始化并运行。
    unsafe {
        if let Err(error) = enable_debug_privilege() {
            record_win32_error("enabling SeDebugPrivilege", error);
            match process_is_elevated() {
                Ok(false) => {
                    return match relaunch_elevated() {
                        Ok(()) => 0,
                        Err(relaunch_error) => {
                            record_win32_error("elevated relaunch", relaunch_error);
                            1
                        }
                    };
                }
                Ok(true) => return 1,
                Err(elevation_error) => {
                    record_win32_error("querying process elevation", elevation_error);
                    return 1;
                }
            }
        }

        let hinstance = GetModuleHandleW(null());
        let mut app = App::new(hinstance);
        app.run_main()
    }
}

unsafe fn relaunch_elevated() -> Result<(), u32> {
    unsafe {
        let executable = env::current_exe()
            .map_err(|error| error.raw_os_error().unwrap_or(ERROR_GEN_FAILURE as i32) as u32)?;
        let executable = to_wide_null(&executable.to_string_lossy());
        let verb = to_wide_null("runas");
        let result = ShellExecuteW(
            null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            null(),
            null(),
            SW_SHOWNORMAL,
        ) as isize;
        if result > 32 {
            Ok(())
        } else {
            Err(result.max(1) as u32)
        }
    }
}

fn app_from_hwnd(hwnd: HWND) -> Option<NonNull<App>> {
    // 主窗口过程通过 `GWLP_USERDATA` 找回唯一的 `App` 实例，
    // 并把可变借用限制在当前消息分发作用域内。
    window_userdata_non_null(hwnd)
}

unsafe extern "system" fn perf_frame_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // 性能页里的“框架控件”需要自绘背景，否则图表重绘时容易出现撕裂和闪烁。
    match msg {
        WM_CREATE => {
            // 安全性: `hwnd` is the window currently receiving WM_CREATE; only style bits are
            // read and updated.
            unsafe {
                let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
                SetWindowLongW(hwnd, GWL_STYLE, (style | WS_CLIPSIBLINGS) as i32);
            }
            0
        }
        WM_ERASEBKGND => {
            // 安全性: WM_ERASEBKGND supplies an HDC in WPARAM when nonzero; otherwise this
            // routine creates and releases its own clipped DC/region for the current window.
            unsafe {
                let mut hdc = wparam as _;
                let mut region = null_mut();

                if wparam == 0 {
                    region = CreateRectRgn(0, 0, 0, 0);
                    if !region.is_null() {
                        GetUpdateRgn(hwnd, region, 1);
                        hdc = GetDCEx(
                            hwnd,
                            region,
                            DCX_CACHE | DCX_CLIPSIBLINGS | DCX_INTERSECTRGN,
                        );
                        // DCX_INTERSECTRGN makes GetDCEx take ownership of HRGN, including when
                        // the returned DC is null. Do not inspect or delete it after the call.
                        region = null_mut();
                    }
                }

                if !hdc.is_null() {
                    let mut client_rect = zeroed::<RECT>();
                    GetClientRect(hwnd, &mut client_rect);
                    FillRect(hdc, &client_rect, GetSysColorBrush(COLOR_3DFACE));
                }

                if wparam == 0 {
                    if !hdc.is_null() {
                        ReleaseDC(hwnd, hdc);
                    }
                    if !region.is_null() {
                        DeleteObject(region as _);
                    }
                }
            }
            TRUE as isize
        }
        _ => {
            if let Some(base_wndproc) = FRAME_BASE_WNDPROC.get().copied().flatten() {
                // 安全性: `base_wndproc` was captured from the registered Button class and
                // receives the original message parameters unchanged.
                unsafe { call_window_proc(Some(base_wndproc), hwnd, msg, wparam, lparam) }
            } else {
                // 安全性: use the standard default processor if class registration has not run.
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
    }
}

impl App {
    fn new(hinstance: HINSTANCE) -> Self {
        // `App` 只构造纯状态；真正的 Win32 句柄都在启动流程中逐步建立。
        Self {
            hinstance,
            main_hwnd: null_mut(),
            status_hwnd: null_mut(),
            timer_id: 0,
            startup_mutex: null_mut(),
            startup_mutex_owned: false,
            accelerator_table: null_mut(),
            main_icon: null_mut(),
            menu: MenuController::default(),
            tray: TrayController::default(),
            strings: GlobalStrings::default(),
            options: Options::default(),
            pages: default_pages(),
            stats: RuntimeStatsController::default(),
            system_sampler: SystemSampler::new(),
            last_system_sample_error: None,
            last_cpu_topology_error: None,
            last_system_worker_error: None,
            window_mode: WindowModeController::default(),
            min_width: 0,
            min_height: 0,
            already_applied_initial_position: false,
            initialization_error: None,
        }
    }

    fn run_main(&mut self) -> i32 {
        let startup_started_ms = unsafe { GetTickCount64() };
        // 启动链路按“单实例检查 -> 环境初始化 -> 创建主对话框 -> 进入消息循环”展开。
        // 这样既能兼容经典 Task Manager 的行为，也便于在失败点提前退出。
        if !self.acquire_startup_mutex() {
            return 1;
        }
        if self.activate_existing_instance() {
            self.release_startup_mutex();
            return 0;
        }

        if self.task_manager_disabled() {
            self.release_startup_mutex();
            return 1;
        }

        if let Err(error) = self.initialize_common_controls() {
            record_win32_error("common controls initialization", error);
            self.release_startup_mutex();
            return 1;
        }
        if let Err(error) = self.register_custom_controls() {
            record_win32_error("custom control registration", error);
            self.release_startup_mutex();
            return 1;
        }
        if let Err(error) = self.load_global_resources() {
            record_win32_error("global resource loading", error);
            self.cleanup_failed_startup();
            self.release_startup_mutex();
            return 1;
        }
        self.stats.processor_count = match self.query_processor_count() {
            Ok(count) => count,
            Err(error) => {
                record_win32_error("processor group count", error);
                self.cleanup_failed_startup();
                self.release_startup_mutex();
                return 1;
            }
        };
        if let Err(error) = self.system_sampler.start(self.stats.processor_count) {
            record_win32_error("system sampler startup", error);
            self.cleanup_failed_startup();
            self.release_startup_mutex();
            return 1;
        }

        self.main_hwnd = match create_dialog(
            self.hinstance,
            IDD_MAINWND,
            null_mut(),
            Some(main_window_proc),
            self as *mut Self as LPARAM,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                record_win32_error("main dialog creation", error);
                self.cleanup_failed_startup();
                self.release_startup_mutex();
                return 1;
            }
        };
        if self.initialization_error.take().is_some() {
            unsafe { DestroyWindow(self.main_hwnd) };
            self.main_hwnd = null_mut();
            self.release_startup_mutex();
            return 1;
        }

        self.already_applied_initial_position = true;
        let saved_rect = self.options.window_rect;
        if width(&saved_rect) > 0 && height(&saved_rect) > 0 {
            // 安全性: the main HWND was just created and `saved_rect` was validated on load.
            unsafe {
                SetWindowPos(
                    self.main_hwnd,
                    null_mut(),
                    saved_rect.left,
                    saved_rect.top,
                    width(&saved_rect),
                    height(&saved_rect),
                    SWP_NOZORDER,
                );
            }
        }

        // 安全性: the main HWND is live after successful dialog creation.
        unsafe { ShowWindow(self.main_hwnd, SW_SHOW) };
        record_startup_timing(
            "main window visible",
            unsafe { GetTickCount64() }.wrapping_sub(startup_started_ms),
        );
        self.release_startup_mutex();
        // Queue every hidden page independently so their first layout/sample can overlap. The
        // tray/icon work stays deferred until after those lightweight warm-up requests.
        unsafe {
            for page_index in 0..self.pages.len() {
                if PostMessageW(self.main_hwnd, PWM_PREWARM_PAGE, page_index, 0) == 0 {
                    let error = windows_sys::Win32::Foundation::GetLastError();
                    record_win32_error(
                        "page prewarm message",
                        if error == 0 { ERROR_GEN_FAILURE } else { error },
                    );
                }
            }
            if PostMessageW(self.main_hwnd, PWM_DEFERREDINIT, 0, 0) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                record_win32_error(
                    "deferred initialization message",
                    if error == 0 { ERROR_GEN_FAILURE } else { error },
                );
            }
        }

        // 安全性: message loop runs on the UI thread; `message` is a valid MSG buffer for all
        // synchronous Win32 message APIs used inside the loop.
        unsafe {
            SetProcessShutdownParameters(1, 0);

            let mut message = zeroed::<MSG>();
            loop {
                let get_message_result = GetMessageW(&raw mut message, null_mut(), 0, 0);
                if get_message_result == 0 {
                    break message.wParam as i32;
                }
                if get_message_result < 0 {
                    let error = windows_sys::Win32::Foundation::GetLastError();
                    record_win32_error(
                        "message loop",
                        if error == 0 { ERROR_GEN_FAILURE } else { error },
                    );
                    if !self.main_hwnd.is_null() {
                        DestroyWindow(self.main_hwnd);
                    }
                    break 1;
                }

                let page_hwnd = if self.options.current_page >= 0 {
                    self.pages[self.options.current_page as usize].hwnd()
                } else {
                    null_mut()
                };

                let mut handled = !self.accelerator_table.is_null()
                    && TranslateAcceleratorW(
                        self.main_hwnd,
                        self.accelerator_table,
                        &raw const message,
                    ) != 0;

                if !handled && !page_hwnd.is_null() && !self.accelerator_table.is_null() {
                    handled = TranslateAcceleratorW(
                        page_hwnd,
                        self.accelerator_table,
                        &raw const message,
                    ) != 0;
                }

                if !handled && IsDialogMessageW(self.main_hwnd, &raw const message) == 0 {
                    TranslateMessage(&raw const message);
                    DispatchMessageW(&raw const message);
                }
            }
        }
    }

    fn acquire_startup_mutex(&mut self) -> bool {
        // 命名互斥体用于串行化启动窗口，避免两个实例同时完成“是否已有实例”的判断。
        let mutex_name = to_wide_null(STARTUP_MUTEX_NAME);
        // 安全性: `mutex_name` is NUL-terminated and the returned handle is owned by App until
        // `release_startup_mutex`.
        unsafe {
            self.startup_mutex = CreateMutexW(null_mut(), TRUE, mutex_name.as_ptr());
            if self.startup_mutex.is_null() {
                record_win32_error(
                    "startup mutex creation",
                    windows_sys::Win32::Foundation::GetLastError(),
                );
                return false;
            }

            if windows_sys::Win32::Foundation::GetLastError() != ERROR_ALREADY_EXISTS {
                self.startup_mutex_owned = true;
                return true;
            }

            let wait_result = WaitForSingleObject(self.startup_mutex, FINDME_TIMEOUT);
            if wait_result == WAIT_OBJECT_0 || wait_result == WAIT_ABANDONED {
                self.startup_mutex_owned = true;
                true
            } else {
                let error = if wait_result == WAIT_TIMEOUT {
                    ERROR_TIMEOUT
                } else {
                    let error = windows_sys::Win32::Foundation::GetLastError();
                    if error == 0 { ERROR_GEN_FAILURE } else { error }
                };
                record_win32_error("startup mutex wait", error);
                CloseHandle(self.startup_mutex);
                self.startup_mutex = null_mut();
                false
            }
        }
    }

    fn release_startup_mutex(&mut self) {
        // 一旦主窗口已经创建或确认无需继续启动，就及时释放互斥体，避免阻塞后续实例探测。
        if !self.startup_mutex.is_null() {
            // 安全性: App owns this mutex HANDLE and releases/closes it at most once.
            unsafe {
                if self.startup_mutex_owned {
                    ReleaseMutex(self.startup_mutex);
                }
                CloseHandle(self.startup_mutex);
            }
            self.startup_mutex = null_mut();
            self.startup_mutex_owned = false;
        }
    }

    fn cleanup_failed_startup(&mut self) {
        self.system_sampler.stop();
        if !self.menu.current_menu().is_null() {
            if !self.main_hwnd.is_null() {
                // 安全性: detach the page-owned menu before page cleanup destroys it.
                unsafe { SetMenu(self.main_hwnd, null_mut()) };
            }
            self.menu.clear_current_menu();
        }
        for page in self.pages.iter_mut() {
            page.destroy();
        }
        self.tray.clear_icons();
        if !self.main_icon.is_null() {
            destroy_icon_handle(self.main_icon);
            self.main_icon = null_mut();
        }
        if !self.accelerator_table.is_null() {
            unsafe { DestroyAcceleratorTable(self.accelerator_table) };
            self.accelerator_table = null_mut();
        }
    }

    fn activate_existing_instance(&self) -> bool {
        // 与历史版本一致，靠主窗口标题找到已运行实例，并通过自定义消息把它激活到前台。
        let title = text(TextKey::AppTitle).to_string();
        if title.is_empty() {
            return false;
        }

        let title_wide = to_wide_null(&title);
        // 安全性: `title_wide` is NUL-terminated and lives through the FindWindowW call.
        let existing_hwnd = unsafe { FindWindowW(null(), title_wide.as_ptr()) };
        if existing_hwnd.is_null() {
            return false;
        }

        let mut result = 0usize;
        // 安全性: `existing_hwnd` was returned by FindWindowW and `result` is a valid out param.
        (unsafe {
            SendMessageTimeoutW(
                existing_hwnd,
                PWM_ACTIVATE,
                0,
                0,
                SMTO_ABORTIFHUNG,
                FINDME_TIMEOUT,
                &mut result,
            )
        }) != 0
            && result as u32 == PWM_ACTIVATE
    }

    fn task_manager_disabled(&self) -> bool {
        // 企业策略或系统策略可能禁用 Task Manager。
        // 这里在真正启动 UI 前读取策略位，并按系统工具习惯弹出阻止提示。
        let policy_key =
            to_wide_null("Software\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
        let value_name = to_wide_null("DisableTaskMgr");
        let mut key: HKEY = null_mut();

        // 安全性: registry path buffers are NUL-terminated and `key` is a valid out parameter.
        if unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                policy_key.as_ptr(),
                0,
                KEY_READ,
                &mut key,
            )
        } != 0
        {
            return false;
        }

        let mut value_type = 0u32;
        let mut raw_value = 0u32;
        let mut raw_size = size_of::<u32>() as u32;
        // 安全性: the value buffers are valid for the synchronous registry query; `key` is closed
        // immediately after the query.
        let status = unsafe {
            let status = RegQueryValueExW(
                key,
                value_name.as_ptr(),
                null_mut(),
                &mut value_type,
                &mut raw_value as *mut u32 as *mut u8,
                &mut raw_size,
            );
            RegCloseKey(key);
            status
        };

        if status == 0 && raw_value != 0 {
            let title = to_wide_null(text(TextKey::AppTitle));
            let body = to_wide_null(text(TextKey::TaskManagerDisabled));
            // 安全性: message box strings are NUL-terminated and valid for the call.
            unsafe {
                MessageBoxW(
                    null_mut(),
                    body.as_ptr(),
                    title.as_ptr(),
                    MB_OK | MB_ICONSTOP,
                );
            }
            true
        } else {
            false
        }
    }

    fn initialize_common_controls(&self) -> Result<(), u32> {
        // 页面里依赖 Tab、ListView、StatusBar 等公共控件类，必须在创建前统一注册。
        let classes = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES | ICC_BAR_CLASSES,
        };
        // 安全性: `classes` is initialized according to the common-controls API contract.
        if unsafe { InitCommonControlsEx(&classes) } == 0 {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
        } else {
            Ok(())
        }
    }

    fn load_global_resources(&mut self) -> Result<(), u32> {
        // 这些资源会被菜单、状态栏和托盘图标反复使用，启动时一次性加载可以减少分散的 API 调用。
        self.accelerator_table = create_accelerator_table();
        if self.accelerator_table.is_null() {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
        }
        self.strings.app_title = text(TextKey::AppTitle).to_string();
        self.strings.fmt_procs = text(TextKey::FormatProcesses).to_string();
        self.strings.fmt_cpu = text(TextKey::FormatCpuUsage).to_string();
        self.strings.fmt_mem = text(TextKey::FormatMemoryUsage).to_string();
        Ok(())
    }

    fn query_processor_count(&self) -> Result<usize, u32> {
        // 统计所有 processor groups，避免 64+ CPU 机器被旧 SYSTEM_INFO 视角截断。
        let count = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        if count == 0 {
            let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
        } else {
            Ok(count as usize)
        }
    }

    fn on_init_dialog(&mut self, hwnd: HWND) -> isize {
        // 主对话框初始化会把“窗口样式、状态栏、标签页、托盘、定时器”全部串起来，
        // 这也是运行期状态第一次与持久化配置合流的地方。
        // 安全性: WM_INITDIALOG supplies the live main HWND; all child-control creation and
        // layout happens synchronously on the UI thread during initialization.
        unsafe {
            self.main_hwnd = hwnd;
            localize_dialog(hwnd, IDD_MAINWND);

            let mut window_rect = zeroed::<RECT>();
            GetWindowRect(hwnd, &mut window_rect);
            self.min_width = width(&window_rect);
            self.min_height = height(&window_rect);
            let framed_style = framed_window_style(GetWindowLongW(hwnd, GWL_STYLE) as u32);
            self.window_mode
                .set_base_styles(framed_style, borderless_window_style(framed_style));

            self.options.load(self.min_width, self.min_height);

            SetWindowPos(
                hwnd,
                if self.options.always_on_top() {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );

            if let Err(error) = self.create_status_bar() {
                self.initialization_error = Some(error);
                return 0;
            }

            // Keep the status bar above sibling child controls without promoting the whole app to topmost.
            if !self.status_hwnd.is_null() {
                SetWindowPos(
                    self.status_hwnd,
                    HWND_TOP,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
                );
            }

            self.set_window_title();

            let tabs_hwnd = GetDlgItem(hwnd, IDC_TABS);
            for (index, page) in self.pages.iter_mut().enumerate() {
                if let Err(error) = page.initialize(
                    self.hinstance,
                    self.main_hwnd,
                    tabs_hwnd,
                    self.stats.processor_count,
                ) {
                    let title = to_wide_null(&self.strings.app_title);
                    let message = to_wide_null(&format!(
                        "Failed to initialize page {} (Win32 error {}).",
                        index, error
                    ));
                    MessageBoxW(hwnd, message.as_ptr(), title.as_ptr(), MB_OK | MB_ICONSTOP);
                    self.initialization_error = Some(error);
                    return 0;
                }

                let title = page.title(self.hinstance);
                let mut title_wide = to_wide_null(&title);
                let mut item = windows_sys::Win32::UI::Controls::TCITEMW {
                    mask: windows_sys::Win32::UI::Controls::TCIF_TEXT,
                    dwState: 0,
                    dwStateMask: 0,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iImage: 0,
                    lParam: 0,
                };

                SendMessageW(
                    tabs_hwnd,
                    TCM_INSERTITEMW,
                    index,
                    &mut item as *mut _ as LPARAM,
                );
            }

            // Configure every already-created page once while it is hidden. First activation then
            // only swaps the menu and presents a prepared control tree.
            self.apply_options_to_pages();

            self.update_menu_states();
            if self.options.current_page < 0 {
                self.options.current_page = 0;
            }

            SendMessageW(
                tabs_hwnd,
                TCM_SETCURSEL,
                self.options.current_page as usize,
                0,
            );
            if let Err(error) = self.activate_page(self.options.current_page as usize) {
                self.initialization_error = Some(error);
                return 0;
            }

            let mut client_rect = zeroed::<RECT>();
            GetClientRect(hwnd, &mut client_rect);
            self.on_size(hwnd, 0, width(&client_rect), height(&client_rect));

            if let Err(error) = self.replace_update_timer(self.options.timer_interval) {
                self.initialization_error = Some(error);
                return 0;
            }

            if self.stats.processor_count <= 1 {
                let menu = GetMenu(hwnd);
                if !menu.is_null() {
                    EnableMenuItem(menu, u32::from(IDM_MULTIGRAPH), MF_BYCOMMAND | MF_GRAYED);
                }
            }

            1
        }
    }

    fn create_status_bar(&mut self) -> Result<(), u32> {
        // 安全性: creates a status bar child for the live main window and configures it
        // synchronously before returning.
        unsafe {
            self.status_hwnd = CreateWindowExW(
                0,
                STATUSCLASSNAMEW,
                null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | SBARS_SIZEGRIP,
                0,
                0,
                0,
                0,
                self.main_hwnd,
                IDC_STATUSWND as usize as HMENU,
                self.hinstance,
                null_mut(),
            );
            if self.status_hwnd.is_null() {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }

            let hdc = GetDC(null_mut());
            if hdc.is_null() {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            let pixels_per_inch = GetDeviceCaps(hdc, LOGPIXELSX as i32);
            ReleaseDC(null_mut(), hdc);
            if pixels_per_inch <= 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }

            let parts = [
                pixels_per_inch,
                pixels_per_inch + (pixels_per_inch * 5) / 4,
                pixels_per_inch + (pixels_per_inch * 15) / 4,
                -1,
            ];
            if SendMessageW(
                self.status_hwnd,
                SB_SETPARTS,
                parts.len(),
                parts.as_ptr() as LPARAM,
            ) == 0
            {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            Ok(())
        }
    }

    fn replace_update_timer(&mut self, interval: u32) -> Result<(), u32> {
        unsafe {
            let old_timer = self.timer_id;
            if interval == 0 {
                if old_timer != 0 && KillTimer(self.main_hwnd, old_timer) == 0 {
                    let error = windows_sys::Win32::Foundation::GetLastError();
                    return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
                }
                self.timer_id = 0;
                return Ok(());
            }

            let new_timer = SetTimer(self.main_hwnd, 0, interval, None);
            if new_timer == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }

            if old_timer != 0 && KillTimer(self.main_hwnd, old_timer) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                KillTimer(self.main_hwnd, new_timer);
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            self.timer_id = new_timer;
            Ok(())
        }
    }

    fn register_custom_controls(&self) -> Result<(), u32> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 性能页的 frame 控件借用了 Button 类的外观，但需要自定义背景擦除过程来降低闪烁。
            let mut button_class = zeroed::<WNDCLASSW>();
            let button_name = to_wide_null(BUTTON_CLASS);
            if GetClassInfoW(null_mut(), button_name.as_ptr(), &mut button_class) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }

            if FRAME_BASE_WNDPROC.set(button_class.lpfnWndProc).is_err() {
                return Err(ERROR_ALREADY_EXISTS);
            }
            button_class.hInstance = self.hinstance;
            button_class.lpfnWndProc = Some(perf_frame_wndproc);
            let class_name = to_wide_null(PERF_FRAME_CLASS_NAME);
            button_class.lpszClassName = class_name.as_ptr();
            if RegisterClassW(&button_class) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
            Ok(())
        }
    }

    fn set_window_title(&self) {
        let title = to_wide_null(&self.strings.app_title);
        // 安全性: `title` is NUL-terminated and valid for the duration of the call.
        unsafe { SetWindowTextW(self.main_hwnd, title.as_ptr()) };
    }

    fn activate_page(&mut self, index: usize) -> Result<(), u32> {
        // 切页不仅是隐藏/显示子对话框，还要同步菜单、页面选项和尺寸布局。
        // 如果新页面激活失败，会尽量恢复上一个页面，避免主窗口进入空白状态。
        if index >= self.pages.len() {
            return Err(ERROR_INVALID_PARAMETER);
        }

        if page_uses_normal_minimum(self.options.no_title(), index) {
            self.ensure_window_minimum_size()?;
        }

        let previous_page = self.options.current_page;
        let switching_pages = previous_page >= 0 && previous_page as usize != index;

        match self.pages[index].activate(
            self.hinstance,
            self.main_hwnd,
            &self.options,
            self.stats.processor_count,
            self.menu.current_menu_mut(),
        ) {
            Ok(()) => {
                if switching_pages {
                    self.pages[previous_page as usize].deactivate(&mut self.options);
                }
                self.options.current_page = index as i32;
                self.update_menu_states();
                self.size_active_page();
                self.refresh_active_page(RefreshReason::Activation);
                self.request_system_sample();
                self.refresh_tray_icon();
                self.refresh_status_bar();
                self.pages[index].show_and_focus();
                Ok(())
            }
            Err(error) => {
                if switching_pages {
                    let previous_index = previous_page as usize;
                    self.options.current_page = previous_page;
                    // 安全性: retrieves and updates the tab control owned by the main window.
                    let tabs_hwnd = unsafe { GetDlgItem(self.main_hwnd, IDC_TABS) };
                    if !tabs_hwnd.is_null() {
                        // 安全性: tab control HWND was returned by GetDlgItem and the message is
                        // synchronous.
                        unsafe { SendMessageW(tabs_hwnd, TCM_SETCURSEL, previous_index, 0) };
                    }
                    self.update_menu_states();
                    self.size_active_page();
                }
                Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
            }
        }
    }

    fn update_menu_states(&self) {
        // 菜单状态完全由 `options` 和当前页状态派生，每次切页/改选项后都重新同步，
        // 避免菜单勾选与真实行为脱节。
        // 安全性: menu queries and updates target the main window's current menu.
        let menu = unsafe { GetMenu(self.main_hwnd) };
        if menu.is_null() {
            return;
        }

        sanitize_task_manager_menu(menu, self.stats.processor_count);

        // 安全性: all calls mutate only the menu handle retrieved from this main window.
        unsafe {
            CheckMenuRadioItem(
                menu,
                u32::from(VM_FIRST),
                u32::from(VM_LAST),
                u32::from(VM_FIRST + self.options.view_mode as u16),
                MF_BYCOMMAND,
            );
            CheckMenuRadioItem(
                menu,
                u32::from(CM_FIRST),
                u32::from(CM_LAST),
                u32::from(CM_FIRST + self.options.cpu_history_mode as u16),
                MF_BYCOMMAND,
            );
            CheckMenuRadioItem(
                menu,
                u32::from(US_FIRST),
                u32::from(US_LAST),
                u32::from(US_FIRST + self.options.update_speed as u16),
                MF_BYCOMMAND,
            );
        }

        self.check_menu(menu, IDM_ALWAYSONTOP, self.options.always_on_top());
        self.check_menu(menu, IDM_MINIMIZEONUSE, self.options.minimize_on_use());
        self.check_menu(menu, IDM_CONFIRMATIONS, self.options.confirmations());
        self.check_menu(menu, IDM_KERNELTIMES, self.options.kernel_times());
        self.check_menu(menu, IDM_NOTITLE, self.options.no_title());
        self.check_menu(menu, IDM_HIDEWHENMIN, self.options.hide_when_minimized());
        if self.options.current_page == USER_PAGE as i32 {
            self.check_menu(
                menu,
                IDM_SHOWDOMAINNAMES,
                self.pages[USER_PAGE]
                    .user_show_domain_names()
                    .unwrap_or(false),
            );
        }

        // 安全性: same menu handle as above.
        unsafe {
            EnableMenuItem(
                menu,
                u32::from(IDM_MULTIGRAPH),
                MF_BYCOMMAND
                    | if self.stats.processor_count <= 1 {
                        MF_GRAYED
                    } else {
                        MF_ENABLED
                    },
            );
        }
    }

    fn check_menu(&self, menu: HMENU, item_id: u16, checked: bool) {
        // 安全性: caller passes a menu handle currently owned by the main window or popup menu.
        unsafe {
            CheckMenuItem(
                menu,
                u32::from(item_id),
                MF_BYCOMMAND | if checked { MF_CHECKED } else { MF_UNCHECKED },
            );
        }
    }

    fn apply_options_to_pages(&mut self) {
        for page in self.pages.iter_mut() {
            page.apply_options(&self.options, self.stats.processor_count);
        }
    }

    fn refresh_task_page(&mut self) {
        self.pages[TASK_PAGE].apply_options(&self.options, self.stats.processor_count);
        self.pages[TASK_PAGE].timer_event(
            &self.options,
            self.stats.processor_count,
            RefreshReason::User,
        );
    }

    fn refresh_performance_page(&mut self) {
        self.pages[PERF_PAGE].apply_options(&self.options, self.stats.processor_count);
        self.pages[PERF_PAGE].timer_event(
            &self.options,
            self.stats.processor_count,
            RefreshReason::User,
        );
    }

    fn refresh_active_page(&mut self, reason: RefreshReason) {
        // 定时器只推动当前可见页，隐藏页在切换到前台时再立即刷新。
        let Some(index) = active_page_index(self.options.current_page, self.pages.len()) else {
            return;
        };
        self.pages[index].timer_event(&self.options, self.stats.processor_count, reason);
    }

    fn size_active_page(&mut self) {
        // 无标题模式和普通模式的布局入口不同：
        // 前者让活动页直接占满主窗口客户区，后者则受 Tab 控件内容区约束。
        let Some(active_page) = active_page_index(self.options.current_page, self.pages.len())
        else {
            return;
        };

        let active_hwnd = self.pages[active_page].hwnd();
        if active_hwnd.is_null() {
            return;
        }

        // 安全性: layout is performed on the UI thread against the main window and active page
        // HWNDs owned by this App; null handles are checked above where needed.
        unsafe {
            if self.options.no_title() {
                let mut client_rect = zeroed::<RECT>();
                GetClientRect(self.main_hwnd, &mut client_rect);
                SetWindowPos(
                    active_hwnd,
                    null_mut(),
                    client_rect.left,
                    client_rect.top,
                    width(&client_rect),
                    height(&client_rect),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );

                // Compute borderless style from live window style (matching C++ behavior)
                let current_style = GetWindowLongW(self.main_hwnd, GWL_STYLE) as u32;
                let live_borderless =
                    current_style & !(WS_DLGFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX);
                set_style(self.main_hwnd, live_borderless);
                SetMenu(self.main_hwnd, null_mut());
                SetWindowPos(
                    self.main_hwnd,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
                );
                DrawMenuBar(self.main_hwnd);
            } else {
                // Compute framed style from live window style (matching C++ behavior)
                let current_style = GetWindowLongW(self.main_hwnd, GWL_STYLE) as u32;
                let live_framed = framed_window_style(current_style);
                set_style(self.main_hwnd, live_framed);

                if !self.menu.current_menu().is_null() {
                    SetMenu(self.main_hwnd, self.menu.current_menu());
                    self.update_menu_states();
                }
                let mut window_rect = zeroed::<RECT>();
                GetWindowRect(self.main_hwnd, &mut window_rect);
                let (window_width, window_height) = clamped_window_size(
                    width(&window_rect),
                    height(&window_rect),
                    self.min_width,
                    self.min_height,
                );
                SetWindowPos(
                    self.main_hwnd,
                    null_mut(),
                    0,
                    0,
                    window_width,
                    window_height,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_FRAMECHANGED,
                );
                DrawMenuBar(self.main_hwnd);
                self.set_window_title();

                let tabs_hwnd = GetDlgItem(self.main_hwnd, IDC_TABS);
                let tabs_rect = adjusted_tab_page_rect(tabs_hwnd, self.main_hwnd);
                SetWindowPos(
                    active_hwnd,
                    null_mut(),
                    tabs_rect.left,
                    tabs_rect.top,
                    width(&tabs_rect),
                    height(&tabs_rect),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn ensure_window_minimum_size(&mut self) -> Result<(), u32> {
        // 页面策略要求正常最小尺寸时，先把主窗口提升到模板定义的最小外框尺寸。
        // 该操作发生在目标页提交前，因此失败不会留下已经切换了一半的页面状态。
        unsafe {
            let mut window_rect = zeroed::<RECT>();
            if GetWindowRect(self.main_hwnd, &mut window_rect) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }

            let current_width = width(&window_rect);
            let current_height = height(&window_rect);
            let (window_width, window_height) = clamped_window_size(
                current_width,
                current_height,
                self.min_width,
                self.min_height,
            );
            if window_width == current_width && window_height == current_height {
                return Ok(());
            }

            if SetWindowPos(
                self.main_hwnd,
                null_mut(),
                0,
                0,
                window_width,
                window_height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) == 0
            {
                let error = windows_sys::Win32::Foundation::GetLastError();
                return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
            }
        }

        Ok(())
    }

    fn prewarm_page(&mut self, index: usize) {
        if index >= self.pages.len()
            || Some(index) == active_page_index(self.options.current_page, self.pages.len())
        {
            return;
        }

        let page_rect = unsafe {
            if self.options.no_title() {
                let mut rect = zeroed::<RECT>();
                GetClientRect(self.main_hwnd, &mut rect);
                rect
            } else {
                let tabs_hwnd = GetDlgItem(self.main_hwnd, IDC_TABS);
                if tabs_hwnd.is_null() {
                    return;
                }
                adjusted_tab_page_rect(tabs_hwnd, self.main_hwnd)
            }
        };

        let page_hwnd = self.pages[index].hwnd();
        if page_hwnd.is_null() {
            return;
        }

        unsafe {
            SetWindowPos(
                page_hwnd,
                null_mut(),
                page_rect.left,
                page_rect.top,
                width(&page_rect),
                height(&page_rect),
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
            );
        }
        self.pages[index].apply_options(&self.options, self.stats.processor_count);
        self.pages[index].timer_event(
            &self.options,
            self.stats.processor_count,
            RefreshReason::Prewarm,
        );
    }

    fn toggle_no_title_mode(&mut self) {
        // 安全性: redraw suppression and final invalidation target the main window owned by App.
        unsafe { SendMessageW(self.main_hwnd, WM_SETREDRAW, 0, 0) };

        self.options.set_no_title(!self.options.no_title());
        self.apply_options_to_pages();
        self.update_menu_states();
        self.size_active_page();
        // 安全性: see the safety note above; this restores redraw and repaints the same window.
        unsafe {
            SendMessageW(self.main_hwnd, WM_SETREDRAW, 1, 0);
            RedrawWindow(
                self.main_hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_UPDATENOW,
            );
        }
        self.redraw_active_page_after_layout();
    }

    fn redraw_active_page_after_layout(&self) {
        let Some(active_page) = active_page_index(self.options.current_page, self.pages.len())
        else {
            return;
        };

        self.pages[active_page].redraw_after_layout();
    }

    fn on_size(&mut self, hwnd: HWND, state: u32, width_px: i32, height_px: i32) {
        // 主窗口尺寸变化时同时维护状态栏、标签页和当前活动页。
        // 安全性: `hwnd` is the main window receiving WM_SIZE; child HWND lookups and moves stay
        // within this window hierarchy.
        unsafe {
            if state == SIZE_MINIMIZED
                && self.options.hide_when_minimized()
                && !GetShellWindow().is_null()
            {
                ShowWindow(hwnd, SW_HIDE);
            }

            if !self.status_hwnd.is_null() {
                SendMessageW(self.status_hwnd, WM_SIZE, state as usize, 0);
            }

            let tabs_hwnd = GetDlgItem(hwnd, IDC_TABS);
            if !tabs_hwnd.is_null() && !self.status_hwnd.is_null() {
                let mut status_rect = zeroed::<RECT>();
                GetClientRect(self.status_hwnd, &mut status_rect);
                MapWindowPoints(
                    self.status_hwnd,
                    self.main_hwnd,
                    &mut status_rect as *mut _ as _,
                    2,
                );

                let mut tabs_rect = zeroed::<RECT>();
                GetWindowRect(tabs_hwnd, &mut tabs_rect);
                MapWindowPoints(null_mut(), self.main_hwnd, &mut tabs_rect as *mut _ as _, 2);

                let adjusted_width = width_px - 2 * tabs_rect.left;
                let adjusted_height = height_px - (height_px - status_rect.top) - tabs_rect.top * 2;
                SetWindowPos(
                    tabs_hwnd,
                    null_mut(),
                    tabs_rect.left,
                    tabs_rect.top,
                    adjusted_width,
                    adjusted_height,
                    SWP_NOZORDER,
                );
            }
        }

        self.size_active_page();
    }

    fn on_timer(&mut self, hwnd: HWND, force: bool) {
        // 按住 Ctrl 时暂停自动刷新，这与经典 Task Manager 的交互保持一致。
        // 安全性: these calls only query foreground window and keyboard state.
        if !force
            && unsafe {
                GetForegroundWindow() == hwnd && GetAsyncKeyState(i32::from(VK_CONTROL)) < 0
            }
        {
            return;
        }

        self.refresh_active_page(if force {
            RefreshReason::User
        } else {
            RefreshReason::Periodic
        });
        self.request_system_sample();
    }

    fn request_system_sample(&mut self) {
        match self.system_sampler.request(self.main_hwnd) {
            Ok(()) => self.last_system_worker_error = None,
            Err(error) => self.record_system_worker_error("system sample request", error),
        }
    }

    fn handle_system_worker_completion(&mut self) {
        let completions = match self.system_sampler.drain(self.main_hwnd) {
            Ok(completions) => {
                self.last_system_worker_error = None;
                completions
            }
            Err(error) => {
                self.record_system_worker_error("system sampler completion", error);
                return;
            }
        };

        for completion in completions {
            match completion {
                Ok(sample) => self.apply_system_sample(&sample),
                Err(error) => self.record_system_sample_error(error),
            }
        }
    }

    fn apply_system_sample(&mut self, sample: &SystemSample) {
        self.record_cpu_topology_error(sample.processor_topology.error());
        let window_is_visible = unsafe { IsIconic(self.main_hwnd) == 0 };
        let redraw_performance_page =
            is_active_page(self.options.current_page, self.pages.len(), PERF_PAGE)
                && window_is_visible;
        if let Err(error) =
            self.pages[PERF_PAGE].apply_system_sample(sample, redraw_performance_page)
        {
            self.record_system_sample_error(SystemSampleError::Win32(error));
            return;
        }
        let redraw_cpu_page = is_active_page(self.options.current_page, self.pages.len(), CPU_PAGE)
            && window_is_visible;
        if let Err(error) = self.pages[CPU_PAGE].apply_system_sample(sample, redraw_cpu_page) {
            self.record_system_sample_error(SystemSampleError::Win32(error));
            return;
        }

        self.stats.apply_sample(sample);
        self.last_system_sample_error = None;
        self.refresh_tray_icon();
        self.refresh_status_bar();
    }

    fn record_system_sample_error(&mut self, error: SystemSampleError) {
        self.pages[CPU_PAGE].mark_system_sample_error();
        if self.last_system_sample_error == Some(error) {
            return;
        }
        match error {
            SystemSampleError::NtStatus(status) => record_ntstatus_error("system sampling", status),
            SystemSampleError::Win32(error) => record_win32_error("system sampling", error),
        }
        self.last_system_sample_error = Some(error);
    }

    fn record_cpu_topology_error(&mut self, error: Option<CpuTopologyError>) {
        if self.last_cpu_topology_error == error {
            return;
        }
        if let Some(error) = error {
            record_win32_error(error.context(), error.win32_code());
        }
        self.last_cpu_topology_error = error;
    }

    fn record_system_worker_error(&mut self, component: &str, error: u32) {
        self.pages[CPU_PAGE].mark_system_sample_error();
        if self.last_system_worker_error != Some(error) {
            record_win32_error(component, error);
        }
        self.last_system_worker_error = Some(error);
    }

    fn refresh_status_bar(&self) {
        // 状态栏只依赖运行期聚合快照，不直接向页面索要细节。
        if self.status_hwnd.is_null() || self.menu.is_tracking() {
            return;
        }

        let process_text = format_resource_string(
            &self.strings.fmt_procs,
            &[self.stats.process_count.to_string()],
        );
        let cpu_text =
            format_resource_string(&self.strings.fmt_cpu, &[self.stats.cpu_usage.to_string()]);
        let mem_text = format_resource_string(
            &self.strings.fmt_mem,
            &[
                self.stats.mem_usage_kb.to_string(),
                self.stats.mem_limit_kb.to_string(),
            ],
        );

        let process_wide = to_wide_null(&process_text);
        let cpu_wide = to_wide_null(&cpu_text);
        let mem_wide = to_wide_null(&mem_text);

        // 安全性: status bar text messages synchronously copy from the provided UTF-16 buffers.
        unsafe {
            SendMessageW(
                self.status_hwnd,
                SB_SETTEXTW,
                0,
                process_wide.as_ptr() as LPARAM,
            );
            SendMessageW(
                self.status_hwnd,
                SB_SETTEXTW,
                1,
                cpu_wide.as_ptr() as LPARAM,
            );
            SendMessageW(
                self.status_hwnd,
                SB_SETTEXTW,
                2,
                mem_wide.as_ptr() as LPARAM,
            );
        }
    }

    fn update_tray(&self, command: u32, icon: HICON, tip: &str) {
        self.tray.update_tray(self.main_hwnd, command, icon, tip);
    }

    fn refresh_tray_icon(&self) {
        // 托盘图标按 CPU 使用率映射到离散图标序列，行为上尽量贴近经典任务管理器。
        self.tray
            .refresh_icon(self.main_hwnd, self.stats.cpu_usage, &self.strings.fmt_cpu);
    }

    fn show_running_instance(&self) {
        // 恢复窗口时顺带重新应用 topmost 状态，保持和当前选项一致。
        // 安全性: operations target the main HWND owned by this App.
        unsafe {
            OpenIcon(self.main_hwnd);
            SetForegroundWindow(self.main_hwnd);
            SetWindowPos(
                self.main_hwnd,
                if self.options.always_on_top() {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }

    fn load_popup_menu(&self, resource_id: u16) -> Result<PopupMenu, u32> {
        // 弹出菜单构造也统一复用运行时菜单系统。
        build_popup_menu(resource_id)
    }

    fn on_tray_notification(&mut self, lparam: LPARAM) {
        // 托盘图标承担“恢复窗口”和“快速菜单”两个入口，所以这里单独处理鼠标消息。
        match lparam as u32 {
            windows_sys::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK => {
                self.show_running_instance()
            }
            WM_RBUTTONDOWN => {
                let popup = match self.load_popup_menu(IDR_TRAYMENU) {
                    Ok(popup) => popup,
                    Err(error) => {
                        record_win32_error("tray popup menu creation", error);
                        return;
                    }
                };
                let popup_handle = popup.as_raw();
                // 安全性: popup menu is valid for this scope; cursor/menu APIs are synchronous
                // and target this app's main window.
                let command = unsafe {
                    let mut cursor = zeroed::<POINT>();
                    GetCursorPos(&mut cursor);

                    if IsWindowVisible(self.main_hwnd) != 0 {
                        DeleteMenu(popup_handle, u32::from(IDM_RESTORETASKMAN), MF_BYCOMMAND);
                    } else {
                        SetMenuDefaultItem(popup_handle, u32::from(IDM_RESTORETASKMAN), 0);
                    }

                    self.check_menu(popup_handle, IDM_ALWAYSONTOP, self.options.always_on_top());
                    SetForegroundWindow(self.main_hwnd);
                    self.menu.enter_popup();
                    let command = TrackPopupMenuEx(
                        popup_handle,
                        TPM_RETURNCMD,
                        cursor.x,
                        cursor.y,
                        self.main_hwnd,
                        null(),
                    );
                    self.menu.leave_popup();
                    command
                };
                if command != 0 {
                    // 安全性: sends a synchronous command to our main window.
                    unsafe { SendMessageW(self.main_hwnd, WM_COMMAND, command as usize, 0) };
                }
            }
            _ => {}
        }
    }

    fn show_help(&self, hwnd: HWND) {
        let help_path = to_wide_null("taskmgr.hlp");
        // 安全性: `help_path` is a NUL-terminated UTF-16 buffer valid for the duration of call.
        unsafe { WinHelpW(hwnd, help_path.as_ptr(), HELP_FINDER, 0) };
    }

    fn on_menu_select(&mut self, wparam: WPARAM, lparam: LPARAM) -> isize {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 菜单高亮时，状态栏会临时切到“帮助文本”模式；
            // 退出菜单跟踪后，再恢复回实时统计栏。
            if self.status_hwnd.is_null() {
                return 0;
            }

            let mut item_id = u32::from(loword(wparam));
            let flags = u32::from(hiword(wparam));
            let menu = lparam as HMENU;

            if (item_id == 0xFFFF && menu.is_null()) || (flags & (MF_SYSMENU | MF_SEPARATOR)) != 0 {
                self.menu.end_tracking();
                SendMessageW(self.status_hwnd, SB_SIMPLE, 0, 0);
                self.refresh_status_bar();
                return 0;
            }

            if (flags & MF_POPUP) != 0 && !menu.is_null() {
                let mut submenu_info = MENUITEMINFOW {
                    cbSize: size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_ID,
                    ..zeroed()
                };
                if GetMenuItemInfoW(menu, item_id, 1, &mut submenu_info) != 0 {
                    item_id = submenu_info.wID;
                }
            }

            let status_text = menu_status_help(item_id as u16)
                .map(str::to_string)
                .unwrap_or_default();
            let status_wide = to_wide_null(&status_text);
            self.menu.begin_tracking();
            SendMessageW(
                self.status_hwnd,
                SB_SETTEXTW,
                (SBT_NOBORDERS | SB_SIMPLEID) as usize,
                status_wide.as_ptr() as LPARAM,
            );
            SendMessageW(self.status_hwnd, SB_SIMPLE, 1, 0);
            SendMessageW(
                self.status_hwnd,
                SB_SETTEXTW,
                SBT_NOBORDERS as usize,
                status_wide.as_ptr() as LPARAM,
            );
            0
        }
    }

    fn on_init_menu(&mut self) -> isize {
        // 菜单刚弹出时先标记“暂时不能隐藏窗口”，避免右键隐藏逻辑误触发。
        self.menu.mark_menu_opened();
        0
    }

    fn on_popup_state(&mut self, active: bool) -> isize {
        // 记录当前是否正处于弹出菜单交互期。
        self.menu.set_popup_active(active);
        0
    }

    fn on_right_button_down(&mut self, hwnd: HWND) -> isize {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if self.menu.can_temporarily_hide()
                && !self.window_mode.is_temporarily_hidden()
                && self.options.always_on_top()
            {
                ShowWindow(hwnd, SW_HIDE);
                SetCapture(hwnd);
                self.window_mode.mark_temporarily_hidden();
            }
            0
        }
    }

    fn on_right_button_up(&mut self, hwnd: HWND) -> isize {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if self.window_mode.is_temporarily_hidden() {
                ReleaseCapture();
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_SHOWMINNOACTIVE);
                } else if IsZoomed(hwnd) != 0 {
                    ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                    SetForegroundWindow(hwnd);
                } else {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    SetForegroundWindow(hwnd);
                }
                self.window_mode.mark_restored();
            }
            0
        }
    }

    fn show_run_dialog(&self) -> Result<(), i32> {
        let _apartment = ComApartment::initialize()?;
        let shell: IShellDispatch = unsafe { CoCreateInstance(&Shell, None, CLSCTX_ALL) }
            .map_err(|error| error.code().0)?;
        // IShellDispatch::FileRun is the documented Shell automation API for the system dialog.
        unsafe { shell.FileRun() }.map_err(|error| error.code().0)
    }

    fn show_hresult_failure(&self, title: &str, error: i32) {
        let title = to_wide_null(title);
        let body = to_wide_null(&format!("HRESULT: 0x{:08X}", error as u32));
        unsafe {
            MessageBoxW(
                self.main_hwnd,
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONSTOP,
            );
        }
    }

    fn on_command(&mut self, hwnd: HWND, command_id: u16) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 主命令分发层只负责修改全局选项、切页和把页面专属命令转发到对应子页面。
            // 真正的进程/任务/用户操作都在各自页面状态对象里完成。
            match command_id {
                IDM_HIDE => {
                    ShowWindow(hwnd, SW_MINIMIZE);
                }
                id if id == IDCANCEL as u16 || id == IDM_EXIT => {
                    DestroyWindow(hwnd);
                }
                IDM_RESTORETASKMAN => {
                    self.show_running_instance();
                }
                IDC_NEXTTAB | IDC_PREVTAB => {
                    self.switch_tabs(command_id == IDC_NEXTTAB);
                }
                IDM_ALWAYSONTOP => {
                    let always_on_top = !self.options.always_on_top();
                    self.options.set_always_on_top(always_on_top);
                    SetWindowPos(
                        hwnd,
                        if always_on_top {
                            HWND_TOPMOST
                        } else {
                            HWND_NOTOPMOST
                        },
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE,
                    );
                    self.update_menu_states();
                }
                IDM_HIDEWHENMIN => {
                    self.options
                        .set_hide_when_minimized(!self.options.hide_when_minimized());
                    self.update_menu_states();
                }
                IDM_MINIMIZEONUSE => {
                    self.options
                        .set_minimize_on_use(!self.options.minimize_on_use());
                    self.update_menu_states();
                }
                IDM_CONFIRMATIONS => {
                    self.options
                        .set_confirmations(!self.options.confirmations());
                    self.update_menu_states();
                }
                IDM_NOTITLE => {
                    self.toggle_no_title_mode();
                }
                IDM_KERNELTIMES => {
                    self.options.set_kernel_times(!self.options.kernel_times());
                    self.refresh_performance_page();
                    self.pages[CPU_PAGE].apply_options(&self.options, self.stats.processor_count);
                    self.update_menu_states();
                }
                IDM_LARGEICONS | IDM_SMALLICONS | IDM_DETAILS => {
                    self.options.view_mode = i32::from(command_id - VM_FIRST);
                    self.update_menu_states();
                    self.refresh_task_page();
                }
                IDM_ALLCPUS | IDM_MULTIGRAPH => {
                    self.options.cpu_history_mode = i32::from(command_id - CM_FIRST);
                    self.refresh_performance_page();
                    self.update_menu_states();
                }
                IDM_HIGH | IDM_NORMAL | IDM_LOW | IDM_PAUSED => {
                    let update_speed = i32::from(command_id - US_FIRST);
                    let Some(timer_delay) = update_speed_timer_interval(update_speed) else {
                        return;
                    };
                    if let Err(error) = self.replace_update_timer(timer_delay) {
                        record_win32_error("update timer replacement", error);
                        return;
                    }

                    self.options.update_speed = update_speed;
                    self.options.timer_interval = timer_delay;
                    self.update_menu_states();
                }
                IDM_REFRESH => {
                    self.on_timer(hwnd, true);
                }
                IDM_ABOUT => {
                    let title = to_wide_null(&self.strings.app_title);
                    let icon = load_icon_resource(
                        MAIN_ICON_RESOURCE,
                        0,
                        0,
                        LR_DEFAULTCOLOR | LR_DEFAULTSIZE,
                    );
                    if !icon.is_null() {
                        ShellAboutW(hwnd, title.as_ptr(), null(), icon);
                        destroy_icon_handle(icon);
                    }
                }
                IDM_TASK_CASCADE
                | IDM_TASK_MINIMIZE
                | IDM_TASK_MAXIMIZE
                | IDM_TASK_TILEHORZ
                | IDM_TASK_TILEVERT
                | IDM_TASK_BRINGTOFRONT => {
                    let task_hwnd = self.pages[TASK_PAGE].hwnd();
                    if !task_hwnd.is_null() {
                        SendMessageW(task_hwnd, WM_COMMAND, command_id as usize, 0);
                    }
                }
                IDM_PROCCOLS
                | IDM_AFFINITY
                | IDM_PROC_DEBUG
                | IDM_PROC_TERMINATE
                | IDM_PROC_ENDTREE
                | IDM_PROC_OPENFILELOCATION
                | IDM_PROC_REALTIME
                | IDM_PROC_HIGH
                | IDM_PROC_ABOVENORMAL
                | IDM_PROC_NORMAL
                | IDM_PROC_BELOWNORMAL
                | IDM_PROC_LOW => {
                    if self.options.current_page == PROC_PAGE as i32 {
                        self.pages[PROC_PAGE]
                            .handle_process_command(command_id, Some(&mut self.options));
                    } else {
                        MessageBeep(0);
                    }
                }
                IDM_SHOWDOMAINNAMES | IDM_SENDMESSAGE | IDM_DISCONNECT | IDM_LOGOFF => {
                    if self.options.current_page == USER_PAGE as i32 {
                        let handled = self.pages[USER_PAGE].handle_user_command(command_id);
                        if handled {
                            self.update_menu_states();
                        }
                    } else {
                        MessageBeep(0);
                    }
                }
                IDM_RUN => {
                    if let Err(error) = self.show_run_dialog() {
                        record_hresult_error("system Run dialog", error);
                        self.show_hresult_failure(text(TextKey::RunTitle), error);
                    }
                }
                IDM_HELP => {
                    self.show_help(hwnd);
                }
                _ => {}
            }
        }
    }

    fn switch_tabs(&mut self, move_forward: bool) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let current_index = self.options.current_page.max(0) as usize;
            let next_index = if move_forward {
                (current_index + 1) % self.pages.len()
            } else if current_index == 0 {
                self.pages.len() - 1
            } else {
                current_index - 1
            };

            let tabs_hwnd = GetDlgItem(self.main_hwnd, IDC_TABS);
            SendMessageW(tabs_hwnd, TCM_SETCURSEL, next_index, 0);
            if let Err(error) = self.activate_page(next_index) {
                record_win32_error("page activation", error);
                MessageBeep(0);
            }
        }
    }

    fn record_window_rect(&mut self, hwnd: HWND) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 只有在初始位置已经应用过之后，后续移动/缩放才应该回写配置，
            // 否则会把对话框默认位置误记成用户偏好。
            if !self.already_applied_initial_position {
                return;
            }

            let mut placement = WINDOWPLACEMENT {
                length: size_of::<WINDOWPLACEMENT>() as u32,
                ..zeroed()
            };
            if GetWindowPlacement(hwnd, &mut placement) != 0 {
                let mut rect = placement.rcNormalPosition;
                if active_page_uses_normal_minimum(
                    self.options.no_title(),
                    self.options.current_page,
                    self.pages.len(),
                ) {
                    let (rect_width, rect_height) = clamped_window_size(
                        width(&rect),
                        height(&rect),
                        self.min_width,
                        self.min_height,
                    );
                    rect.right = rect.left + rect_width;
                    rect.bottom = rect.top + rect_height;
                }
                self.options.window_rect = rect;
            }
        }
    }

    fn on_notify(&mut self, lparam: LPARAM) -> isize {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let header = &*(lparam as *const NMHDR);
            if header.idFrom as i32 == IDC_TABS && header.code == TCN_SELCHANGE {
                let tabs_hwnd = GetDlgItem(self.main_hwnd, IDC_TABS);
                let selected = SendMessageW(tabs_hwnd, TCM_GETCURSEL, 0, 0);
                let Ok(selected) = usize::try_from(selected) else {
                    return 0;
                };
                return match self.activate_page(selected) {
                    Ok(()) => 1,
                    Err(error) => {
                        record_win32_error("page activation", error);
                        MessageBeep(0);
                        0
                    }
                };
            }

            0
        }
    }

    fn on_find_process(&mut self, identity: ProcIdentity) -> isize {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // “转到进程”来自任务页，需要先切到进程页，再尝试把对应进程行选中并滚动到可见区域。
            let tabs_hwnd = GetDlgItem(self.main_hwnd, IDC_TABS);
            if tabs_hwnd.is_null() {
                MessageBeep(0);
                return 0;
            }

            SendMessageW(tabs_hwnd, TCM_SETCURSEL, PROC_PAGE, 0);
            match self.activate_page(PROC_PAGE) {
                Ok(()) => isize::from(self.pages[PROC_PAGE].find_process(identity)),
                Err(error) => {
                    record_win32_error("process page activation", error);
                    MessageBeep(0);
                    0
                }
            }
        }
    }

    fn shutdown(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 关闭顺序按“停定时器 -> 保存页面状态 -> 解绑菜单 -> 销毁页面资源”执行，
            // 避免主窗口继续引用页面即将释放的 HMENU。
            if self.timer_id != 0 {
                KillTimer(self.main_hwnd, self.timer_id);
                self.timer_id = 0;
            }
            self.system_sampler.stop();

            if self.options.current_page >= 0 {
                self.pages[self.options.current_page as usize].deactivate(&mut self.options);
            }
            if !self.menu.current_menu().is_null() {
                SetMenu(self.main_hwnd, null_mut());
                self.menu.clear_current_menu();
            }
            for page in self.pages.iter_mut() {
                page.destroy();
            }

            self.update_tray(NIM_DELETE, null_mut(), "");
            if let Err(error) = self.options.save() {
                record_win32_error("saving options", error);
            }

            self.tray.clear_icons();
            if !self.main_icon.is_null() {
                SendMessageW(self.main_hwnd, WM_SETICON, 1, 0);
                destroy_icon_handle(self.main_icon);
                self.main_icon = null_mut();
            }
            if !self.accelerator_table.is_null() {
                DestroyAcceleratorTable(self.accelerator_table);
                self.accelerator_table = null_mut();
            }

            PostQuitMessage(0);
        }
    }
}

fn active_page_index(current_page: i32, page_count: usize) -> Option<usize> {
    let index = usize::try_from(current_page).ok()?;
    (index < page_count).then_some(index)
}

fn is_active_page(current_page: i32, page_count: usize, page_index: usize) -> bool {
    active_page_index(current_page, page_count) == Some(page_index)
}

fn page_uses_normal_minimum(no_title: bool, page_index: usize) -> bool {
    !no_title || matches!(page_index, CPU_PAGE | GPU_PAGE)
}

fn active_page_uses_normal_minimum(no_title: bool, current_page: i32, page_count: usize) -> bool {
    active_page_index(current_page, page_count)
        .map(|page_index| page_uses_normal_minimum(no_title, page_index))
        .unwrap_or(true)
}

fn adjusted_tab_page_rect(tabs_hwnd: HWND, owner_hwnd: HWND) -> RECT {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // Tab 控件的客户区需要通过 `TCM_ADJUSTRECT` 扣掉页签边框后，才能得到真正的页面矩形。
        let mut page_rect = zeroed::<RECT>();
        GetClientRect(tabs_hwnd, &mut page_rect);
        SendMessageW(
            tabs_hwnd,
            TCM_ADJUSTRECT,
            0,
            &mut page_rect as *mut _ as LPARAM,
        );
        MapWindowPoints(tabs_hwnd, owner_hwnd, &mut page_rect as *mut _ as _, 2);
        page_rect
    }
}

fn framed_window_style(current_style: u32) -> u32 {
    // 把当前样式收敛到带标题栏/系统菜单的经典有框窗口形态。
    let preserved_style_bits = current_style & !(WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_DLGFRAME);
    preserved_style_bits | WS_TILEDWINDOW
}

fn borderless_window_style(framed_style: u32) -> u32 {
    // 从有框样式中剥离标题栏相关位，得到无标题模式。
    framed_style & !(WS_DLGFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX)
}

fn clamped_window_size(
    width_px: i32,
    height_px: i32,
    min_width: i32,
    min_height: i32,
) -> (i32, i32) {
    (width_px.max(min_width), height_px.max(min_height))
}

unsafe extern "system" fn main_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    unsafe {
        // 主窗口过程只做最薄的一层 Win32 消息路由，
        // 具体行为统一委托给 `App`，避免消息逻辑散落在全局回调里。
        if msg == WM_INITDIALOG {
            let app_ptr = lparam as *mut App;
            if app_ptr.is_null() {
                return 0;
            }
            set_window_userdata_ptr(hwnd, app_ptr);
            return (*app_ptr).on_init_dialog(hwnd);
        }

        let Some(mut application_ptr) = app_from_hwnd(hwnd) else {
            return 0;
        };
        let application = application_ptr.as_mut();

        if msg == WM_SIZE || msg == WM_MOVE {
            application.record_window_rect(hwnd);
        }

        match msg {
            WM_SIZE => {
                let width_px = (lparam & 0xFFFF) as i32;
                let height_px = ((lparam >> 16) & 0xFFFF) as i32;
                application.on_size(hwnd, wparam as u32, width_px, height_px);
                0
            }
            WM_TIMER => {
                application.on_timer(hwnd, false);
                0
            }
            PWM_SYSTEM_WORKER_COMPLETE => {
                application.handle_system_worker_completion();
                0
            }
            WM_COMMAND => {
                application.on_command(hwnd, (wparam & 0xFFFF) as u16);
                0
            }
            WM_NOTIFY => application.on_notify(lparam),
            WM_MENUSELECT => application.on_menu_select(wparam, lparam),
            WM_INITMENU => application.on_init_menu(),
            WM_FINDPROC => {
                let Ok(pid) = u32::try_from(wparam) else {
                    return 0;
                };
                let creation_time_100ns = lparam as u64;
                if pid == 0 || creation_time_100ns == 0 {
                    0
                } else {
                    application.on_find_process(ProcIdentity::new(pid, creation_time_100ns))
                }
            }
            PWM_INPOPUP => application.on_popup_state(wparam != 0),
            PWM_DEFERREDINIT => {
                if let Err(error) = application.tray.load_icons() {
                    record_win32_error("tray icon loading", error);
                }
                // 安全性: main HWND is live; icon and tray setup after deferred icon loading.
                let icon =
                    load_icon_resource(MAIN_ICON_RESOURCE, 0, 0, LR_DEFAULTCOLOR | LR_DEFAULTSIZE);
                if !icon.is_null() {
                    let old_owned_icon = application.main_icon;
                    SendMessageW(hwnd, WM_SETICON, 1, icon as LPARAM);
                    if !old_owned_icon.is_null() {
                        destroy_icon_handle(old_owned_icon);
                    }
                    application.main_icon = icon;
                } else {
                    let error = windows_sys::Win32::Foundation::GetLastError();
                    record_win32_error(
                        "main window icon loading",
                        if error == 0 { ERROR_GEN_FAILURE } else { error },
                    );
                }
                if let Some(first_icon) = application.tray.first_icon() {
                    application.update_tray(NIM_ADD, first_icon, "");
                }
                0
            }
            PWM_PREWARM_PAGE => {
                let page_index = wparam;
                application.prewarm_page(page_index);
                0
            }
            WM_GETMINMAXINFO => {
                if active_page_uses_normal_minimum(
                    application.options.no_title(),
                    application.options.current_page,
                    application.pages.len(),
                ) {
                    let info = &mut *(lparam as *mut MINMAXINFO);
                    info.ptMinTrackSize.x = application.min_width;
                    info.ptMinTrackSize.y = application.min_height;
                }
                0
            }
            PWM_TRAYICON => {
                application.on_tray_notification(lparam);
                0
            }
            PWM_ACTIVATE => {
                application.show_running_instance();
                PWM_ACTIVATE as isize
            }
            WM_NCHITTEST => {
                let mut result = DefWindowProcW(hwnd, msg, wparam, lparam);
                if application.options.no_title()
                    && result == HTCLIENT as isize
                    && IsZoomed(hwnd) == 0
                {
                    result = HTCAPTION as isize;
                }
                set_dialog_msg_result(hwnd, result);
                1
            }
            WM_RBUTTONDOWN | WM_NCRBUTTONDOWN => application.on_right_button_down(hwnd),
            WM_RBUTTONUP | WM_NCRBUTTONUP => application.on_right_button_up(hwnd),
            WM_NCLBUTTONDBLCLK => {
                // 仅在已处于无标题模式时才切换回普通模式
                if !application.options.no_title() {
                    return 0;
                }
                application.toggle_no_title_mode();
                0
            }
            WM_LBUTTONDBLCLK => {
                application.toggle_no_title_mode();
                0
            }
            WM_ENDSESSION => {
                if wparam != 0 {
                    DestroyWindow(hwnd);
                }
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                application.shutdown();
                set_window_userdata_ptr::<App>(hwnd, null_mut());
                0
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_page_index, active_page_uses_normal_minimum, clamped_window_size, is_active_page,
        page_uses_normal_minimum,
    };
    use crate::resource::{CPU_PAGE, GPU_PAGE, NUM_PAGES, PERF_PAGE};

    #[test]
    fn active_page_index_rejects_invalid_values() {
        assert_eq!(active_page_index(-1, 5), None);
        assert_eq!(active_page_index(5, 5), None);
        assert_eq!(active_page_index(2, 0), None);
    }

    #[test]
    fn active_page_index_accepts_current_visible_page() {
        assert_eq!(active_page_index(2, 5), Some(2));
        assert!(is_active_page(2, NUM_PAGES, PERF_PAGE));
        assert!(!is_active_page(1, NUM_PAGES, PERF_PAGE));
    }

    #[test]
    fn clamped_window_size_raises_both_dimensions_to_minimums() {
        assert_eq!(clamped_window_size(100, 80, 320, 240), (320, 240));
    }

    #[test]
    fn clamped_window_size_only_adjusts_undersized_dimension() {
        assert_eq!(clamped_window_size(500, 80, 320, 240), (500, 240));
        assert_eq!(clamped_window_size(100, 400, 320, 240), (320, 400));
    }

    #[test]
    fn clamped_window_size_preserves_valid_dimensions() {
        assert_eq!(clamped_window_size(500, 400, 320, 240), (500, 400));
    }

    #[test]
    fn framed_pages_always_use_the_normal_minimum() {
        assert!(page_uses_normal_minimum(false, PERF_PAGE));
        assert!(page_uses_normal_minimum(false, GPU_PAGE));
    }

    #[test]
    fn borderless_gpu_keeps_the_normal_minimum() {
        assert!(page_uses_normal_minimum(true, GPU_PAGE));
        assert!(active_page_uses_normal_minimum(
            true,
            GPU_PAGE as i32,
            NUM_PAGES
        ));
    }

    #[test]
    fn borderless_cpu_diagnostics_keeps_the_normal_minimum() {
        assert!(page_uses_normal_minimum(true, CPU_PAGE));
        assert!(active_page_uses_normal_minimum(
            true,
            CPU_PAGE as i32,
            NUM_PAGES
        ));
    }

    #[test]
    fn borderless_performance_page_can_still_use_compact_sizes() {
        assert!(!page_uses_normal_minimum(true, PERF_PAGE));
        assert!(!active_page_uses_normal_minimum(
            true,
            PERF_PAGE as i32,
            NUM_PAGES
        ));
    }

    #[test]
    fn invalid_active_page_keeps_the_safe_normal_minimum() {
        assert!(active_page_uses_normal_minimum(true, -1, NUM_PAGES));
        assert!(active_page_uses_normal_minimum(
            true,
            NUM_PAGES as i32,
            NUM_PAGES
        ));
    }
}
