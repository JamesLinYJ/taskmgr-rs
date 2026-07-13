use std::collections::HashMap;

// 用户页实现。
// 这里负责枚举终端服务会话、刷新用户列表，并处理发送消息、断开连接、
// 注销等会话级操作。
use std::mem::{size_of, zeroed};
use std::ptr::null_mut;
use std::slice;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_GEN_FAILURE, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER,
    ERROR_NOT_ENOUGH_MEMORY, HWND, LPARAM, RECT, WPARAM,
};

use windows_sys::Win32::System::RemoteDesktop::{
    WTSActive, WTSClientName, WTSConnectQuery, WTSConnected, WTSDisconnectSession, WTSDisconnected,
    WTSDown, WTSEnumerateSessionsW, WTSIdle, WTSInit, WTSListen, WTSLogoffSession,
    WTSQuerySessionInformationW, WTSReset, WTSSendMessageW, WTSSessionInfo, WTSShadow, WTSINFOW,
    WTS_CONNECTSTATE_CLASS, WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW,
};
use windows_sys::Win32::UI::Controls::{
    LVCFMT_LEFT, LVCFMT_RIGHT, LVCF_FMT, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW,
    LVIF_PARAM, LVIF_STATE, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVITEMW, LVM_DELETECOLUMN,
    LVM_DELETEITEM, LVM_ENSUREVISIBLE, LVM_GETITEMCOUNT, LVM_GETITEMW, LVM_GETNEXTITEM,
    LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETITEMSTATE, LVM_SETITEMW, LVNI_SELECTED,
    LVN_COLUMNCLICK, LVN_ITEMCHANGED, NMLISTVIEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, EndDialog, GetClientRect,
    GetDialogBaseUnits, GetDlgItem, GetWindowTextLengthW, GetWindowTextW, MessageBoxW,
    PostMessageW, SendMessageW, TrackPopupMenuEx, HMENU, IDCANCEL, IDOK, IDYES, MB_DEFBUTTON2,
    MB_ICONERROR, MB_ICONEXCLAMATION, MB_ICONINFORMATION, MB_OK, MB_TOPMOST, MB_YESNO,
    MF_BYCOMMAND, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_UNCHECKED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, TPM_RETURNCMD, WM_COMMAND, WM_INITDIALOG, WM_SETREDRAW,
};

use crate::dialog_templates::dialog_box;
use crate::language::{
    localize_dialog, session_state, text, user_column_titles, user_session_column_title, TextKey,
};
use crate::menus::build_popup_menu;
use crate::options::Options;
use crate::resource::{
    IDC_MESSAGE_MESSAGE, IDC_MESSAGE_TITLE, IDC_USERLIST, IDD_MESSAGE, IDM_DISCONNECT, IDM_LOGOFF,
    IDM_SENDMESSAGE, IDM_SHOWDOMAINNAMES, IDR_USER_CONTEXT, PWM_USER_WORKER_COMPLETE,
};
use crate::winutil::{
    destroy_menu_handle, finish_list_view_update, get_window_userdata, loword, record_win32_error,
    set_window_userdata, subclass_list_view, to_wide_null, window_rect_relative_to_page,
    OwnedWtsMemory,
};
const DEFSPACING_BASE: i32 = 3;
const DLG_SCALE_X: i32 = 4;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UserSessionIdentity {
    session_id: u32,
    logon_time_100ns: i64,
    user_name: String,
    domain_name: String,
}

struct UserSessionSnapshot {
    identity: UserSessionIdentity,
    status: String,
    client_name: String,
    session_name: String,
}

struct UserSessionEntry {
    // `UserSessionEntry` 保存一行用户/会话信息以及最小重绘所需的脏标志。
    identity: UserSessionIdentity,
    display_name: String,
    display_name_lower: String,
    status: String,
    status_lower: String,
    client_name: String,
    client_name_lower: String,
    session_name: String,
    session_name_lower: String,
    dirty: bool,
}

type UserWorkerResult = Result<Vec<UserSessionSnapshot>, u32>;

enum UserWorkerCommand {
    Collect { notify_hwnd: isize },
    Shutdown,
}

#[derive(Default)]
struct MessageDialogResult {
    // 发消息对话框退出后，把标题和正文一起打包回调用点。
    title: String,
    body: String,
}

#[derive(Default)]
pub struct UserPageState {
    // 用户页状态对象维护会话列表、当前排序方式以及与上下文菜单相关的选中状态。
    hinstance: isize,
    hwnd: HWND,
    no_title: bool,
    show_domain_names: bool,
    selected_session_identity: Option<UserSessionIdentity>,
    sessions: Vec<UserSessionEntry>,
    sort_column: usize,
    sort_ascending: bool,
    worker_sender: Option<Sender<UserWorkerCommand>>,
    worker_result_receiver: Option<Receiver<UserWorkerResult>>,
    worker_thread: Option<JoinHandle<()>>,
    collection_in_flight: bool,
    refresh_requested: bool,
    last_refresh_error: Option<u32>,
}

