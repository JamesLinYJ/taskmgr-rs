use std::collections::HashMap;

// 用户页实现。
// 这里负责枚举终端服务会话、刷新用户列表，并处理发送消息、断开连接、
// 注销等会话级操作。
use std::mem::zeroed;
use std::ptr::null_mut;
use std::slice;

use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;

use windows_sys::Win32::System::RemoteDesktop::{
    WTSActive, WTSClientName, WTSConnectQuery, WTSConnected, WTSDisconnectSession, WTSDisconnected,
    WTSDomainName, WTSDown, WTSEnumerateSessionsW, WTSIdle, WTSInit, WTSListen, WTSLogoffSession,
    WTSQuerySessionInformationW, WTSReset, WTSSendMessageW, WTSShadow, WTSUserName,
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
    SendMessageW, TrackPopupMenuEx, HMENU, IDCANCEL, IDNO, IDOK, MB_DEFBUTTON2, MB_ICONERROR,
    MB_ICONEXCLAMATION, MB_ICONINFORMATION, MB_OK, MB_TOPMOST, MB_YESNO, MF_BYCOMMAND, MF_CHECKED,
    MF_DISABLED, MF_GRAYED, MF_UNCHECKED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    TPM_RETURNCMD, WM_COMMAND, WM_INITDIALOG, WM_SETREDRAW,
};

use crate::dialog_templates::dialog_box;
use crate::language::{
    localize_dialog, session_state, text, user_column_titles, user_session_column_title, TextKey,
};
use crate::menus::build_popup_menu;
use crate::options::Options;
use crate::resource::{
    IDC_MESSAGE_MESSAGE, IDC_MESSAGE_TITLE, IDC_USERLIST, IDD_MESSAGE, IDM_DISCONNECT, IDM_LOGOFF,
    IDM_SENDMESSAGE, IDM_SHOWDOMAINNAMES, IDR_USER_CONTEXT,
};
use crate::winutil::{
    destroy_menu_handle, finish_list_view_update_deferred, get_window_userdata, loword,
    set_window_userdata, subclass_list_view, to_wide_null, widestr_ptr_to_string,
    window_rect_relative_to_page, ListViewDirtyRange, OwnedWtsMemory,
};
const DEFSPACING_BASE: i32 = 3;
const DLG_SCALE_X: i32 = 4;

#[derive(Clone)]
struct UserSessionEntry {
    // `UserSessionEntry` 保存一行用户/会话信息以及最小重绘所需的脏标志。
    session_id: u32,
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
    selected_session_id: Option<u32>,
    sessions: Vec<UserSessionEntry>,
    sort_column: usize,
    sort_ascending: bool,
}

impl UserPageState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn initialize(&mut self, hwnd: HWND) {
        // 用户页初始化时把 ListView 立刻配置好并做首轮会话枚举，
        // 这样页面第一次切入就已经带着当前在线用户状态。
        // 安全性: all Win32 calls target the user page HWND and its child controls during UI-thread
        // initialization.
        unsafe {
            self.hinstance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(null_mut()) as isize;
            self.hwnd = hwnd;
            let list = self.list_hwnd();
            if !list.is_null() {
                subclass_list_view(list);
            }
            self.configure_columns();
            self.refresh();
            self.size_page();
        }
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

