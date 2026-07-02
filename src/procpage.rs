use std::cmp::Ordering;

// 进程页实现。
// 这里负责采集进程列表、计算每轮刷新之间的增量数据、维护排序状态，
// 并处理结束进程、调试、设置优先级和亲和性等操作。
//
// 线程模型：
//   ProcWorkerCommand 工作线程在后台执行 collect_process_entries() 采样，
//   UI 线程发送命令后继续处理消息，采样完成后通过页面消息消费最新结果。
//
// 缓存失效策略：
//   pid_to_index (HashMap) 在每次 resort_entries 后从头重建，以匹配排序后的 entries 顺序。
//   previous_samples (HashMap) 在 refresh_processes 末尾整体替换为当轮采样值，
//   供下轮 delta（CPU、内存、缺页）计算使用。
//   DirtyColumns / DirtyRowRange 作为行/列级脏标记，避免整表重绘。
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::slice;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, HINSTANCE, HWND, INVALID_HANDLE_VALUE,
    LPARAM, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{InvalidateRect, MapWindowPoints};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsWellKnownSid, LookupAccountSidW, TokenSessionId, TokenUser,
    WinLocalServiceSid, WinLocalSystemSid, WinNetworkServiceSid, SID_NAME_USE, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_EXPAND_SZ, REG_SZ,
};
use windows_sys::Win32::System::RemoteDesktop::{
    WTSEnumerateProcessesW, WTSFreeMemory, WTS_CURRENT_SERVER_HANDLE, WTS_PROCESS_INFOW,
};
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetPriorityClass, GetProcessAffinityMask, GetProcessHandleCount,
    GetProcessTimes, GetSystemTimes, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    SetPriorityClass, SetProcessAffinityMask, TerminateProcess, ABOVE_NORMAL_PRIORITY_CLASS,
    BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ, REALTIME_PRIORITY_CLASS,
    STARTUPINFOW,
};
use windows_sys::Win32::UI::Controls::{
    CheckDlgButton, IsDlgButtonChecked, BST_CHECKED, BST_UNCHECKED, LVCFMT_LEFT, LVCFMT_RIGHT,
    LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_PARAM, LVIF_STATE, LVIF_TEXT,
    LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS, LVM_DELETECOLUMN, LVM_DELETEITEM,
    LVM_ENSUREVISIBLE, LVM_GETCOLUMNWIDTH, LVM_GETITEMCOUNT, LVM_GETITEMW, LVM_GETNEXTITEM,
    LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETITEMSTATE, LVM_SETITEMW, LVNI_SELECTED,
    LVN_COLUMNCLICK, LVN_GETDISPINFOW, LVN_ITEMCHANGED, LVS_SHOWSELALWAYS, NMHDR, NMLISTVIEW,
    NMLVDISPINFOW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, EndDialog, GetClientRect, GetCursorPos,
    GetDlgItem, GetWindowLongW, MessageBoxW, PostMessageW, SendMessageW, SetWindowLongW,
    TrackPopupMenuEx, GWL_STYLE, IDCANCEL, IDOK, IDYES, MB_ICONERROR, MB_ICONEXCLAMATION, MB_OK,
    MB_YESNO, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, TPM_RETURNCMD, WM_COMMAND,
    WM_INITDIALOG, WM_SETREDRAW,
};

use crate::dialog_templates::dialog_box;
use crate::language::{localize_dialog, text, TextKey};
use crate::options::Options;
use crate::options::{ColumnId, UpdateSpeed};
use crate::resource::*;
use crate::runtime_menu::{MenuItemState, PopupMenu};
use crate::winutil::{
    append_32_bit_suffix, finish_list_view_update_deferred, get_window_userdata,
    is_32_bit_process_handle, loword, set_window_userdata, subclass_list_view, to_wide_null,
    ListViewDirtyRange,
};

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
const TEXT_CALLBACK_WIDE: *mut u16 = -1isize as *mut u16;

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

#[derive(Clone, Default)]
struct PreviousProcSample {
    // 上一轮采样值用于计算 CPU、内存增量和缺页增量。
    raw_cpu_time_100ns: u64,
    mem_usage_kb: u32,
    page_faults: u32,
}

// 列级脏标记位图。每轮刷新时标记变更列，rebuild_listview 只重绘被标记的行。
#[derive(Clone, Copy, Default)]
struct DirtyColumns(u32);

impl DirtyColumns {
    fn all() -> Self {
        Self(u32::MAX)
    }

    fn from_column(column_id: ColumnId) -> Self {
        Self(1u32 << column_id as u32)
    }

    fn mark(&mut self, column_id: ColumnId) {
        self.0 |= Self::from_column(column_id).0;
    }

    fn any(self) -> bool {
        self.0 != 0
    }
}

#[derive(Clone, Copy)]
struct ProcessIdentity {
    pid: u32,
    creation_time_100ns: u64,
}

#[derive(Clone)]
pub struct ProcEntry {
    // `ProcEntry` 同时承载原始采样值、展示值和刷新期的脏信息。
    pid: u32,
    creation_time_100ns: u64,
    image_name: String,
    image_name_lower: String,
    is_32_bit: bool,
    user_name: String,
    user_name_lower: String,
    session_id: u32,
    cpu: u8,
    cpu_time_100ns: u64,
    display_cpu_time_100ns: u64,
    mem_usage_kb: u32,
    mem_diff_kb: i64,
    page_faults: u32,
    page_faults_diff: i64,
    commit_charge_kb: u32,
    paged_pool_kb: u32,
    nonpaged_pool_kb: u32,
    priority_class: u32,
    handle_count: u32,
    thread_count: u32,
    pass_count: u64,
    dirty_columns: DirtyColumns,
}

// 常用文案缓存，避免命令执行路径上反复查本地化资源。
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

