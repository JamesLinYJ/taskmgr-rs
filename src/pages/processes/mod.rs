// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 进程页面
//
//   文件:       src/pages/processes/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

mod actions;
mod model;
mod sampler;

// 进程页实现。
// 这里负责采集进程列表、计算每轮刷新之间的增量数据、维护排序状态，
// 并处理结束进程、调试、设置优先级和亲和性等操作。
//
// 线程模型：
//   SingleFlightWorker 工作线程在后台执行 collect_process_entries() 采样，
//   UI 线程只提交刷新意图并异步提交结果；CPU delta 基线由 worker 独占。
//
// 缓存失效策略：
//   previous_samples (HashMap) 只在完整采样成功后由 worker 整体替换，供下轮
//   delta（CPU、内存、缺页）计算使用。失败不会推进基线。
//   DirtyColumns / DirtyRowRange 作为行/列级脏标记，避免整表重绘。
use std::collections::HashMap;
use std::mem::zeroed;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_DATA, HINSTANCE, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
};
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, CheckDlgButton, IsDlgButtonChecked, LVCF_FMT, LVCF_SUBITEM,
    LVCF_TEXT, LVCF_WIDTH, LVCFMT_LEFT, LVCFMT_RIGHT, LVCOLUMNW, LVIF_STATE, LVIF_TEXT,
    LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETECOLUMN, LVM_ENSUREVISIBLE,
    LVM_GETCOLUMNORDERARRAY, LVM_GETCOLUMNWIDTH, LVM_GETCOUNTPERPAGE, LVM_GETITEMCOUNT,
    LVM_GETNEXTITEM, LVM_GETTOPINDEX, LVM_INSERTCOLUMNW, LVM_REDRAWITEMS, LVM_SETITEMCOUNT,
    LVM_SETITEMSTATE, LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVNI_SELECTED,
    LVS_SHOWSELALWAYS, LVSICF_NOINVALIDATEALL, LVSICF_NOSCROLL, NMHDR, NMLISTVIEW, NMLVDISPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, EndDialog, GWL_STYLE, GetClientRect,
    GetCursorPos, GetDlgItem, GetWindowLongW, IDCANCEL, IDOK, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowLongW, TPM_RETURNCMD, TrackPopupMenuEx,
    WM_COMMAND, WM_INITDIALOG, WM_SETREDRAW,
};

use self::actions::load_debugger_path;
use self::model::{DirtyColumns, ProcEntry, column_text, compare_entries, update_process_entry};
use self::sampler::{ProcWorkerRequest, ProcWorkerResult, ProcWorkerState};
use crate::config::options::{ColumnId, Options, UpdateSpeed};
use crate::infrastructure::native::{
    copy_text_to_callback_buffer, finish_list_view_update, get_window_userdata, loword,
    record_win32_error, set_window_userdata, subclass_list_view, to_wide_null,
    window_rect_relative_to_page,
};
use crate::infrastructure::worker::{SingleFlightWorker, replace_pending};
use crate::system::process_identity::ProcIdentity;
use crate::ui::dialogs::dialog_box;
use crate::ui::localization::{TextKey, localize_dialog, text};
use crate::ui::resource_ids::*;
use crate::ui::runtime_menu::{MenuItemState, PopupMenu};

const PROCESS_COLUMNS: [ProcessColumn; NUM_COLUMN] = [
    // 列定义和旧版 Task Manager 保持兼容，既决定标题也决定默认宽度与对齐方式。
    ProcessColumn::new(TextKey::ProcessColumnImageName, LVCFMT_LEFT, 107),
    ProcessColumn::new(TextKey::ProcessColumnPid, LVCFMT_RIGHT, 50),
    ProcessColumn::new(TextKey::ProcessColumnUserName, LVCFMT_LEFT, 107),
    ProcessColumn::new(TextKey::ProcessColumnSessionId, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnCpu, LVCFMT_RIGHT, 35),
    ProcessColumn::new(TextKey::ProcessColumnCpuTime, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnMemoryUsage, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnMemoryUsageDelta, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPageFaults, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPageFaultsDelta, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnVirtualMemorySize, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnPagedPool, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnNonPagedPool, LVCFMT_RIGHT, 70),
    ProcessColumn::new(TextKey::ProcessColumnBasePriority, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnHandleCount, LVCFMT_RIGHT, 60),
    ProcessColumn::new(TextKey::ProcessColumnThreadCount, LVCFMT_RIGHT, 60),
];

const COLUMN_DIALOG_IDS: [i32; NUM_COLUMN] = [
    // “选择列”对话框里的勾选框与列定义保持一一对应。
    IDC_IMAGENAME,
    IDC_PID,
    IDC_USERNAME,
    IDC_SESSIONID,
    IDC_CPU,
    IDC_CPUTIME,
    IDC_MEMUSAGE,
    IDC_MEMUSAGEDIFF,
    IDC_PAGEFAULTS,
    IDC_PAGEFAULTSDIFF,
    IDC_COMMITCHARGE,
    IDC_PAGEDPOOL,
    IDC_NONPAGEDPOOL,
    IDC_BASEPRIORITY,
    IDC_HANDLECOUNT,
    IDC_THREADCOUNT,
];

const DEFAULT_MARGIN: i32 = 8;

#[derive(Clone, Copy)]
struct ProcessColumn {
    // `ProcessColumn` 描述一列在 UI 层的静态元数据。
    title_key: TextKey,
    fmt: i32,
    default_width: i32,
}

