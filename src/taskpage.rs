use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

// 应用页实现。
// 该模块枚举顶层窗口，将其映射为任务列表中的行，并提供切换、平铺、
// 层叠、最小化、结束任务等窗口级操作。
//
// 图标流动：
//   1. Worker 线程在工作站/桌面枚举期间通过 fetch_window_icon() 抓取窗口图标（后台线程）。
//   2. 图标句柄通过 mpsc channel 传递回 UI 线程。
//   3. UI 线程在 refresh_tasks() 中将图标加入 ImageList，并在 TaskEntry 中记录图标索引。
//   4. 默认图标 (default.ico) 用于没有自定义图标的窗口，作为 ImageList 的索引 0。
//   5. 任务移除时，remove_stale_tasks() 回收对应的 ImageList 槽位并调整剩余条目的图标索引。
//
// 线程模型：
//   WorkerCommand 工作线程在后台执行 collect_tasks_worker()（枚举窗口 + 抓取图标）。
//   UI 线程发起采集后继续处理窗口消息，完成消息到达后只消费最新结果。
//   线程退出时发送 Shutdown 命令并 join。
//
// 缓存失效策略：
//   hwnd_to_index (HashMap) 在每次排序或刷新后从头重建。
//   bitness_by_pid (HashMap) 在枚举窗口时按需填充并缓存，提升同进程多窗口的效率。
//   DirtyTaskColumns 作为列级脏标记，避免 ListView 全量重绘。
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{BOOL, HANDLE, HINSTANCE, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, EnumDesktopWindows, EnumDesktopsW, GetProcessWindowStation, GetThreadDesktop,
    GetUserObjectInformationW, OpenDesktopW, DESKTOP_ENUMERATE, DESKTOP_READOBJECTS, UOI_NAME,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Controls::{
    ImageList_Create, ImageList_Destroy, ImageList_Remove, ImageList_ReplaceIcon, HIMAGELIST,
    LVCFMT_LEFT, LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_IMAGE, LVIF_PARAM,
    LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS,
    LVM_DELETECOLUMN, LVM_DELETEITEM, LVM_GETITEMCOUNT, LVM_GETITEMW, LVM_GETNEXTITEM,
    LVM_GETSELECTEDCOUNT, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETIMAGELIST, LVM_SETITEMW,
    LVNI_SELECTED, LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVSIL_NORMAL, LVSIL_SMALL,
    LVS_AUTOARRANGE, LVS_ICON, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SMALLICON, LVS_TYPEMASK, NMHDR,
    NMLISTVIEW, NMLVDISPINFOW, NM_DBLCLK,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetKeyState, SetFocus, VK_CONTROL,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CascadeWindows, CheckMenuRadioItem, CopyIcon, DeferWindowPos, DrawMenuBar,
    EnableMenuItem, EndDeferWindowPos, GetClassLongPtrW, GetClientRect, GetDesktopWindow,
    GetDlgItem, GetWindow, GetWindowLongW, GetWindowThreadProcessId, InternalGetWindowText,
    IsHungAppWindow, IsIconic, IsWindowVisible, PostMessageW, SendMessageTimeoutW, SendMessageW,
    SetForegroundWindow, SetMenuDefaultItem, SetWindowLongW, SetWindowPos, ShowWindow,
    ShowWindowAsync, TileWindows, TrackPopupMenuEx, GCL_HICON, GCL_HICONSM, HICON,
    MDITILE_HORIZONTAL, MDITILE_VERTICAL, MF_BYCOMMAND, MF_DISABLED, MF_GRAYED, SMTO_ABORTIFHUNG,
    SMTO_NORMAL, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE,
    SW_RESTORE, TPM_RETURNCMD, WM_COMMAND, WM_GETICON, WM_SETREDRAW,
};

use crate::assets::{load_icon_resource, DEFAULT_ICON_RESOURCE};
use crate::language::{text, TextKey};
use crate::menus::build_popup_menu;
use crate::options::{Options, ViewMode};
use crate::resource::{
    IDC_CASCADE, IDC_ENDTASK, IDC_MAXIMIZE, IDC_MINIMIZE, IDC_SWITCHTO, IDC_TASKLIST, IDC_TILEHORZ,
    IDC_TILEVERT, IDM_DETAILS, IDM_LARGEICONS, IDM_RUN, IDM_SMALLICONS, IDM_TASK_BRINGTOFRONT,
    IDM_TASK_CASCADE, IDM_TASK_ENDTASK, IDM_TASK_FINDPROCESS, IDM_TASK_MAXIMIZE, IDM_TASK_MINIMIZE,
    IDM_TASK_SWITCHTO, IDM_TASK_TILEHORZ, IDM_TASK_TILEVERT, IDR_TASKVIEW, IDR_TASK_CONTEXT,
    PWM_TASK_REFRESH_COMPLETE,
};
use crate::winutil::{
    append_32_bit_suffix, copy_text_to_callback_buffer, destroy_icon_handle, destroy_menu_handle,
    finish_list_view_update_deferred, is_32_bit_process_pid, subclass_list_view, to_wide_null,
    widestr_ptr_to_string, window_rect_relative_to_page, ListViewDirtyRange,
};
const TASK_COLUMNS: [TaskColumn; 4] = [
    // 应用程序页默认只展示经典任务管理器里的四列。
    TaskColumn::new(TextKey::TaskColumnTask, 250),
    TaskColumn::new(TextKey::TaskColumnStatus, 97),
    TaskColumn::new(TextKey::TaskColumnWinstation, 70),
    TaskColumn::new(TextKey::TaskColumnDesktop, 70),
];

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

#[derive(Clone)]
pub struct TaskEntry {
    // `TaskEntry` 代表一个顶层窗口/任务，并附带图标索引和脏列状态。
    pub hwnd: HWND,
    pub title: String,
    title_lower: String,
    pub is_32_bit: bool,
    pub winstation: String,
    winstation_lower: String,
    pub desktop: String,
    desktop_lower: String,
    pub is_hung: bool,
    pub small_icon: usize,
    pub large_icon: usize,
    pass_count: u64,
    dirty_columns: DirtyTaskColumns,
}

// 工作线程采集到的任务条目，包含顶层窗口基本信息和已抓取的图标句柄。
// small_icon / large_icon 在后台线程中以 isize（HICON）形式存储，
// 传递到 UI 线程后再通过 add_icon() 加入 ImageList 并转换为索引。
struct WorkerTaskEntry {
    // 后台线程负责窗口枚举和图标抓取；图标句柄通过 channel 安全传递回 UI 线程。
    hwnd: isize,
    title: String,
    is_32_bit: bool,
    winstation: String,
    desktop: String,
    is_hung: bool,
    small_icon: isize,
    large_icon: isize,
}

enum WorkerCommand {
    // 后台线程当前只负责枚举任务窗口和有序退出。
    Collect {
        seq: u64,
        main_hwnd: isize,
        notify_hwnd: isize,
    },
    Shutdown,
}

struct WorkerResult {
    seq: u64,
    tasks: Vec<WorkerTaskEntry>,
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
}

pub struct TaskPageState {
    // 任务页状态对象持有窗口列表、图标列表以及与任务视图相关的排序/选择状态。
    hinstance: HINSTANCE,
    hwnd_page: HWND,
    main_hwnd: HWND,
    tasks: Vec<TaskEntry>,
    hwnd_to_index: HashMap<isize, usize>,
    small_icons: HIMAGELIST,
    large_icons: HIMAGELIST,
    default_small_icon: HICON,
    default_large_icon: HICON,
    selected_count: u32,
    current_view_mode: i32,
    minimize_on_use: bool,
    no_title: bool,
    paused: bool,
    sort_column: TaskColumnId,
    sort_direction: i32,
    pass_count: u64,
    worker_sender: Option<Sender<WorkerCommand>>,
    worker_result_receiver: Option<Receiver<WorkerResult>>,
    worker_thread: Option<JoinHandle<()>>,
    refresh_inflight: bool,
    refresh_pending: bool,
    refresh_seq: u64,
    last_applied_refresh_seq: u64,
}

impl Default for TaskPageState {
    fn default() -> Self {
        Self {
            hinstance: null_mut(),
            hwnd_page: null_mut(),
            main_hwnd: null_mut(),
            tasks: Vec::with_capacity(128),
            hwnd_to_index: HashMap::new(),
            small_icons: 0,
            large_icons: 0,
            default_small_icon: null_mut(),
            default_large_icon: null_mut(),
            selected_count: 0,
            current_view_mode: ViewMode::Details as i32,
            minimize_on_use: true,
            no_title: false,
            paused: false,
            sort_column: TaskColumnId::Name,
            sort_direction: 1,
            pass_count: 0,
            worker_sender: None,
            worker_result_receiver: None,
            worker_thread: None,
            refresh_inflight: false,
            refresh_pending: false,
            refresh_seq: 0,
            last_applied_refresh_seq: 0,
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
        unsafe {
            self.hinstance = hinstance;
            self.main_hwnd = main_hwnd;
            self.start_worker_thread();

            self.small_icons = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0x21, // ILC_COLOR32 | ILC_MASK（32 位色深 + 掩码）
                1,
                1,
            );
            self.large_icons = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0x21, // ILC_COLOR32 | ILC_MASK（32 位色深 + 掩码）
                1,
                1,
            );
            if self.small_icons == 0 || self.large_icons == 0 {
                return Err(windows_sys::Win32::Foundation::GetLastError());
            }

            self.default_small_icon = load_icon_resource(
                DEFAULT_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0,
            );
            self.default_large_icon = load_icon_resource(
                DEFAULT_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0,
            );
            if self.default_small_icon.is_null() || self.default_large_icon.is_null() {
                return Err(windows_sys::Win32::Foundation::GetLastError());
            }

            Ok(())
        }
    }

    pub fn handle_init_dialog(&mut self, hwnd_page: HWND) -> isize {
        // 页面窗口建立后，图标列表和 ListView 才能真正绑定到控件上。
        // 安全性: WM_INITDIALOG supplies the page HWND; all child-control messages stay within
        // this page and run synchronously on the UI thread.
        unsafe {
            self.hwnd_page = hwnd_page;
            self.reset_imagelists();

            let list_hwnd = self.list_hwnd();
            if !list_hwnd.is_null() {
                subclass_list_view(list_hwnd);
                SendMessageW(
                    list_hwnd,
                    LVM_SETIMAGELIST,
                    LVSIL_SMALL as usize,
                    self.small_icons,
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
        // 后置初始化统一负责“建列 -> 应用视图模式 -> 首次采样 -> 首次布局”。
        self.setup_columns()?;
        self.apply_view_mode(ViewMode::Details as i32);
        self.refresh_tasks();
        self.size_page();
        Ok(())
    }

    pub fn apply_options(&mut self, options: &Options) {
        // 任务页的运行期选项主要影响无标题模式、切换后最小化，以及列表视图样式。
        self.no_title = options.no_title();
        self.minimize_on_use = options.minimize_on_use();
        if self.current_view_mode != options.view_mode {
            self.apply_view_mode(options.view_mode);
            self.refresh_tasks();
        }
    }

    pub fn timer_event(&mut self, options: &Options) {
        // 刷新任务列表时会先取后台采集结果，再做排序和最小重绘提交。
        self.apply_options(options);
        self.consume_refresh_results();
        if !self.paused {
            self.refresh_tasks();
        }
    }

    pub fn handle_refresh_complete(&mut self, _seq: u64) -> isize {
        self.consume_refresh_results();
        if self.refresh_pending && !self.paused {
            self.refresh_pending = false;
            self.refresh_tasks();
        }
        1
    }

    pub fn destroy(&mut self) {
        // 安全性: destruction releases resources exclusively owned by this page state.
        unsafe {
            self.stop_worker_thread();
            if self.small_icons != 0 {
                ImageList_Destroy(self.small_icons);
                self.small_icons = 0;
            }
            if self.large_icons != 0 {
                ImageList_Destroy(self.large_icons);
                self.large_icons = 0;
            }
            if !self.default_small_icon.is_null() {
                destroy_icon_handle(self.default_small_icon);
                self.default_small_icon = null_mut();
            }
            if !self.default_large_icon.is_null() {
                destroy_icon_handle(self.default_large_icon);
                self.default_large_icon = null_mut();
            }
            self.tasks.clear();
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
                    for hwnd in self.selected_hwnds(true) {
                        EndTask(hwnd, 0, if force { 1 } else { 0 });
                    }
                }
                IDM_TASK_FINDPROCESS => {
                    if let Some(hwnd) = self.selected_hwnds(true).first().copied() {
                        let mut pid = 0u32;
                        let thread_id = GetWindowThreadProcessId(hwnd, &mut pid);
                        if pid != 0 {
                            PostMessageW(
                                self.main_hwnd,
                                crate::resource::WM_FINDPROC,
                                thread_id as usize,
                                pid as isize,
                            );
                        }
                    }
                }
                _ => {
                    let _ = command_id;
                }
            }
        }

        self.paused = false;
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        // 没有选择项时显示“视图菜单”，有选择项时显示“窗口操作菜单”。
        // 安全性: popup construction and tracking are synchronous UI-thread operations; the menu
        // handle is destroyed before returning.
        unsafe {
            let selected_hwnds = self.selected_hwnds(true);
            let popup = if selected_hwnds.is_empty() {
                load_popup_menu(self.hinstance, IDR_TASKVIEW)
            } else {
                load_popup_menu(self.hinstance, IDR_TASK_CONTEXT)
            };

            if popup.is_null() {
                return;
            }

            if selected_hwnds.is_empty() {
                let checked_id = match self.current_view_mode {
                    value if value == ViewMode::LargeIcon as i32 => IDM_LARGEICONS,
                    value if value == ViewMode::SmallIcon as i32 => IDM_SMALLICONS,
                    _ => IDM_DETAILS,
                };
                CheckMenuRadioItem(
                    popup,
                    u32::from(IDM_LARGEICONS),
                    u32::from(IDM_DETAILS),
                    u32::from(checked_id),
                    MF_BYCOMMAND,
                );
            } else {
                SetMenuDefaultItem(popup, u32::from(IDM_TASK_SWITCHTO), 0);
                if selected_hwnds.len() < 2 {
                    for command_id in [IDM_TASK_CASCADE, IDM_TASK_TILEHORZ, IDM_TASK_TILEVERT] {
                        EnableMenuItem(
                            popup,
                            u32::from(command_id),
                            MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                        );
                    }
                }
            }

            self.paused = true;
            SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 1, 0);
            let command = TrackPopupMenuEx(popup, TPM_RETURNCMD, x, y, self.hwnd_page, null());
            SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 0, 0);
            destroy_menu_handle(popup);

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
            let hdwp = BeginDeferWindowPos(10);
            if hdwp.is_null() {
                return;
            }

            let master_hwnd = GetDlgItem(self.hwnd_page, i32::from(IDM_RUN));
            let list_hwnd = self.list_hwnd();
            if master_hwnd.is_null() || list_hwnd.is_null() {
                return;
            }

            let master_rect = window_rect_relative_to_page(master_hwnd, self.hwnd_page);
            let dx = (parent_rect.right - DEFAULT_MARGIN * 2) - master_rect.right;
            let dy = (parent_rect.bottom - DEFAULT_MARGIN * 2) - master_rect.bottom;

            let list_rect = window_rect_relative_to_page(list_hwnd, self.hwnd_page);
            let list_width = (master_rect.right - list_rect.left + dx).max(0);
            let list_height = (master_rect.top - list_rect.top + dy - DEFAULT_MARGIN).max(0);

            DeferWindowPos(
                hdwp,
                list_hwnd,
                null_mut(),
                0,
                0,
                list_width,
                list_height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );

            for control_id in [IDC_SWITCHTO, IDC_ENDTASK, i32::from(IDM_RUN)] {
                let control_hwnd = GetDlgItem(self.hwnd_page, control_id);
                if control_hwnd.is_null() {
                    continue;
                }

                let control_rect = window_rect_relative_to_page(control_hwnd, self.hwnd_page);
                DeferWindowPos(
                    hdwp,
                    control_hwnd,
                    null_mut(),
                    control_rect.left + dx,
                    control_rect.top + dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
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
                    self.large_icons
                } else {
                    self.small_icons
                },
            );
            DrawMenuBar(self.main_hwnd);
        }
    }

    fn refresh_tasks(&mut self) {
        // 正常刷新只投递后台采集请求，不再同步等待跨桌面枚举和图标抓取。
        self.consume_refresh_results();
        if self.refresh_inflight {
            self.refresh_pending = true;
            return;
        }

        if let Some(sender) = self.worker_sender.as_ref() {
            self.refresh_seq = self.refresh_seq.wrapping_add(1);
            let seq = self.refresh_seq;
            if sender
                .send(WorkerCommand::Collect {
                    seq,
                    main_hwnd: self.main_hwnd as isize,
                    notify_hwnd: self.hwnd_page as isize,
                })
                .is_ok()
            {
                self.refresh_inflight = true;
                return;
            }
        }

        self.refresh_seq = self.refresh_seq.wrapping_add(1);
        self.apply_refresh_result(WorkerResult {
            seq: self.refresh_seq,
            tasks: collect_tasks_current_winsta(self.main_hwnd),
        });
    }

    fn consume_refresh_results(&mut self) {
        let mut latest = None;
        if let Some(receiver) = self.worker_result_receiver.as_ref() {
            while let Ok(result) = receiver.try_recv() {
                latest = Some(result);
            }
        }

        if let Some(result) = latest {
            self.refresh_inflight = false;
            if result.seq > self.last_applied_refresh_seq {
                self.apply_refresh_result(result);
            }
        }
    }

    fn apply_refresh_result(&mut self, result: WorkerResult) {
        // 任务刷新采用“枚举窗口 -> 合并已有条目 -> 删除过期条目 -> 刷新 ListView”。
        // 这样可以尽量复用已有行，减少窗口切换时的闪烁。
        let current_pass = self.pass_count;
        let mut task_index_by_hwnd = HashMap::with_capacity(self.tasks.len());
        for (index, task) in self.tasks.iter().enumerate() {
            task_index_by_hwnd.insert(task.hwnd as isize, index);
        }

        for worker_task in result.tasks {
            let hwnd = worker_task.hwnd as HWND;
            if let Some(&index) = task_index_by_hwnd.get(&worker_task.hwnd) {
                update_task_entry(&mut self.tasks[index], &worker_task, current_pass);
            } else {
                let small_icon = add_icon(
                    self.small_icons,
                    worker_task.small_icon as HICON,
                    self.default_small_icon,
                );
                let large_icon = add_icon(
                    self.large_icons,
                    worker_task.large_icon as HICON,
                    self.default_large_icon,
                );
                self.tasks.push(TaskEntry::from_worker(
                    worker_task,
                    small_icon,
                    large_icon,
                    current_pass,
                ));
                task_index_by_hwnd.insert(hwnd as isize, self.tasks.len() - 1);
            }
        }

        self.remove_stale_tasks(current_pass);
        self.rebuild_hwnd_index_map();
        self.update_task_listview();
        self.last_applied_refresh_seq = result.seq;
        self.pass_count = self.pass_count.wrapping_add(1);
    }

    fn resort_tasks(&mut self) {
        self.tasks.sort_by(|left, right| {
            compare_tasks(left, right, self.sort_column, self.sort_direction)
        });
        self.rebuild_hwnd_index_map();
    }

    fn rebuild_hwnd_index_map(&mut self) {
        self.hwnd_to_index.clear();
        for (index, task) in self.tasks.iter().enumerate() {
            self.hwnd_to_index.insert(task.hwnd as isize, index);
        }
    }

    fn start_worker_thread(&mut self) {
        // 顶层窗口枚举可能涉及跨窗口站和桌面切换，
        // 放到后台线程可以避免主线程在刷新时明显卡顿。
        if self.worker_sender.is_some() {
            return;
        }

        let (command_tx, command_rx) = channel::<WorkerCommand>();
        let (result_tx, result_rx) = channel::<WorkerResult>();
        let worker = thread::spawn(move || {
            while let Ok(command) = command_rx.recv() {
                match command {
                    WorkerCommand::Collect {
                        seq,
                        main_hwnd,
                        notify_hwnd,
                    } => {
                        let tasks = collect_tasks_worker(main_hwnd);
                        if result_tx.send(WorkerResult { seq, tasks }).is_ok() {
                            unsafe {
                                PostMessageW(
                                    notify_hwnd as HWND,
                                    PWM_TASK_REFRESH_COMPLETE,
                                    seq as usize,
                                    0,
                                );
                            }
                        }
                    }
                    WorkerCommand::Shutdown => break,
                }
            }
        });

        self.worker_sender = Some(command_tx);
        self.worker_result_receiver = Some(result_rx);
        self.worker_thread = Some(worker);
    }

    // 发送 Shutdown 命令并等待工作线程退出。清理线程句柄和 channel。
    fn stop_worker_thread(&mut self) {
        if let Some(sender) = self.worker_sender.take() {
            let _ = sender.send(WorkerCommand::Shutdown);
        }

        if let Some(worker) = self.worker_thread.take() {
            let _ = worker.join();
        }
        self.worker_result_receiver = None;
        self.refresh_inflight = false;
        self.refresh_pending = false;
    }

    fn remove_stale_tasks(&mut self, current_pass: u64) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let mut removed_small = Vec::with_capacity(self.tasks.len());
            let mut removed_large = Vec::with_capacity(self.tasks.len());

            self.tasks.retain(|task| {
                if task.pass_count == current_pass {
                    true
                } else {
                    removed_small.push(task.small_icon);
                    removed_large.push(task.large_icon);
                    false
                }
            });

            normalize_removed_icon_indices(&mut removed_small);
            normalize_removed_icon_indices(&mut removed_large);
            remove_imagelist_indices(self.small_icons, &removed_small);
            remove_imagelist_indices(self.large_icons, &removed_large);

            if !removed_small.is_empty() || !removed_large.is_empty() {
                for task in &mut self.tasks {
                    task.small_icon = adjusted_icon_index(task.small_icon, &removed_small);
                    task.large_icon = adjusted_icon_index(task.large_icon, &removed_large);
                }
            }
        }
    }

    fn update_task_listview(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 只有行身份/数量变化时才暂停整表重绘；普通状态变化只重绘脏行。
            let list_hwnd = self.list_hwnd();

            let mut existing_count = SendMessageW(list_hwnd, LVM_GETITEMCOUNT, 0, 0) as usize;
            let common_count = existing_count.min(self.tasks.len());
            let mut current_hwnds = Vec::with_capacity(common_count);
            let mut order_changed = existing_count != self.tasks.len();

            for index in 0..common_count {
                let mut current_item = LVITEMW {
                    mask: LVIF_PARAM,
                    iItem: index as i32,
                    ..zeroed()
                };
                let current_hwnd = if SendMessageW(
                    list_hwnd,
                    LVM_GETITEMW,
                    0,
                    &mut current_item as *mut _ as LPARAM,
                ) != 0
                {
                    Some(current_item.lParam as HWND)
                } else {
                    None
                };
                if current_hwnd != Some(self.tasks[index].hwnd) {
                    order_changed = true;
                }
                current_hwnds.push(current_hwnd);
            }

            if order_changed {
                SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);
            }

            let mut dirty_rows = ListViewDirtyRange::default();

            for (index, current_hwnd) in current_hwnds.iter().copied().enumerate() {
                let task = &self.tasks[index];
                if current_hwnd != Some(task.hwnd) {
                    self.replace_row(list_hwnd, index, task);
                    self.tasks[index].dirty_columns = DirtyTaskColumns::default();
                    dirty_rows.mark(index);
                } else if task.dirty_columns.any() {
                    self.tasks[index].dirty_columns = DirtyTaskColumns::default();
                    dirty_rows.mark(index);
                }
            }

            while existing_count > self.tasks.len() {
                existing_count -= 1;
                SendMessageW(list_hwnd, LVM_DELETEITEM, existing_count, 0);
            }

            for index in common_count..self.tasks.len() {
                let task = &self.tasks[index];
                self.insert_row(list_hwnd, index, task);
                self.tasks[index].dirty_columns = DirtyTaskColumns::default();
                dirty_rows.mark(index);
            }

            if order_changed {
                finish_list_view_update_deferred(list_hwnd);
                InvalidateRect(list_hwnd, null_mut(), 0);
            }
            dirty_rows.redraw(list_hwnd, self.tasks.len());

            self.selected_count = self.selected_count();
            self.update_ui_state();
        }
    }

    fn insert_row(&self, list_hwnd: HWND, index: usize, task: &TaskEntry) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let image_index = if self.current_view_mode == ViewMode::LargeIcon as i32 {
                task.large_icon as i32
            } else {
                task.small_icon as i32
            };
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM | LVIF_IMAGE,
                iItem: index as i32,
                iSubItem: 0,
                pszText: TEXT_CALLBACK_WIDE,
                cchTextMax: 0,
                iImage: image_index,
                lParam: task.hwnd as isize,
                ..zeroed()
            };
            if index == 0 {
                item.mask |= LVIF_STATE;
                item.state = LVIS_SELECTED | LVIS_FOCUSED;
                item.stateMask = item.state;
            }
            SendMessageW(list_hwnd, LVM_INSERTITEMW, 0, &mut item as *mut _ as LPARAM);
        }
    }

    fn replace_row(&self, list_hwnd: HWND, index: usize, task: &TaskEntry) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let image_index = if self.current_view_mode == ViewMode::LargeIcon as i32 {
                task.large_icon as i32
            } else {
                task.small_icon as i32
            };
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM | LVIF_IMAGE,
                iItem: index as i32,
                iSubItem: 0,
                pszText: TEXT_CALLBACK_WIDE,
                cchTextMax: 0,
                iImage: image_index,
                lParam: task.hwnd as isize,
                ..zeroed()
            };
            SendMessageW(list_hwnd, LVM_SETITEMW, 0, &mut item as *mut _ as LPARAM);
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

            let task = if item.lParam != 0 {
                self.hwnd_to_index
                    .get(&item.lParam)
                    .and_then(|&idx| self.tasks.get(idx))
            } else {
                self.tasks.get(item.iItem as usize)
            };
            let Some(task) = task else {
                *item.pszText = 0;
                return;
            };
            let Some(column_id) = ACTIVE_COLUMNS.get(item.iSubItem as usize).copied() else {
                *item.pszText = 0;
                return;
            };

            let text = match column_id {
                TaskColumnId::Name => append_32_bit_suffix(&task.title, task.is_32_bit),
                TaskColumnId::Status => task.status_text().to_string(),
                TaskColumnId::Winstation => task.winstation.clone(),
                TaskColumnId::Desktop => task.desktop.clone(),
            };
            copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, &text);
        }
    }

    fn reset_imagelists(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            ImageList_Remove(self.small_icons, -1);
            ImageList_Remove(self.large_icons, -1);
            ImageList_ReplaceIcon(self.small_icons, -1, self.default_small_icon);
            ImageList_ReplaceIcon(self.large_icons, -1, self.default_large_icon);
        }
    }

    fn selected_count(&self) -> u32 {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe { SendMessageW(self.list_hwnd(), LVM_GETSELECTEDCOUNT, 0, 0) as u32 }
    }

    fn update_ui_state(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let enabled = self.selected_count > 0;
            for control_id in [IDC_ENDTASK, IDC_SWITCHTO] {
                let hwnd = GetDlgItem(self.hwnd_page, control_id);
                if !hwnd.is_null() {
                    EnableWindow(hwnd, i32::from(enabled));
                }
            }
        }
    }

    fn selected_hwnds(&self, selected_only: bool) -> Vec<HWND> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            if !selected_only {
                return self.tasks.iter().map(|task| task.hwnd).collect();
            }

            let list_hwnd = self.list_hwnd();
            let mut hwnds = Vec::new();
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

                let mut item = LVITEMW {
                    mask: LVIF_PARAM,
                    iItem: next_index,
                    ..zeroed()
                };
                if SendMessageW(
                    list_hwnd,
                    windows_sys::Win32::UI::Controls::LVM_GETITEMW,
                    0,
                    &mut item as *mut _ as LPARAM,
                ) != 0
                {
                    hwnds.push(item.lParam as HWND);
                }
                last_index = next_index;
            }
            hwnds
        }
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