// “设置亲和性”对话框的上下文，包含当前进程掩码。
struct AffinityDialogContext {
    page: *mut ProcessPageState,
    process_mask: usize,
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

// 工作线程命令枚举。
// Collect:  在后台线程执行 collect_process_entries()，结果写入共享结果 channel。
// Shutdown: 通知线程退出主循环，配合 join 保证安全回收。
enum ProcWorkerCommand {
    Collect {
        seq: u64,
        notify_hwnd: isize,
        previous_samples: Arc<HashMap<u32, PreviousProcSample>>,
        system_total: u64,
        total_delta: u64,
    },
    Shutdown,
}

struct ProcWorkerResult {
    seq: u64,
    system_total: u64,
    entries: Vec<ProcEntry>,
    next_samples: HashMap<u32, PreviousProcSample>,
}

pub struct ProcessPageState {
    // 进程页状态对象持有采样缓存、排序设置、列配置以及当前选中项。
    hinstance: HINSTANCE,
    hwnd_page: HWND,
    main_hwnd: HWND,
    entries: Vec<ProcEntry>,
    pid_to_index: HashMap<u32, usize>,
    previous_samples: Arc<HashMap<u32, PreviousProcSample>>,
    previous_system_time: u64,
    active_columns: Vec<ColumnId>,
    selected_pid: Option<u32>,
    sort_column: ColumnId,
    sort_direction: i32,
    paused: bool,
    confirmations: bool,
    no_title: bool,
    processor_count: usize,
    debugger_path: Option<String>,
    strings: ProcessStrings,
    pass_count: u64,
    worker_sender: Option<Sender<ProcWorkerCommand>>,
    worker_result_receiver: Option<Receiver<ProcWorkerResult>>,
    worker_thread: Option<JoinHandle<()>>,
    refresh_inflight: bool,
    refresh_pending: bool,
    refresh_seq: u64,
    last_applied_refresh_seq: u64,
}

impl Default for ProcessPageState {
    fn default() -> Self {
        Self {
            hinstance: null_mut(),
            hwnd_page: null_mut(),
            main_hwnd: null_mut(),
            entries: Vec::with_capacity(128),
            pid_to_index: HashMap::new(),
            previous_samples: Arc::new(HashMap::new()),
            previous_system_time: 0,
            active_columns: Vec::with_capacity(NUM_COLUMN),
            selected_pid: None,
            sort_column: ColumnId::Pid,
            sort_direction: 1,
            paused: false,
            confirmations: true,
            no_title: false,
            processor_count: 1,
            debugger_path: None,
            strings: ProcessStrings::default(),
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

impl ProcessPageState {
    // 创建工作线程和 mpsc channel。如果线程已存在则直接返回（幂等）。
    // 工作线程循环接收 ProcWorkerCommand，遇到 Shutdown 才退出。
    fn start_worker_thread(&mut self) {
        if self.worker_sender.is_some() {
            return;
        }
        let (tx, rx) = channel::<ProcWorkerCommand>();
        let (result_tx, result_rx) = channel::<ProcWorkerResult>();
        let worker = thread::spawn(move || {
            while let Ok(command) = rx.recv() {
                match command {
                    ProcWorkerCommand::Collect {
                        seq,
                        notify_hwnd,
                        previous_samples,
                        system_total,
                        total_delta,
                    } => {
                        let (entries, next_samples) = unsafe {
                            collect_process_entries(previous_samples.as_ref(), total_delta)
                        };
                        if result_tx
                            .send(ProcWorkerResult {
                                seq,
                                system_total,
                                entries,
                                next_samples,
                            })
                            .is_ok()
                        {
                            unsafe {
                                PostMessageW(
                                    notify_hwnd as HWND,
                                    PWM_PROC_REFRESH_COMPLETE,
                                    seq as usize,
                                    0,
                                );
                            }
                        }
                    }
                    ProcWorkerCommand::Shutdown => break,
                }
            }
        });
        self.worker_sender = Some(tx);
        self.worker_result_receiver = Some(result_rx);
        self.worker_thread = Some(worker);
    }

    // 发送 Shutdown 命令并等待工作线程退出。清理线程句柄和 channel。
    fn stop_worker_thread(&mut self) {
        if let Some(sender) = self.worker_sender.take() {
            let _ = sender.send(ProcWorkerCommand::Shutdown);
        }
        if let Some(worker) = self.worker_thread.take() {
            let _ = worker.join();
        }
        self.worker_result_receiver = None;
        self.refresh_inflight = false;
        self.refresh_pending = false;
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
        // 进程页初始化主要做三件事：
        // 加载文案、准备调试器路径、并把 ListView 切到更适合频繁刷新的显示模式。
        self.hinstance = hinstance;
        self.hwnd_page = hwnd_page;
        self.main_hwnd = main_hwnd;
        self.load_strings();
        self.debugger_path = load_debugger_path();
        self.start_worker_thread();

        let list_hwnd = self.list_hwnd();
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

    pub unsafe fn apply_options(&mut self, options: &Options, processor_count: usize) {
        // 进程页的选项既影响行为，也影响列结构。
        // 当列配置发生变化时，直接重建列和数据比做局部修补更可靠。
        self.no_title = options.no_title();
        self.confirmations = options.confirmations();
        self.processor_count = processor_count.max(1);

        let desired_columns = columns_from_options(options);
        if desired_columns != self.active_columns {
            self.active_columns = desired_columns;
            self.setup_columns(options);
            self.rebuild_listview();
            self.refresh_processes();
        }
    }

    pub unsafe fn timer_event(&mut self, options: &Options) {
        // 每一轮刷新都走“采样 -> 合并旧状态 -> 排序/重绘”这条统一链路。
        self.paused = options.update_speed == UpdateSpeed::Paused as i32;
        self.consume_refresh_results();
        if !self.paused {
            self.refresh_processes();
        }
    }

    pub unsafe fn handle_refresh_complete(&mut self, _seq: u64) -> isize {
        self.consume_refresh_results();
        if self.refresh_pending && !self.paused {
            self.refresh_pending = false;
            self.refresh_processes();
        }
        1
    }

    pub unsafe fn deactivate(&mut self, options: &mut Options) {
        self.save_column_widths(options);
    }

    pub unsafe fn destroy(&mut self) {
        self.stop_worker_thread();
        self.entries.clear();
        self.previous_samples = Arc::new(HashMap::new());
    }

    pub unsafe fn handle_notify(&mut self, lparam: LPARAM) -> isize {
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
                    self.selected_pid = self.current_selected_pid();
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
                self.resort_entries();
                self.rebuild_listview();
                self.refresh_processes();
                1
            }
            _ => 0,
        }
    }

    // 将命令 ID 分派到具体的进程操作（结束、调试、优先级、亲和性等）。
    pub unsafe fn handle_command(&mut self, command_id: u16, options: Option<&mut Options>) {
        let Some(command) = ProcCommand::from_command_id(command_id, IDC_TERMINATE as u16) else {
            return;
        };

        match command {
            ProcCommand::PickColumns => {
                if let Some(options) = options {
                    self.pick_columns(options);
                }
            }
            ProcCommand::Terminate => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.kill_process(&identity);
                }
            }
            ProcCommand::TerminateTree => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.kill_process_tree(&identity);
                }
            }
            ProcCommand::Debug => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.attach_debugger(&identity);
                }
            }
            ProcCommand::OpenFileLocation => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.open_file_location(&identity);
                }
            }
            ProcCommand::Affinity => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.set_affinity(&identity);
                }
            }
            ProcCommand::SetPriority(priority) => {
                if let Some(identity) = self.current_selected_process_identity() {
                    self.set_priority(&identity, priority);
                }
            }
        }
    }

    pub unsafe fn show_context_menu(&mut self, x: i32, y: i32) {
        // 右键菜单会按当前选中进程和系统能力动态裁剪。
        self.selected_pid = self.current_selected_pid();
        let Some(entry) = self.selected_entry() else {
            return;
        };

        let Some(popup) = self.build_context_menu(entry) else {
            return;
        };

        self.paused = true;
        let mut cursor = POINT { x, y };
        if cursor.x == -1 && cursor.y == -1 {
            GetCursorPos(&mut cursor);
        }

        SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 1, 0);
        let command = TrackPopupMenuEx(
            popup.as_raw(),
            TPM_RETURNCMD,
            cursor.x,
            cursor.y,
            self.hwnd_page,
            null(),
        );
        SendMessageW(self.main_hwnd, crate::resource::PWM_INPOPUP, 0, 0);
        self.paused = false;

        if command != 0 {
            self.handle_command(command as u16, None);
        }
    }

    // 构造进程右键菜单，包含结束进程、调试、打开文件位置、优先级和亲和性子菜单。
    unsafe fn build_context_menu(&self, entry: &ProcEntry) -> Option<PopupMenu> {
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
            if !priority_menu.append_item(
                priority.command_id(),
                text(priority.text_key()),
                if priority.command_id() == checked_priority {
                    MenuItemState::checked()
                } else {
                    MenuItemState::ENABLED
                },
            ) {
                return None;
            }
        }

        let mut popup = PopupMenu::new()?;
        for (command, label_key, state) in [
            (
                ProcCommand::Terminate,
                TextKey::EndProcess,
                MenuItemState::ENABLED,
            ),
            (
                ProcCommand::TerminateTree,
                TextKey::EndProcessTree,
                MenuItemState::ENABLED,
            ),
            (
                ProcCommand::OpenFileLocation,
                TextKey::OpenFileLocation,
                MenuItemState::ENABLED,
            ),
            (
                ProcCommand::Debug,
                TextKey::Debug,
                if self.debugger_path.is_some() {
                    MenuItemState::ENABLED
                } else {
                    MenuItemState::disabled()
                },
            ),
        ] {
            if !popup.append_item(command.command_id(), text(label_key), state) {
                return None;
            }
        }

        if !popup.append_separator() {
            return None;
        }

        if !popup.append_submenu(text(TextKey::SetPriority), priority_menu) {
            return None;
        }

        if self.processor_count > 1
            && !popup.append_item(
                ProcCommand::Affinity.command_id(),
                text(TextKey::SetAffinity),
                MenuItemState::ENABLED,
            )
        {
            return None;
        }

        Some(popup)
    }

    pub unsafe fn size_page(&self) {
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

    // 从进程页跳转到指定 PID 的行并高亮选中。由任务页的“转到进程”命令触发。
    pub unsafe fn find_process(&mut self, _thread_id: u32, pid: u32) -> bool {
        let target_pid = pid;
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.pid == target_pid)
        else {
            return false;
        };

        self.selected_pid = Some(target_pid);
        let list_hwnd = self.list_hwnd();
        for item_index in 0..self.entries.len() {
            let mut item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: if item_index == index {
                    LVIS_SELECTED | LVIS_FOCUSED
                } else {
                    0
                },
                ..zeroed()
            };
            SendMessageW(
                list_hwnd,
                LVM_SETITEMSTATE,
                item_index,
                &mut item as *mut _ as LPARAM,
            );
        }
        SendMessageW(list_hwnd, LVM_ENSUREVISIBLE, index, 0);
        self.update_ui_state();
        true
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
        // 当前实现里只有“结束进程”按钮依赖选择状态，
        // 但统一收口在这里，后续扩展其它按钮更容易。
        let has_selection = self.current_selected_pid().is_some();
        let terminate_button = GetDlgItem(self.hwnd_page, IDC_TERMINATE);
        if !terminate_button.is_null() {
            EnableWindow(terminate_button, i32::from(has_selection));
        }
    }

    unsafe fn refresh_processes(&mut self) {
        // 刷新过程分为“发起采样 -> 后台采集 -> 消费结果 -> 重建 ListView”四步。
        // 正常路径不再同步等待 worker，避免进程枚举 syscall 堵住 UI 线程。
        self.consume_refresh_results();
        if self.refresh_inflight {
            self.refresh_pending = true;
            return;
        }

        let system_total = current_system_time();
        let total_delta = system_total.saturating_sub(self.previous_system_time);
        let previous_samples = Arc::clone(&self.previous_samples);
        if let Some(sender) = self.worker_sender.as_ref() {
            self.refresh_seq = self.refresh_seq.wrapping_add(1);
            let seq = self.refresh_seq;
            let command = ProcWorkerCommand::Collect {
                seq,
                notify_hwnd: self.hwnd_page as isize,
                previous_samples,
                system_total,
                total_delta,
            };
            if sender.send(command).is_err() {
                let (entries, next_samples) =
                    collect_process_entries(self.previous_samples.as_ref(), total_delta);
                self.apply_refresh_result(ProcWorkerResult {
                    seq,
                    system_total,
                    entries,
                    next_samples,
                });
                return;
            }
            self.refresh_inflight = true;
        } else {
            let (entries, next_samples) =
                collect_process_entries(self.previous_samples.as_ref(), total_delta);
            self.refresh_seq = self.refresh_seq.wrapping_add(1);
            self.apply_refresh_result(ProcWorkerResult {
                seq: self.refresh_seq,
                system_total,
                entries,
                next_samples,
            });
        }
    }

    unsafe fn consume_refresh_results(&mut self) {
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

    unsafe fn apply_refresh_result(&mut self, result: ProcWorkerResult) {
        let previous_selection = self.current_selected_pid().or(self.selected_pid);
        let current_pass = self.pass_count;
        let mut existing_by_pid = HashMap::with_capacity(self.entries.len());
        for (index, entry) in self.entries.iter_mut().enumerate() {
            existing_by_pid.insert(entry.pid, index);
        }

        for snapshot in result.entries {
            if let Some(&index) = existing_by_pid.get(&snapshot.pid) {
                let existing = &mut self.entries[index];
                update_process_entry(existing, &snapshot, current_pass);
            } else {
                self.entries.push(snapshot.with_pass_count(current_pass));
            }
        }

        self.remove_stale_entries(current_pass);
        self.resort_entries();
        self.previous_samples = Arc::new(result.next_samples);
        self.previous_system_time = result.system_total;
        self.last_applied_refresh_seq = result.seq;
        self.selected_pid = previous_selection;
        self.rebuild_listview();
        self.pass_count = self.pass_count.wrapping_add(1);
    }

    // 按当前排序列和方向重排 entries。排序后 pid_to_index 索引从头重建以保证一致性。
    fn resort_entries(&mut self) {
        self.entries.sort_by(|left, right| {
            compare_entries(left, right, self.sort_column, self.sort_direction)
        });
        self.pid_to_index.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.pid_to_index.insert(entry.pid, index);
        }
    }

    fn remove_stale_entries(&mut self, current_pass: u64) {
        self.entries
            .retain(|entry| entry.pass_count == current_pass);
    }

    unsafe fn rebuild_listview(&mut self) {
        // 这里优先复用现有行，只在身份变化时替换整行，以减少闪烁和选择状态丢失。
        let list_hwnd = self.list_hwnd();
        let selected_pid = self.selected_pid;
        let mut selected_index = None;
        let existing_count = SendMessageW(list_hwnd, LVM_GETITEMCOUNT, 0, 0) as usize;
        let common_count = existing_count.min(self.entries.len());
        let mut current_pids = Vec::with_capacity(common_count);
        let structure_changed = existing_count != self.entries.len();

        for index in 0..common_count {
            let mut current_item = LVITEMW {
                mask: LVIF_PARAM,
                iItem: index as i32,
                ..zeroed()
            };
            let current_pid = if SendMessageW(
                list_hwnd,
                LVM_GETITEMW,
                0,
                &mut current_item as *mut _ as LPARAM,
            ) != 0
            {
                Some(current_item.lParam as u32)
            } else {
                None
            };
            current_pids.push(current_pid);
        }

        if structure_changed {
            SendMessageW(list_hwnd, WM_SETREDRAW, 0, 0);
        }

        let mut dirty_rows = ListViewDirtyRange::default();

        for (index, current_pid) in current_pids.iter().copied().enumerate() {
            let entry = &self.entries[index];

            let item_state = if selected_pid == Some(entry.pid) {
                selected_index = Some(index);
                LVIS_SELECTED | LVIS_FOCUSED
            } else {
                0
            };

            if current_pid != Some(entry.pid) {
                self.replace_row(list_hwnd, index, entry, item_state);
                self.entries[index].dirty_columns = DirtyColumns::default();
                dirty_rows.mark(index);
            } else if entry.dirty_columns.any() {
                self.entries[index].dirty_columns = DirtyColumns::default();
                dirty_rows.mark(index);
            }
        }

        if structure_changed {
            let mut remaining_count = existing_count;
            while remaining_count > self.entries.len() {
                remaining_count -= 1;
                SendMessageW(list_hwnd, LVM_DELETEITEM, remaining_count, 0);
            }

            for index in common_count..self.entries.len() {
                let entry = &self.entries[index];
                let item_state = if selected_pid == Some(entry.pid) {
                    selected_index = Some(index);
                    LVIS_SELECTED | LVIS_FOCUSED
                } else {
                    0
                };
                self.insert_row(list_hwnd, index, entry, item_state);
                self.entries[index].dirty_columns = DirtyColumns::default();
                dirty_rows.mark(index);
            }

            finish_list_view_update_deferred(list_hwnd);
            InvalidateRect(list_hwnd, null_mut(), 0);
        }
        dirty_rows.redraw(list_hwnd, self.entries.len());

        if selected_index.is_none() {
            self.selected_pid = None;
        }

        self.update_ui_state();
    }

    unsafe fn insert_row(&self, list_hwnd: HWND, index: usize, entry: &ProcEntry, item_state: u32) {
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM | LVIF_STATE,
            iItem: index as i32,
            iSubItem: 0,
            pszText: TEXT_CALLBACK_WIDE,
            cchTextMax: 0,
            lParam: entry.pid as isize,
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: item_state,
            ..zeroed()
        };
        SendMessageW(list_hwnd, LVM_INSERTITEMW, 0, &mut item as *mut _ as LPARAM);
    }

    unsafe fn replace_row(
        &self,
        list_hwnd: HWND,
        index: usize,
        entry: &ProcEntry,
        item_state: u32,
    ) {
        let mut item = LVITEMW {
            mask: LVIF_TEXT | LVIF_PARAM | LVIF_STATE,
            iItem: index as i32,
            iSubItem: 0,
            pszText: TEXT_CALLBACK_WIDE,
            cchTextMax: 0,
            lParam: entry.pid as isize,
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: item_state,
            ..zeroed()
        };
        SendMessageW(list_hwnd, LVM_SETITEMW, 0, &mut item as *mut _ as LPARAM);
    }

    unsafe fn fill_display_info(&self, item: &mut LVITEMW) {
        if (item.mask & LVIF_TEXT) == 0
            || item.iItem < 0
            || item.pszText.is_null()
            || item.cchTextMax <= 0
        {
            return;
        }

        let entry = if item.lParam != 0 {
            self.pid_to_index
                .get(&(item.lParam as u32))
                .and_then(|&idx| self.entries.get(idx))
        } else {
            self.entries.get(item.iItem as usize)
        };
        let Some(entry) = entry else {
            *item.pszText = 0;
            return;
        };
        let Some(column_id) = self.active_columns.get(item.iSubItem as usize).copied() else {
            *item.pszText = 0;
            return;
        };

        let text = column_text(entry, column_id, &self.strings);
        copy_text_to_callback_buffer(item.pszText, item.cchTextMax as usize, &text);
    }

    // 销毁现有列并按照 active_columns 重建所有列头。列宽优先从 options 读取，否则用默认值。
    unsafe fn setup_columns(&self, options: &Options) {
        let list_hwnd = self.list_hwnd();
        SendMessageW(list_hwnd, LVM_DELETEALLITEMS, 0, 0);
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
    }

    unsafe fn save_column_widths(&mut self, options: &mut Options) {
        // 列宽始终按“当前显示列顺序”写回配置，而不是按枚举顺序写，
        // 这样下次恢复时才能对上用户真正看到的列布局。
        for value in options.column_widths.iter_mut() {
            *value = -1;
        }

        for index in 0..self.active_columns.len() {
            let cx = SendMessageW(self.list_hwnd(), LVM_GETCOLUMNWIDTH, index, 0) as i32;
            if index < options.column_widths.len() {
                options.column_widths[index] = cx;
            }
        }
    }

    unsafe fn current_selected_pid(&self) -> Option<u32> {
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

        let mut item = LVITEMW {
            mask: LVIF_PARAM,
            iItem: index,
            ..zeroed()
        };
        if SendMessageW(list_hwnd, LVM_GETITEMW, 0, &mut item as *mut _ as LPARAM) != 0 {
            Some(item.lParam as u32)
        } else {
            None
        }
    }

    unsafe fn current_selected_process_identity(&self) -> Option<ProcessIdentity> {
        let pid = self.current_selected_pid()?;
        self.entries
            .iter()
            .find(|entry| entry.pid == pid)
            .map(ProcEntry::identity)
    }

    fn selected_entry(&self) -> Option<&ProcEntry> {
        let pid = self.selected_pid?;
        self.entries.iter().find(|entry| entry.pid == pid)
    }

    unsafe fn quick_confirm(&self, title: &str, body: &str) -> bool {
        // 用户关闭“确认”选项后，危险操作直接放行，保持与原版 Task Manager 行为一致。
        if !self.confirmations {
            return true;
        }

        let title_wide = to_wide_null(title);
        let body_wide = to_wide_null(body);
        MessageBoxW(
            self.hwnd_page,
            body_wide.as_ptr(),
            title_wide.as_ptr(),
            MB_ICONEXCLAMATION | MB_YESNO,
        ) == IDYES
    }

    unsafe fn show_failure_message(&self, body: &str, error: u32) {
        let title = if self.strings.warning.is_empty() {
            "Task Manager".to_string()
        } else {
            self.strings.warning.clone()
        };
        let message = format!("{body}\r\n\r\nWin32 error: {error}");
        let title_wide = to_wide_null(&title);
        let message_wide = to_wide_null(&message);
        MessageBoxW(
            self.hwnd_page,
            message_wide.as_ptr(),
            title_wide.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }

    // 结束指定 PID 的进程。先弹确认框，再通过 TerminateProcess 终止。
    unsafe fn kill_process(&mut self, identity: &ProcessIdentity) -> bool {
        if !self.quick_confirm(&self.strings.warning, &self.strings.kill) {
            return false;
        }

        let handle = match open_revalidated_process(identity, PROCESS_TERMINATE) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_kill, error);
                return false;
            }
        };

        let result = TerminateProcess(handle, 1);
        let error = windows_sys::Win32::Foundation::GetLastError();
        CloseHandle(handle);

        if result == 0 {
            self.show_failure_message(&self.strings.cant_kill, error);
            false
        } else {
            self.paused = false;
            self.refresh_processes();
            true
        }
    }

    // 结束进程以及所有子进程。按叶子优先的顺序遍历进程树，逐进程 TerminateProcess。
    unsafe fn kill_process_tree(&mut self, identity: &ProcessIdentity) -> bool {
        if !self.quick_confirm(&self.strings.warning, &self.strings.kill_tree) {
            return false;
        }

        let termination_order = collect_process_tree_termination_order(identity.pid);
        if termination_order.is_empty() {
            return self.kill_process(identity);
        }

        let mut any_success = false;
        let mut any_failure = false;
        let mut root_error = 0u32;

        for mut target in termination_order {
            if target.pid == identity.pid {
                target.creation_time_100ns = identity.creation_time_100ns;
            } else if target.creation_time_100ns == 0 {
                any_failure = true;
                continue;
            }

            let handle = match open_revalidated_process(&target, PROCESS_TERMINATE) {
                Ok(handle) => handle,
                Err(error) => {
                    any_failure = true;
                    if target.pid == identity.pid {
                        root_error = error;
                    }
                    continue;
                }
            };

            if TerminateProcess(handle, 1) == 0 {
                any_failure = true;
                if target.pid == identity.pid {
                    root_error = windows_sys::Win32::Foundation::GetLastError();
                }
            } else {
                any_success = true;
            }
            CloseHandle(handle);
        }

        if any_success {
            self.paused = false;
            self.refresh_processes();
        }

        if root_error != 0 && !any_success {
            self.show_failure_message(&self.strings.cant_kill, root_error);
            return false;
        }

        if any_failure {
            let body_wide = to_wide_null(&self.strings.kill_tree_fail_body);
            let title_wide = to_wide_null(&self.strings.kill_tree_fail);
            MessageBoxW(
                self.hwnd_page,
                body_wide.as_ptr(),
                title_wide.as_ptr(),
                MB_OK | MB_ICONEXCLAMATION,
            );
            return false;
        }

        any_success
    }

    // 以 AeDebug 注册表配置的调试器启动并附加到目标进程。命令行传 -p <pid>。
    unsafe fn attach_debugger(&mut self, identity: &ProcessIdentity) -> bool {
        let Some(debugger_path) = self.debugger_path.as_ref() else {
            self.show_failure_message(&self.strings.cant_debug, 2);
            return false;
        };

        if !self.quick_confirm(&self.strings.warning, &self.strings.debug) {
            return false;
        }

        let process_handle = match open_revalidated_process(identity, 0) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        CloseHandle(process_handle);

        let command_line = format!(
            "{} -p {}",
            quote_command_line_arg(debugger_path),
            identity.pid
        );
        let mut command_line_wide = to_wide_null(&command_line);
        let application_name = to_wide_null(debugger_path);
        let startup_info = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..zeroed()
        };
        let mut process_info = zeroed::<PROCESS_INFORMATION>();

        let created = CreateProcessW(
            application_name.as_ptr(),
            command_line_wide.as_mut_ptr(),
            null_mut(),
            null_mut(),
            0,
            windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE,
            null(),
            null(),
            &startup_info,
            &mut process_info,
        );

        if created == 0 {
            self.show_failure_message(
                &self.strings.cant_debug,
                windows_sys::Win32::Foundation::GetLastError(),
            );
            false
        } else {
            CloseHandle(process_info.hThread);
            CloseHandle(process_info.hProcess);
            true
        }
    }

    // 通过 explorer.exe /select 命令在资源管理器中定位进程的可执行文件。
    unsafe fn open_file_location(&mut self, identity: &ProcessIdentity) -> bool {
        let handle = match open_revalidated_process(identity, 0) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_open_file_location, error);
                return false;
            }
        };
        let image_path = match query_process_image_path_from_handle(handle) {
            Some(path) => path,
            None => {
                let error = windows_sys::Win32::Foundation::GetLastError();
                CloseHandle(handle);
                self.show_failure_message(&self.strings.cant_open_file_location, error);
                return false;
            }
        };
        CloseHandle(handle);

        if !Path::new(&image_path).exists() {
            self.show_failure_message(&self.strings.cant_open_file_location, 2);
            return false;
        }

        let mut windir = vec![0u16; 260];
        // 安全性: GetWindowsDirectoryW fills the buffer with the canonical Windows directory.
        let windir_len =
            unsafe { GetWindowsDirectoryW(windir.as_mut_ptr(), windir.len() as u32) } as usize;
        let explorer_path = if windir_len > 0 && windir_len < windir.len() {
            format!(
                "{}\\explorer.exe",
                String::from_utf16_lossy(&windir[..windir_len])
            )
        } else {
            return false;
        };
        let command_line = format!(
            "{explorer_path} /select,{}",
            quote_command_line_arg(&image_path)
        );
        let application_name = to_wide_null(&explorer_path);
        let mut command_line_wide = to_wide_null(&command_line);
        let startup_info = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..zeroed()
        };
        let mut process_info = zeroed::<PROCESS_INFORMATION>();
        let created = CreateProcessW(
            application_name.as_ptr(),
            command_line_wide.as_mut_ptr(),
            null_mut(),
            null_mut(),
            0,
            0,
            null(),
            null(),
            &startup_info,
            &mut process_info,
        );
        if created == 0 {
            self.show_failure_message(
                &self.strings.cant_open_file_location,
                windows_sys::Win32::Foundation::GetLastError(),
            );
            return false;
        }

        CloseHandle(process_info.hThread);
        CloseHandle(process_info.hProcess);
        true
    }

    // 通过 SetPriorityClass 修改进程优先级类。先弹确认框，操作成功后刷新列表。
    unsafe fn set_priority(&mut self, identity: &ProcessIdentity, priority: ProcPriority) -> bool {
        let priority_class = match priority {
            ProcPriority::Low => IDLE_PRIORITY_CLASS,
            ProcPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            ProcPriority::Normal => NORMAL_PRIORITY_CLASS,
            ProcPriority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
            ProcPriority::High => HIGH_PRIORITY_CLASS,
            ProcPriority::Realtime => REALTIME_PRIORITY_CLASS,
        };

        if !self.quick_confirm(&self.strings.warning, &self.strings.prichange) {
            return false;
        }

        let handle = match open_revalidated_process(identity, PROCESS_SET_INFORMATION) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_change_priority, error);
                return false;
            }
        };

        let result = SetPriorityClass(handle, priority_class);
        let error = windows_sys::Win32::Foundation::GetLastError();
        CloseHandle(handle);

        if result == 0 {
            self.show_failure_message(&self.strings.cant_change_priority, error);
            false
        } else {
            self.paused = false;
            self.refresh_processes();
            true
        }
    }

    // 通过 SetProcessAffinityMask 设置进程 CPU 亲和性。用户通过对话框选择 CPU。
    unsafe fn set_affinity(&mut self, identity: &ProcessIdentity) -> bool {
        let handle = match open_revalidated_process(
            identity,
            PROCESS_QUERY_INFORMATION | PROCESS_SET_INFORMATION,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_set_affinity, error);
                return false;
            }
        };

        let mut process_mask = 0usize;
        let mut system_mask = 0usize;
        let mut success = false;

        if GetProcessAffinityMask(handle, &mut process_mask, &mut system_mask) != 0 {
            process_mask &= system_mask;
            let mut context = AffinityDialogContext {
                page: self as *mut ProcessPageState,
                process_mask,
            };
            if dialog_box(
                self.hinstance,
                IDD_AFFINITY,
                self.hwnd_page,
                Some(affinity_dialog_proc),
                &mut context as *mut AffinityDialogContext as LPARAM,
            ) == IDOK as isize
            {
                if SetProcessAffinityMask(handle, context.process_mask) == 0 {
                    self.show_failure_message(
                        &self.strings.cant_set_affinity,
                        windows_sys::Win32::Foundation::GetLastError(),
                    );
                } else {
                    self.refresh_processes();
                    success = true;
                }
            }
        }

        CloseHandle(handle);
        success
    }

    // 打开“选择列”对话框。通过 ColumnDialogContext 传递页面和选项指针给 dialog proc。
    unsafe fn pick_columns(&mut self, options: &mut Options) {
        let mut context = ColumnDialogContext {
            page: self as *mut ProcessPageState,
            options: options as *mut Options,
        };
        dialog_box(
            self.hinstance,
            IDD_SELECTPROCCOLS,
            self.main_hwnd,
            Some(column_select_dialog_proc),
            &mut context as *mut ColumnDialogContext as LPARAM,
        );
    }
}