impl ProcessColumn {
    const fn new(title_key: TextKey, fmt: i32, default_width: i32) -> Self {
        Self {
            title_key,
            fmt,
            default_width,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct DirtyRowRange {
    // 列表刷新时只记录真正变更的行范围，避免整表重绘。
    start: Option<usize>,
    end: usize,
}

impl DirtyRowRange {
    fn mark(&mut self, index: usize) {
        self.start = Some(self.start.map_or(index, |current| current.min(index)));
        self.end = self.end.max(index);
    }

    unsafe fn redraw_visible(self, list_hwnd: HWND, item_count: usize) {
        unsafe {
            let Some(start) = self.start else {
                return;
            };
            if item_count == 0 {
                return;
            }

            let top = SendMessageW(list_hwnd, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
            let visible_count = SendMessageW(list_hwnd, LVM_GETCOUNTPERPAGE, 0, 0).max(0) as usize;
            if visible_count == 0 {
                return;
            }
            let visible_end = top
                .saturating_add(visible_count)
                .min(item_count.saturating_sub(1));
            let start = start.max(top);
            let end = self.end.min(visible_end);
            if start > end {
                return;
            }

            SendMessageW(list_hwnd, LVM_REDRAWITEMS, start, end as LPARAM);
        }
    }
}

#[derive(Default)]
struct ProcessStrings {
    warning: String,
    invalid_option: String,
    no_affinity_mask: String,
    kill: String,
    kill_tree: String,
    kill_tree_fail: String,
    kill_tree_fail_body: String,
    debug: String,
    prichange: String,
    cant_kill: String,
    cant_debug: String,
    cant_change_priority: String,
    cant_set_affinity: String,
    cant_open_file_location: String,
    priority_low: String,
    priority_below_normal: String,
    priority_normal: String,
    priority_above_normal: String,
    priority_high: String,
    priority_realtime: String,
    priority_unknown: String,
}

// “选择列”对话框的上下文，通过 LPARAM 传递给 dialog proc。
struct ColumnDialogContext {
    page: *mut ProcessPageState,
    options: *mut Options,
}

// 进程优先级枚举，对应 Win32 优先级类常量。
#[derive(Clone, Copy)]
enum ProcPriority {
    Low,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
    Realtime,
}

// 进程右键菜单/命令按钮支持的操作命令枚举。
#[derive(Clone, Copy)]
enum ProcCommand {
    Terminate,
    TerminateTree,
    Debug,
    OpenFileLocation,
    Affinity,
    SetPriority(ProcPriority),
    PickColumns,
}

impl ProcPriority {
    const fn command_id(self) -> u16 {
        match self {
            Self::Low => IDM_PROC_LOW,
            Self::BelowNormal => IDM_PROC_BELOWNORMAL,
            Self::Normal => IDM_PROC_NORMAL,
            Self::AboveNormal => IDM_PROC_ABOVENORMAL,
            Self::High => IDM_PROC_HIGH,
            Self::Realtime => IDM_PROC_REALTIME,
        }
    }

    const fn text_key(self) -> TextKey {
        match self {
            Self::Low => TextKey::Low,
            Self::BelowNormal => TextKey::BelowNormal,
            Self::Normal => TextKey::Normal,
            Self::AboveNormal => TextKey::AboveNormal,
            Self::High => TextKey::High,
            Self::Realtime => TextKey::Realtime,
        }
    }
}

impl ProcCommand {
    const fn command_id(self) -> u16 {
        match self {
            Self::Terminate => IDM_PROC_TERMINATE,
            Self::TerminateTree => IDM_PROC_ENDTREE,
            Self::Debug => IDM_PROC_DEBUG,
            Self::OpenFileLocation => IDM_PROC_OPENFILELOCATION,
            Self::Affinity => IDM_AFFINITY,
            Self::SetPriority(priority) => priority.command_id(),
            Self::PickColumns => IDM_PROCCOLS,
        }
    }

    fn from_command_id(command_id: u16, terminate_button_id: u16) -> Option<Self> {
        match command_id {
            id if id == terminate_button_id || id == IDM_PROC_TERMINATE => Some(Self::Terminate),
            IDM_PROC_ENDTREE => Some(Self::TerminateTree),
            id if id == IDC_DEBUG as u16 || id == IDM_PROC_DEBUG => Some(Self::Debug),
            IDM_PROC_OPENFILELOCATION => Some(Self::OpenFileLocation),
            IDM_AFFINITY => Some(Self::Affinity),
            IDM_PROC_LOW => Some(Self::SetPriority(ProcPriority::Low)),
            IDM_PROC_BELOWNORMAL => Some(Self::SetPriority(ProcPriority::BelowNormal)),
            IDM_PROC_NORMAL => Some(Self::SetPriority(ProcPriority::Normal)),
            IDM_PROC_ABOVENORMAL => Some(Self::SetPriority(ProcPriority::AboveNormal)),
            IDM_PROC_HIGH => Some(Self::SetPriority(ProcPriority::High)),
            IDM_PROC_REALTIME => Some(Self::SetPriority(ProcPriority::Realtime)),
            IDM_PROCCOLS => Some(Self::PickColumns),
            _ => None,
        }
    }
}

pub struct ProcessPageState {
    // 进程页状态对象持有采样缓存、排序设置、列配置以及当前选中项。
    hinstance: HINSTANCE,
    hwnd_page: HWND,
    main_hwnd: HWND,
    entries: Vec<ProcEntry>,
    displayed_identities: Vec<ProcIdentity>,
    active_columns: Vec<ColumnId>,
    selected_identity: Option<ProcIdentity>,
    pending_find_identity: Option<ProcIdentity>,
    sort_column: ColumnId,
    sort_direction: i32,
    paused: bool,
    confirmations: bool,
    no_title: bool,
    processor_count: usize,
    debugger_path: Option<String>,
    debugger_error: Option<u32>,
    strings: ProcessStrings,
    pass_count: u64,
    worker: Option<SingleFlightWorker<ProcWorkerRequest, ProcWorkerResult>>,
    last_refresh_error: Option<u32>,
    last_row_error: Option<u32>,
}

impl Default for ProcessPageState {
    fn default() -> Self {
        Self {
            hinstance: null_mut(),
            hwnd_page: null_mut(),
            main_hwnd: null_mut(),
            entries: Vec::with_capacity(128),
            displayed_identities: Vec::with_capacity(128),
            active_columns: Vec::with_capacity(NUM_COLUMN),
            selected_identity: None,
            pending_find_identity: None,
            sort_column: ColumnId::Pid,
            sort_direction: 1,
            paused: false,
            confirmations: true,
            no_title: false,
            processor_count: 1,
            debugger_path: None,
            debugger_error: None,
            strings: ProcessStrings::default(),
            pass_count: 0,
            worker: None,
            last_refresh_error: None,
            last_row_error: None,
        }
    }
}

impl ProcessPageState {
    // 创建持久采样线程。线程、通道和单在途语义由 SingleFlightWorker 统一负责。
    fn start_worker_thread(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        self.worker = Some(SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-process-sampler",
            PWM_PROC_WORKER_COMPLETE,
            replace_pending,
            || {
                let mut state = ProcWorkerState::default();
                move |request| state.collect(request)
            },
        )?);
        Ok(())
    }

    fn stop_worker_thread(&mut self) {
        self.worker = None;
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub unsafe fn no_title(&self) -> bool {
        self.no_title
    }

    pub unsafe fn initialize(
        &mut self,
        hinstance: HINSTANCE,
        hwnd_page: HWND,
        main_hwnd: HWND,
    ) -> Result<(), u32> {
        unsafe {
            // 进程页初始化主要做三件事：
            // 加载文案、准备调试器路径、并把 ListView 切到更适合频繁刷新的显示模式。
            self.hinstance = hinstance;
            self.hwnd_page = hwnd_page;
            self.main_hwnd = main_hwnd;
            self.load_strings();
            match load_debugger_path() {
                Ok(path) => {
                    self.debugger_path = path;
                    self.debugger_error = None;
                }
                Err(error) => {
                    self.debugger_path = None;
                    self.debugger_error = Some(error);
                    record_win32_error("debugger configuration", error);
                }
            }

            let list_hwnd = self.list_hwnd();
            if list_hwnd.is_null() {
                return Err(windows_sys::Win32::Foundation::ERROR_INVALID_WINDOW_HANDLE);
            }
            self.start_worker_thread()?;
            subclass_list_view(list_hwnd);
            let current_style = GetWindowLongW(list_hwnd, GWL_STYLE) as u32;
            SetWindowLongW(
                list_hwnd,
                GWL_STYLE,
                (current_style | LVS_SHOWSELALWAYS) as i32,
            );
            self.update_ui_state();
            Ok(())
        }
    }

    pub unsafe fn apply_options(&mut self, options: &Options, processor_count: usize) {
        unsafe {
            // 进程页的选项既影响行为，也影响列结构。
            // 当列配置发生变化时，直接重建列和数据比做局部修补更可靠。
            self.no_title = options.no_title();
            self.confirmations = options.confirmations();
            self.processor_count = processor_count.max(1);

            let desired_columns = columns_from_options(options);
            if desired_columns != self.active_columns {
                self.active_columns = desired_columns;
                let visible_columns = DirtyColumns::from_columns(&self.active_columns);
                for entry in &mut self.entries {
                    entry.rebuild_display_columns(&self.active_columns);
                    entry.dirty_columns = visible_columns;
                }
                self.setup_columns(options);
            }
        }
    }

    pub unsafe fn timer_event(&mut self, options: &Options, force: bool) {
        unsafe {
            // 每一轮刷新都走“采样 -> 合并旧状态 -> 排序/重绘”这条统一链路。
            self.paused = options.update_speed == UpdateSpeed::Paused as i32;
            if force || !self.paused {
                self.refresh_processes();
            }
        }
    }

    pub unsafe fn deactivate(&mut self, options: &mut Options) {
        unsafe {
            if let Err(error) = self.save_column_layout(options) {
                record_win32_error("process column layout persistence", error);
            }
        }
    }

    pub unsafe fn destroy(&mut self) {
        self.stop_worker_thread();
        self.entries.clear();
        self.displayed_identities.clear();
    }

    pub unsafe fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        unsafe {
            // ListView 处于 owner-data 风格，因此文本、排序和选择同步都靠通知消息驱动。
            let notify_header = &*(lparam as *const NMHDR);
            match notify_header.code {
                code if code == LVN_GETDISPINFOW => {
                    let display_info = &mut *(lparam as *mut NMLVDISPINFOW);
                    self.fill_display_info(&mut display_info.item);
                    1
                }
                code if code == LVN_ITEMCHANGED => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    if (notify.uChanged & LVIF_STATE) != 0 {
                        self.selected_identity = self.current_selected_identity();
                        self.update_ui_state();
                    }
                    1
                }
                code if code == LVN_COLUMNCLICK => {
                    let notify = &*(lparam as *const NMLISTVIEW);
                    let clicked = self
                        .active_columns
                        .get(notify.iSubItem as usize)
                        .copied()
                        .unwrap_or(ColumnId::Pid);
                    if self.sort_column == clicked {
                        self.sort_direction *= -1;
                    } else {
                        self.sort_column = clicked;
                        self.sort_direction = -1;
                    }
                    self.selected_identity =
                        self.current_selected_identity().or(self.selected_identity);
                    self.resort_entries();
                    self.rebuild_listview();
                    self.refresh_processes();
                    1
                }
                _ => 0,
            }
        }
    }

    // 将命令 ID 分派到具体的进程操作（结束、调试、优先级、亲和性等）。
    pub unsafe fn handle_command(&mut self, command_id: u16, options: Option<&mut Options>) {
        unsafe {
            let Some(command) = ProcCommand::from_command_id(command_id, IDC_TERMINATE as u16)
            else {
                return;
            };

            match command {
                ProcCommand::PickColumns => {
                    if let Some(options) = options {
                        self.pick_columns(options);
                    }
                }
                ProcCommand::Terminate => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.kill_process(identity);
                    }
                }
                ProcCommand::TerminateTree => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.kill_process_tree(identity);
                    }
                }
                ProcCommand::Debug => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.attach_debugger(identity);
                    }
                }
                ProcCommand::OpenFileLocation => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.open_file_location(identity);
                    }
                }
                ProcCommand::Affinity => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.set_affinity(identity);
                    }
                }
                ProcCommand::SetPriority(priority) => {
                    if let Some(identity) = self.current_selected_identity() {
                        self.set_priority(identity, priority);
                    }
                }
            }
        }
    }

    pub unsafe fn show_context_menu(&mut self, x: i32, y: i32) {
        unsafe {
            // 右键菜单会按当前选中进程和系统能力动态裁剪。
            self.selected_identity = self.current_selected_identity();
            let Some(entry) = self.selected_entry() else {
                return;
            };

            let popup = match self.build_context_menu(entry) {
                Ok(popup) => popup,
                Err(error) => {
                    record_win32_error("process popup menu creation", error);
                    return;
                }
            };

            self.paused = true;
            let mut cursor = POINT { x, y };
            if cursor.x == -1 && cursor.y == -1 {
                GetCursorPos(&mut cursor);
            }

            SendMessageW(self.main_hwnd, crate::ui::resource_ids::PWM_INPOPUP, 1, 0);
            let command = TrackPopupMenuEx(
                popup.as_raw(),
                TPM_RETURNCMD,
                cursor.x,
                cursor.y,
                self.hwnd_page,
                null(),
            );
            SendMessageW(self.main_hwnd, crate::ui::resource_ids::PWM_INPOPUP, 0, 0);
            self.paused = false;

            if command != 0 {
                self.handle_command(command as u16, None);
            }
        }
    }

    // 构造进程右键菜单，包含结束进程、调试、打开文件位置、优先级和亲和性子菜单。
    unsafe fn build_context_menu(&self, entry: &ProcEntry) -> Result<PopupMenu, u32> {
        let identity_verified = entry.identity.is_verified();
        let mut priority_menu = PopupMenu::new()?;
        let checked_priority = match entry.priority_class {
            value if value == IDLE_PRIORITY_CLASS => ProcPriority::Low.command_id(),
            value if value == BELOW_NORMAL_PRIORITY_CLASS => ProcPriority::BelowNormal.command_id(),
            value if value == ABOVE_NORMAL_PRIORITY_CLASS => ProcPriority::AboveNormal.command_id(),
            value if value == HIGH_PRIORITY_CLASS => ProcPriority::High.command_id(),
            value if value == REALTIME_PRIORITY_CLASS => ProcPriority::Realtime.command_id(),
            _ => ProcPriority::Normal.command_id(),
        };
        for priority in [
            ProcPriority::Realtime,
            ProcPriority::High,
            ProcPriority::AboveNormal,
            ProcPriority::Normal,
            ProcPriority::BelowNormal,
            ProcPriority::Low,
        ] {
            priority_menu.append_item(
                priority.command_id(),
                text(priority.text_key()),
                if !identity_verified {
                    MenuItemState {
                        enabled: false,
                        checked: priority.command_id() == checked_priority,
                    }
                } else if priority.command_id() == checked_priority {
                    MenuItemState::checked()
                } else {
                    MenuItemState::ENABLED
                },
            )?;
        }

        let mut popup = PopupMenu::new()?;
        for (command, label_key, state) in [
            (
                ProcCommand::Terminate,
                TextKey::EndProcess,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::TerminateTree,
                TextKey::EndProcessTree,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::OpenFileLocation,
                TextKey::OpenFileLocation,
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
            (
                ProcCommand::Debug,
                TextKey::Debug,
                if identity_verified && self.debugger_path.is_some() {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
        ] {
            popup.append_item(command.command_id(), text(label_key), state)?;
        }

        popup.append_separator()?;
        popup.append_submenu(text(TextKey::SetPriority), priority_menu)?;

        if self.processor_count > 1 {
            popup.append_item(
                ProcCommand::Affinity.command_id(),
                text(TextKey::SetAffinity),
                if identity_verified {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            )?;
        }

        Ok(popup)
    }

    pub unsafe fn size_page(&self) {
        unsafe {
            // 进程页布局以“列表吃掉剩余空间，按钮贴右下角”为核心规则。
            let mut parent_rect = zeroed::<RECT>();
            GetClientRect(self.hwnd_page, &mut parent_rect);

            let mut hdwp = BeginDeferWindowPos(10);
            if hdwp.is_null() {
                return;
            }

            let terminate_hwnd = GetDlgItem(self.hwnd_page, IDC_TERMINATE);
            let list_hwnd = self.list_hwnd();
            if terminate_hwnd.is_null() || list_hwnd.is_null() {
                EndDeferWindowPos(hdwp);
                return;
            }

            let terminate_rect = window_rect_relative_to_page(terminate_hwnd, self.hwnd_page);
            let dx = (parent_rect.right - DEFAULT_MARGIN * 2) - terminate_rect.right;
            let dy = (parent_rect.bottom - DEFAULT_MARGIN * 2) - terminate_rect.bottom;

            hdwp = DeferWindowPos(
                hdwp,
                terminate_hwnd,
                null_mut(),
                terminate_rect.left + dx,
                terminate_rect.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );

            let list_rect = window_rect_relative_to_page(list_hwnd, self.hwnd_page);
            hdwp = DeferWindowPos(
                hdwp,
                list_hwnd,
                null_mut(),
                0,
                0,
                (terminate_rect.right - list_rect.left + dx).max(0),
                (terminate_rect.top - list_rect.top + dy - DEFAULT_MARGIN).max(0),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );

            EndDeferWindowPos(hdwp);
        }
    }

    // 从进程页跳转到指定 PID 的行并高亮选中。由任务页的“转到进程”命令触发。
    pub unsafe fn find_process(&mut self, identity: ProcIdentity) -> bool {
        unsafe {
            if !identity.is_verified() {
                return false;
            }
            let Some(index) = self
                .entries
                .iter()
                .position(|entry| entry.identity == identity)
            else {
                self.pending_find_identity = Some(identity);
                self.refresh_processes();
                return true;
            };

            self.selected_identity = Some(self.entries[index].identity);
            let list_hwnd = self.list_hwnd();
            self.set_list_selection(list_hwnd, Some(index));
            self.update_ui_state();
            true
        }
    }

    fn list_hwnd(&self) -> HWND {
        unsafe { GetDlgItem(self.hwnd_page, IDC_PROCLIST) }
    }

    fn load_strings(&mut self) {
        // 常用错误文案和优先级文本在这里集中缓存，避免命令执行路径上反复查资源。
        self.strings.warning = text(TextKey::WarningTitle).to_string();
        self.strings.invalid_option = text(TextKey::InvalidOptionTitle).to_string();
        self.strings.no_affinity_mask = text(TextKey::NoAffinityMaskMessage).to_string();
        self.strings.kill = text(TextKey::KillProcessWarning).to_string();
        self.strings.kill_tree = text(TextKey::KillProcessTreePrompt).to_string();
        self.strings.kill_tree_fail = text(TextKey::KillProcessTreeFailed).to_string();
        self.strings.kill_tree_fail_body = text(TextKey::KillProcessTreeFailedBody).to_string();
        self.strings.debug = text(TextKey::DebugProcessWarning).to_string();
        self.strings.prichange = text(TextKey::PriorityChangeWarning).to_string();
        self.strings.cant_kill = text(TextKey::UnableToTerminateProcess).to_string();
        self.strings.cant_debug = text(TextKey::UnableToAttachDebugger).to_string();
        self.strings.cant_change_priority = text(TextKey::UnableToChangePriority).to_string();
        self.strings.cant_set_affinity = text(TextKey::UnableToSetAffinity).to_string();
        self.strings.cant_open_file_location = text(TextKey::UnableToOpenFileLocation).to_string();
        self.strings.priority_low = text(TextKey::Low).to_string();
        self.strings.priority_below_normal = text(TextKey::BelowNormal).to_string();
        self.strings.priority_normal = text(TextKey::Normal).to_string();
        self.strings.priority_above_normal = text(TextKey::AboveNormal).to_string();
        self.strings.priority_high = text(TextKey::High).to_string();
        self.strings.priority_realtime = text(TextKey::Realtime).to_string();
        self.strings.priority_unknown = text(TextKey::Unknown).to_string();
    }

    unsafe fn update_ui_state(&self) {
        unsafe {
            // 当前实现里只有“结束进程”按钮依赖选择状态，
            // 但统一收口在这里，后续扩展其它按钮更容易。
            let has_selection = self
                .current_selected_identity()
                .is_some_and(ProcIdentity::is_verified);
            let terminate_button = GetDlgItem(self.hwnd_page, IDC_TERMINATE);
            if !terminate_button.is_null() {
                EnableWindow(terminate_button, i32::from(has_selection));
            }
        }
    }

    unsafe fn refresh_processes(&mut self) {
        unsafe {
            self.drain_worker_results();
            self.schedule_process_collection();
        }
    }

    unsafe fn schedule_process_collection(&mut self) {
        let Some(worker) = self.worker.as_mut() else {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };
        let request = ProcWorkerRequest {
            processor_count: self.processor_count,
        };
        if let Err(error) = worker.request(request, self.hwnd_page) {
            self.set_refresh_error(error);
        }
    }

    unsafe fn drain_worker_results(&mut self) {
        unsafe {
            let drain = match self.worker.as_mut() {
                Some(worker) => worker.drain(self.hwnd_page),
                None => return,
            };
            for completion in drain.completions {
                self.apply_worker_completion(completion);
            }
            if let Some(error) = drain.error {
                self.worker = None;
                self.set_refresh_error(error);
            }
        }
    }

    unsafe fn apply_worker_completion(&mut self, completion: ProcWorkerResult) {
        unsafe {
            match completion {
                Ok(snapshot) => {
                    self.last_refresh_error = None;
                    if self.last_row_error != snapshot.row_error
                        && let Some(error) = snapshot.row_error
                    {
                        record_win32_error("process row metadata", error);
                    }
                    self.last_row_error = snapshot.row_error;
                    self.apply_process_snapshot(snapshot.entries);
                }
                Err(error) => {
                    self.set_refresh_error(error);
                }
            }
        }
    }

    unsafe fn apply_process_snapshot(&mut self, entries: Vec<ProcEntry>) {
        unsafe {
            let previous_selection = self.current_selected_identity().or(self.selected_identity);
            let current_pass = self.pass_count;
            let visible_columns = DirtyColumns::from_columns(&self.active_columns);
            let mut sort_dirty = false;
            let mut existing_by_identity = HashMap::with_capacity(self.entries.len());
            for (index, entry) in self.entries.iter_mut().enumerate() {
                existing_by_identity.insert(entry.identity, index);
            }

            for snapshot in entries {
                if let Some(&index) = existing_by_identity.get(&snapshot.identity) {
                    let existing = &mut self.entries[index];
                    let changed =
                        update_process_entry(existing, &snapshot, current_pass, visible_columns);
                    sort_dirty |= changed.contains(self.sort_column);
                } else {
                    self.entries.push(snapshot.with_pass_count(
                        current_pass,
                        &self.active_columns,
                        visible_columns,
                    ));
                    sort_dirty = true;
                }
            }

            sort_dirty |= self.remove_stale_entries(current_pass);
            if sort_dirty {
                self.resort_entries();
            }
            let requested_selection = self
                .pending_find_identity
                .take()
                .filter(|identity| self.entries.iter().any(|entry| entry.identity == *identity));
            self.selected_identity = requested_selection.or(previous_selection);
            self.rebuild_listview();
            self.pass_count = self.pass_count.wrapping_add(1);
        }
    }

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("process refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    pub unsafe fn handle_worker_completion(&mut self) {
        unsafe {
            self.drain_worker_results();
        }
    }

    // 按当前排序列和方向重排 entries；文本列直接比较预先缓存的小写字符串。
    fn resort_entries(&mut self) {
        self.entries.sort_by(|left, right| {
            compare_entries(left, right, self.sort_column, self.sort_direction)
        });
    }

    fn remove_stale_entries(&mut self, current_pass: u64) -> bool {
        let previous_len = self.entries.len();
        self.entries
            .retain(|entry| entry.pass_count == current_pass);
        previous_len != self.entries.len()
    }

    unsafe fn rebuild_listview(&mut self) {
        unsafe {
            // 进程列表使用 LVS_OWNERDATA；刷新只更新虚拟项数量和索引映射，
            // 不再为每个进程创建、删除或移动 Win32 ListView 项。
            let list_hwnd = self.list_hwnd();
            if list_hwnd.is_null() {
                return;
            }
            let selected_identity = self.selected_identity;
            let selected_index = selected_identity.and_then(|identity| {
                self.entries
                    .iter()
                    .position(|entry| entry.identity == identity)
            });
            let existing_count = SendMessageW(list_hwnd, LVM_GETITEMCOUNT, 0, 0) as usize;
            let structure_changed = existing_count != self.entries.len();
            let order_changed = self.displayed_identities.len() != self.entries.len()
                || self
                    .displayed_identities
                    .iter()
                    .copied()
                    .ne(self.entries.iter().map(|entry| entry.identity));
            let bulk_update = structure_changed || order_changed;

            if bulk_update {
                SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);
            }

            let mut dirty_rows = DirtyRowRange::default();
            for (index, entry) in self.entries.iter_mut().enumerate() {
                if entry.dirty_columns.any() {
                    entry.dirty_columns = DirtyColumns::default();
                    dirty_rows.mark(index);
                }
            }

            if structure_changed {
                SendMessageW(
                    list_hwnd,
                    LVM_SETITEMCOUNT,
                    self.entries.len(),
                    (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as LPARAM,
                );
            }

            if bulk_update {
                self.set_list_selection(list_hwnd, selected_index);
                finish_list_view_update(list_hwnd);
            } else {
                dirty_rows.redraw_visible(list_hwnd, self.entries.len());
            }

            self.displayed_identities.clear();
            self.displayed_identities
                .extend(self.entries.iter().map(|entry| entry.identity));

            if selected_index.is_none() {
                self.selected_identity = None;
            }

            self.update_ui_state();
        }
    }

    unsafe fn set_list_selection(&self, list_hwnd: HWND, selected_index: Option<usize>) {
        unsafe {
            // Clearing all virtual item states is one ListView operation instead of an O(n) loop.
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: 0,
                ..zeroed()
            };
            SendMessageW(
                list_hwnd,
                LVM_SETITEMSTATE,
                usize::MAX,
                &mut item as *mut _ as LPARAM,
            );

            if let Some(index) = selected_index {
                item.state = LVIS_SELECTED | LVIS_FOCUSED;
                SendMessageW(
                    list_hwnd,
                    LVM_SETITEMSTATE,
                    index,
                    &mut item as *mut _ as LPARAM,
                );
                SendMessageW(list_hwnd, LVM_ENSUREVISIBLE, index, 0);
            }
        }
    }

    unsafe fn fill_display_info(&self, item: &mut LVITEMW) {
        unsafe {
            if (item.mask & LVIF_TEXT) == 0
                || item.iItem < 0
                || item.pszText.is_null()
                || item.cchTextMax <= 0
            {
                return;
            }

            let entry = self.entries.get(item.iItem as usize);
            let Some(entry) = entry else {
                *item.pszText = 0;
                return;
            };
            let Some(column_id) = self.active_columns.get(item.iSubItem as usize).copied() else {
                *item.pszText = 0;
                return;
            };

            let text = column_text(entry, column_id, &self.strings);
            copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, text);
        }
    }

    // 销毁现有列并按照 active_columns 重建所有列头。列宽优先从 options 读取，否则用默认值。
    unsafe fn setup_columns(&self, options: &Options) {
        unsafe {
            let list_hwnd = self.list_hwnd();
            while SendMessageW(list_hwnd, LVM_DELETECOLUMN, 0, 0) != 0 {}

            for (index, column_id) in self.active_columns.iter().enumerate() {
                let column = PROCESS_COLUMNS[*column_id as usize];
                let width = options
                    .column_widths
                    .get(index)
                    .copied()
                    .filter(|value| *value > 0)
                    .unwrap_or(column.default_width);
                let title = text(column.title_key).to_string();
                let mut title_wide = to_wide_null(&title);
                let mut lv_column = LVCOLUMNW {
                    mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: column.fmt,
                    cx: width,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iSubItem: index as i32,
                    ..zeroed()
                };
                SendMessageW(
                    list_hwnd,
                    LVM_INSERTCOLUMNW,
                    index,
                    &mut lv_column as *mut _ as LPARAM,
                );
            }

            if !self.entries.is_empty() {
                SendMessageW(
                    list_hwnd,
                    LVM_REDRAWITEMS,
                    0,
                    self.entries.len().saturating_sub(1) as LPARAM,
                );
            }
        }
    }

    unsafe fn save_column_layout(&self, options: &mut Options) -> Result<(), u32> {
        unsafe {
            let column_count = self.active_columns.len();
            if column_count == 0 {
                return Err(ERROR_INVALID_DATA);
            }

            let list = self.list_hwnd();
            let mut display_order = vec![0i32; column_count];
            if SendMessageW(
                list,
                LVM_GETCOLUMNORDERARRAY,
                column_count,
                display_order.as_mut_ptr() as LPARAM,
            ) == 0
            {
                return Err(ERROR_GEN_FAILURE);
            }

            let ordered_columns = reorder_process_columns(&self.active_columns, &display_order)
                .ok_or(ERROR_INVALID_DATA)?;
            let widths = self
                .active_columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    (
                        *column as i32,
                        SendMessageW(list, LVM_GETCOLUMNWIDTH, index, 0) as i32,
                    )
                })
                .collect::<HashMap<_, _>>();
            write_process_column_layout(options, &ordered_columns, &widths);
            Ok(())
        }
    }

    unsafe fn current_selected_identity(&self) -> Option<ProcIdentity> {
        unsafe {
            let list_hwnd = self.list_hwnd();
            let index = SendMessageW(
                list_hwnd,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_SELECTED as LPARAM,
            ) as i32;
            if index < 0 {
                return None;
            }

            self.entries.get(index as usize).map(|entry| entry.identity)
        }
    }

    fn selected_entry(&self) -> Option<&ProcEntry> {
        let identity = self.selected_identity?;
        self.entries.iter().find(|entry| entry.identity == identity)
    }

    // 打开“选择列”对话框。通过 ColumnDialogContext 传递页面和选项指针给 dialog proc。
    unsafe fn pick_columns(&mut self, options: &mut Options) {
        let mut context = ColumnDialogContext {
            page: self as *mut ProcessPageState,
            options: options as *mut Options,
        };
        if let Err(error) = dialog_box(
            self.hinstance,
            IDD_SELECTPROCCOLS,
            self.main_hwnd,
            Some(column_select_dialog_proc),
            &mut context as *mut ColumnDialogContext as LPARAM,
        ) {
            record_win32_error("process column dialog creation", error);
            self.show_failure_message(text(TextKey::SelectColumnsMenu), error);
        }
    }
}

