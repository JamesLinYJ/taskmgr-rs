use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

// 应用页实现。
// 该模块枚举顶层窗口，将其映射为任务列表中的行，并提供切换、平铺、
// 层叠、最小化、结束任务等窗口级操作。
//
// 图标流动：
//   1. Worker 线程在工作站/桌面枚举期间通过 fetch_window_icon() 抓取窗口图标（后台线程）。
//   2. 图标句柄通过 mpsc channel 传递回 UI 线程。
//   3. UI 线程把大小图标写入同一个稳定槽位，并在 TaskEntry 中只记录一个槽位 ID。
//   4. 默认图标 (default.ico) 用于没有自定义图标的窗口，作为 ImageList 的索引 0。
//   5. 任务移除时，remove_stale_tasks() 把槽位放回空闲表；其它任务索引不会移动。
//
// 线程模型：
//   一个 BackgroundWorker 枚举窗口，另一个 BackgroundWorker 并行抓取图标。
//   UI 线程只提交请求和应用完整结果，采集失败不会覆盖上一轮可信列表。
//
// 缓存失效策略：
//   bitness_by_pid (HashMap) 在枚举窗口时按需填充并缓存，提升同进程多窗口的效率。
//   DirtyTaskColumns 作为列级脏标记，避免 ListView 全量重绘。
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc::TryRecvError;
use std::thread;

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_DATA, ERROR_RESOURCE_DATA_NOT_FOUND, HANDLE, HINSTANCE, HWND, LPARAM, RECT,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    EnumDesktopWindows, GetProcessWindowStation, GetThreadDesktop, GetUserObjectInformationW,
    UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Controls::{
    HIMAGELIST, ImageList_Create, ImageList_Destroy, ImageList_GetImageCount, ImageList_Remove,
    ImageList_ReplaceIcon, LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCFMT_LEFT, LVCOLUMNW,
    LVIF_IMAGE, LVIF_PARAM, LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW,
    LVM_DELETEALLITEMS, LVM_DELETECOLUMN, LVM_DELETEITEM, LVM_GETITEMCOUNT, LVM_GETNEXTITEM,
    LVM_GETSELECTEDCOUNT, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_REDRAWITEMS, LVM_SETIMAGELIST,
    LVM_SETITEMW, LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVNI_SELECTED,
    LVS_AUTOARRANGE, LVS_ICON, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SMALLICON, LVS_TYPEMASK,
    LVSIL_NORMAL, LVSIL_SMALL, NM_DBLCLK, NMHDR, NMLISTVIEW, NMLVDISPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, VK_CONTROL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CascadeWindows, CheckMenuRadioItem, CopyIcon, DeferWindowPos, DrawMenuBar,
    EnableMenuItem, EndDeferWindowPos, GCL_HICON, GCL_HICONSM, GetClassLongPtrW, GetClientRect,
    GetDesktopWindow, GetDlgItem, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, HICON, IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible,
    MB_ICONERROR, MB_OK, MDITILE_HORIZONTAL, MDITILE_VERTICAL, MF_BYCOMMAND, MF_DISABLED,
    MF_GRAYED, MessageBoxW, SMTO_ABORTIFHUNG, SMTO_NORMAL, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageTimeoutW, SendMessageW,
    SetForegroundWindow, SetMenuDefaultItem, SetWindowLongW, SetWindowPos, ShowWindow,
    ShowWindowAsync, TPM_RETURNCMD, TileWindows, TrackPopupMenuEx, WM_COMMAND, WM_GETICON,
    WM_SETREDRAW,
};
use windows_sys::core::BOOL;

use crate::assets::{DEFAULT_ICON_RESOURCE, load_icon_resource};
use crate::background_worker::BackgroundWorker;
use crate::language::{TextKey, text};
use crate::menus::build_popup_menu;
use crate::options::{Options, ViewMode};
use crate::process_identity::{
    ProcIdentity, query_process_identity_for_pid, query_process_is_32_bit,
};
use crate::resource::{
    IDC_CASCADE, IDC_ENDTASK, IDC_MAXIMIZE, IDC_MINIMIZE, IDC_SWITCHTO, IDC_TASKLIST, IDC_TILEHORZ,
    IDC_TILEVERT, IDM_DETAILS, IDM_LARGEICONS, IDM_RUN, IDM_SMALLICONS, IDM_TASK_BRINGTOFRONT,
    IDM_TASK_CASCADE, IDM_TASK_ENDTASK, IDM_TASK_FINDPROCESS, IDM_TASK_MAXIMIZE, IDM_TASK_MINIMIZE,
    IDM_TASK_SWITCHTO, IDM_TASK_TILEHORZ, IDM_TASK_TILEVERT, IDR_TASK_CONTEXT, IDR_TASKVIEW,
    PWM_TASK_WORKER_COMPLETE,
};
use crate::winutil::{
    append_32_bit_suffix, copy_text_to_callback_buffer, destroy_icon_handle,
    finish_list_view_update, record_win32_error, subclass_list_view, to_wide_null,
    window_rect_relative_to_page,
};
const TASK_COLUMNS: [TaskColumn; 4] = [
    // 应用程序页默认只展示经典任务管理器里的四列。
    TaskColumn::new(TextKey::TaskColumnTask, 250),
    TaskColumn::new(TextKey::TaskColumnStatus, 97),
    TaskColumn::new(TextKey::TaskColumnWinstation, 70),
    TaskColumn::new(TextKey::TaskColumnDesktop, 70),
];
const MAX_TASK_ICON_WORKERS: usize = 8;

const ACTIVE_COLUMNS: [TaskColumnId; 2] = [TaskColumnId::Name, TaskColumnId::Status];
// 图标拉取放在后台线程里，避免顶层窗口枚举时阻塞 UI。
const ICON_FETCH_TIMEOUT_MS: u32 = 100;
const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;
const ICON_SMALL2: usize = 2;
const DEFAULT_MARGIN: i32 = 8;
const TEXT_CALLBACK_WIDE: *mut u16 = -1isize as *mut u16;