// 从资源加载弹出菜单。当前始终通过 build_popup_menu 构造，忽略 hinstance。
fn load_popup_menu(
    hinstance: HINSTANCE,
    resource_id: u16,
) -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    let _ = hinstance;
    build_popup_menu(resource_id, usize::MAX).unwrap_or(null_mut())
}

fn collect_tasks_worker(main_hwnd: isize) -> Vec<WorkerTaskEntry> {
    // SetProcessWindowStation 是进程级设置，不能在后台线程调用，
    // 因此工作线程只枚举当前窗口站，跨窗口站的枚举在 UI 线程完成。
    let mut tasks = collect_tasks_current_winsta_worker(main_hwnd as HWND);
    // 图标抓取通过 SendMessageTimeoutW 完成，放在后台线程避免了 UI 卡顿。
    // SMTO_ABORTIFHUNG 确保单次不超过 100ms；挂起的窗口会被跳过。
    for task in &mut tasks {
        let hwnd = task.hwnd as HWND;
        task.small_icon = fetch_window_icon(hwnd, true) as isize;
        task.large_icon = fetch_window_icon(hwnd, false) as isize;
    }
    tasks
}

// 回退方案：在 UI 线程同步枚举当前窗口站的任务（不抓取图标）。
fn collect_tasks_current_winsta(main_hwnd: HWND) -> Vec<WorkerTaskEntry> {
    collect_tasks_current_winsta_worker(main_hwnd)
}