// “选择列”对话框过程。初始化时同步选项状态，确认时将选中的列写回 options。
unsafe extern "system" fn column_select_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                set_window_userdata(hwnd, lparam);
                localize_dialog(hwnd, IDD_SELECTPROCCOLS);
                let context = &*(lparam as *const ColumnDialogContext);
                let options = &*context.options;

                for &control_id in &COLUMN_DIALOG_IDS {
                    CheckDlgButton(hwnd, control_id, BST_UNCHECKED);
                }
                CheckDlgButton(hwnd, IDC_IMAGENAME, BST_CHECKED);

                for column in columns_from_options(options) {
                    CheckDlgButton(hwnd, COLUMN_DIALOG_IDS[column as usize], BST_CHECKED);
                }
                1
            }
            WM_COMMAND => match i32::from(loword(wparam)) {
                IDOK => {
                    let context = &mut *(get_window_userdata(hwnd) as *mut ColumnDialogContext);
                    let page = &mut *context.page;
                    let options = &mut *context.options;

                    if let Err(error) = page.save_column_layout(options) {
                        record_win32_error("process column layout persistence", error);
                        return 1;
                    }
                    apply_selected_columns(hwnd, options);
                    page.active_columns = columns_from_options(options);
                    page.setup_columns(options);
                    page.refresh_processes();
                    EndDialog(hwnd, IDOK as isize);
                    1
                }
                IDCANCEL => {
                    EndDialog(hwnd, IDCANCEL as isize);
                    1
                }
                _ => 0,
            },
            _ => 0,
        }
    }
}