impl UserPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hwnd: HWND) -> Result<(), u32> {
        // 初始化只配置 ListView 和布局；首轮会话枚举由激活或首帧后的预热入口触发。
        // 安全性: all Win32 calls target the user page HWND and its child controls during UI-thread
        // initialization.
        unsafe {
            self.hinstance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null_mut()) as isize;
            self.hwnd = hwnd;
            self.start_worker_thread()?;
            let list = self.list_hwnd();
            if !list.is_null() {
                subclass_list_view(list);
            }
            self.configure_columns();
            self.size_page();
        }
        Ok(())
    }

    pub fn apply_options(&mut self, options: &Options) {
        // 用户页当前只跟随全局“无标题模式”。
        self.no_title = options.no_title();
    }

    pub fn no_title(&self) -> bool {
        self.no_title
    }

    pub fn show_domain_names(&self) -> bool {
        self.show_domain_names
    }

    pub fn timer_event(&mut self) {
        // 用户/会话状态变化相对较慢，所以每轮刷新只做一次重新枚举。
        self.refresh();
    }

    pub fn destroy(&mut self) {
        self.stop_worker_thread();
    }

    fn start_worker_thread(&mut self) -> Result<(), u32> {
        if self.worker_sender.is_some() {
            return Ok(());
        }

        let (command_sender, command_receiver) = channel::<UserWorkerCommand>();
        let (result_sender, result_receiver) = channel::<UserWorkerResult>();
        let worker = thread::Builder::new()
            .name("rtaskmgr-user-sampler".to_string())
            .spawn(move || {
                while let Ok(command) = command_receiver.recv() {
                    match command {
                        UserWorkerCommand::Collect { notify_hwnd } => {
                            let result = collect_user_sessions();
                            if result_sender.send(result).is_ok() {
                                unsafe {
                                    PostMessageW(
                                        notify_hwnd as HWND,
                                        PWM_USER_WORKER_COMPLETE,
                                        0,
                                        0,
                                    );
                                }
                            }
                        }
                        UserWorkerCommand::Shutdown => break,
                    }
                }
            })
            .map_err(thread_spawn_error)?;

        self.worker_sender = Some(command_sender);
        self.worker_result_receiver = Some(result_receiver);
        self.worker_thread = Some(worker);
        Ok(())
    }

    fn stop_worker_thread(&mut self) {
        if let Some(sender) = self.worker_sender.take() {
            let _ = sender.send(UserWorkerCommand::Shutdown);
        }
        if let Some(worker) = self.worker_thread.take() {
            let _ = worker.join();
        }
        self.worker_result_receiver = None;
        self.collection_in_flight = false;
        self.refresh_requested = false;
    }

    pub fn size_page(&self) {
        // 用户页采用“列表占满上方，按钮固定在下方右侧”的经典布局。
        // 安全性: layout only queries and moves child controls belonging to this page HWND.
        unsafe {
            if self.hwnd.is_null() {
                return;
            }
            let mut parent_rect = zeroed::<RECT>();
            GetClientRect(self.hwnd, &mut parent_rect);
            let units = GetDialogBaseUnits() as usize;
            let def_spacing = (DEFSPACING_BASE * i32::from(loword(units))) / DLG_SCALE_X;
            let mut hdwp = BeginDeferWindowPos(10);
            if hdwp.is_null() {
                return;
            }
            let master_hwnd = GetDlgItem(self.hwnd, i32::from(IDM_SENDMESSAGE));
            let list_hwnd = self.list_hwnd();
            if master_hwnd.is_null() || list_hwnd.is_null() {
                EndDeferWindowPos(hdwp);
                return;
            }
            let master_rect = window_rect_relative_to_page(master_hwnd, self.hwnd);
            let dx = (parent_rect.right - def_spacing * 2) - master_rect.right;
            let dy = (parent_rect.bottom - def_spacing * 2) - master_rect.bottom;
            let list_rect = window_rect_relative_to_page(list_hwnd, self.hwnd);
            hdwp = DeferWindowPos(
                hdwp,
                list_hwnd,
                null_mut(),
                0,
                0,
                (master_rect.right - list_rect.left + dx).max(0),
                (master_rect.top - list_rect.top + dy - def_spacing).max(0),
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            for control_id in [
                i32::from(IDM_DISCONNECT),
                i32::from(IDM_LOGOFF),
                i32::from(IDM_SENDMESSAGE),
            ] {
                let control_hwnd = GetDlgItem(self.hwnd, control_id);
                if control_hwnd.is_null() {
                    continue;
                }
                let control_rect = window_rect_relative_to_page(control_hwnd, self.hwnd);
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
            }
            EndDeferWindowPos(hdwp);
        }
    }
    pub fn handle_notify(&mut self, lparam: isize) -> isize {
        // 选择变化用于驱动按钮可用性，列点击则触发当前会话列表重新排序。
        // 安全性: `lparam` is provided by WM_NOTIFY and points to an NMLISTVIEW for this handler.
        unsafe {
            let notify = &*(lparam as *const NMLISTVIEW);
            if notify.hdr.idFrom as i32 == IDC_USERLIST {
                if notify.hdr.code == LVN_ITEMCHANGED {
                    self.selected_session_identity = self.current_selected_session_identity();
                    self.update_ui_state();
                    return 1;
                }
                if notify.hdr.code == LVN_COLUMNCLICK {
                    let column = notify.iSubItem.max(0) as usize;
                    if self.sort_column == column {
                        self.sort_ascending = !self.sort_ascending;
                    } else {
                        self.sort_column = column;
                        self.sort_ascending = true;
                    }
                    self.sort_sessions();
                    self.update_listview();
                    if let Some(identity) = self.selected_session_identity.clone() {
                        self.restore_selection(&identity);
                    }
                    return 1;
                }
            }
            0
        }
    }

    pub fn handle_command(&mut self, command_id: u16) -> bool {
        // 用户页命令都围绕会话管理：发消息、断开、注销、切换显示域名。
        match command_id {
            IDM_SENDMESSAGE => {
                self.send_message();
                true
            }
            IDM_DISCONNECT => {
                self.change_session_state(command_id);
                true
            }
            IDM_LOGOFF => {
                self.change_session_state(command_id);
                true
            }
            IDM_SHOWDOMAINNAMES => {
                self.show_domain_names = !self.show_domain_names;
                self.rebuild_display_names();
                true
            }
            _ => false,
        }
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        // 右键菜单只在有选择时弹出，并按当前会话状态动态禁用不合法操作。
        // 安全性: context menu and selection queries are UI-thread operations for this page.
        unsafe {
            let selected = self.selected_session_identities();
            if selected.is_empty() {
                return;
            }

            let Some(popup) = build_popup_menu(IDR_USER_CONTEXT, usize::MAX) else {
                return;
            };

            self.update_menu_state(popup, &selected);
            let command = TrackPopupMenuEx(popup, TPM_RETURNCMD, x, y, self.hwnd, null_mut());
            destroy_menu_handle(popup);
            if command != 0 {
                self.handle_command(command as u16);
            }
        }
    }

    fn configure_columns(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 用户页列固定，直接按当前语言文本重建整套列表头即可。
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }

            while SendMessageW(list, LVM_DELETECOLUMN, 0, 0) != 0 {}

            let titles = user_column_titles();
            let columns = [
                (titles[0], 160, LVCFMT_LEFT),
                (titles[1], 80, LVCFMT_RIGHT),
                (titles[2], 90, LVCFMT_LEFT),
                (titles[3], 120, LVCFMT_LEFT),
                (user_session_column_title(), 90, LVCFMT_LEFT),
            ];

            for (index, (title, width, fmt)) in columns.iter().enumerate() {
                let mut title_wide = to_wide_null(title);
                let mut column = LVCOLUMNW {
                    mask: LVCF_FMT | LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                    fmt: *fmt,
                    cx: *width,
                    pszText: title_wide.as_mut_ptr(),
                    cchTextMax: title_wide.len() as i32,
                    iSubItem: index as i32,
                    ..zeroed()
                };
                SendMessageW(
                    list,
                    LVM_INSERTCOLUMNW,
                    index,
                    &mut column as *mut _ as isize,
                );
            }
        }
    }

    fn refresh(&mut self) {
        self.drain_worker_results();
        if self.collection_in_flight {
            self.refresh_requested = true;
            return;
        }

        self.refresh_requested = false;
        self.schedule_collection();
    }

    fn schedule_collection(&mut self) {
        let Some(sender) = self.worker_sender.as_ref() else {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        };
        if sender
            .send(UserWorkerCommand::Collect {
                notify_hwnd: self.hwnd as isize,
            })
            .is_err()
        {
            self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
            return;
        }
        self.collection_in_flight = true;
    }

    fn drain_worker_results(&mut self) {
        loop {
            let result = match self.worker_result_receiver.as_ref() {
                Some(receiver) => receiver.try_recv(),
                None => return,
            };

            match result {
                Ok(result) => {
                    self.collection_in_flight = false;
                    match result {
                        Ok(sessions) => {
                            self.last_refresh_error = None;
                            self.apply_session_snapshot(sessions);
                        }
                        Err(error) => self.set_refresh_error(error),
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.worker_result_receiver = None;
                    self.worker_sender = None;
                    self.collection_in_flight = false;
                    self.set_refresh_error(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                    return;
                }
            }
        }
    }

    pub fn handle_worker_completion(&mut self) {
        self.drain_worker_results();
        if self.refresh_requested && !self.collection_in_flight {
            self.refresh_requested = false;
            self.schedule_collection();
        }
    }

    fn apply_session_snapshot(&mut self, snapshots: Vec<UserSessionSnapshot>) {
        let previous_selection = self.selected_session_identity.clone();
        let mut sessions = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let display_name =
                format_session_display_name(&snapshot.identity, self.show_domain_names);
            sessions.push(UserSessionEntry {
                identity: snapshot.identity,
                display_name_lower: display_name.to_lowercase(),
                display_name,
                status_lower: snapshot.status.to_lowercase(),
                status: snapshot.status,
                client_name_lower: snapshot.client_name.to_lowercase(),
                client_name: snapshot.client_name,
                session_name_lower: snapshot.session_name.to_lowercase(),
                session_name: snapshot.session_name,
                dirty: true,
            });
        }

        let mut previous_sessions = HashMap::with_capacity(self.sessions.len());
        for session in self.sessions.drain(..) {
            previous_sessions.insert(session.identity.clone(), session);
        }
        for entry in &mut sessions {
            if let Some(previous) = previous_sessions.remove(&entry.identity) {
                entry.dirty = previous.display_name != entry.display_name
                    || previous.status != entry.status
                    || previous.client_name != entry.client_name
                    || previous.session_name != entry.session_name;
            }
        }

        self.sessions = sessions;
        self.sort_sessions();
        self.update_listview();

        self.selected_session_identity = previous_selection.filter(|identity| {
            self.sessions
                .iter()
                .any(|entry| entry.identity == *identity)
        });
        if let Some(identity) = self.selected_session_identity.clone() {
            self.restore_selection(&identity);
        } else {
            self.update_ui_state();
        }
    }

    fn sort_sessions(&mut self) {
        self.sessions.sort_by(|left, right| {
            compare_user_sessions(left, right, self.sort_column, self.sort_ascending)
        });
    }

    fn rebuild_display_names(&mut self) {
        let previous_selection = self.selected_session_identity.clone();
        for session in &mut self.sessions {
            let display_name =
                format_session_display_name(&session.identity, self.show_domain_names);
            if session.display_name != display_name {
                session.display_name_lower = display_name.to_lowercase();
                session.display_name = display_name;
                session.dirty = true;
            }
        }
        self.sort_sessions();
        self.update_listview();
        if let Some(identity) = previous_selection {
            self.restore_selection(&identity);
        } else {
            self.update_ui_state();
        }
    }

    fn set_refresh_error(&mut self, error: u32) {
        if self.last_refresh_error != Some(error) {
            record_win32_error("user session refresh", error);
        }
        self.last_refresh_error = Some(error);
    }

    fn update_listview(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 用户列表也采用增量同步策略，减少重排带来的闪烁和选择状态丢失。
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }

            SendMessageW(list, WM_SETREDRAW, 0, 0);

            let mut existing_count = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as usize;
            let common_count = existing_count.min(self.sessions.len());

            for index in 0..common_count {
                let session = &self.sessions[index];
                let mut current_item = LVITEMW {
                    mask: LVIF_PARAM,
                    iItem: index as i32,
                    ..zeroed()
                };
                let current_session_id =
                    if SendMessageW(list, LVM_GETITEMW, 0, &mut current_item as *mut _ as isize)
                        != 0
                    {
                        Some(current_item.lParam as u32)
                    } else {
                        None
                    };

                if current_session_id != Some(session.identity.session_id) {
                    self.replace_row(list, index, session);
                } else if session.dirty {
                    self.update_row(list, index, session);
                }
            }

            while existing_count > self.sessions.len() {
                existing_count -= 1;
                SendMessageW(list, LVM_DELETEITEM, existing_count, 0);
            }

            for index in common_count..self.sessions.len() {
                self.insert_row(list, index, &self.sessions[index]);
            }

            finish_list_view_update(list);
        }
    }

    fn insert_row(&self, list: HWND, index: usize, session: &UserSessionEntry) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let mut user_name = to_wide_null(&session.display_name);
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM,
                iItem: index as i32,
                iSubItem: 0,
                pszText: user_name.as_mut_ptr(),
                cchTextMax: user_name.len() as i32,
                lParam: session.identity.session_id as isize,
                ..zeroed()
            };
            SendMessageW(list, LVM_INSERTITEMW, 0, &mut item as *mut _ as isize);
            self.update_row(list, index, session);
        }
    }

    fn replace_row(&self, list: HWND, index: usize, session: &UserSessionEntry) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let mut user_name = to_wide_null(&session.display_name);
            let mut item = LVITEMW {
                mask: LVIF_TEXT | LVIF_PARAM,
                iItem: index as i32,
                iSubItem: 0,
                pszText: user_name.as_mut_ptr(),
                cchTextMax: user_name.len() as i32,
                lParam: session.identity.session_id as isize,
                ..zeroed()
            };
            SendMessageW(list, LVM_SETITEMW, 0, &mut item as *mut _ as isize);
            self.update_row(list, index, session);
        }
    }

    fn update_row(&self, list: HWND, index: usize, session: &UserSessionEntry) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 第 1 列是字符串，第 2 列显示 session id，其余列回填状态和客户端信息。
            let row = [
                session.display_name.as_str(),
                "",
                session.status.as_str(),
                session.client_name.as_str(),
                session.session_name.as_str(),
            ];
            for (subitem, text) in row.iter().enumerate() {
                let content = if subitem == 1 {
                    session.identity.session_id.to_string()
                } else {
                    (*text).to_string()
                };
                let mut value = to_wide_null(&content);
                let mut subitem_item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: index as i32,
                    iSubItem: subitem as i32,
                    pszText: value.as_mut_ptr(),
                    cchTextMax: value.len() as i32,
                    ..zeroed()
                };
                SendMessageW(list, LVM_SETITEMW, 0, &mut subitem_item as *mut _ as isize);
            }
        }
    }

    fn restore_selection(&self, identity: &UserSessionIdentity) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }

            for (index, session) in self.sessions.iter().enumerate() {
                if session.identity != *identity {
                    continue;
                }

                let mut item = LVITEMW {
                    stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                    state: LVIS_SELECTED | LVIS_FOCUSED,
                    ..zeroed()
                };
                SendMessageW(list, LVM_SETITEMSTATE, index, &mut item as *mut _ as isize);
                SendMessageW(list, LVM_ENSUREVISIBLE, index, 0);
                break;
            }

            self.update_ui_state();
        }
    }

    fn current_selected_session_identity(&self) -> Option<UserSessionIdentity> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return None;
            }

            let index =
                SendMessageW(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) as i32;
            if index < 0 {
                return None;
            }

            let mut item = LVITEMW {
                mask: LVIF_PARAM | LVIF_STATE,
                iItem: index,
                ..zeroed()
            };
            if SendMessageW(list, LVM_GETITEMW, 0, &mut item as *mut _ as isize) != 0 {
                let session_id = item.lParam as u32;
                self.sessions
                    .iter()
                    .find(|entry| entry.identity.session_id == session_id)
                    .map(|entry| entry.identity.clone())
            } else {
                None
            }
        }
    }

    fn update_ui_state(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // “发送消息”只要有选择就可用；
            // “断开”则不能对已经断开的会话再次执行。
            let selected = self.selected_session_identities();
            let send_enabled = !selected.is_empty();
            let mut disconnect_enabled = !selected.is_empty();
            let logoff_enabled = !selected.is_empty();

            for identity in &selected {
                if let Some(session) = self
                    .sessions
                    .iter()
                    .find(|entry| entry.identity == *identity)
                {
                    if session.status == session_state("Disconnected") {
                        disconnect_enabled = false;
                    }
                }
            }

            for control_id in [IDM_DISCONNECT, IDM_LOGOFF, IDM_SENDMESSAGE] {
                let control = GetDlgItem(self.hwnd, i32::from(control_id));
                if !control.is_null() {
                    let enabled = match control_id {
                        IDM_DISCONNECT => disconnect_enabled,
                        IDM_LOGOFF => logoff_enabled,
                        IDM_SENDMESSAGE => send_enabled,
                        _ => false,
                    };
                    EnableWindow(control, i32::from(enabled));
                }
            }
        }
    }

    fn selected_session_identities(&self) -> Vec<UserSessionIdentity> {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 批量操作都基于当前多选会话列表，因此这里统一把所有选中项提取出来。
            let list = self.list_hwnd();
            if list.is_null() {
                return Vec::new();
            }

            let mut selected = Vec::with_capacity(8);
            let mut index = -1;
            loop {
                index = SendMessageW(
                    list,
                    LVM_GETNEXTITEM,
                    index.max(-1) as usize,
                    LVNI_SELECTED as isize,
                ) as i32;
                if index < 0 {
                    break;
                }

                let mut item = LVITEMW {
                    mask: LVIF_PARAM,
                    iItem: index,
                    ..zeroed()
                };
                if SendMessageW(list, LVM_GETITEMW, 0, &mut item as *mut _ as isize) != 0 {
                    let session_id = item.lParam as u32;
                    if let Some(identity) = self
                        .sessions
                        .iter()
                        .find(|entry| entry.identity.session_id == session_id)
                        .map(|entry| entry.identity.clone())
                    {
                        selected.push(identity);
                    }
                }
            }
            selected
        }
    }

    fn update_menu_state(&self, popup: HMENU, selected: &[UserSessionIdentity]) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let send_enabled = !selected.is_empty();
            let mut disconnect_enabled = !selected.is_empty();
            let logoff_enabled = !selected.is_empty();

            for identity in selected {
                if let Some(session) = self
                    .sessions
                    .iter()
                    .find(|entry| entry.identity == *identity)
                {
                    if session.status == session_state("Disconnected") {
                        disconnect_enabled = false;
                    }
                }
            }

            if !send_enabled {
                windows_sys::Win32::UI::WindowsAndMessaging::EnableMenuItem(
                    popup,
                    u32::from(IDM_SENDMESSAGE),
                    MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                );
            }
            if !disconnect_enabled {
                windows_sys::Win32::UI::WindowsAndMessaging::EnableMenuItem(
                    popup,
                    u32::from(IDM_DISCONNECT),
                    MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                );
            }
            if !logoff_enabled {
                windows_sys::Win32::UI::WindowsAndMessaging::EnableMenuItem(
                    popup,
                    u32::from(IDM_LOGOFF),
                    MF_BYCOMMAND | MF_GRAYED | MF_DISABLED,
                );
            }
            windows_sys::Win32::UI::WindowsAndMessaging::CheckMenuItem(
                popup,
                u32::from(IDM_SHOWDOMAINNAMES),
                MF_BYCOMMAND
                    | if self.show_domain_names {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    },
            );
        }
    }

    fn send_message(&mut self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 发送消息会先弹出输入对话框，再逐个会话调用 WTSSendMessageW。
            let selected = self.selected_session_identities();
            if selected.is_empty() {
                return;
            }

            let mut result = MessageDialogResult::default();
            if dialog_box(
                self.hinstance as _,
                IDD_MESSAGE,
                self.hwnd,
                Some(message_dialog_proc),
                &mut result as *mut _ as LPARAM,
            ) != IDOK as isize
            {
                return;
            }

            if let Err(error) = validate_session_identities(&selected) {
                self.show_command_failure_with_error(text(TextKey::MessageCouldNotBeSent), error);
                self.refresh();
                return;
            }

            let title = to_wide_null(&result.title);
            let body = to_wide_null(&result.body);
            for identity in selected {
                if let Err(error) = validate_session_identity(&identity) {
                    self.show_command_failure_with_error(
                        text(TextKey::MessageCouldNotBeSent),
                        error,
                    );
                    break;
                }
                let mut response = 0i32;
                if WTSSendMessageW(
                    WTS_CURRENT_SERVER_HANDLE,
                    identity.session_id,
                    title.as_ptr(),
                    (result.title.encode_utf16().count() * 2) as u32,
                    body.as_ptr(),
                    (result.body.encode_utf16().count() * 2) as u32,
                    MB_OK | MB_TOPMOST | MB_ICONINFORMATION,
                    0,
                    &mut response,
                    0,
                ) == 0
                {
                    self.show_command_failure(text(TextKey::MessageCouldNotBeSent));
                    break;
                }
            }
        }
    }

    fn change_session_state(&mut self, command_id: u16) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 断开/注销属于高影响操作，先确认，再逐个会话执行，失败时立即报错并停止。
            let selected = self.selected_session_identities();
            if selected.is_empty() {
                return;
            }

            let prompt = if command_id == IDM_LOGOFF {
                text(TextKey::ConfirmLogoffSelectedUsers)
            } else {
                text(TextKey::ConfirmDisconnectSelectedUsers)
            };
            let prompt_wide = to_wide_null(prompt);
            let caption_wide = to_wide_null(text(TextKey::AppTitle));
            if MessageBoxW(
                self.hwnd,
                prompt_wide.as_ptr(),
                caption_wide.as_ptr(),
                MB_YESNO | MB_DEFBUTTON2 | MB_ICONEXCLAMATION,
            ) != IDYES
            {
                return;
            }

            let failure_message = if command_id == IDM_LOGOFF {
                text(TextKey::SelectedUserCouldNotBeLoggedOff)
            } else {
                text(TextKey::SelectedUserCouldNotBeDisconnected)
            };
            if let Err(error) = validate_session_identities(&selected) {
                self.show_command_failure_with_error(failure_message, error);
                self.refresh();
                return;
            }

            for identity in selected {
                if let Err(error) = validate_session_identity(&identity) {
                    self.show_command_failure_with_error(failure_message, error);
                    break;
                }
                let succeeded = if command_id == IDM_LOGOFF {
                    WTSLogoffSession(WTS_CURRENT_SERVER_HANDLE, identity.session_id, 0) != 0
                } else {
                    WTSDisconnectSession(WTS_CURRENT_SERVER_HANDLE, identity.session_id, 0) != 0
                };
                if !succeeded {
                    self.show_command_failure(failure_message);
                    break;
                }
            }

            self.refresh();
        }
    }

    fn show_command_failure(&self, message: &str) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 统一附带最后一个 Win32 错误码，方便排查权限或会话状态问题。
            self.show_command_failure_with_error(message, GetLastError());
        }
    }

    fn show_command_failure_with_error(&self, message: &str, error: u32) {
        // 安全性: the strings are converted to stable, null-terminated UTF-16 buffers for the
        // duration of the synchronous MessageBoxW call.
        unsafe {
            let body = if error == 0 {
                message.to_string()
            } else {
                format!("{}\n\n{} {error}", message, text(TextKey::Win32ErrorPrefix))
            };
            let body_wide = to_wide_null(&body);
            let caption_wide = to_wide_null(text(TextKey::AppTitle));
            MessageBoxW(
                self.hwnd,
                body_wide.as_ptr(),
                caption_wide.as_ptr(),
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn list_hwnd(&self) -> HWND {
        // 安全性: this only queries a child HWND from this page dialog; null is allowed.
        unsafe { GetDlgItem(self.hwnd, IDC_USERLIST) }
    }
}

fn collect_user_sessions() -> UserWorkerResult {
    // WTSEnumerateSessionsW and all per-session queries run on the sampler thread. A single
    // failed query rejects the whole candidate snapshot so the UI never mixes old and partial
    // session data.
    unsafe {
        let mut sessions_ptr = null_mut::<WTS_SESSION_INFOW>();
        let mut session_count = 0u32;
        let succeeded = WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut sessions_ptr,
            &mut session_count,
        ) != 0;
        let error = GetLastError();
        if !succeeded {
            if let Some(memory) = OwnedWtsMemory::new(sessions_ptr) {
                drop(memory);
            }
            return Err(win32_error_or_gen_failure(error));
        }

        if session_count == 0 {
            if let Some(memory) = OwnedWtsMemory::new(sessions_ptr) {
                drop(memory);
            }
            return Ok(Vec::new());
        }

        let Some(sessions_memory) = OwnedWtsMemory::new(sessions_ptr) else {
            return Err(ERROR_INVALID_DATA);
        };
        let raw_sessions = slice::from_raw_parts(
            sessions_memory.as_ptr(),
            usize::try_from(session_count).map_err(|_| ERROR_INVALID_DATA)?,
        );
        let mut sessions = Vec::with_capacity(raw_sessions.len());
        for raw_session in raw_sessions {
            let info = query_session_info(raw_session.SessionId)?;
            let identity = session_identity_from_info(&info);
            if identity.user_name.is_empty() {
                continue;
            }

            let client_name = query_session_string(raw_session.SessionId, WTSClientName)?;
            sessions.push(UserSessionSnapshot {
                identity,
                status: session_state_text(info.State),
                client_name: if client_name.is_empty() {
                    "-".to_string()
                } else {
                    client_name
                },
                session_name: wide_array_to_string(&info.WinStationName),
            });
        }
        Ok(sessions)
    }
}

fn query_session_info(session_id: u32) -> Result<WTSINFOW, u32> {
    unsafe {
        let mut buffer = null_mut::<u16>();
        let mut bytes = 0u32;
        let succeeded = WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            WTSSessionInfo,
            &mut buffer,
            &mut bytes,
        ) != 0;
        let error = GetLastError();
        if !succeeded {
            if let Some(memory) = OwnedWtsMemory::new(buffer) {
                drop(memory);
            }
            return Err(win32_error_or_gen_failure(error));
        }

        let Some(memory) = OwnedWtsMemory::new(buffer) else {
            return Err(ERROR_INVALID_DATA);
        };
        if bytes < size_of::<WTSINFOW>() as u32 {
            return Err(ERROR_INVALID_DATA);
        }

        let info = std::ptr::read_unaligned(memory.as_ptr().cast::<WTSINFOW>());
        if info.SessionId != session_id {
            return Err(ERROR_INVALID_DATA);
        }
        Ok(info)
    }
}