    pub fn destroy(&mut self) {}

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
                    self.selected_session_id = self.current_selected_session_id();
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
                    self.refresh();
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
                self.refresh();
                true
            }
            _ => false,
        }
    }

    pub fn show_context_menu(&mut self, x: i32, y: i32) {
        // 右键菜单只在有选择时弹出，并按当前会话状态动态禁用不合法操作。
        // 安全性: context menu and selection queries are UI-thread operations for this page.
        unsafe {
            let selected = self.selected_session_ids();
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
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 刷新时先保存上一轮会话映射，再和新枚举结果做比对，
            // 这样可以知道哪些行真正发生了变化。
            let previous_selection = self.selected_session_id;
            let mut previous_sessions = HashMap::with_capacity(self.sessions.len());
            for session in self.sessions.drain(..) {
                previous_sessions.insert(session.session_id, session);
            }
            let mut sessions_ptr = null_mut::<WTS_SESSION_INFOW>();
            let mut session_count = 0u32;
            if WTSEnumerateSessionsW(
                WTS_CURRENT_SERVER_HANDLE,
                0,
                1,
                &mut sessions_ptr,
                &mut session_count,
            ) == 0
                || sessions_ptr.is_null()
            {
                self.sessions.clear();
                self.update_listview();
                self.update_ui_state();
                return;
            }

            let Some(sessions_memory) = OwnedWtsMemory::new(sessions_ptr) else {
                return;
            };

            let mut sessions = Vec::with_capacity(session_count as usize);
            for session in slice::from_raw_parts(sessions_memory.as_ptr(), session_count as usize) {
                let user_name = query_session_string(session.SessionId, WTSUserName);
                if user_name.is_empty() {
                    continue;
                }

                let domain_name = query_session_string(session.SessionId, WTSDomainName);
                let client_name = query_session_string(session.SessionId, WTSClientName);
                let display_name = if domain_name.is_empty() || !self.show_domain_names {
                    user_name.clone()
                } else {
                    format!("{domain_name}\\{user_name}")
                };

                let status = session_state_text(session.State);
                let client_name = if client_name.is_empty() {
                    "-".to_string()
                } else {
                    client_name
                };
                let session_name = widestr_ptr_to_string(session.pWinStationName);
                let mut entry = UserSessionEntry {
                    session_id: session.SessionId,
                    display_name_lower: display_name.to_lowercase(),
                    status_lower: status.to_lowercase(),
                    client_name_lower: client_name.to_lowercase(),
                    session_name_lower: session_name.to_lowercase(),
                    display_name,
                    status,
                    client_name,
                    session_name,
                    dirty: true,
                };
                if let Some(previous) = previous_sessions.remove(&entry.session_id) {
                    entry.dirty = previous.display_name != entry.display_name
                        || previous.status != entry.status
                        || previous.client_name != entry.client_name
                        || previous.session_name != entry.session_name;
                }
                sessions.push(entry);
            }

            sessions.sort_by(|left, right| {
                compare_user_sessions(left, right, self.sort_column, self.sort_ascending)
            });
            self.sessions = sessions;
            self.update_listview();

            self.selected_session_id = previous_selection;
            if let Some(session_id) = previous_selection {
                self.restore_selection(session_id);
            } else {
                self.update_ui_state();
            }
        }
    }

    fn update_listview(&self) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            // 用户列表也采用增量同步策略，减少重排带来的闪烁和选择状态丢失。
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }

            let mut existing_count = SendMessageW(list, LVM_GETITEMCOUNT, 0, 0) as usize;
            let common_count = existing_count.min(self.sessions.len());
            let mut current_session_ids = Vec::with_capacity(common_count);
            let mut structure_changed = existing_count != self.sessions.len();

            for index in 0..common_count {
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
                if current_session_id != Some(self.sessions[index].session_id) {
                    structure_changed = true;
                }
                current_session_ids.push(current_session_id);
            }

            if structure_changed {
                SendMessageW(list, WM_SETREDRAW, 0, 0);
            }

            let mut dirty_rows = ListViewDirtyRange::default();

            for (index, current_session_id) in current_session_ids.iter().copied().enumerate() {
                let session = &self.sessions[index];
                if current_session_id != Some(session.session_id) {
                    self.replace_row(list, index, session);
                    dirty_rows.mark(index);
                } else if session.dirty {
                    self.update_row(list, index, session);
                    dirty_rows.mark(index);
                }
            }

            while existing_count > self.sessions.len() {
                existing_count -= 1;
                SendMessageW(list, LVM_DELETEITEM, existing_count, 0);
            }

            for index in common_count..self.sessions.len() {
                self.insert_row(list, index, &self.sessions[index]);
                dirty_rows.mark(index);
            }

            if structure_changed {
                finish_list_view_update_deferred(list);
                InvalidateRect(list, null_mut(), 0);
            }
            dirty_rows.redraw(list, self.sessions.len());
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
                lParam: session.session_id as isize,
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
                lParam: session.session_id as isize,
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
                    session.session_id.to_string()
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

    fn restore_selection(&self, session_id: u32) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let list = self.list_hwnd();
            if list.is_null() {
                return;
            }

            for (index, session) in self.sessions.iter().enumerate() {
                if session.session_id != session_id {
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

    fn current_selected_session_id(&self) -> Option<u32> {
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
                Some(item.lParam as u32)
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
            let selected = self.selected_session_ids();
            let send_enabled = !selected.is_empty();
            let mut disconnect_enabled = !selected.is_empty();
            let logoff_enabled = !selected.is_empty();

            for session_id in &selected {
                if let Some(session) = self
                    .sessions
                    .iter()
                    .find(|entry| entry.session_id == *session_id)
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

    fn selected_session_ids(&self) -> Vec<u32> {
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
                    selected.push(item.lParam as u32);
                }
            }
            selected
        }
    }

    fn selected_sessions(&self) -> Vec<UserSessionEntry> {
        let selected = self.selected_session_ids();
        selected
            .iter()
            .filter_map(|session_id| {
                self.sessions
                    .iter()
                    .find(|entry| entry.session_id == *session_id)
                    .cloned()
            })
            .collect()
    }

    fn update_menu_state(&self, popup: HMENU, selected: &[u32]) {
        // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
        unsafe {
            let send_enabled = !selected.is_empty();
            let mut disconnect_enabled = !selected.is_empty();
            let logoff_enabled = !selected.is_empty();

            for session_id in selected {
                if let Some(session) = self
                    .sessions
                    .iter()
                    .find(|entry| entry.session_id == *session_id)
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
            let selected = self.selected_sessions();
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

            let title = to_wide_null(&result.title);
            let body = to_wide_null(&result.body);
            for session in selected {
                if !session_matches_current_state(&session, self.show_domain_names) {
                    self.show_command_failure(text(TextKey::MessageCouldNotBeSent));
                    break;
                }
                let mut response = 0i32;
                if WTSSendMessageW(
                    WTS_CURRENT_SERVER_HANDLE,
                    session.session_id,
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
            let selected = self.selected_sessions();
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
            ) == IDNO
            {
                return;
            }

            for session in selected {
                if !session_matches_current_state(&session, self.show_domain_names) {
                    self.show_command_failure(if command_id == IDM_LOGOFF {
                        text(TextKey::SelectedUserCouldNotBeLoggedOff)
                    } else {
                        text(TextKey::SelectedUserCouldNotBeDisconnected)
                    });
                    break;
                }
                let succeeded = if command_id == IDM_LOGOFF {
                    WTSLogoffSession(WTS_CURRENT_SERVER_HANDLE, session.session_id, 0) != 0
                } else {
                    WTSDisconnectSession(WTS_CURRENT_SERVER_HANDLE, session.session_id, 0) != 0
                };
                if !succeeded {
                    self.show_command_failure(if command_id == IDM_LOGOFF {
                        text(TextKey::SelectedUserCouldNotBeLoggedOff)
                    } else {
                        text(TextKey::SelectedUserCouldNotBeDisconnected)
                    });
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
            let last_error = GetLastError();
            let body = if last_error == 0 {
                message.to_string()
            } else {
                format!(
                    "{}\n\n{} {last_error}",
                    message,
                    text(TextKey::Win32ErrorPrefix)
                )
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

fn query_session_string(session_id: u32, info_class: i32) -> String {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        // 终端服务 API 返回的是系统分配的 UTF-16 缓冲区，需要在复制完字符串后手动释放。
        let mut buffer = null_mut();
        let mut bytes = 0u32;
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            session_id,
            info_class,
            &mut buffer,
            &mut bytes,
        ) == 0
            || buffer.is_null()
            || bytes == 0
        {
            return String::new();
        }

        let Some(buffer) = OwnedWtsMemory::new(buffer) else {
            return String::new();
        };
        let len = (bytes as usize / 2).saturating_sub(1);
        String::from_utf16_lossy(slice::from_raw_parts(buffer.as_ptr(), len))
    }
}

fn query_session_entry(session_id: u32, show_domain_names: bool) -> Option<UserSessionEntry> {
    unsafe {
        let mut sessions_ptr = null_mut::<WTS_SESSION_INFOW>();
        let mut session_count = 0u32;
        if WTSEnumerateSessionsW(
            WTS_CURRENT_SERVER_HANDLE,
            0,
            1,
            &mut sessions_ptr,
            &mut session_count,
        ) == 0
            || sessions_ptr.is_null()
        {
            return None;
        }

        let sessions_memory = OwnedWtsMemory::new(sessions_ptr)?;
        let session = slice::from_raw_parts(sessions_memory.as_ptr(), session_count as usize)
            .iter()
            .find(|session| session.SessionId == session_id)?;
        let user_name = query_session_string(session.SessionId, WTSUserName);
        if user_name.is_empty() {
            return None;
        }

        let domain_name = query_session_string(session.SessionId, WTSDomainName);
        let client_name = query_session_string(session.SessionId, WTSClientName);
        let display_name = if domain_name.is_empty() || !show_domain_names {
            user_name
        } else {
            format!("{domain_name}\\{user_name}")
        };

        let display_name_lower = display_name.to_lowercase();
        let status = session_state_text(session.State);
        let status_lower = status.to_lowercase();
        let client_name = if client_name.is_empty() {
            "-".to_string()
        } else {
            client_name
        };
        let client_name_lower = client_name.to_lowercase();
        let session_name = widestr_ptr_to_string(session.pWinStationName);
        let session_name_lower = session_name.to_lowercase();
        Some(UserSessionEntry {
            session_id: session.SessionId,
            display_name_lower,
            status_lower,
            client_name_lower,
            session_name_lower,
            display_name,
            status,
            client_name,
            session_name,
            dirty: false,
        })
    }
}

fn session_matches_current_state(expected: &UserSessionEntry, show_domain_names: bool) -> bool {
    query_session_entry(expected.session_id, show_domain_names).is_some_and(|current| {
        current.display_name == expected.display_name
            && current.status == expected.status
            && current.session_name == expected.session_name
    })
}

fn compare_user_sessions(
    left: &UserSessionEntry,
    right: &UserSessionEntry,
    sort_column: usize,
    sort_ascending: bool,
) -> std::cmp::Ordering {
    // 用户页排序按当前列切换；字符串列使用缓存小写键，避免每轮刷新重复分配。
    let ordering = match sort_column {
        1 => left.session_id.cmp(&right.session_id),
        2 => left.status_lower.cmp(&right.status_lower),
        3 => left.client_name_lower.cmp(&right.client_name_lower),
        4 => left.session_name_lower.cmp(&right.session_name_lower),
        _ => left.display_name_lower.cmp(&right.display_name_lower),
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