fn reorder_process_columns(active: &[ColumnId], display_order: &[i32]) -> Option<Vec<ColumnId>> {
    if active.is_empty() || active.len() != display_order.len() {
        return None;
    }

    let mut seen = vec![false; active.len()];
    let mut ordered = Vec::with_capacity(active.len());
    for &logical_index in display_order {
        let logical_index = usize::try_from(logical_index).ok()?;
        if logical_index >= active.len() || seen[logical_index] {
            return None;
        }
        seen[logical_index] = true;
        ordered.push(active[logical_index]);
    }
    (ordered.first() == Some(&ColumnId::ImageName)).then_some(ordered)
}

fn write_process_column_layout(
    options: &mut Options,
    columns: &[ColumnId],
    widths: &HashMap<i32, i32>,
) {
    options.active_process_columns.fill(-1);
    options.column_widths.fill(-1);
    for (index, &column) in columns.iter().take(NUM_COLUMN).enumerate() {
        options.active_process_columns[index] = column as i32;
        options.column_widths[index] = widths
            .get(&(column as i32))
            .copied()
            .filter(|width| *width > 0)
            .unwrap_or(PROCESS_COLUMNS[column as usize].default_width);
    }
}

// 将对话框中的列勾选状态持久化到 options。保留已有顺序和列宽，新增列追加到末尾。
unsafe fn apply_selected_columns(hwnd: HWND, options: &mut Options) {
    unsafe {
        let existing_columns = columns_from_options(options);
        let mut existing_widths = HashMap::with_capacity(NUM_COLUMN);
        for (index, value) in options.active_process_columns.iter().copied().enumerate() {
            let Some(column) = column_id_from_i32(value) else {
                break;
            };
            existing_widths.insert(column as i32, options.column_widths[index]);
        }

        let mut selected = [false; NUM_COLUMN];
        selected[ColumnId::ImageName as usize] = true;
        for (column_index, &control_id) in COLUMN_DIALOG_IDS
            .iter()
            .enumerate()
            .take(NUM_COLUMN)
            .skip(1)
        {
            if IsDlgButtonChecked(hwnd, control_id) == BST_CHECKED {
                selected[column_index] = true;
            }
        }

        let mut columns = Vec::with_capacity(NUM_COLUMN);
        columns.push(ColumnId::ImageName);
        for column in existing_columns.into_iter().skip(1) {
            if selected[column as usize] {
                columns.push(column);
                selected[column as usize] = false;
            }
        }
        for (column_index, is_selected) in selected.iter().copied().enumerate().skip(1) {
            if is_selected {
                columns.push(column_id_from_i32(column_index as i32).unwrap_or(ColumnId::Pid));
            }
        }
        write_process_column_layout(options, &columns, &existing_widths);
    }
}