fn collect_tasks_current_winsta_worker(main_hwnd: HWND) -> Vec<WorkerTaskEntry> {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut tasks = Vec::with_capacity(64);
        let mut seen_hwnds = HashSet::new();
        let mut bitness_by_pid = HashMap::with_capacity(64);
        let winstation = current_user_object_name(GetProcessWindowStation() as HANDLE)
            .unwrap_or_else(|| "WinSta0".to_string());
        let mut context = WindowStationEnumContext {
            tasks: &mut tasks as *mut Vec<WorkerTaskEntry>,
            seen_hwnds: &mut seen_hwnds as *mut HashSet<isize>,
            bitness_by_pid: &mut bitness_by_pid as *mut HashMap<u32, bool>,
            main_hwnd,
            winstation,
        };
        enum_desktops_for_current_winsta(&mut context);
        tasks
    }
}

// 窗口站级别的枚举上下文，传递给 enum_desktop_proc 回调。
struct WindowStationEnumContext {
    tasks: *mut Vec<WorkerTaskEntry>,
    seen_hwnds: *mut HashSet<isize>,
    bitness_by_pid: *mut HashMap<u32, bool>,
    main_hwnd: HWND,
    winstation: String,
}

// 桌面级别的枚举上下文，传递给 enum_window_proc 回调。
struct WindowEnumContext {
    tasks: *mut Vec<WorkerTaskEntry>,
    seen_hwnds: *mut HashSet<isize>,
    bitness_by_pid: *mut HashMap<u32, bool>,
    main_hwnd: HWND,
    winstation: String,
    desktop: String,
}