// “选择列”对话框过程。初始化时同步选项状态，确认时将选中的列写回 options。
unsafe extern "system" fn column_select_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
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

                page.save_column_widths(options);
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

// “设置亲和性”对话框过程。根据 processor_count 启用/禁用 CPU 勾选框，
// 确认时检查至少选中一个 CPU，否则弹错误提示。
unsafe extern "system" fn affinity_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            set_window_userdata(hwnd, lparam);
            localize_dialog(hwnd, IDD_AFFINITY);
            let context = &*(lparam as *const AffinityDialogContext);
            let page = &*context.page;

            for cpu_index in 0..=MAX_AFFINITY_CPU {
                let control_id = IDC_CPU0 + cpu_index;
                let enabled = cpu_index < page.processor_count as i32;
                EnableWindow(GetDlgItem(hwnd, control_id), i32::from(enabled));
                CheckDlgButton(
                    hwnd,
                    control_id,
                    if enabled && (context.process_mask & (1usize << cpu_index)) != 0 {
                        BST_CHECKED
                    } else {
                        BST_UNCHECKED
                    },
                );
            }
            1
        }
        WM_COMMAND => match i32::from(loword(wparam)) {
            IDCANCEL => {
                EndDialog(hwnd, IDCANCEL as isize);
                1
            }
            IDOK => {
                let context = &mut *(get_window_userdata(hwnd) as *mut AffinityDialogContext);
                let page = &*context.page;

                context.process_mask = 0;
                for cpu_index in 0..page.processor_count.min((MAX_AFFINITY_CPU + 1) as usize) {
                    if IsDlgButtonChecked(hwnd, IDC_CPU0 + cpu_index as i32) == BST_CHECKED {
                        context.process_mask |= 1usize << cpu_index;
                    }
                }

                if context.process_mask == 0 {
                    let title_wide = to_wide_null(&page.strings.invalid_option);
                    let body_wide = to_wide_null(&page.strings.no_affinity_mask);
                    MessageBoxW(hwnd, body_wide.as_ptr(), title_wide.as_ptr(), MB_ICONERROR);
                    1
                } else {
                    EndDialog(hwnd, IDOK as isize);
                    1
                }
            }
            _ => 0,
        },
        _ => 0,
    }
}