fn columns_from_options(options: &Options) -> Vec<ColumnId> {
    options
        .active_process_columns
        .iter()
        .copied()
        .filter_map(column_id_from_i32)
        .collect()
}

// 将 i32 列 ID 映射回 ColumnId 枚举。超出范围的值返回 None。
fn column_id_from_i32(value: i32) -> Option<ColumnId> {
    match value {
        x if x == ColumnId::ImageName as i32 => Some(ColumnId::ImageName),
        x if x == ColumnId::Pid as i32 => Some(ColumnId::Pid),
        x if x == ColumnId::Username as i32 => Some(ColumnId::Username),
        x if x == ColumnId::SessionId as i32 => Some(ColumnId::SessionId),
        x if x == ColumnId::Cpu as i32 => Some(ColumnId::Cpu),
        x if x == ColumnId::CpuTime as i32 => Some(ColumnId::CpuTime),
        x if x == ColumnId::MemUsage as i32 => Some(ColumnId::MemUsage),
        x if x == ColumnId::MemUsageDiff as i32 => Some(ColumnId::MemUsageDiff),
        x if x == ColumnId::PageFaults as i32 => Some(ColumnId::PageFaults),
        x if x == ColumnId::PageFaultsDiff as i32 => Some(ColumnId::PageFaultsDiff),
        x if x == ColumnId::CommitCharge as i32 => Some(ColumnId::CommitCharge),
        x if x == ColumnId::PagedPool as i32 => Some(ColumnId::PagedPool),
        x if x == ColumnId::NonPagedPool as i32 => Some(ColumnId::NonPagedPool),
        x if x == ColumnId::BasePriority as i32 => Some(ColumnId::BasePriority),
        x if x == ColumnId::HandleCount as i32 => Some(ColumnId::HandleCount),
        x if x == ColumnId::ThreadCount as i32 => Some(ColumnId::ThreadCount),
        _ => None,
    }
}