// 外部函数 EndTask（user32），用于强制结束指定窗口的任务。
#[link(name = "user32")]
unsafe extern "system" {
    fn EndTask(hwnd: HWND, shutdown: BOOL, force: BOOL) -> BOOL;
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskColumnId {
    Name = 0,
    Status = 1,
    Winstation = 2,
    Desktop = 3,
}

#[derive(Clone, Copy)]
struct TaskColumn {
    // 任务页列定义比进程页更简单，只需要标题和宽度。
    title_key: TextKey,
    width: i32,
}

impl TaskColumn {
    const fn new(title_key: TextKey, width: i32) -> Self {
        Self { title_key, width }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct TaskIdentity {
    hwnd: isize,
    process: ProcIdentity,
    thread_id: u32,
}

impl TaskIdentity {
    fn hwnd(self) -> HWND {
        self.hwnd as HWND
    }
}

#[derive(Clone)]
pub struct TaskEntry {
    // `TaskEntry` 代表一个顶层窗口/任务，并附带图标索引和脏列状态。
    identity: TaskIdentity,
    pub title: String,
    display_title: String,
    title_lower: String,
    pub is_32_bit: Option<bool>,
    pub winstation: String,
    winstation_lower: String,
    pub desktop: String,
    desktop_lower: String,
    pub is_hung: bool,
    pub icon_slot: usize,
    icons_loaded: bool,
    pass_count: u64,
    dirty_columns: DirtyTaskColumns,
}

// 快照 worker 只采集顶层窗口基本信息；图标由独立 worker 处理。
struct WorkerTaskEntry {
    identity: TaskIdentity,
    title: String,
    is_32_bit: Option<bool>,
    winstation: String,
    desktop: String,
    is_hung: bool,
}

#[derive(Default)]
struct TaskSamplerCache {
    desktop_names: Option<(String, String)>,
    bitness_by_process: HashMap<ProcIdentity, bool>,
}

struct TaskWorkerSnapshot {
    tasks: Vec<WorkerTaskEntry>,
    row_error: Option<u32>,
}

type TaskWorkerResult = Result<TaskWorkerSnapshot, u32>;

#[derive(Clone, Copy)]
struct TaskIconRequest {
    identity: TaskIdentity,
    is_hung: bool,
}

struct TaskIconResult {
    identity: TaskIdentity,
    small_icon: isize,
    large_icon: isize,
}

struct TaskIconCompletion {
    requested_identities: Vec<TaskIdentity>,
    result: Result<Vec<TaskIconResult>, u32>,
}

impl TaskIconResult {
    fn take_small_icon(&mut self) -> HICON {
        let icon = self.small_icon as HICON;
        self.small_icon = 0;
        icon
    }

    fn take_large_icon(&mut self) -> HICON {
        let icon = self.large_icon as HICON;
        self.large_icon = 0;
        icon
    }
}

impl Drop for TaskIconResult {
    fn drop(&mut self) {
        if self.small_icon != 0 {
            destroy_icon_handle(self.small_icon as HICON);
        }
        if self.large_icon != 0 {
            destroy_icon_handle(self.large_icon as HICON);
        }
    }
}

#[derive(Default)]
struct TaskIconStore {
    small: HIMAGELIST,
    large: HIMAGELIST,
    default_small: HICON,
    default_large: HICON,
    free_slots: Vec<usize>,
}

impl TaskIconStore {
    fn initialize(&mut self) -> Result<(), u32> {
        let mut next = Self::default();
        unsafe {
            next.small = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0x21,
                1,
                1,
            );
            next.large = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0x21,
                1,
                1,
            );
            if next.small == 0 || next.large == 0 {
                return Err(last_error_or_gen_failure());
            }

            next.default_small = load_icon_resource(
                DEFAULT_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0,
            );
            next.default_large = load_icon_resource(
                DEFAULT_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0,
            );
            if next.default_small.is_null() || next.default_large.is_null() {
                return Err(last_error_or_gen_failure());
            }
            next.reset()?;
        }
        *self = next;
        Ok(())
    }

    fn small(&self) -> HIMAGELIST {
        self.small
    }

    fn large(&self) -> HIMAGELIST {
        self.large
    }

    fn allocate(&mut self, small_icon: HICON, large_icon: HICON) -> Result<usize, u32> {
        if small_icon.is_null() && large_icon.is_null() {
            return Ok(0);
        }
        let requested_slot = self.free_slots.last().copied();
        let appended = requested_slot.is_none();
        let target = match requested_slot {
            Some(slot) => match i32::try_from(slot) {
                Ok(slot) => slot,
                Err(_) => {
                    destroy_icon_handle(small_icon);
                    destroy_icon_handle(large_icon);
                    return Err(ERROR_INVALID_DATA);
                }
            },
            None => -1,
        };

        let small_index =
            match replace_owned_icon(self.small, target, small_icon, self.default_small) {
                Ok(index) => index,
                Err(error) => {
                    destroy_icon_handle(large_icon);
                    return Err(error);
                }
            };
        let large_index =
            match replace_owned_icon(self.large, target, large_icon, self.default_large) {
                Ok(index) => index,
                Err(error) => {
                    rollback_icon_slot(
                        self.small,
                        small_index,
                        self.default_small,
                        appended,
                        "task small icon rollback",
                    );
                    return Err(error);
                }
            };

        if small_index != large_index || requested_slot.is_some_and(|slot| slot != small_index) {
            rollback_icon_slot(
                self.small,
                small_index,
                self.default_small,
                appended,
                "task small icon rollback",
            );
            rollback_icon_slot(
                self.large,
                large_index,
                self.default_large,
                appended,
                "task large icon rollback",
            );
            return Err(ERROR_INVALID_DATA);
        }
        if requested_slot.is_some() {
            self.free_slots.pop();
        }
        Ok(small_index)
    }

    fn release(&mut self, slot: usize) -> Result<(), u32> {
        if slot == 0 {
            return Ok(());
        }
        let Ok(slot_i32) = i32::try_from(slot) else {
            return Err(ERROR_INVALID_DATA);
        };
        let small_result =
            unsafe { ImageList_ReplaceIcon(self.small, slot_i32, self.default_small) };
        let small_error = (small_result < 0).then(last_error_or_gen_failure);
        let large_result =
            unsafe { ImageList_ReplaceIcon(self.large, slot_i32, self.default_large) };
        let large_error = (large_result < 0).then(last_error_or_gen_failure);
        if let Some(error) = small_error.or(large_error) {
            return Err(error);
        }
        self.free_slots.push(slot);
        Ok(())
    }

    unsafe fn reset(&mut self) -> Result<(), u32> {
        unsafe {
            ImageList_Remove(self.small, -1);
            ImageList_Remove(self.large, -1);
            let small_index = ImageList_ReplaceIcon(self.small, -1, self.default_small);
            let large_index = ImageList_ReplaceIcon(self.large, -1, self.default_large);
            if small_index != 0 || large_index != 0 {
                let error = last_error_or_gen_failure();
                ImageList_Remove(self.small, -1);
                ImageList_Remove(self.large, -1);
                return Err(error);
            }
            self.free_slots.clear();
            Ok(())
        }
    }

    fn destroy(&mut self) {
        unsafe {
            if self.small != 0 {
                ImageList_Destroy(self.small);
                self.small = 0;
            }
            if self.large != 0 {
                ImageList_Destroy(self.large);
                self.large = 0;
            }
        }
        destroy_icon_handle(self.default_small);
        destroy_icon_handle(self.default_large);
        self.default_small = null_mut();
        self.default_large = null_mut();
        self.free_slots.clear();
    }
}

impl Drop for TaskIconStore {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl TaskEntry {
    fn status_text(&self) -> &'static str {
        // 任务状态在当前实现里只区分“响应”和“未响应”两种。
        if self.is_hung {
            text(TextKey::NotResponding)
        } else {
            text(TextKey::Running)
        }
    }
}

// 任务页列级脏标记位图，与进程页的 DirtyColumns 设计相同。
#[derive(Clone, Copy, Default)]
struct DirtyTaskColumns(u32);

impl DirtyTaskColumns {
    fn all() -> Self {
        Self(u32::MAX)
    }

    fn mark(&mut self, column_id: TaskColumnId) {
        self.0 |= 1u32 << column_id as u32;
    }

    fn any(self) -> bool {
        self.0 != 0
    }

    fn contains(self, column_id: TaskColumnId) -> bool {
        self.0 & (1u32 << column_id as u32) != 0
    }
}

pub struct TaskPageState {
    // 任务页状态对象持有窗口列表、图标列表以及与任务视图相关的排序/选择状态。
    hinstance: HINSTANCE,
    hwnd_page: HWND,
    main_hwnd: HWND,
    tasks: Vec<TaskEntry>,
    displayed_identities: Vec<TaskIdentity>,
    icons: TaskIconStore,
    selected_count: u32,
    current_view_mode: i32,
    minimize_on_use: bool,
    no_title: bool,
    paused: bool,
    sort_column: TaskColumnId,
    sort_direction: i32,
    pass_count: u64,
    snapshot_worker: Option<BackgroundWorker<isize, TaskWorkerResult>>,
    icon_worker: Option<BackgroundWorker<Vec<TaskIconRequest>, TaskIconCompletion>>,
    pending_icon_identities: HashSet<TaskIdentity>,
    collection_in_flight: bool,
    refresh_requested: bool,
    last_refresh_error: Option<u32>,
    last_row_error: Option<u32>,
    last_icon_error: Option<u32>,
}

impl Default for TaskPageState {
    fn default() -> Self {
        Self {
            hinstance: null_mut(),
            hwnd_page: null_mut(),
            main_hwnd: null_mut(),
            tasks: Vec::with_capacity(128),
            displayed_identities: Vec::with_capacity(128),
            icons: TaskIconStore::default(),
            selected_count: 0,
            current_view_mode: ViewMode::Details as i32,
            minimize_on_use: true,
            no_title: false,
            paused: false,
            sort_column: TaskColumnId::Name,
            sort_direction: 1,
            pass_count: 0,
            snapshot_worker: None,
            icon_worker: None,
            pending_icon_identities: HashSet::new(),
            collection_in_flight: false,
            refresh_requested: false,
            last_refresh_error: None,
            last_row_error: None,
            last_icon_error: None,
        }
    }
}

impl TaskPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn no_title(&self) -> bool {
        self.no_title
    }

    pub fn prepare_initialize(&mut self, hinstance: HINSTANCE, main_hwnd: HWND) -> Result<(), u32> {
        // 任务页真正创建窗口前，先把后台枚举线程和图标列表资源准备好，
        // 避免页面显示出来后才临时分配这些较重的对象。
        // 安全性: this pre-initialization runs on the UI thread and only creates resources owned
        // by this page state.
        self.hinstance = hinstance;
        self.main_hwnd = main_hwnd;
        self.start_worker_thread()?;
        self.icons.initialize()
    }

    pub fn handle_init_dialog(&mut self, hwnd_page: HWND) -> isize {
        // 页面窗口建立后，图标列表和 ListView 才能真正绑定到控件上。
        // 安全性: WM_INITDIALOG supplies the page HWND; all child-control messages stay within
        // this page and run synchronously on the UI thread.
        unsafe {
            self.hwnd_page = hwnd_page;

            let list_hwnd = self.list_hwnd();
            if !list_hwnd.is_null() {
                subclass_list_view(list_hwnd);
                SendMessageW(
                    list_hwnd,
                    LVM_SETIMAGELIST,
                    LVSIL_SMALL as usize,
                    self.icons.small(),
                );
                let current_style = GetWindowLongW(
                    list_hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                ) as u32;
                SetWindowLongW(
                    list_hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                    (current_style | LVS_SHOWSELALWAYS) as i32,
                );
                SetFocus(list_hwnd);
            }
        }
        0
    }

    pub fn complete_initialize(&mut self) -> Result<(), u32> {
        // 后置初始化统一负责“建列 -> 应用视图模式 -> 首次布局”；首次采样由页面激活
        // 或后台预热入口统一触发，避免初始化与激活连续排两轮 worker。
        if unsafe { ImageList_GetImageCount(self.icons.small()) } < 1
            || unsafe { ImageList_GetImageCount(self.icons.large()) } < 1
        {
            return Err(ERROR_RESOURCE_DATA_NOT_FOUND);
        }
        self.setup_columns()?;
        self.apply_view_mode(ViewMode::Details as i32);
        self.size_page();
        Ok(())
    }

    pub fn apply_options(&mut self, options: &Options) {
        // 任务页的运行期选项主要影响无标题模式、切换后最小化，以及列表视图样式。
        self.no_title = options.no_title();
        self.minimize_on_use = options.minimize_on_use();
        if self.current_view_mode != options.view_mode {
            self.apply_view_mode(options.view_mode);
        }
    }

    pub fn timer_event(&mut self, options: &Options, force: bool) {
        // 刷新任务列表时会先取后台采集结果，再做排序和最小重绘提交。
        self.apply_options(options);
        if force || !self.paused {
            self.refresh_tasks();
        }
    }

    pub fn destroy(&mut self) {
        // 安全性: destruction releases resources exclusively owned by this page state.
        unsafe {
            self.stop_worker_thread();
            let list_hwnd = self.list_hwnd();
            if !list_hwnd.is_null() {
                SendMessageW(list_hwnd, LVM_SETIMAGELIST, LVSIL_SMALL as usize, 0);
                SendMessageW(list_hwnd, LVM_SETIMAGELIST, LVSIL_NORMAL as usize, 0);
            }
            self.icons.destroy();
            self.tasks.clear();
            self.displayed_identities.clear();
        }
    }

    pub fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        // 任务页同样依赖 ListView 通知来驱动选择同步、双击切换和列表排序。
        // 安全性: task dialog proc forwards only WM_NOTIFY LPARAM values from Win32; each cast is
        // matched to the notification code before accessing the payload.
        unsafe {
            let notify_header = &*(lparam as *const NMHDR);
            match notify_header.code {
                code if code == LVN_GETDISPINFOW => {
                    let display_info = &mut *(lparam as *mut NMLVDISPINFOW);
                    self.fill_display_info(&mut display_info.item);
                    1
                }
                code if code == NM_DBLCLK => {
                    self.handle_command(IDC_SWITCHTO as u16);
                    1
                }
                code if code == LVN_ITEMCHANGED => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    if (notify.uChanged & LVIF_STATE) != 0 {
                        let selected_count = self.selected_count();
                        if selected_count != self.selected_count {
                            self.selected_count = selected_count;
                            self.update_ui_state();
                        }
                    }
                    1
                }
                code if code == LVN_COLUMNCLICK => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    let clicked = ACTIVE_COLUMNS
                        .get(notify.iSubItem as usize)
                        .copied()
                        .unwrap_or(TaskColumnId::Name);
                    if self.sort_column == clicked {
                        self.sort_direction *= -1;
                    } else {
                        self.sort_column = clicked;
                        self.sort_direction = -1;
                    }
                    self.resort_tasks();
                    self.refresh_tasks();
                    1
                }
                _ => 0,
            }
        }
    }

    pub fn handle_command(&mut self, command_id: u16) {
        // 任务页命令大多直接映射到窗口管理动作：
        // 切换、平铺、层叠、最小化、最大化、结束任务或跳转到进程页。
        // 安全性: commands are handled on the UI thread and operate only on HWNDs collected from
        // Win32 enumeration or this page's own controls.
        unsafe {
            match command_id {
                IDM_LARGEICONS | IDM_SMALLICONS | IDM_DETAILS | IDM_RUN => {
                    SendMessageW(self.main_hwnd, WM_COMMAND, command_id as usize, 0);
                }
                id if id == IDM_TASK_SWITCHTO || id == IDC_SWITCHTO as u16 => {
                    if let Some(hwnd) = self.selected_hwnds(true).first().copied() {
                        if IsIconic(hwnd) != 0 {
                            ShowWindow(hwnd, SW_RESTORE);
                        }
                        if SetForegroundWindow(hwnd) != 0 && self.minimize_on_use {
                            ShowWindow(self.main_hwnd, SW_MINIMIZE);
                            SetForegroundWindow(hwnd);
                        }
                    }
                }
                id if id == IDM_TASK_TILEHORZ || id == IDC_TILEHORZ as u16 => {
                    self.tile_selected(MDITILE_HORIZONTAL);
                }
                id if id == IDM_TASK_TILEVERT || id == IDC_TILEVERT as u16 => {
                    self.tile_selected(MDITILE_VERTICAL);
                }
                id if id == IDM_TASK_CASCADE || id == IDC_CASCADE as u16 => {
                    self.cascade_selected();
                }
                id if id == IDM_TASK_MINIMIZE || id == IDC_MINIMIZE as u16 => {
                    self.show_selected_windows(SW_MINIMIZE);
                }
                id if id == IDM_TASK_MAXIMIZE || id == IDC_MAXIMIZE as u16 => {
                    self.show_selected_windows(SW_MAXIMIZE);
                }
                IDM_TASK_BRINGTOFRONT => {
                    let hwnds = self.selected_hwnds(true);
                    self.ensure_not_minimized(&hwnds);
                    for hwnd in hwnds.iter().rev().copied() {
                        SetWindowPos(
                            hwnd,
                            windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE,
                        );
                    }
                    if let Some(first) = hwnds.first().copied() {
                        SetForegroundWindow(first);
                        SetForegroundWindow(self.main_hwnd);
                        let list_hwnd = self.list_hwnd();
                        if !list_hwnd.is_null() {
                            SetFocus(list_hwnd);
                        }
                    }
                }
                id if id == IDM_TASK_ENDTASK || id == IDC_ENDTASK as u16 => {
                    let force = (GetKeyState(i32::from(VK_CONTROL)) & (1 << 15)) != 0;
                    let mut first_error = None;
                    for hwnd in self.selected_hwnds(true) {
                        if EndTask(hwnd, 0, if force { 1 } else { 0 }) == 0 {
                            let error = last_error_or_gen_failure();
                            record_win32_error("ending task", error);
                            first_error.get_or_insert(error);
                        }
                    }
                    if let Some(error) = first_error {
                        let title = to_wide_null(text(TextKey::WarningTitle));
                        let body =
                            to_wide_null(&format!("{} {error}", text(TextKey::Win32ErrorPrefix)));
                        MessageBoxW(
                            self.hwnd_page,
                            body.as_ptr(),
                            title.as_ptr(),
                            MB_OK | MB_ICONERROR,
                        );
                    }
                }
                IDM_TASK_FINDPROCESS => {
                    if let Some(identity) = self.selected_task_identities(true).first().copied()
                        && window_matches_actionable_identity(identity)
                    {
                        SendMessageW(
                            self.main_hwnd,
                            crate::resource::WM_FINDPROC,
                            identity.process.pid as usize,
                            identity.process.creation_time_100ns as isize,
                        );
                    }
                }
                _ => {}
            }
        }

        self.paused = false;
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        // 没有选择项时显示“视图菜单”，有选择项时显示“窗口操作菜单”。
        // 安全性: popup construction and tracking are synchronous UI-thread operations; the menu
        // handle is destroyed before returning.
        unsafe {
            let has_selection = self.selected_count > 0;
            let selected_hwnds = self.selected_hwnds(true);
            let popup = if has_selection {
                build_popup_menu(IDR_TASK_CONTEXT)
            } else {
                build_popup_menu(IDR_TASKVIEW)
            };

            let popup = match popup {
                Ok(popup) => popup,
                Err(error) => {
                    record_win32_error("task popup menu creation", error);
                    return;
                }
            };
            let popup_handle = popup.as_raw();

            if !has_selection {
                let checked_id = match self.current_view_mode {
                    value if value == ViewMode::LargeIcon as i32 => IDM_LARGEICONS,
                    value if value == ViewMode::SmallIcon as i32 => IDM_SMALLICONS,
                    _ => IDM_DETAILS,
                };
                CheckMenuRadioItem(
                    popup_handle,
                    u32::from(IDM_LARGEICONS),
                    u32::from(IDM_DETAILS),
                    u32::from(checked_id),
                    MF_BYCOMMAND,
                );
            } else {
                SetMenuDefaultItem(popup_handle, u32::from(IDM_TASK_SWITCHTO), 0);
                if selected_hwnds.is_empty() {
                    for command_id in [
                        IDM_TASK_SWITCHTO,
                        IDM_TASK_BRINGTOFRONT,
                        IDM_TASK_MINIMIZE,
                        IDM_TASK_MAXIMIZE,
                        IDM_TASK_CASCADE,
                        IDM_TASK_TILEHORZ,
                        IDM_TASK_TILEVERT,
                        IDM_TASK_ENDTASK,
                        IDM_TASK_FINDPROCESS,
                    ] {
                        EnableMenuItem(
                            popup_handle,
                            u32::from(command_id),
                            MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                        );
                    }
                }
                if selected_hwnds.len() < 2 {
                    for command_id in [IDM_TASK_CASCADE, IDM_TASK_TILEHORZ, IDM_TASK_TILEVERT] {
                        EnableMenuItem(
                            popup_handle,
                            u32::from(command_id),
                            MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                        );
                    }
                }
            }

            self.paused = true;
            SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 1, 0);
            let command =
                TrackPopupMenuEx(popup_handle, TPM_RETURNCMD, x, y, self.hwnd_page, null());
            SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 0, 0);

            if command != 0 {
                self.handle_command(command as u16);
            } else {
                self.paused = false;
            }
        }
    }

    pub fn size_page(&self) {
        // 任务页布局规则与进程页类似：列表控件吃满剩余区域，右下角保留操作按钮。
        // 安全性: layout only reads/moves child controls owned by this page HWND.
        unsafe {
            let mut parent_rect = zeroed::<RECT>();
            GetClientRect(self.hwnd_page, &mut parent_rect);
            let master_hwnd = GetDlgItem(self.hwnd_page, i32::from(IDM_RUN));
            let list_hwnd = self.list_hwnd();
            if master_hwnd.is_null() || list_hwnd.is_null() {
                return;
            }

            let mut hdwp = BeginDeferWindowPos(10);
            if hdwp.is_null() {
                return;
            }

            let master_rect = window_rect_relative_to_page(master_hwnd, self.hwnd_page);
            let dx = (parent_rect.right - DEFAULT_MARGIN * 2) - master_rect.right;
            let dy = (parent_rect.bottom - DEFAULT_MARGIN * 2) - master_rect.bottom;

            let list_rect = window_rect_relative_to_page(list_hwnd, self.hwnd_page);
            let list_width = (master_rect.right - list_rect.left + dx).max(0);
            let list_height = (master_rect.top - list_rect.top + dy - DEFAULT_MARGIN).max(0);

            hdwp = DeferWindowPos(
                hdwp,
                list_hwnd,
                null_mut(),
                0,
                0,
                list_width,
                list_height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            if hdwp.is_null() {
                return;
            }

            for control_id in [IDC_SWITCHTO, IDC_ENDTASK, i32::from(IDM_RUN)] {
                let control_hwnd = GetDlgItem(self.hwnd_page, control_id);
                if control_hwnd.is_null() {
                    continue;
                }

                let control_rect = window_rect_relative_to_page(control_hwnd, self.hwnd_page);
                hdwp = DeferWindowPos(
                    hdwp,
                    control_hwnd,
                    null_mut(),
                    control_rect.left + dx,
                    control_rect.top + dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                if hdwp.is_null() {
                    return;
                }
            }

            EndDeferWindowPos(hdwp);
        }
    }

    fn list_hwnd(&self) -> HWND {
        // 安全性: this only queries a child HWND from this page dialog; null is allowed.
        unsafe { GetDlgItem(self.hwnd_page, IDC_TASKLIST) }
    }

    fn setup_columns(&self) -> Result<(), u32> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 任务页列是固定集合，所以建列时可以完全按静态定义重建。
            let list_hwnd = self.list_hwnd();
            if list_hwnd.is_null() {
                return Err(windows_sys::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE);
            }
            SendMessageW(list_hwnd, LVM_DELETEALLITEMS, 0, 0);
            while SendMessageW(list_hwnd, LVM_DELETECOLUMN, 0, 0) != 0 {}

            for (index, column_id) in ACTIVE_COLUMNS.iter().enumerate() {
                let column = TASK_COLUMNS[*column_id as usize];
                let title = text(column.title_key).to_string();
                let mut title_wide = to_wide_null(&title);
                let mut lv_column = LVCOLUMNW {
                    mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: LVCFMT_LEFT,
                    cx: column.width,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iSubItem: index as i32,
                    ..zeroed()
                };
                if SendMessageW(
                    list_hwnd,
                    LVM_INSERTCOLUMNW,
                    index,
                    &mut lv_column as *mut _ as LPARAM,
                ) == -1
                {
                    return Err(windows_sys::Win32::Foundation::ERROR_GEN_FAILURE);
                }
            }
            Ok(())
        }
    }

    fn apply_view_mode(&mut self, view_mode: i32) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 大图标/小图标/详细信息本质上是同一个 ListView 的不同 style 组合。
            self.current_view_mode = view_mode;

            let list_hwnd = self.list_hwnd();
            let current_style = GetWindowLongW(
                list_hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::GWL_STYLE,
            ) as u32;
            let new_style = (current_style & !LVS_TYPEMASK)
                | if view_mode == ViewMode::SmallIcon as i32 {
                    LVS_SMALLICON | LVS_AUTOARRANGE
                } else if view_mode == ViewMode::Details as i32 {
                    LVS_REPORT
                } else {
                    LVS_ICON | LVS_AUTOARRANGE
                };

            SetWindowLongW(
                list_hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                (new_style | LVS_SHOWSELALWAYS) as i32,
            );

            SendMessageW(
                list_hwnd,
                LVM_SETIMAGELIST,
                if view_mode == ViewMode::LargeIcon as i32 {
                    LVSIL_NORMAL as usize
                } else {
                    LVSIL_SMALL as usize
                },
                if view_mode == ViewMode::LargeIcon as i32 {
                    self.icons.large()
                } else {
                    self.icons.small()
                },
            );
            if !self.tasks.is_empty() {
                SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);
                for (index, task) in self.tasks.iter().enumerate() {
                    let mut item = LVITEMW {
                        mask: LVIF_IMAGE,
                        iItem: index as i32,
                        iImage: task.icon_slot as i32,
                        ..zeroed()
                    };
                    SendMessageW(list_hwnd, LVM_SETITEMW, 0, &mut item as *mut _ as LPARAM);
                }
                finish_list_view_update(list_hwnd);
            }
            DrawMenuBar(self.main_hwnd);
        }
    }

    fn refresh_tasks(&mut self) {
        self.drain_worker_results();
        self.drain_icon_worker_results();
        if self.collection_in_flight {
            self.refresh_requested = true;
            return;
        }

        self.refresh_requested = false;
        self.schedule_task_collection();
    }

    fn schedule_task_collection(&mut self) {
        let Some(worker) = self.snapshot_worker.as_ref() else {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };

        if let Err(error) = worker.submit(self.main_hwnd as isize, self.hwnd_page) {
            self.set_refresh_error(error);
            return;
        }

        self.collection_in_flight = true;
    }

    fn drain_worker_results(&mut self) {
        loop {
            let result = match self.snapshot_worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => return,
            };

            match result {
                Ok(result) => self.apply_task_worker_result(result),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.snapshot_worker = None;
                    self.collection_in_flight = false;
                    self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                    return;
                }
            }
        }
    }

    fn apply_task_worker_result(&mut self, result: TaskWorkerResult) {
        self.collection_in_flight = false;
        match result {
            Ok(snapshot) => {
                self.last_refresh_error = None;
                self.set_row_error(snapshot.row_error);
                self.apply_task_snapshot(snapshot.tasks);
            }
            Err(error) => self.set_refresh_error(error),
        }
    }

    fn drain_icon_worker_results(&mut self) {
        loop {
            let result = match self.icon_worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => return,
            };

            match result {
                Ok(completion) => self.apply_task_icon_completion(completion),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.icon_worker = None;
                    self.pending_icon_identities.clear();
                    self.set_icon_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                    return;
                }
            }
        }
    }

    fn apply_task_snapshot(&mut self, worker_tasks: Vec<WorkerTaskEntry>) {
        // 在替换列表前保存稳定窗口身份，HWND 被复用时不会把选择转移给新窗口。
        let selected_identities: HashSet<_> =
            self.selected_task_identities(true).into_iter().collect();
        let current_pass = self.pass_count;
        let mut sort_dirty = false;
        let mut icon_requests = Vec::new();
        let mut task_index_by_identity = HashMap::with_capacity(self.tasks.len());
        for (index, task) in self.tasks.iter().enumerate() {
            task_index_by_identity.insert(task.identity, index);
        }

        for worker_task in worker_tasks {
            let identity = worker_task.identity;
            if let Some(&index) = task_index_by_identity.get(&identity) {
                let changed = update_task_entry(&mut self.tasks[index], &worker_task, current_pass);
                sort_dirty |= changed.contains(self.sort_column);
                if !self.tasks[index].icons_loaded {
                    icon_requests.push(TaskIconRequest {
                        identity,
                        is_hung: worker_task.is_hung,
                    });
                }
            } else {
                let is_hung = worker_task.is_hung;
                self.tasks
                    .push(TaskEntry::from_worker(worker_task, current_pass));
                task_index_by_identity.insert(identity, self.tasks.len() - 1);
                sort_dirty = true;
                icon_requests.push(TaskIconRequest { identity, is_hung });
            }
        }

        sort_dirty |= self.remove_stale_tasks(current_pass);
        if sort_dirty {
            self.resort_tasks();
        }
        self.update_task_listview(&selected_identities);
        self.schedule_icon_collection(icon_requests);
        self.pass_count = self.pass_count.wrapping_add(1);
    }

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("task refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    fn set_row_error(&mut self, error: Option<u32>) {
        if let Some(error) = error
            && self.last_row_error != Some(error)
        {
            record_win32_error("task row sampling", error);
        }
        self.last_row_error = error;
    }

    fn set_icon_error(&mut self, error: u32) {
        if self.last_icon_error != Some(error) {
            record_win32_error("task icon refresh", error);
        }
        self.last_icon_error = Some(error);
    }

    fn schedule_icon_collection(&mut self, mut requests: Vec<TaskIconRequest>) {
        requests.retain(|request| self.pending_icon_identities.insert(request.identity));
        if requests.is_empty() {
            return;
        }

        let identities = requests
            .iter()
            .map(|request| request.identity)
            .collect::<Vec<_>>();
        let Some(worker) = self.icon_worker.as_ref() else {
            for identity in identities {
                self.pending_icon_identities.remove(&identity);
            }
            self.set_icon_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };

        if let Err(error) = worker.submit(requests, self.hwnd_page) {
            for identity in identities {
                self.pending_icon_identities.remove(&identity);
            }
            self.icon_worker = None;
            self.set_icon_error(error);
        }
    }

    fn apply_task_icon_completion(&mut self, completion: TaskIconCompletion) {
        for identity in completion.requested_identities {
            self.pending_icon_identities.remove(&identity);
        }

        match completion.result {
            Ok(results) => {
                self.last_icon_error = None;
                self.apply_task_icon_results(results);
            }
            Err(error) => self.set_icon_error(error),
        }
    }

    fn apply_task_icon_results(&mut self, results: Vec<TaskIconResult>) {
        let list_hwnd = self.list_hwnd();
        let index_by_identity = self
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| (task.identity, index))
            .collect::<HashMap<_, _>>();
        for mut result in results {
            let Some(&index) = index_by_identity.get(&result.identity) else {
                continue;
            };
            if !window_matches_identity(result.identity) {
                continue;
            }

            let icon_slot = match self
                .icons
                .allocate(result.take_small_icon(), result.take_large_icon())
            {
                Ok(slot) => slot,
                Err(error) => {
                    self.set_icon_error(error);
                    continue;
                }
            };
            let task = &mut self.tasks[index];
            task.icon_slot = icon_slot;
            task.icons_loaded = true;

            if !list_hwnd.is_null() {
                let mut item = LVITEMW {
                    mask: LVIF_IMAGE,
                    iItem: index as i32,
                    iImage: icon_slot as i32,
                    ..unsafe { zeroed() }
                };
                unsafe {
                    SendMessageW(list_hwnd, LVM_SETITEMW, 0, &mut item as *mut _ as LPARAM);
                    SendMessageW(list_hwnd, LVM_REDRAWITEMS, index, index as LPARAM);
                }
            }
        }
    }

    pub fn handle_worker_completion(&mut self) {
        self.drain_worker_results();
        self.drain_icon_worker_results();
        if self.refresh_requested && !self.collection_in_flight {
            self.refresh_requested = false;
            self.schedule_task_collection();
        }
    }

    fn resort_tasks(&mut self) {
        self.tasks.sort_by(|left, right| {
            compare_tasks(left, right, self.sort_column, self.sort_direction)
        });
    }

    fn start_worker_thread(&mut self) -> Result<(), u32> {
        // 顶层窗口枚举可能涉及跨窗口站和桌面切换，
        // 放到后台线程可以避免主线程在刷新时明显卡顿。
        if self.snapshot_worker.is_some() && self.icon_worker.is_some() {
            return Ok(());
        }

        if self.snapshot_worker.is_none() {
            let mut cache = TaskSamplerCache::default();
            self.snapshot_worker = Some(BackgroundWorker::spawn(
                "taskmgr-rs-task-sampler",
                PWM_TASK_WORKER_COMPLETE,
                move |main_hwnd: isize| collect_tasks_worker(main_hwnd, &mut cache),
            )?);
        }
        if self.icon_worker.is_none() {
            self.icon_worker = Some(BackgroundWorker::spawn(
                "taskmgr-rs-task-icons",
                PWM_TASK_WORKER_COMPLETE,
                collect_task_icons,
            )?);
        }
        Ok(())
    }

    fn stop_worker_thread(&mut self) {
        self.snapshot_worker = None;
        self.icon_worker = None;
        self.pending_icon_identities.clear();
        self.collection_in_flight = false;
        self.refresh_requested = false;
    }

    fn remove_stale_tasks(&mut self, current_pass: u64) -> bool {
        let previous_len = self.tasks.len();
        let mut released_slots = Vec::new();
        self.tasks.retain(|task| {
            if task.pass_count == current_pass {
                true
            } else {
                if task.icon_slot != 0 {
                    released_slots.push(task.icon_slot);
                }
                false
            }
        });
        released_slots.sort_unstable();
        released_slots.dedup();

        let mut release_error = None;
        for slot in released_slots {
            if let Err(error) = self.icons.release(slot) {
                release_error.get_or_insert(error);
            }
        }
        if let Some(error) = release_error {
            self.set_icon_error(error);
        }
        previous_len != self.tasks.len()
    }

    fn update_task_listview(&mut self, selected_identities: &HashSet<TaskIdentity>) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 更新 ListView 时先暂停重绘，批量完成替换/删除/插入后再统一恢复。
            let list_hwnd = self.list_hwnd();
            SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);

            let mut existing_count = SendMessageW(list_hwnd, LVM_GETITEMCOUNT, 0, 0) as usize;
            let common_count = existing_count.min(self.tasks.len());
            let select_first = self.pass_count == 0 && selected_identities.is_empty();

            for index in 0..common_count {
                let task = &self.tasks[index];
                if self.displayed_identities.get(index).copied() != Some(task.identity) {
                    self.replace_row(
                        list_hwnd,
                        index,
                        task,
                        selected_identities.contains(&task.identity),
                    );
                    self.tasks[index].dirty_columns = DirtyTaskColumns::default();
                } else if task.dirty_columns.any() {
                    SendMessageW(list_hwnd, LVM_REDRAWITEMS, index, index as LPARAM);
                    self.tasks[index].dirty_columns = DirtyTaskColumns::default();
                }
            }

            while existing_count > self.tasks.len() {
                existing_count -= 1;
                SendMessageW(list_hwnd, LVM_DELETEITEM, existing_count, 0);
            }

            for index in common_count..self.tasks.len() {
                let task = &self.tasks[index];
                self.insert_row(
                    list_hwnd,
                    index,
                    task,
                    selected_identities.contains(&task.identity) || (select_first && index == 0),
                );
                self.tasks[index].dirty_columns = DirtyTaskColumns::default();
            }

            self.displayed_identities.clear();
            self.displayed_identities
                .extend(self.tasks.iter().map(|task| task.identity));

            finish_list_view_update(list_hwnd);

            self.selected_count = self.selected_count();
            self.update_ui_state();
        }
    }

    fn insert_row(&self, list_hwnd: HWND, index: usize, task: &TaskEntry, selected: bool) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM | LVIF_IMAGE,
                iItem: index as i32,
                iSubItem: 0,
                pszText: TEXT_CALLBACK_WIDE,
                cchTextMax: 0,
                iImage: task.icon_slot as i32,
                lParam: task.identity.hwnd,
                ..zeroed()
            };
            if selected {
                item.mask |= LVIF_STATE;
                item.state = LVIS_SELECTED | LVIS_FOCUSED;
                item.stateMask = item.state;
            }
            SendMessageW(list_hwnd, LVM_INSERTITEMW, 0, &mut item as *mut _ as LPARAM);
        }
    }

    fn replace_row(&self, list_hwnd: HWND, index: usize, task: &TaskEntry, selected: bool) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM | LVIF_IMAGE | LVIF_STATE,
                iItem: index as i32,
                iSubItem: 0,
                pszText: TEXT_CALLBACK_WIDE,
                cchTextMax: 0,
                iImage: task.icon_slot as i32,
                lParam: task.identity.hwnd,
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: if selected {
                    LVIS_SELECTED | LVIS_FOCUSED
                } else {
                    0
                },
                ..zeroed()
            };
            SendMessageW(list_hwnd, LVM_SETITEMW, 0, &mut item as *mut _ as LPARAM);
            SendMessageW(list_hwnd, LVM_REDRAWITEMS, index, index as LPARAM);
        }
    }

    fn fill_display_info(&self, item: &mut LVITEMW) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if (item.mask & LVIF_TEXT) == 0
                || item.iItem < 0
                || item.pszText.is_null()
                || item.cchTextMax <= 0
            {
                return;
            }

            let task = self.tasks.get(item.iItem as usize);
            let Some(task) = task else {
                *item.pszText = 0;
                return;
            };
            let Some(column_id) = ACTIVE_COLUMNS.get(item.iSubItem as usize).copied() else {
                *item.pszText = 0;
                return;
            };

            let text = match column_id {
                TaskColumnId::Name => task.display_title.as_str(),
                TaskColumnId::Status => task.status_text(),
                TaskColumnId::Winstation => task.winstation.as_str(),
                TaskColumnId::Desktop => task.desktop.as_str(),
            };
            copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, text);
        }
    }

    fn selected_count(&self) -> u32 {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe { SendMessageW(self.list_hwnd(), LVM_GETSELECTEDCOUNT, 0, 0) as u32 }
    }

    fn update_ui_state(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let enabled = !self.selected_hwnds(true).is_empty();
            for control_id in [IDC_ENDTASK, IDC_SWITCHTO] {
                let hwnd = GetDlgItem(self.hwnd_page, control_id);
                if !hwnd.is_null() {
                    EnableWindow(hwnd, i32::from(enabled));
                }
            }
        }
    }

    fn selected_task_identities(&self, selected_only: bool) -> Vec<TaskIdentity> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if !selected_only {
                return self.tasks.iter().map(|task| task.identity).collect();
            }

            let list_hwnd = self.list_hwnd();
            let mut identities = Vec::new();
            let mut last_index = -1;
            loop {
                let next_index = SendMessageW(
                    list_hwnd,
                    LVM_GETNEXTITEM,
                    last_index as usize,
                    LVNI_SELECTED as LPARAM,
                ) as i32;
                if next_index < 0 {
                    break;
                }

                if let Some(task) = self.tasks.get(next_index as usize) {
                    identities.push(task.identity);
                }
                last_index = next_index;
            }
            identities
        }
    }

    fn selected_hwnds(&self, selected_only: bool) -> Vec<HWND> {
        self.selected_task_identities(selected_only)
            .into_iter()
            .filter(|identity| window_matches_actionable_identity(*identity))
            .map(TaskIdentity::hwnd)
            .collect()
    }

    fn ensure_not_minimized(&self, hwnds: &[HWND]) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            for hwnd in hwnds {
                if IsIconic(*hwnd) != 0 {
                    ShowWindow(*hwnd, SW_RESTORE);
                }
            }
        }
    }

    fn show_selected_windows(&self, cmd_show: i32) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            for hwnd in self.selected_hwnds(self.selected_count > 0) {
                ShowWindowAsync(hwnd, cmd_show);
            }
        }
    }

    fn tile_selected(&self, how: u32) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let hwnds = self.selected_hwnds(self.selected_count > 0);
            self.ensure_not_minimized(&hwnds);
            TileWindows(
                GetDesktopWindow(),
                how,
                null(),
                hwnds.len() as u32,
                hwnds.as_ptr(),
            );
        }
    }

    fn cascade_selected(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let hwnds = self.selected_hwnds(self.selected_count > 0);
            self.ensure_not_minimized(&hwnds);
            CascadeWindows(
                GetDesktopWindow(),
                0,
                null(),
                hwnds.len() as u32,
                hwnds.as_ptr(),
            );
        }
    }
}