unsafe extern "system" fn enum_desktop_proc(desktop_name: *const u16, lparam: LPARAM) -> BOOL {
    // 每个桌面都单独打开并枚举顶层窗口，最终合并到同一份任务列表。
    let context = &mut *(lparam as *mut WindowStationEnumContext);
    if desktop_name.is_null() {
        return 1;
    }

    let desktop = widestr_ptr_to_string(desktop_name);
    let desktop_handle = OpenDesktopW(desktop_name, 0, 0, DESKTOP_ENUMERATE | DESKTOP_READOBJECTS);
    if desktop_handle.is_null() {
        return 1;
    }

    let mut window_context = WindowEnumContext {
        tasks: context.tasks,
        seen_hwnds: context.seen_hwnds,
        bitness_by_pid: context.bitness_by_pid,
        main_hwnd: context.main_hwnd,
        winstation: context.winstation.clone(),
        desktop,
    };
    EnumDesktopWindows(
        desktop_handle,
        Some(enum_window_proc),
        &mut window_context as *mut WindowEnumContext as LPARAM,
    );
    CloseDesktop(desktop_handle);
    1
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // 任务列表只关心可见、无 owner 的顶层窗口，并显式排除我们自己的主窗口。
    let context = &mut *(lparam as *mut WindowEnumContext);

    if !GetWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::GW_OWNER).is_null()
        || IsWindowVisible(hwnd) == 0
        || hwnd == context.main_hwnd
    {
        return 1;
    }

    let mut stack_buf = [0u16; 260];
    let length = InternalGetWindowText(
        hwnd,
        stack_buf.as_mut_ptr(),
        i32::try_from(stack_buf.len()).expect("InternalGetWindowText buffer length fits in i32"),
    );
    let Ok(length) = usize::try_from(length) else {
        return 1;
    };

    let title = String::from_utf16_lossy(&stack_buf[..length]);
    if title.is_empty() || title.eq_ignore_ascii_case("Program Manager") {
        return 1;
    }

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    let seen_hwnds = &mut *context.seen_hwnds;
    if !seen_hwnds.insert(hwnd as isize) {
        return 1;
    }
    let bitness_by_pid = &mut *context.bitness_by_pid;
    let is_32_bit = if pid == 0 {
        false
    } else if let Some(&cached) = bitness_by_pid.get(&pid) {
        cached
    } else {
        let detected = is_32_bit_process_pid(pid);
        bitness_by_pid.insert(pid, detected);
        detected
    };
    let tasks = &mut *context.tasks;
    tasks.push(WorkerTaskEntry {
        hwnd: hwnd as isize,
        title,
        is_32_bit,
        winstation: context.winstation.clone(),
        desktop: context.desktop.clone(),
        is_hung: IsHungAppWindow(hwnd) != 0,
        small_icon: 0,
        large_icon: 0,
    });
    1
}