// 将对话框中的列勾选状态持久化到 options。保留已有列宽，新增列使用默认宽度。
unsafe fn apply_selected_columns(hwnd: HWND, options: &mut Options) {
    let mut existing_widths = HashMap::with_capacity(NUM_COLUMN);
    for (index, value) in options.active_process_columns.iter().copied().enumerate() {
        let Some(column) = column_id_from_i32(value) else {
            break;
        };
        existing_widths.insert(column as i32, options.column_widths[index]);
    }

    for value in options.active_process_columns.iter_mut() {
        *value = -1;
    }
    for value in options.column_widths.iter_mut() {
        *value = -1;
    }

    let mut next_index = 0usize;
    options.active_process_columns[next_index] = ColumnId::ImageName as i32;
    options.column_widths[next_index] = existing_widths
        .get(&(ColumnId::ImageName as i32))
        .copied()
        .filter(|width| *width > 0)
        .unwrap_or(PROCESS_COLUMNS[ColumnId::ImageName as usize].default_width);
    next_index += 1;

    for (column_index, &control_id) in COLUMN_DIALOG_IDS
        .iter()
        .enumerate()
        .take(NUM_COLUMN)
        .skip(1)
    {
        if IsDlgButtonChecked(hwnd, control_id) == BST_CHECKED {
            let column = column_id_from_i32(column_index as i32).unwrap_or(ColumnId::Pid);
            options.active_process_columns[next_index] = column as i32;
            options.column_widths[next_index] = existing_widths
                .get(&(column as i32))
                .copied()
                .filter(|width| *width > 0)
                .unwrap_or(PROCESS_COLUMNS[column as usize].default_width);
            next_index += 1;
        }
    }
}