fn collect_tasks_worker(main_hwnd: isize, cache: &mut TaskSamplerCache) -> TaskWorkerResult {
    // 应用程序页只展示当前交互桌面的顶层窗口。直接枚举 worker 所属桌面，避免把
    // Winlogon 等不可访问安全桌面误判成整轮采样失败。
    let TaskWorkerSnapshot { tasks, row_error } =
        collect_tasks_current_winsta_worker(main_hwnd as HWND, cache)?;
    let mut valid_tasks = Vec::with_capacity(tasks.len());
    // 首先提交轻量窗口快照；图标由独立 worker 补全，慢窗口不会再阻塞列表出现。
    for task in tasks {
        if !window_matches_identity(task.identity) {
            continue;
        }
        valid_tasks.push(task);
    }
    Ok(TaskWorkerSnapshot {
        tasks: valid_tasks,
        row_error,
    })
}

fn collect_task_icons(requests: Vec<TaskIconRequest>) -> TaskIconCompletion {
    let requested_identities = requests
        .iter()
        .map(|request| request.identity)
        .collect::<Vec<_>>();
    let result = collect_task_icons_parallel(&requests);
    TaskIconCompletion {
        requested_identities,
        result,
    }
}

fn collect_task_icons_parallel(requests: &[TaskIconRequest]) -> Result<Vec<TaskIconResult>, u32> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }

    // A bounded group prevents one unresponsive application from serializing icon discovery for
    // every other row while avoiding an unbounded thread per window.
    let worker_count = thread::available_parallelism()
        .map_err(io_error_code)
        .map(usize::from)?
        .min(MAX_TASK_ICON_WORKERS)
        .min(requests.len());
    let next_request = AtomicUsize::new(0);

    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            workers.push(scope.spawn(|| {
                let mut results = Vec::new();
                loop {
                    let index = next_request.fetch_add(1, AtomicOrdering::Relaxed);
                    let Some(request) = requests.get(index).copied() else {
                        break;
                    };
                    if !window_matches_identity(request.identity) {
                        continue;
                    }

                    let (small_icon, large_icon) =
                        fetch_window_icons(request.identity.hwnd(), request.is_hung);
                    if window_matches_identity(request.identity) {
                        results.push(TaskIconResult {
                            identity: request.identity,
                            small_icon: small_icon as isize,
                            large_icon: large_icon as isize,
                        });
                    } else {
                        if !small_icon.is_null() {
                            destroy_icon_handle(small_icon);
                        }
                        if !large_icon.is_null() {
                            destroy_icon_handle(large_icon);
                        }
                    }
                }
                results
            }));
        }

        let mut results = Vec::with_capacity(requests.len());
        let mut worker_failed = false;
        for worker in workers {
            let joined = worker.join();
            match joined {
                Ok(mut worker_results) => results.append(&mut worker_results),
                Err(_) => worker_failed = true,
            }
        }
        if worker_failed {
            Err(windows_sys::Win32::Foundation::ERROR_GEN_FAILURE)
        } else {
            Ok(results)
        }
    })
}