fn enum_desktops_for_current_winsta(context: &mut WindowStationEnumContext) {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 某些环境下按窗口站直接枚举桌面会失败；
        // 这时回退到当前线程桌面，保证至少能拿到当前桌面的任务窗口。
        if EnumDesktopsW(
            GetProcessWindowStation(),
            Some(enum_desktop_proc),
            context as *mut WindowStationEnumContext as LPARAM,
        ) == 0
        {
            let fallback_desktop =
                current_user_object_name(GetThreadDesktop(GetCurrentThreadId()) as HANDLE)
                    .unwrap_or_else(|| "Default".to_string());
            let mut fallback_context = WindowEnumContext {
                tasks: context.tasks,
                seen_hwnds: context.seen_hwnds,
                bitness_by_pid: context.bitness_by_pid,
                main_hwnd: context.main_hwnd,
                winstation: context.winstation.clone(),
                desktop: fallback_desktop,
            };
            EnumDesktopWindows(
                GetThreadDesktop(GetCurrentThreadId()),
                Some(enum_window_proc),
                &mut fallback_context as *mut WindowEnumContext as LPARAM,
            );
        }
    }
}

fn current_user_object_name(handle: HANDLE) -> Option<String> {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 窗口站和桌面名都通过 `GetUserObjectInformationW(UOI_NAME)` 读取，
        // 这里统一封装成一个 UTF-16 -> Rust String 的助手。
        let mut needed = 0u32;
        GetUserObjectInformationW(handle, UOI_NAME, null_mut(), 0, &mut needed);
        if needed == 0 {
            return None;
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
            return None;
        }

        let length = buffer
            .iter()
            .position(|&value| value == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..length]))
    }
}