unsafe fn load_debugger_path() -> Option<String> {
    // 进程页的“调试”命令依赖 AeDebug 注册表配置。
    // 这里只提取真正的可执行文件路径，过滤掉旧式 drwtsn32 之类的无效值。
    let mut key: HKEY = null_mut();
    let key_name = to_wide_null("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AeDebug");
    let value_name = to_wide_null("Debugger");
    if RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_name.as_ptr(), 0, KEY_READ, &mut key) != 0 {
        return None;
    }

    let mut value_size = 0u32;
    let size_status = RegQueryValueExW(
        key,
        value_name.as_ptr(),
        null_mut(),
        null_mut(),
        null_mut(),
        &mut value_size,
    );
    if size_status != 0 || value_size < 2 {
        RegCloseKey(key);
        return None;
    }

    let mut buffer = vec![0u16; (value_size as usize / size_of::<u16>()).max(2)];
    let mut value_type = 0u32;
    let status = RegQueryValueExW(
        key,
        value_name.as_ptr(),
        null_mut(),
        &mut value_type,
        buffer.as_mut_ptr() as *mut u8,
        &mut value_size,
    );
    RegCloseKey(key);

    if status != 0 || value_size < 2 || !(value_type == REG_SZ || value_type == REG_EXPAND_SZ) {
        return None;
    }

    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    let raw = String::from_utf16_lossy(&buffer[..length]);
    let executable = normalize_debugger_command(&raw, value_type)?;
    Path::new(&executable).is_file().then_some(executable)
}

// 引用命令行参数。只在包含空格、制表符或引号时加引号，并正确处理反斜杠转义。
fn quote_command_line_arg(value: &str) -> String {
    if !value.contains([' ', '\t', '"']) {
        return value.to_string();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;

    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }

        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
            continue;
        }

        if backslashes > 0 {
            quoted.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
        }
        quoted.push(ch);
    }

    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

// 从命令行字符串中提取第一个 token（即可执行文件路径）。处理引号包裹和非引号两种格式。
fn extract_first_command_token(command_line: &str) -> String {
    let trimmed = command_line.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        rest.split('"').next().unwrap_or_default().to_string()
    } else {
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

// 将 AeDebug 注册表值规范化：展开环境变量后提取可执行文件路径，过滤无效调试器。
fn normalize_debugger_command(command_line: &str, value_type: u32) -> Option<String> {
    normalize_debugger_command_with(command_line, value_type, expand_environment_variables)
}

fn normalize_debugger_command_with<F>(
    command_line: &str,
    value_type: u32,
    expand_environment_variables: F,
) -> Option<String>
where
    F: Fn(&str) -> String,
{
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)
    } else {
        command_line.to_string()
    };
    let executable = extract_first_command_token(&expanded);

    if executable.is_empty()
        || executable.eq_ignore_ascii_case("drwtsn32")
        || executable.eq_ignore_ascii_case("drwtsn32.exe")
    {
        None
    } else {
        Some(executable)
    }
}