fn io_error_code(error: std::io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(windows_sys::Win32::Foundation::ERROR_GEN_FAILURE)
}

fn collect_tasks_current_winsta_worker(
    main_hwnd: HWND,
    cache: &mut TaskSamplerCache,
) -> TaskWorkerResult {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut tasks = Vec::<WorkerTaskEntry>::with_capacity(64);
        let mut seen_tasks = HashSet::new();
        let mut process_identities = HashMap::with_capacity(64);
        let desktop_handle = GetThreadDesktop(GetCurrentThreadId());
        if desktop_handle.is_null() {
            return Err(last_error_or_gen_failure());
        }
        if cache.desktop_names.is_none() {
            let window_station = GetProcessWindowStation();
            if window_station.is_null() {
                return Err(last_error_or_gen_failure());
            }
            cache.desktop_names = Some((
                current_user_object_name(window_station as HANDLE)?,
                current_user_object_name(desktop_handle as HANDLE)?,
            ));
        }
        let (winstation, desktop) = cache
            .desktop_names
            .as_ref()
            .cloned()
            .ok_or(ERROR_INVALID_DATA)?;
        let mut context = WindowEnumContext {
            tasks: &mut tasks as *mut Vec<WorkerTaskEntry>,
            seen_tasks: &mut seen_tasks as *mut HashSet<TaskIdentity>,
            bitness_by_process: &mut cache.bitness_by_process as *mut HashMap<ProcIdentity, bool>,
            process_identities: &mut process_identities
                as *mut HashMap<u32, Result<ProcIdentity, u32>>,
            row_error: None,
            main_hwnd,
            winstation,
            desktop,
        };
        let enumerated = EnumDesktopWindows(
            desktop_handle,
            Some(enum_window_proc),
            &mut context as *mut WindowEnumContext as LPARAM,
        );
        if enumerated == 0 {
            return Err(last_error_or_gen_failure());
        }
        let current_processes = tasks
            .iter()
            .map(|task| task.identity.process)
            .collect::<HashSet<_>>();
        cache
            .bitness_by_process
            .retain(|identity, _| current_processes.contains(identity));
        Ok(TaskWorkerSnapshot {
            tasks,
            row_error: context.row_error,
        })
    }
}