// 获取窗口图标。先尝试 WM_GETICON（大/小图标），再回退到类图标。
// 句柄通过 CopyIcon 复制，调用方负责释放。
fn fetch_window_icon(hwnd: HWND, small: bool) -> HICON {
    // 图标获取只走窗口自身暴露的 HICON 链路：
    // 先查 WM_GETICON，再回退到类图标；同时复制句柄，确保后续释放是安全的。
    let preferred_icon_types: &[usize] = if small {
        &[ICON_SMALL2, ICON_SMALL, ICON_BIG]
    } else {
        &[ICON_BIG, ICON_SMALL, ICON_SMALL2]
    };

    for &icon_type in preferred_icon_types {
        let icon = query_window_icon(hwnd, icon_type);
        if !icon.is_null() {
            return icon;
        }
    }

    for &class_index in if small {
        &[GCL_HICONSM, GCL_HICON]
    } else {
        &[GCL_HICON, GCL_HICONSM]
    } {
        let icon = query_class_icon(hwnd, class_index);
        if !icon.is_null() {
            return icon;
        }
    }

    null_mut()
}

// 通过 SendMessageTimeoutW(WM_GETICON) 查询窗口图标。
// 超时使用 SMTO_ABORTIFHUNG 防止阻塞在挂起窗口上。
fn query_window_icon(hwnd: HWND, icon_type: usize) -> HICON {
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
        if result != 0 {
            CopyIcon(result as HICON)
        } else {
            null_mut()
        }
    }
}