fn session_identity_from_info(info: &WTSINFOW) -> UserSessionIdentity {
    UserSessionIdentity {
        session_id: info.SessionId,
        logon_time_100ns: info.LogonTime,
        user_name: wide_array_to_string(&info.UserName),
        domain_name: wide_array_to_string(&info.Domain),
    }
}

fn wide_array_to_string(values: &[u16]) -> String {
    let length = values
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(values.len());
    String::from_utf16_lossy(&values[..length])
}

fn format_session_display_name(identity: &UserSessionIdentity, show_domain_names: bool) -> String {
    if show_domain_names && !identity.domain_name.is_empty() {
        format!("{}\\{}", identity.domain_name, identity.user_name)
    } else {
        identity.user_name.clone()
    }
}

fn validate_session_identity(expected: &UserSessionIdentity) -> Result<(), u32> {
    let current = session_identity_from_info(&query_session_info(expected.session_id)?);
    validate_observed_session_identity(expected, &current)
}

fn validate_observed_session_identity(
    expected: &UserSessionIdentity,
    current: &UserSessionIdentity,
) -> Result<(), u32> {
    if current == expected {
        Ok(())
    } else {
        Err(ERROR_INVALID_PARAMETER)
    }
}

fn validate_session_identities(expected: &[UserSessionIdentity]) -> Result<(), u32> {
    for identity in expected {
        validate_session_identity(identity)?;
    }
    Ok(())
}