// 桌面级别的枚举上下文，传递给 enum_window_proc 回调。
struct WindowEnumContext {
    tasks: *mut Vec<WorkerTaskEntry>,
    seen_tasks: *mut HashSet<TaskIdentity>,
    bitness_by_process: *mut HashMap<ProcIdentity, bool>,
    process_identities: *mut HashMap<u32, Result<ProcIdentity, u32>>,
    row_error: Option<u32>,
    main_hwnd: HWND,
    winstation: String,
    desktop: String,
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        // 任务列表只关心可见、无 owner 的顶层窗口，并显式排除我们自己的主窗口。
        let context = &mut *(lparam as *mut WindowEnumContext);

        if !GetWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::GW_OWNER).is_null()
            || IsWindowVisible(hwnd) == 0
            || hwnd == context.main_hwnd
        {
            return 1;
        }

        let title = window_title(hwnd);
        if title.is_empty() || title.eq_ignore_ascii_case("Program Manager") {
            return 1;
        }

        let mut pid = 0u32;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 || thread_id == 0 {
            return 1;
        }
        let process_identities = &mut *context.process_identities;
        let process_result = if let Some(identity) = process_identities.get(&pid).copied() {
            identity
        } else {
            let identity = query_process_identity_for_pid(pid);
            process_identities.insert(pid, identity);
            identity
        };
        let process = match process_result {
            Ok(identity) => identity,
            Err(error) => {
                if !window_still_has_identity(hwnd, pid, thread_id) {
                    return 1;
                }
                context.row_error.get_or_insert(if error == 0 {
                    ERROR_INVALID_DATA
                } else {
                    error
                });
                return 1;
            }
        };
        if !process.is_verified() {
            context.row_error.get_or_insert(ERROR_INVALID_DATA);
            return 1;
        }
        let identity = TaskIdentity {
            hwnd: hwnd as isize,
            process,
            thread_id,
        };
        let seen_tasks = &mut *context.seen_tasks;
        if !seen_tasks.insert(identity) {
            return 1;
        }
        let bitness_by_process = &mut *context.bitness_by_process;
        let is_32_bit = if let Some(&cached) = bitness_by_process.get(&process) {
            Some(cached)
        } else {
            match query_process_is_32_bit(process) {
                Ok(detected) => {
                    bitness_by_process.insert(process, detected);
                    Some(detected)
                }
                Err(error) => {
                    if window_still_has_identity(hwnd, pid, thread_id) {
                        context.row_error.get_or_insert(if error == 0 {
                            ERROR_INVALID_DATA
                        } else {
                            error
                        });
                    }
                    None
                }
            }
        };
        let tasks = &mut *context.tasks;
        tasks.push(WorkerTaskEntry {
            identity,
            title,
            is_32_bit,
            winstation: context.winstation.clone(),
            desktop: context.desktop.clone(),
            is_hung: IsHungAppWindow(hwnd) != 0,
        });
        1
    }
}