// 通过 GetClassLongPtrW 查询窗口类默认图标。
fn query_class_icon(hwnd: HWND, class_index: i32) -> HICON {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let class_icon = GetClassLongPtrW(hwnd, class_index) as usize;
        if class_icon != 0 {
            CopyIcon(class_icon as HICON)
        } else {
            null_mut()
        }
    }
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
        (left.hwnd as usize).cmp(&(right.hwnd as usize))
    } else {
        ordering
    };

    if sort_direction < 0 {
        ordering.reverse()
    } else {
        ordering
    }
}

fn add_icon(imagelist: HIMAGELIST, icon: HICON, default_icon: HICON) -> usize {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 自己复制得到的图标句柄在加入 ImageList 后就可以释放；
        // 默认图标是共享资源，不应在这里销毁。
        let uses_default_icon = icon.is_null();
        let icon_handle = if uses_default_icon {
            default_icon
        } else {
            icon
        };
        let index = ImageList_ReplaceIcon(imagelist, -1, icon_handle);
        if !uses_default_icon {
            destroy_icon_handle(icon);
        }
        if index < 0 {
            0
        } else {
            index as usize
        }
    }
}

// 规范化删除的图标索引：排除索引 0（默认图标）、去重、排序。
fn normalize_removed_icon_indices(indices: &mut Vec<usize>) {
    indices.retain(|index| *index > 0);
    indices.sort_unstable();
    indices.dedup();
}