fn win32_error_or_gen_failure(error: u32) -> u32 {
    if error == 0 {
        ERROR_GEN_FAILURE
    } else {
        error
    }
}

fn thread_spawn_error(error: std::io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(ERROR_NOT_ENOUGH_MEMORY)
}

fn query_session_string(session_id: u32, info_class: i32) -> Result<String, u32> {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 终端服务 API 返回的是系统分配的 UTF-16 缓冲区，需要在复制完字符串后手动释放。
        let mut buffer = null_mut();
        let mut bytes = 0u32;
        let succeeded = WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            info_class,
            &mut buffer,
            &mut bytes,
        ) != 0;
        let error = GetLastError();
        if !succeeded {
            if let Some(buffer) = OwnedWtsMemory::new(buffer) {
                drop(buffer);
            }
            return Err(win32_error_or_gen_failure(error));
        }
        let Some(buffer) = OwnedWtsMemory::new(buffer) else {
            return Err(ERROR_INVALID_DATA);
        };
        if bytes < size_of::<u16>() as u32 || !bytes.is_multiple_of(size_of::<u16>() as u32) {
            return Err(ERROR_INVALID_DATA);
        }
        let values = slice::from_raw_parts(buffer.as_ptr(), bytes as usize / size_of::<u16>());
        let length = values
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(values.len());
        Ok(String::from_utf16_lossy(&values[..length]))
    }
}