unsafe fn window_still_has_identity(hwnd: HWND, pid: u32, thread_id: u32) -> bool {
    unsafe {
        if IsWindow(hwnd) == 0 {
            return false;
        }
        let mut current_pid = 0u32;
        let current_thread_id = GetWindowThreadProcessId(hwnd, &mut current_pid);
        current_thread_id == thread_id && current_pid == pid
    }
}

unsafe fn window_title(hwnd: HWND) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd);
        let Ok(length) = usize::try_from(length) else {
            return String::new();
        };
        if length == 0 {
            return String::new();
        }

        let capacity = length.saturating_add(1);
        if capacity <= 260 {
            let mut buffer = [0u16; 260];
            let actual = GetWindowTextW(hwnd, buffer.as_mut_ptr(), capacity as i32).max(0) as usize;
            String::from_utf16_lossy(&buffer[..actual.min(length)])
        } else {
            let Ok(capacity_i32) = i32::try_from(capacity) else {
                return String::new();
            };
            let mut buffer = vec![0u16; capacity];
            let actual = GetWindowTextW(hwnd, buffer.as_mut_ptr(), capacity_i32).max(0) as usize;
            String::from_utf16_lossy(&buffer[..actual.min(length)])
        }
    }
}

fn current_user_object_name(handle: HANDLE) -> Result<String, u32> {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 窗口站和桌面名都通过 `GetUserObjectInformationW(UOI_NAME)` 读取，
        // 这里统一封装成一个 UTF-16 -> Rust String 的助手。
        let mut needed = 0u32;
        GetUserObjectInformationW(handle, UOI_NAME, null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(last_error_or_gen_failure());
        }

        let mut buffer = vec![0u16; (needed as usize / size_of::<u16>()).max(1)];
        if GetUserObjectInformationW(
            handle,
            UOI_NAME,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        ) == 0
        {
            return Err(last_error_or_gen_failure());
        }

        let length = buffer
            .iter()
            .position(|&value| value == 0)
            .unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }
}