// 展开字符串中的环境变量（如 %SystemRoot%）。
// 使用 Win32 ExpandEnvironmentStringsW API，正确处理 WOW64 重定向和 %% 转义。
fn expand_environment_variables(command_line: &str) -> String {
    // 安全性: the Win32 ExpandEnvironmentStringsW API reads the process environment block
    // maintained by the kernel, which handles system-variable edge cases (WOW64 redirections,
    // %% escaping, variable-length limits) correctly.
    let wide_input = to_wide_null(command_line);
    let required = unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), null_mut(), 0) };
    if required == 0 {
        return command_line.to_string();
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), buffer.as_mut_ptr(), required) };
    if written == 0 || written > required {
        return command_line.to_string();
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

// 从进程令牌查询用户名和 SessionId。
// 安全性: 通过 OpenProcessToken + GetTokenInformation 读取令牌信息，
// TOKEN_USER 结构中的 SID 指针指向令牌内的内存，直接 read_unaligned 并用 LookupAccountSidW 解析。
// handle 由调用方保证有效（OpenProcess 返回值且不为 NULL）。
unsafe fn query_process_identity(process_handle: HANDLE) -> (String, u32) {
    // 从访问令牌补采用户名和 SessionId，
    // 这是对 WTS 进程枚举结果的补强，能覆盖更多权限和边界情况。
    let mut token: HANDLE = null_mut();
    if OpenProcessToken(process_handle, TOKEN_QUERY, &mut token) == 0 || token.is_null() {
        return (String::new(), 0);
    }

    let mut session_id = 0u32;
    let mut session_bytes = size_of::<u32>() as u32;
    let _ = GetTokenInformation(
        token,
        TokenSessionId,
        &mut session_id as *mut _ as *mut _,
        session_bytes,
        &mut session_bytes,
    );

    let mut required = 0u32;
    let _ = GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required);
    if required == 0 {
        CloseHandle(token);
        return (String::new(), session_id);
    }

    let mut buffer = vec![0u8; required as usize];
    let mut user_name = String::new();
    if GetTokenInformation(
        token,
        TokenUser,
        buffer.as_mut_ptr() as *mut _,
        required,
        &mut required,
    ) != 0
    {
        let token_user = std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER);
        user_name = lookup_account_name_from_sid(token_user.User.Sid);
    }

    CloseHandle(token);
    (user_name, session_id)
}

// 检查 SID 是否为已知服务帐户（SYSTEM、LOCAL SERVICE、NETWORK SERVICE），返回对应名称。
fn well_known_service_name(sid: *mut core::ffi::c_void) -> Option<String> {
    unsafe {
        if IsWellKnownSid(sid, WinLocalSystemSid) != 0 {
            Some("SYSTEM".to_string())
        } else if IsWellKnownSid(sid, WinLocalServiceSid) != 0 {
            Some("LOCAL SERVICE".to_string())
        } else if IsWellKnownSid(sid, WinNetworkServiceSid) != 0 {
            Some("NETWORK SERVICE".to_string())
        } else {
            None
        }
    }
}

// 将进程令牌中的用户名、SessionId 和 32-bit 标记合并进 ProcEntry。
unsafe fn merge_process_identity(entry: &mut ProcEntry, process_handle: HANDLE) {
    let (user_name, session_id) = query_process_identity(process_handle);
    if !user_name.is_empty() {
        entry.user_name = user_name;
        entry.user_name_lower = entry.user_name.to_lowercase();
    }
    if session_id != 0 || entry.session_id == 0 {
        entry.session_id = session_id;
    }
    entry.is_32_bit = is_32_bit_process_handle(process_handle);
}

// 通过 LookupAccountSidW 将 SID 解析为用户名。失败时回退到 well_known_service_name。
unsafe fn lookup_account_name_from_sid(sid: *mut core::ffi::c_void) -> String {
    if sid.is_null() {
        return String::new();
    }

    let mut name_len = 0u32;
    let mut domain_len = 0u32;
    let mut sid_use = 0 as SID_NAME_USE;
    let _ = LookupAccountSidW(
        null_mut(),
        sid,
        null_mut(),
        &mut name_len,
        null_mut(),
        &mut domain_len,
        &mut sid_use,
    );

    if name_len != 0 {
        let mut name = vec![0u16; name_len as usize];
        let mut domain = vec![0u16; domain_len as usize];
        if LookupAccountSidW(
            null_mut(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut sid_use,
        ) != 0
        {
            return String::from_utf16_lossy(&name[..name_len as usize]);
        }
    }

    well_known_service_name(sid).unwrap_or_default()
}

unsafe fn collect_process_identity_map() -> HashMap<u32, (String, u32)> {
    // WTS 进程枚举能一次拿到大量进程对应的 SID / Session 信息，
    // 先建表再回填到快照里，比逐进程单查用户名更高效。
    let mut process_info = null_mut::<WTS_PROCESS_INFOW>();
    let mut count = 0u32;

    if WTSEnumerateProcessesW(
        WTS_CURRENT_SERVER_HANDLE,
        0,
        1,
        &mut process_info,
        &mut count,
    ) == 0
        || process_info.is_null()
    {
        return HashMap::new();
    }

    let mut identities = HashMap::with_capacity(count as usize);
    let processes = slice::from_raw_parts(process_info, count as usize);
    for process in processes {
        let pid = process.ProcessId;
        let user_name = if pid == 0 {
            "SYSTEM".to_string()
        } else {
            lookup_account_name_from_sid(process.pUserSid)
        };
        identities.insert(pid, (user_name, process.SessionId));
    }

    WTSFreeMemory(process_info as _);
    identities
}

// 从 Options 中解析当前激活的列列表，过滤无效列 ID。
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
fn compare_entries(
    left: &ProcEntry,
    right: &ProcEntry,
    sort_column: ColumnId,
    sort_direction: i32,
) -> Ordering {
    let ordering = match sort_column {
        ColumnId::ImageName => left.image_name_lower.cmp(&right.image_name_lower),
        ColumnId::Pid => left.pid.cmp(&right.pid),
        ColumnId::Username => left.user_name_lower.cmp(&right.user_name_lower),
        ColumnId::SessionId => left.session_id.cmp(&right.session_id),
        ColumnId::Cpu => left.cpu.cmp(&right.cpu),
        ColumnId::CpuTime => left.cpu_time_100ns.cmp(&right.cpu_time_100ns),
        ColumnId::MemUsage => left.mem_usage_kb.cmp(&right.mem_usage_kb),
        ColumnId::MemUsageDiff => left.mem_diff_kb.cmp(&right.mem_diff_kb),
        ColumnId::PageFaults => left.page_faults.cmp(&right.page_faults),
        ColumnId::PageFaultsDiff => left.page_faults_diff.cmp(&right.page_faults_diff),
        ColumnId::CommitCharge => left.commit_charge_kb.cmp(&right.commit_charge_kb),
        ColumnId::PagedPool => left.paged_pool_kb.cmp(&right.paged_pool_kb),
        ColumnId::NonPagedPool => left.nonpaged_pool_kb.cmp(&right.nonpaged_pool_kb),
        ColumnId::BasePriority => {
            priority_rank(left.priority_class).cmp(&priority_rank(right.priority_class))
        }
        ColumnId::HandleCount => left.handle_count.cmp(&right.handle_count),
        ColumnId::ThreadCount => left.thread_count.cmp(&right.thread_count),
    };

    if ordering == Ordering::Equal {
        let tie_break = left.pid.cmp(&right.pid);
        if sort_direction < 0 {
            tie_break.reverse()
        } else {
            tie_break
        }
    } else if sort_direction < 0 {
        ordering.reverse()
    } else {
        ordering
    }
}

// 将 Win32 优先级类常量映射为排序用的数值等级。
fn priority_rank(priority_class: u32) -> u8 {
    match priority_class {
        REALTIME_PRIORITY_CLASS => 5,
        HIGH_PRIORITY_CLASS => 4,
        ABOVE_NORMAL_PRIORITY_CLASS => 3,
        NORMAL_PRIORITY_CLASS => 2,
        BELOW_NORMAL_PRIORITY_CLASS => 1,
        _ => 0,
    }
}

fn column_text(entry: &ProcEntry, column_id: ColumnId, strings: &ProcessStrings) -> String {
    // 所有列表文本都统一从这里派生，便于保持格式一致，
    // 也方便 owner-data 回调按列生成内容。
    match column_id {
        ColumnId::ImageName => append_32_bit_suffix(&entry.image_name, entry.is_32_bit),
        ColumnId::Pid => entry.pid.to_string(),
        ColumnId::Username => entry.user_name.clone(),
        ColumnId::SessionId => entry.session_id.to_string(),
        ColumnId::Cpu => format!("{:02} %", entry.cpu),
        ColumnId::CpuTime => format_elapsed_time(entry.display_cpu_time_100ns),
        ColumnId::MemUsage => format_kilobytes(entry.mem_usage_kb),
        ColumnId::MemUsageDiff => format_signed_kilobytes(entry.mem_diff_kb),
        ColumnId::PageFaults => entry.page_faults.to_string(),
        ColumnId::PageFaultsDiff => entry.page_faults_diff.to_string(),
        ColumnId::CommitCharge => format_kilobytes(entry.commit_charge_kb),
        ColumnId::PagedPool => format_kilobytes(entry.paged_pool_kb),
        ColumnId::NonPagedPool => format_kilobytes(entry.nonpaged_pool_kb),
        ColumnId::BasePriority => match entry.priority_class {
            value if value == IDLE_PRIORITY_CLASS => strings.priority_low.clone(),
            value if value == BELOW_NORMAL_PRIORITY_CLASS => strings.priority_below_normal.clone(),
            value if value == HIGH_PRIORITY_CLASS => strings.priority_high.clone(),
            value if value == ABOVE_NORMAL_PRIORITY_CLASS => strings.priority_above_normal.clone(),
            value if value == REALTIME_PRIORITY_CLASS => strings.priority_realtime.clone(),
            value if value == NORMAL_PRIORITY_CLASS => strings.priority_normal.clone(),
            _ => strings.priority_unknown.clone(),
        },
        ColumnId::HandleCount => entry.handle_count.to_string(),
        ColumnId::ThreadCount => entry.thread_count.to_string(),
    }
}

// 将 100ns 精度的 CPU 时间格式化为 HH:MM:SS 友好显示。
fn format_elapsed_time(total_100ns: u64) -> String {
    let total_seconds = total_100ns / 10_000_000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:2}:{minutes:02}:{seconds:02}")
}