fn compare_user_sessions(
    left: &UserSessionEntry,
    right: &UserSessionEntry,
    sort_column: usize,
    sort_ascending: bool,
) -> std::cmp::Ordering {
    // 用户页排序按当前列切换；字符串列统一转小写比较，保证大小写不影响顺序。
    let ordering = match sort_column {
        1 => left.identity.session_id.cmp(&right.identity.session_id),
        2 => left.status_lower.cmp(&right.status_lower),
        3 => left.client_name_lower.cmp(&right.client_name_lower),
        4 => left.session_name_lower.cmp(&right.session_name_lower),
        _ => left.display_name_lower.cmp(&right.display_name_lower),
    };

    let ordering = if ordering == std::cmp::Ordering::Equal {
        left.identity.session_id.cmp(&right.identity.session_id)
    } else {
        ordering
    };

    if sort_ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn session_state_text(state: WTS_CONNECTSTATE_CLASS) -> String {
    // 会话状态枚举集中映射成可本地化的短文本，供列表和菜单状态共用。
    if state == WTSActive {
        session_state("Active").to_string()
    } else if state == WTSConnected {
        session_state("Connected").to_string()
    } else if state == WTSConnectQuery {
        session_state("Connect Query").to_string()
    } else if state == WTSShadow {
        session_state("Shadow").to_string()
    } else if state == WTSDisconnected {
        session_state("Disconnected").to_string()
    } else if state == WTSIdle {
        session_state("Idle").to_string()
    } else if state == WTSListen {
        session_state("Listening").to_string()
    } else if state == WTSReset {
        session_state("Reset").to_string()
    } else if state == WTSDown {
        session_state("Down").to_string()
    } else if state == WTSInit {
        session_state("Init").to_string()
    } else {
        session_state("Unknown").to_string()
    }
}

unsafe extern "system" fn message_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // 发送消息对话框只负责收集标题和正文，并通过窗口用户数据回写结果结构体。
    match msg {
        WM_INITDIALOG => {
            set_window_userdata(hwnd, lparam);
            localize_dialog(hwnd, IDD_MESSAGE);
            1
        }
        WM_COMMAND => match i32::from(loword(wparam)) {
            IDOK => {
                let result = &mut *(get_window_userdata(hwnd) as *mut MessageDialogResult);
                result.title = get_dialog_item_text(hwnd, IDC_MESSAGE_TITLE);
                result.body = get_dialog_item_text(hwnd, IDC_MESSAGE_MESSAGE);
                if result.body.trim().is_empty() {
                    return 1;
                }
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

fn get_dialog_item_text(hwnd: HWND, control_id: i32) -> String {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 小型输入对话框直接把控件文本读回为 Rust String，便于后续传给 WTS API。
        let control = GetDlgItem(hwnd, control_id);
        if control.is_null() {
            return String::new();
        }

        let length = GetWindowTextLengthW(control);
        if length <= 0 {
            return String::new();
        }

        let Ok(length) = usize::try_from(length) else {
            return String::new();
        };

        let mut buffer = vec![0u16; length + 1];
        let actual = GetWindowTextW(
            control,
            buffer.as_mut_ptr(),
            i32::try_from(buffer.len()).expect("GetWindowTextW buffer length fits in i32"),
        );
        let Ok(actual) = usize::try_from(actual) else {
            return String::new();
        };

        String::from_utf16_lossy(&buffer[..actual])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(session_id: u32, logon_time_100ns: i64) -> UserSessionIdentity {
        UserSessionIdentity {
            session_id,
            logon_time_100ns,
            user_name: "James".to_string(),
            domain_name: "WORKGROUP".to_string(),
        }
    }

    fn entry(identity: UserSessionIdentity) -> UserSessionEntry {
        UserSessionEntry {
            identity,
            display_name: "James".to_string(),
            display_name_lower: "james".to_string(),
            status: "Active".to_string(),
            status_lower: "active".to_string(),
            client_name: "-".to_string(),
            client_name_lower: "-".to_string(),
            session_name: "Console".to_string(),
            session_name_lower: "console".to_string(),
            dirty: false,
        }
    }

    #[test]
    fn reused_session_id_is_not_the_same_identity() {
        let expected = identity(1, 100);
        let current = identity(1, 200);

        assert_eq!(
            validate_observed_session_identity(&expected, &current),
            Err(ERROR_INVALID_PARAMETER)
        );
    }

    #[test]
    fn failed_worker_result_preserves_previous_snapshot() {
        let expected = identity(1, 100);
        let mut state = UserPageState::default();
        state.sessions.push(entry(expected.clone()));
        state.collection_in_flight = true;
        let (sender, receiver) = channel::<UserWorkerResult>();
        sender.send(Err(1722)).unwrap();
        state.worker_result_receiver = Some(receiver);

        state.drain_worker_results();

        assert_eq!(state.sessions.len(), 1);
        assert_eq!(state.sessions[0].identity, expected);
        assert_eq!(state.last_refresh_error, Some(1722));
        assert!(!state.collection_in_flight);
    }

    #[test]
    fn fixed_wide_string_stops_at_first_nul() {
        let values = ['A' as u16, 'B' as u16, 0, 'C' as u16];
        assert_eq!(wide_array_to_string(&values), "AB");
    }

    #[test]
    fn domain_visibility_only_changes_display_name() {
        let identity = identity(1, 100);
        assert_eq!(format_session_display_name(&identity, false), "James");
        assert_eq!(
            format_session_display_name(&identity, true),
            "WORKGROUP\\James"
        );
    }
}