// 每种 WM_GETICON 类型最多查询一次，再分别选出大小图标，避免重复跨进程超时等待。
fn fetch_window_icons(hwnd: HWND, is_hung: bool) -> (HICON, HICON) {
    let (small2, big) = if is_hung {
        (null_mut(), null_mut())
    } else {
        (
            query_window_icon_source(hwnd, ICON_SMALL2),
            query_window_icon_source(hwnd, ICON_BIG),
        )
    };
    let small = if is_hung || (!small2.is_null() && !big.is_null()) {
        null_mut()
    } else {
        query_window_icon_source(hwnd, ICON_SMALL)
    };

    let mut small_source = [small2, small, big]
        .into_iter()
        .find(|icon| !icon.is_null())
        .unwrap_or(null_mut());
    let mut large_source = [big, small, small2]
        .into_iter()
        .find(|icon| !icon.is_null())
        .unwrap_or(null_mut());

    if small_source.is_null() || large_source.is_null() {
        let class_small = query_class_icon_source(hwnd, GCL_HICONSM);
        let class_large = query_class_icon_source(hwnd, GCL_HICON);
        if small_source.is_null() {
            small_source = if !class_small.is_null() {
                class_small
            } else {
                class_large
            };
        }
        if large_source.is_null() {
            large_source = if !class_large.is_null() {
                class_large
            } else {
                class_small
            };
        }
    }

    unsafe {
        (
            if small_source.is_null() {
                null_mut()
            } else {
                CopyIcon(small_source)
            },
            if large_source.is_null() {
                null_mut()
            } else {
                CopyIcon(large_source)
            },
        )
    }
}