fn format_kilobytes(value: u32) -> String {
    format!("{value} K")
}

fn format_signed_kilobytes(value: i64) -> String {
    format!("{value} K")
}

// 将 Rust 字符串复制到 Win32 LVITEMW.pszText 回调缓冲区，自动截断并置空终止。
// 安全性: buffer 由 ListView 控件保证有效且容量足够，调用方负责确保不超过 cchTextMax。
fn copy_text_to_callback_buffer(buffer: *mut u16, capacity: usize, text: &str) {
    if buffer.is_null() || capacity == 0 {
        return;
    }
    let max_len = capacity.saturating_sub(1);
    let mut written = 0usize;
    for code_unit in text.encode_utf16().take(max_len) {
        unsafe {
            *buffer.add(written) = code_unit;
        }
        written += 1;
    }
    unsafe {
        *buffer.add(written) = 0;
    }
}

// 遍历进程树，按“子进程先于父进程”的顺序返回终止列表（后序遍历）。
// 使用 Toolhelp 快照建立父子关系表，递归收集子进程。
fn collect_process_tree_termination_order(root_pid: u32) -> Vec<ProcessIdentity> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return vec![ProcessIdentity {
                pid: root_pid,
                creation_time_100ns: query_process_creation_time_for_pid(root_pid).unwrap_or(0),
            }];
        }

        let mut child_map = HashMap::<u32, Vec<u32>>::new();
        let mut process_entry = zeroed::<PROCESSENTRY32W>();
        process_entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut process_entry) != 0 {
            loop {
                child_map
                    .entry(process_entry.th32ParentProcessID)
                    .or_default()
                    .push(process_entry.th32ProcessID);
                if Process32NextW(snapshot, &mut process_entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);

        let mut order = Vec::new();
        let mut visited = HashMap::<u32, ()>::new();
        collect_process_tree_children(root_pid, &child_map, &mut visited, &mut order);
        order
            .into_iter()
            .map(|pid| ProcessIdentity {
                pid,
                creation_time_100ns: query_process_creation_time_for_pid(pid).unwrap_or(0),
            })
            .collect()
    }
}

// 后序遍历进程树，将 PID 按叶子优先的顺序加入 order。visited 防止循环引用。
fn collect_process_tree_children(
    pid: u32,
    child_map: &HashMap<u32, Vec<u32>>,
    visited: &mut HashMap<u32, ()>,
    order: &mut Vec<u32>,
) {
    if visited.insert(pid, ()).is_some() {
        return;
    }

    if let Some(children) = child_map.get(&pid) {
        for &child_pid in children {
            collect_process_tree_children(child_pid, child_map, visited, order);
        }
    }

    order.push(pid);
}

unsafe fn query_process_image_path_from_handle(handle: HANDLE) -> Option<String> {
    let mut capacity = 32768u32;
    let mut buffer = vec![0u16; capacity as usize];
    let success = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut capacity);
    if success == 0 {
        return None;
    }

    Some(String::from_utf16_lossy(&buffer[..capacity as usize]))
}

unsafe fn query_process_creation_time(handle: HANDLE) -> Option<u64> {
    let mut creation = zeroed::<FILETIME>();
    let mut exit = zeroed::<FILETIME>();
    let mut kernel = zeroed::<FILETIME>();
    let mut user = zeroed::<FILETIME>();
    (GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0)
        .then_some(filetime_to_u64(creation))
}

unsafe fn query_process_creation_time_for_pid(pid: u32) -> Option<u64> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
        return None;
    }
    let creation_time = query_process_creation_time(handle);
    CloseHandle(handle);
    creation_time
}

unsafe fn open_revalidated_process(
    identity: &ProcessIdentity,
    desired_access: u32,
) -> Result<HANDLE, u32> {
    let handle = OpenProcess(
        desired_access | PROCESS_QUERY_LIMITED_INFORMATION,
        0,
        identity.pid,
    );
    if handle.is_null() {
        return Err(windows_sys::Win32::Foundation::GetLastError());
    }

    if identity.creation_time_100ns != 0 {
        match query_process_creation_time(handle) {
            Some(creation_time) if creation_time == identity.creation_time_100ns => {}
            _ => {
                CloseHandle(handle);
                return Err(ERROR_INVALID_PARAMETER);
            }
        }
    }

    Ok(handle)
}