// 从 ImageList 中删除指定索引的图标。必须从大到小删除，避免索引错位。
unsafe fn remove_imagelist_indices(imagelist: HIMAGELIST, indices: &[usize]) {
    for &index in indices.iter().rev() {
        ImageList_Remove(imagelist, index as i32);
    }
}

// 调整条目图标索引：删除某些图标后，后续图标的索引需要前移。
fn adjusted_icon_index(index: usize, removed_indices: &[usize]) -> usize {
    if index == 0 {
        return 0;
    }

    let removed_before = removed_indices
        .iter()
        .take_while(|&&removed| removed < index)
        .count();
    index.saturating_sub(removed_before)
}

// 将工作线程采集的 WorkerTaskEntry 转换为 UI 线程的 TaskEntry。
// small_icon / large_icon 是 ImageList 中的索引而非原始 HICON。
impl TaskEntry {
    fn from_worker(
        worker: WorkerTaskEntry,
        small_icon: usize,
        large_icon: usize,
        pass_count: u64,
    ) -> Self {
        let title_lower = worker.title.to_lowercase();
        let winstation_lower = worker.winstation.to_lowercase();
        let desktop_lower = worker.desktop.to_lowercase();
        Self {
            hwnd: worker.hwnd as HWND,
            title: worker.title,
            title_lower,
            is_32_bit: worker.is_32_bit,
            winstation: worker.winstation,
            winstation_lower,
            desktop: worker.desktop,
            desktop_lower,
            is_hung: worker.is_hung,
            small_icon,
            large_icon,
            pass_count,
            dirty_columns: DirtyTaskColumns::all(),
        }
    }
}

fn update_task_entry(task: &mut TaskEntry, worker: &WorkerTaskEntry, pass_count: u64) {
    // 增量更新只标记真正变化的列，这样详细视图刷新时能减少不必要重绘。
    task.pass_count = pass_count;

    if task.winstation != worker.winstation {
        task.winstation.clone_from(&worker.winstation);
        task.winstation_lower = worker.winstation.to_lowercase();
        task.dirty_columns.mark(TaskColumnId::Winstation);
    }
    if task.desktop != worker.desktop {
        task.desktop.clone_from(&worker.desktop);
        task.desktop_lower = worker.desktop.to_lowercase();
        task.dirty_columns.mark(TaskColumnId::Desktop);
    }
    if task.title != worker.title {
        task.title.clone_from(&worker.title);
        task.title_lower = worker.title.to_lowercase();
        task.dirty_columns.mark(TaskColumnId::Name);
    }
    if task.is_32_bit != worker.is_32_bit {
        task.is_32_bit = worker.is_32_bit;
        task.dirty_columns.mark(TaskColumnId::Name);
    }
    if task.is_hung != worker.is_hung {
        task.is_hung = worker.is_hung;
        task.dirty_columns.mark(TaskColumnId::Status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_removed_icon_indices_keeps_sorted_unique_non_default_indices() {
        let mut indices = vec![5, 0, 2, 5, 1];
        normalize_removed_icon_indices(&mut indices);

        assert_eq!(indices, vec![1, 2, 5]);
    }

    #[test]
    fn adjusted_icon_index_accounts_for_lower_removed_indices() {
        let removed = vec![1, 3, 7];

        assert_eq!(adjusted_icon_index(0, &removed), 0);
        assert_eq!(adjusted_icon_index(2, &removed), 1);
        assert_eq!(adjusted_icon_index(8, &removed), 5);
    }
}