// 通过 SendMessageTimeoutW(WM_GETICON) 查询窗口图标。
// 超时使用 SMTO_ABORTIFHUNG 防止阻塞在挂起窗口上。
fn query_window_icon_source(hwnd: HWND, icon_type: usize) -> HICON {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut result = 0usize;
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            icon_type,
            0,
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            ICON_FETCH_TIMEOUT_MS,
            &mut result,
        );
        result as HICON
    }
}

// 通过 GetClassLongPtrW 查询窗口类默认图标。
fn query_class_icon_source(hwnd: HWND, class_index: i32) -> HICON {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe { GetClassLongPtrW(hwnd, class_index) as HICON }
}

fn compare_tasks(
    left: &TaskEntry,
    right: &TaskEntry,
    sort_column: TaskColumnId,
    sort_direction: i32,
) -> Ordering {
    // 排序的最后兜底键是窗口句柄，保证同值情况下结果稳定，不会每轮刷新都乱跳。
    let ordering = match sort_column {
        TaskColumnId::Name => left.title_lower.cmp(&right.title_lower),
        TaskColumnId::Status => left.is_hung.cmp(&right.is_hung),
        TaskColumnId::Winstation => left.winstation_lower.cmp(&right.winstation_lower),
        TaskColumnId::Desktop => left.desktop_lower.cmp(&right.desktop_lower),
    };

    let ordering = if ordering == Ordering::Equal {
        (
            left.identity.hwnd,
            left.identity.process.pid,
            left.identity.process.creation_time_100ns,
            left.identity.thread_id,
        )
            .cmp(&(
                right.identity.hwnd,
                right.identity.process.pid,
                right.identity.process.creation_time_100ns,
                right.identity.thread_id,
            ))
    } else {
        ordering
    };

    if sort_direction < 0 {
        ordering.reverse()
    } else {
        ordering
    }
}

fn replace_owned_icon(
    imagelist: HIMAGELIST,
    target: i32,
    owned_icon: HICON,
    default_icon: HICON,
) -> Result<usize, u32> {
    let source = if owned_icon.is_null() {
        default_icon
    } else {
        owned_icon
    };
    let index = unsafe { ImageList_ReplaceIcon(imagelist, target, source) };
    destroy_icon_handle(owned_icon);
    if index < 0 {
        Err(last_error_or_gen_failure())
    } else {
        Ok(index as usize)
    }
}

fn rollback_icon_slot(
    imagelist: HIMAGELIST,
    slot: usize,
    default_icon: HICON,
    appended: bool,
    context: &str,
) {
    let Ok(slot) = i32::try_from(slot) else {
        record_win32_error(context, ERROR_INVALID_DATA);
        return;
    };
    let succeeded = unsafe {
        if appended {
            ImageList_Remove(imagelist, slot) != 0
        } else {
            ImageList_ReplaceIcon(imagelist, slot, default_icon) >= 0
        }
    };
    if !succeeded {
        record_win32_error(context, last_error_or_gen_failure());
    }
}

// 将工作线程采集的 WorkerTaskEntry 转换为 UI 线程的 TaskEntry。
impl TaskEntry {
    fn from_worker(worker: WorkerTaskEntry, pass_count: u64) -> Self {
        let WorkerTaskEntry {
            identity,
            title,
            is_32_bit,
            winstation,
            desktop,
            is_hung,
        } = worker;
        let display_title = task_display_title(&title, is_32_bit);
        Self {
            identity,
            title_lower: title.to_lowercase(),
            title,
            display_title,
            is_32_bit,
            winstation_lower: winstation.to_lowercase(),
            winstation,
            desktop_lower: desktop.to_lowercase(),
            desktop,
            is_hung,
            icon_slot: 0,
            icons_loaded: false,
            pass_count,
            dirty_columns: DirtyTaskColumns::all(),
        }
    }
}

fn update_task_entry(
    task: &mut TaskEntry,
    worker: &WorkerTaskEntry,
    pass_count: u64,
) -> DirtyTaskColumns {
    // 增量更新只标记真正变化的列，这样详细视图刷新时能减少不必要重绘。
    task.pass_count = pass_count;
    let mut changed = DirtyTaskColumns::default();

    if task.winstation != worker.winstation {
        task.winstation.clone_from(&worker.winstation);
        task.winstation_lower = worker.winstation.to_lowercase();
        mark_task_column_changed(task, &mut changed, TaskColumnId::Winstation);
    }
    if task.desktop != worker.desktop {
        task.desktop.clone_from(&worker.desktop);
        task.desktop_lower = worker.desktop.to_lowercase();
        mark_task_column_changed(task, &mut changed, TaskColumnId::Desktop);
    }
    let title_changed = task.title != worker.title;
    let bitness_changed = task.is_32_bit != worker.is_32_bit;
    if title_changed {
        task.title.clone_from(&worker.title);
        task.title_lower = worker.title.to_lowercase();
    }
    if bitness_changed {
        task.is_32_bit = worker.is_32_bit;
    }
    if title_changed || bitness_changed {
        task.display_title = task_display_title(&task.title, task.is_32_bit);
        mark_task_column_changed(task, &mut changed, TaskColumnId::Name);
    }
    if task.is_hung != worker.is_hung {
        task.is_hung = worker.is_hung;
        mark_task_column_changed(task, &mut changed, TaskColumnId::Status);
    }

    changed
}

fn task_display_title(title: &str, is_32_bit: Option<bool>) -> String {
    match is_32_bit {
        Some(true) => append_32_bit_suffix(title, true).into_owned(),
        Some(false) | None => title.to_string(),
    }
}

fn mark_task_column_changed(
    task: &mut TaskEntry,
    changed: &mut DirtyTaskColumns,
    column_id: TaskColumnId,
) {
    task.dirty_columns.mark(column_id);
    changed.mark(column_id);
}

fn window_matches_identity(identity: TaskIdentity) -> bool {
    unsafe {
        if !identity.process.is_verified() {
            return false;
        }
        let hwnd = identity.hwnd();
        if IsWindow(hwnd) == 0 {
            return false;
        }

        let mut process_id = 0u32;
        let thread_id = GetWindowThreadProcessId(hwnd, &mut process_id);
        if process_id != identity.process.pid || thread_id != identity.thread_id {
            return false;
        }

        query_process_identity_for_pid(process_id).is_ok_and(|current| current == identity.process)
    }
}

fn window_matches_actionable_identity(identity: TaskIdentity) -> bool {
    identity.process.is_verified() && window_matches_identity(identity)
}

fn last_error_or_gen_failure() -> u32 {
    let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    if error == 0 {
        windows_sys::Win32::Foundation::ERROR_GEN_FAILURE
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_custom_icons_share_the_default_slot() {
        let mut store = TaskIconStore::default();
        assert_eq!(store.allocate(null_mut(), null_mut()), Ok(0));
        assert!(store.free_slots.is_empty());
    }

    #[test]
    fn unverified_process_identity_is_never_actionable() {
        let identity = TaskIdentity {
            hwnd: 1,
            process: ProcIdentity::pid_only(1234),
            thread_id: 2,
        };
        assert!(!window_matches_actionable_identity(identity));
    }

    #[test]
    fn unknown_bitness_does_not_claim_a_process_architecture() {
        assert_eq!(task_display_title("Editor", None), "Editor");
        assert_eq!(
            task_display_title("Editor", None),
            task_display_title("Editor", Some(false))
        );
        assert_ne!(
            task_display_title("Editor", Some(true)),
            task_display_title("Editor", None)
        );
    }

    #[test]
    fn failed_task_worker_keeps_the_previous_pass_state() {
        let mut state = TaskPageState {
            collection_in_flight: true,
            pass_count: 9,
            ..TaskPageState::default()
        };

        state.apply_task_worker_result(Err(5));

        assert!(!state.collection_in_flight);
        assert_eq!(state.pass_count, 9);
        assert_eq!(state.last_refresh_error, Some(5));
    }
}