// 进程表的通用比较函数。排序键由 sort_column 决定，兜底使用 PID 保证稳定性。
#[cfg(test)]
mod tests {
    use super::actions::{
        affinity_cpu_mask, extract_first_command_token, is_valid_process_tree_edge,
        normalize_debugger_command_with, validate_snapshot_root_identity,
    };
    use super::model::{DirtyColumns, ProcEntry};
    use super::sampler::{
        WtsProcessIdentity, cpu_percent_from_delta, merge_wts_process_identity, signed_kb_delta,
        system_time_delta, wts_identity_matches,
    };
    use super::{
        ProcIdentity, ProcessPageState, reorder_process_columns, write_process_column_layout,
    };
    use crate::config::options::{ColumnId, Options};
    use std::collections::HashMap;
    use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ};
    use windows_sys::Win32::System::Threading::NORMAL_PRIORITY_CLASS;

    fn empty_process_entry(image_name: &str) -> ProcEntry {
        ProcEntry {
            identity: ProcIdentity::new(1234, 10),
            pid: 1234,
            image_name: image_name.to_string(),
            image_name_lower: image_name.to_lowercase(),
            is_32_bit: None,
            user_name: String::new(),
            user_name_lower: String::new(),
            session_id: None,
            cpu: 0,
            cpu_time_100ns: 0,
            display_cpu_time_100ns: 0,
            mem_usage_kb: 0,
            mem_diff_kb: 0,
            page_faults: 0,
            page_faults_diff: 0,
            commit_charge_kb: 0,
            paged_pool_kb: 0,
            nonpaged_pool_kb: 0,
            priority_class: NORMAL_PRIORITY_CLASS,
            handle_count: 0,
            thread_count: 0,
            display_text: std::array::from_fn(|_| String::new()),
            pass_count: 0,
            dirty_columns: DirtyColumns::default(),
        }
    }

    #[test]
    fn dragged_process_columns_map_display_order_to_stable_ids() {
        let active = [
            ColumnId::ImageName,
            ColumnId::Username,
            ColumnId::Cpu,
            ColumnId::MemUsage,
        ];
        assert_eq!(
            reorder_process_columns(&active, &[0, 2, 1, 3]),
            Some(vec![
                ColumnId::ImageName,
                ColumnId::Cpu,
                ColumnId::Username,
                ColumnId::MemUsage,
            ])
        );
        assert_eq!(reorder_process_columns(&active, &[1, 0, 2, 3]), None);
        assert_eq!(reorder_process_columns(&active, &[0, 2, 2, 3]), None);
    }

    #[test]
    fn persisted_process_layout_keeps_widths_attached_to_column_ids() {
        let mut options = Options::default();
        let widths = HashMap::from([
            (ColumnId::ImageName as i32, 120),
            (ColumnId::Cpu as i32, 44),
            (ColumnId::Username as i32, 160),
        ]);
        write_process_column_layout(
            &mut options,
            &[ColumnId::ImageName, ColumnId::Cpu, ColumnId::Username],
            &widths,
        );
        assert_eq!(
            &options.active_process_columns[..4],
            &[
                ColumnId::ImageName as i32,
                ColumnId::Cpu as i32,
                ColumnId::Username as i32,
                -1,
            ]
        );
        assert_eq!(&options.column_widths[..3], &[120, 44, 160]);
    }

    #[test]
    fn extracts_quoted_absolute_debugger_path() {
        let command = r#""C:\Tools\Debugger\dbg.exe" -p %ld -e %ld -g"#;
        let debugger = normalize_debugger_command_with(command, REG_SZ, |value| value.to_string());
        assert_eq!(debugger.as_deref(), Some(r"C:\Tools\Debugger\dbg.exe"));
    }

    #[test]
    fn expands_environment_variables_before_extracting_debugger_path() {
        let command = r#""%SystemRoot%\System32\vsjitdebugger.exe" -p %ld"#;
        let debugger = normalize_debugger_command_with(command, REG_EXPAND_SZ, |_| {
            r#""C:\Windows\System32\vsjitdebugger.exe" -p %ld"#.to_string()
        });
        assert_eq!(
            debugger.as_deref(),
            Some(r"C:\Windows\System32\vsjitdebugger.exe")
        );
    }

    #[test]
    fn rejects_legacy_drwtsn32_debugger_commands() {
        let debugger =
            normalize_debugger_command_with(r"drwtsn32 -p %ld -e %ld -g", REG_SZ, |value| {
                value.to_string()
            });
        assert!(debugger.is_none());
    }

    #[test]
    fn extracts_unquoted_debugger_path_with_parameters() {
        let command = r"C:\Debuggers\dbg.exe -p %ld -e %ld";
        assert_eq!(
            extract_first_command_token(command),
            r"C:\Debuggers\dbg.exe"
        );
    }

    #[test]
    fn proc_identity_distinguishes_pid_reuse() {
        let first = ProcIdentity::new(1234, 10);
        let reused = ProcIdentity::new(1234, 20);
        let mut samples = HashMap::new();
        samples.insert(first, "old");
        samples.insert(reused, "new");

        assert_ne!(first, reused);
        assert_eq!(samples.get(&first), Some(&"old"));
        assert_eq!(samples.get(&reused), Some(&"new"));
    }

    #[test]
    fn signed_kb_delta_saturates_to_i64_bounds() {
        assert_eq!(signed_kb_delta(12, 5), 7);
        assert_eq!(signed_kb_delta(5, 12), -7);
        assert_eq!(signed_kb_delta(u64::MAX, 0), i64::MAX);
        assert_eq!(signed_kb_delta(0, u64::MAX), -i64::MAX);
    }

    #[test]
    fn affinity_mask_supports_the_full_64_bit_processor_group() {
        assert_eq!(affinity_cpu_mask(0), 1);
        assert_eq!(affinity_cpu_mask(63), 1usize << 63);
        assert_eq!(affinity_cpu_mask(64), 0);
    }

    #[test]
    fn process_tree_edges_reject_stale_parent_pid_reuse() {
        let old_child = ProcIdentity::new(20, 100);
        let reused_parent = ProcIdentity::new(10, 200);
        assert!(!is_valid_process_tree_edge(reused_parent, old_child, 300));

        let current_child = ProcIdentity::new(20, 250);
        assert!(is_valid_process_tree_edge(
            reused_parent,
            current_child,
            300
        ));
        assert!(!is_valid_process_tree_edge(
            reused_parent,
            current_child,
            225
        ));
    }

    #[test]
    fn process_tree_snapshot_rejects_reused_root_pid() {
        let expected = ProcIdentity::new(10, 100);

        assert!(validate_snapshot_root_identity(expected, expected).is_ok());
        assert!(validate_snapshot_root_identity(ProcIdentity::pid_only(10), expected).is_err());
        assert!(validate_snapshot_root_identity(expected, ProcIdentity::new(10, 200)).is_err());
    }

    #[test]
    fn wts_identity_source_rejects_stale_image_or_session() {
        let identity = WtsProcessIdentity {
            user_name: Some("SYSTEM".to_string()),
            session_id: 0,
            image_name_lower: "services.exe".to_string(),
        };

        assert!(wts_identity_matches("services.exe", None, &identity));
        assert!(wts_identity_matches("services.exe", Some(0), &identity));
        assert!(!wts_identity_matches("other.exe", Some(0), &identity));
        assert!(!wts_identity_matches("services.exe", Some(1), &identity));
    }

    #[test]
    fn missing_wts_user_name_remains_retryable() {
        let mut entry = empty_process_entry("services.exe");
        let identity = WtsProcessIdentity {
            user_name: None,
            session_id: 0,
            image_name_lower: "services.exe".to_string(),
        };

        assert!(!merge_wts_process_identity(&mut entry, Some(&identity)));
        assert!(entry.user_name.is_empty());
        assert_eq!(entry.session_id, Some(0));
    }

    #[test]
    fn failed_process_worker_keeps_the_last_visible_snapshot() {
        let mut state = ProcessPageState {
            entries: vec![empty_process_entry("trusted.exe")],
            pass_count: 7,
            ..ProcessPageState::default()
        };

        unsafe {
            state.apply_worker_completion(Err(5));
        }

        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].image_name, "trusted.exe");
        assert_eq!(state.pass_count, 7);
        assert_eq!(state.last_refresh_error, Some(5));
    }

    #[test]
    fn system_time_delta_requires_a_monotonic_baseline() {
        assert_eq!(system_time_delta(100, None), None);
        assert_eq!(system_time_delta(90, Some(100)), None);
        assert_eq!(system_time_delta(125, Some(100)), Some(25));
    }

    #[test]
    fn process_cpu_percentage_uses_exact_integer_rounding() {
        assert_eq!(cpu_percent_from_delta(0, 100), 0);
        assert_eq!(cpu_percent_from_delta(1, 3), 33);
        assert_eq!(cpu_percent_from_delta(2, 3), 67);
        assert_eq!(cpu_percent_from_delta(100, 100), 100);
    }
}