unsafe fn collect_process_entries(
    previous_samples: &HashMap<u32, PreviousProcSample>,
    total_delta: u64,
) -> (Vec<ProcEntry>, HashMap<u32, PreviousProcSample>) {
    // 采样阶段只构造“当下这一轮”的快照，真正的增量计算依赖外部传入的历史样本。
    let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snapshot == INVALID_HANDLE_VALUE {
        return (Vec::new(), HashMap::new());
    }

    let mut entries = Vec::with_capacity(previous_samples.len().max(64));
    let mut next_samples = HashMap::with_capacity(previous_samples.len().max(64));
    let identities = collect_process_identity_map();
    let mut process_entry = zeroed::<PROCESSENTRY32W>();
    process_entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    if Process32FirstW(snapshot, &mut process_entry) != 0 {
        loop {
            let pid = process_entry.th32ProcessID;
            let thread_count = process_entry.cntThreads;
            let image_name = utf16_buffer_to_string(&process_entry.szExeFile);
            let mut entry = ProcEntry {
                pid,
                creation_time_100ns: 0,
                image_name: image_name.clone(),
                image_name_lower: image_name.to_lowercase(),
                is_32_bit: false,
                user_name: String::new(),
                user_name_lower: String::new(),
                session_id: 0,
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
                thread_count,
                pass_count: 0,
                dirty_columns: DirtyColumns::default(),
            };
            let mut raw_cpu_time_100ns = 0u64;

            if let Some((user_name, session_id)) = identities.get(&pid) {
                entry.user_name = user_name.clone();
                entry.user_name_lower = user_name.to_lowercase();
                entry.session_id = *session_id;
            }

            if pid == 0 {
                entry.user_name = "SYSTEM".to_string();
                entry.user_name_lower = "system".to_string();
            }

            let process_handle = if pid == 0 {
                null_mut()
            } else {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid)
            };
            if !process_handle.is_null() {
                merge_process_identity(&mut entry, process_handle);
                let mut creation = zeroed::<FILETIME>();
                let mut exit = zeroed::<FILETIME>();
                let mut kernel = zeroed::<FILETIME>();
                let mut user = zeroed::<FILETIME>();
                if GetProcessTimes(
                    process_handle,
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                ) != 0
                {
                    entry.creation_time_100ns = filetime_to_u64(creation);
                    let cpu_time_100ns =
                        filetime_to_u64(kernel).saturating_add(filetime_to_u64(user));
                    let previous = previous_samples.get(&pid).cloned().unwrap_or_default();
                    let delta = cpu_time_100ns.saturating_sub(previous.raw_cpu_time_100ns);
                    raw_cpu_time_100ns = cpu_time_100ns;
                    entry.cpu_time_100ns = cpu_time_100ns;
                    entry.display_cpu_time_100ns = cpu_time_100ns;
                    entry.cpu = cpu_percent_from_delta(delta, total_delta);
                }

                let mut counters = PROCESS_MEMORY_COUNTERS_EX {
                    cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ..zeroed()
                };
                if K32GetProcessMemoryInfo(
                    process_handle,
                    &mut counters as *mut _ as *mut _,
                    size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                ) != 0
                {
                    let previous = previous_samples.get(&pid).cloned().unwrap_or_default();
                    entry.mem_usage_kb = (counters.WorkingSetSize / 1024) as u32;
                    entry.mem_diff_kb =
                        i64::from(entry.mem_usage_kb) - i64::from(previous.mem_usage_kb);
                    entry.page_faults = counters.PageFaultCount;
                    entry.page_faults_diff =
                        i64::from(entry.page_faults) - i64::from(previous.page_faults);
                    entry.commit_charge_kb = (counters.PrivateUsage / 1024) as u32;
                    entry.paged_pool_kb = (counters.QuotaPagedPoolUsage / 1024) as u32;
                    entry.nonpaged_pool_kb = (counters.QuotaNonPagedPoolUsage / 1024) as u32;
                }

                let mut handle_count = 0u32;
                if GetProcessHandleCount(process_handle, &mut handle_count) != 0 {
                    entry.handle_count = handle_count;
                }

                let priority_class = GetPriorityClass(process_handle);
                if priority_class != 0 {
                    entry.priority_class = priority_class;
                }

                next_samples.insert(
                    pid,
                    PreviousProcSample {
                        raw_cpu_time_100ns,
                        mem_usage_kb: entry.mem_usage_kb,
                        page_faults: entry.page_faults,
                    },
                );

                CloseHandle(process_handle);
            } else if pid != 0 {
                let identity_handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !identity_handle.is_null() {
                    merge_process_identity(&mut entry, identity_handle);
                    CloseHandle(identity_handle);
                }
            }

            entries.push(entry);

            if Process32NextW(snapshot, &mut process_entry) == 0 {
                break;
            }
        }
    }

    CloseHandle(snapshot);

    (entries, next_samples)
}

// 读取系统总 CPU 时间（kernel + user），用于计算每轮刷新的 CPU 时间增量。
unsafe fn current_system_time() -> u64 {
    let mut idle = zeroed::<FILETIME>();
    let mut kernel = zeroed::<FILETIME>();
    let mut user = zeroed::<FILETIME>();
    if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
        0
    } else {
        filetime_to_u64(kernel).saturating_add(filetime_to_u64(user))
    }
}

// 将 Win32 FILETIME 结构合并为一个 u64（100ns 单位）。
fn filetime_to_u64(filetime: FILETIME) -> u64 {
    (u64::from(filetime.dwHighDateTime) << 32) | u64::from(filetime.dwLowDateTime)
}

// 将以 null 结尾的 UTF-16 切片转换为 Rust String，忽略 BOM 和无效代理对。
fn utf16_buffer_to_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

// 根据进程 CPU 时间增量与系统总时间增量计算 CPU 使用率百分比（0-99）。
// 除 1000 再除 10 的等效运算，避免浮点计算。
fn cpu_percent_from_delta(delta_100ns: u64, total_delta_100ns: u64) -> u8 {
    if total_delta_100ns == 0 {
        return 0;
    }

    let scaled_total = (total_delta_100ns / 1000).max(1);
    (((delta_100ns / scaled_total) + 5) / 10).min(99) as u8
}

// 获取窗口相对于页面客户区的矩形坐标（MapWindowPoints）。
unsafe fn window_rect_relative_to_page(hwnd: HWND, page_hwnd: HWND) -> RECT {
    let mut rect = zeroed::<RECT>();
    windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
    MapWindowPoints(null_mut(), page_hwnd, &mut rect as *mut _ as _, 2);
    rect
}

impl ProcEntry {
    fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            creation_time_100ns: self.creation_time_100ns,
        }
    }

    fn with_pass_count(mut self, pass_count: u64) -> Self {
        self.pass_count = pass_count;
        self.dirty_columns = DirtyColumns::all();
        self
    }
}

fn update_process_entry(entry: &mut ProcEntry, snapshot: &ProcEntry, pass_count: u64) {
    // 增量更新时只给真正变更的列打脏标记，
    // 后续 ListView 才能做到“只重绘必要行/列”。
    entry.pass_count = pass_count;

    if entry.image_name != snapshot.image_name {
        entry.image_name.clone_from(&snapshot.image_name);
        entry.image_name_lower = snapshot.image_name_lower.clone();
        entry.dirty_columns.mark(ColumnId::ImageName);
    }
    if entry.is_32_bit != snapshot.is_32_bit {
        entry.is_32_bit = snapshot.is_32_bit;
        entry.dirty_columns.mark(ColumnId::ImageName);
    }
    if entry.pid != snapshot.pid {
        entry.pid = snapshot.pid;
        entry.dirty_columns.mark(ColumnId::Pid);
    }
    entry.creation_time_100ns = snapshot.creation_time_100ns;
    if entry.user_name != snapshot.user_name {
        entry.user_name.clone_from(&snapshot.user_name);
        entry.user_name_lower = snapshot.user_name_lower.clone();
        entry.dirty_columns.mark(ColumnId::Username);
    }
    if entry.session_id != snapshot.session_id {
        entry.session_id = snapshot.session_id;
        entry.dirty_columns.mark(ColumnId::SessionId);
    }
    if entry.cpu != snapshot.cpu {
        entry.cpu = snapshot.cpu;
        entry.dirty_columns.mark(ColumnId::Cpu);
    }
    if entry.cpu_time_100ns != snapshot.cpu_time_100ns {
        entry.cpu_time_100ns = snapshot.cpu_time_100ns;
    }
    if entry.display_cpu_time_100ns != snapshot.display_cpu_time_100ns {
        entry.display_cpu_time_100ns = snapshot.display_cpu_time_100ns;
        entry.dirty_columns.mark(ColumnId::CpuTime);
    }
    if entry.mem_usage_kb != snapshot.mem_usage_kb {
        entry.mem_usage_kb = snapshot.mem_usage_kb;
        entry.dirty_columns.mark(ColumnId::MemUsage);
    }
    if entry.mem_diff_kb != snapshot.mem_diff_kb {
        entry.mem_diff_kb = snapshot.mem_diff_kb;
        entry.dirty_columns.mark(ColumnId::MemUsageDiff);
    }
    if entry.page_faults != snapshot.page_faults {
        entry.page_faults = snapshot.page_faults;
        entry.dirty_columns.mark(ColumnId::PageFaults);
    }
    if entry.page_faults_diff != snapshot.page_faults_diff {
        entry.page_faults_diff = snapshot.page_faults_diff;
        entry.dirty_columns.mark(ColumnId::PageFaultsDiff);
    }
    if entry.commit_charge_kb != snapshot.commit_charge_kb {
        entry.commit_charge_kb = snapshot.commit_charge_kb;
        entry.dirty_columns.mark(ColumnId::CommitCharge);
    }
    if entry.paged_pool_kb != snapshot.paged_pool_kb {
        entry.paged_pool_kb = snapshot.paged_pool_kb;
        entry.dirty_columns.mark(ColumnId::PagedPool);
    }
    if entry.nonpaged_pool_kb != snapshot.nonpaged_pool_kb {
        entry.nonpaged_pool_kb = snapshot.nonpaged_pool_kb;
        entry.dirty_columns.mark(ColumnId::NonPagedPool);
    }
    if entry.priority_class != snapshot.priority_class {
        entry.priority_class = snapshot.priority_class;
        entry.dirty_columns.mark(ColumnId::BasePriority);
    }
    if entry.handle_count != snapshot.handle_count {
        entry.handle_count = snapshot.handle_count;
        entry.dirty_columns.mark(ColumnId::HandleCount);
    }
    if entry.thread_count != snapshot.thread_count {
        entry.thread_count = snapshot.thread_count;
        entry.dirty_columns.mark(ColumnId::ThreadCount);
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_first_command_token, normalize_debugger_command_with};
    use windows_sys::Win32::System::Registry::{REG_EXPAND_SZ, REG_SZ};

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
}
