use std::borrow::Cow;
use std::ffi::OsStr;

// 跨模块复用的 Win32 工具函数。
// 这里集中放 UTF-16 转换、菜单裁剪、ListView 子类化、重绘控制以及
// 一些与指针宽度相关的安全包装逻辑。
use std::iter;
use std::mem::zeroed;
use std::os::windows::ffi::OsStrExt;
use std::ptr::NonNull;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, HWND,
    INVALID_HANDLE_VALUE, LPARAM, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, FillRgn, GetSysColor,
    InvalidateRect, MapWindowPoints, SetRectRgn, COLOR_WINDOW, HBRUSH, HDC, HRGN, RGN_DIFF, RGN_OR,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation,
    LUID_AND_ATTRIBUTES, SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::RemoteDesktop::WTSFreeMemory;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, IsWow64Process, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Controls::{
    LVIR_BOUNDS, LVM_GETCOUNTPERPAGE, LVM_GETITEMCOUNT, LVM_GETITEMRECT, LVM_GETTOPINDEX,
};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DeleteMenu, DestroyIcon, DestroyMenu, EnableMenuItem, GetClientRect,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, SendMessageW, SetWindowLongPtrW,
    DWLP_MSGRESULT, GWLP_USERDATA, GWL_STYLE, HICON, HMENU, MF_BYCOMMAND, MF_ENABLED, MF_GRAYED,
    SM_CXEDGE, WM_ERASEBKGND, WM_NCDESTROY, WM_SETREDRAW, WM_SYSCOLORCHANGE, WNDPROC,
};

use crate::language::{text, TextKey};
use crate::resource::{IDM_ALLCPUS, IDM_MULTIGRAPH, IDM_RUN, STATIC_CPU_GRAPH_COUNT};

const REST_NORUN: u32 = 0x0000_0001;
const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = 0x1036;
const LVS_EX_DOUBLEBUFFER: usize = 0x0001_0000;
const LIST_VIEW_SUBCLASS_ID: usize = 0x5254_4D47;

pub struct OwnedHandle {
    handle: HANDLE,
}

pub struct OwnedWtsMemory<T> {
    ptr: *mut T,
}

pub fn destroy_menu_handle(menu: HMENU) {
    if !menu.is_null() {
        // 安全性: callers pass a raw menu handle they own and want to release.
        unsafe { DestroyMenu(menu) };
    }
}

pub fn destroy_icon_handle(icon: HICON) {
    if !icon.is_null() {
        // 安全性: callers pass an icon handle they own and want to release.
        unsafe { DestroyIcon(icon) };
    }
}

pub fn record_win32_error(component: &str, error: u32) {
    let message = to_wide_null(&format!(
        "rtaskmgr: {component} failed with Win32 error {error}\r\n"
    ));
    // 安全性: `message` is null-terminated and remains alive for the synchronous call.
    unsafe { OutputDebugStringW(message.as_ptr()) };
}

pub fn record_ntstatus_error(component: &str, status: i32) {
    let message = to_wide_null(&format!(
        "rtaskmgr: {component} failed with NTSTATUS 0x{:08X}\r\n",
        status as u32
    ));
    // 安全性: `message` is null-terminated and remains alive for the synchronous call.
    unsafe { OutputDebugStringW(message.as_ptr()) };
}

pub fn enable_debug_privilege() -> Result<(), u32> {
    // Task Manager needs SeDebugPrivilege to query process tokens owned by services and SYSTEM.
    // AdjustTokenPrivileges may return success while reporting ERROR_NOT_ALL_ASSIGNED, so both
    // return channels must be checked.
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut raw_token,
        ) == 0
        {
            return Err(GetLastError());
        }
        let Some(token) = OwnedHandle::new(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut luid = zeroed();
        if LookupPrivilegeValueW(null(), SE_DEBUG_NAME, &mut luid) == 0 {
            return Err(GetLastError());
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(0);
        if AdjustTokenPrivileges(token.as_raw(), 0, &privileges, 0, null_mut(), null_mut()) == 0 {
            return Err(GetLastError());
        }

        let error = GetLastError();
        if error == 0 {
            Ok(())
        } else {
            Err(error)
        }
    }
}

pub fn process_is_elevated() -> Result<bool, u32> {
    unsafe {
        let mut raw_token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }
        let Some(token) = OwnedHandle::new(raw_token) else {
            return Err(ERROR_NOT_ALL_ASSIGNED);
        };

        let mut elevation = zeroed::<TOKEN_ELEVATION>();
        let mut returned = 0u32;
        if GetTokenInformation(
            token.as_raw(),
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        ) == 0
        {
            let error = GetLastError();
            return Err(if error == 0 {
                ERROR_NOT_ALL_ASSIGNED
            } else {
                error
            });
        }

        Ok(elevation.TokenIsElevated != 0)
    }
}

impl<T> OwnedWtsMemory<T> {
    pub fn new(ptr: *mut T) -> Option<Self> {
        (!ptr.is_null()).then_some(Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut T {
        self.ptr
    }
}

impl<T> Drop for OwnedWtsMemory<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // 安全性: `OwnedWtsMemory` exclusively owns a buffer allocated by WTS APIs.
            unsafe { WTSFreeMemory(self.ptr as _) };
        }
    }
}

impl OwnedHandle {
    pub fn new(handle: HANDLE) -> Option<Self> {
        (!handle.is_null() && handle != INVALID_HANDLE_VALUE).then_some(Self { handle })
    }

    pub fn as_raw(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            // 安全性: `OwnedHandle` exclusively owns this Win32 HANDLE.
            unsafe { CloseHandle(self.handle) };
        }
    }
}

struct ListViewPaintState {
    brush: HBRUSH,
    view_rgn: HRGN,
    clip_rgn: HRGN,
}

impl ListViewPaintState {
    fn new() -> Self {
        Self {
            brush: null_mut(),
            view_rgn: null_mut(),
            clip_rgn: null_mut(),
        }
    }

    fn ensure_resources(&mut self) {
        // 安全性: these GDI objects are created for and owned by this paint state; null means
        // the corresponding object has not been allocated yet.
        unsafe {
            if self.brush.is_null() {
                self.brush = CreateSolidBrush(GetSysColor(COLOR_WINDOW));
            }
            if self.view_rgn.is_null() {
                self.view_rgn = CreateRectRgn(0, 0, 0, 0);
            }
            if self.clip_rgn.is_null() {
                self.clip_rgn = CreateRectRgn(0, 0, 0, 0);
            }
        }
    }
}

impl Drop for ListViewPaintState {
    fn drop(&mut self) {
        // 安全性: the GDI objects are owned by this per-window paint state and are released once.
        unsafe {
            if !self.brush.is_null() {
                DeleteObject(self.brush as _);
            }
            if !self.view_rgn.is_null() {
                DeleteObject(self.view_rgn as _);
            }
            if !self.clip_rgn.is_null() {
                DeleteObject(self.clip_rgn as _);
            }
        }
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHRestricted(rest: u32) -> u32;
}

pub fn to_wide_null(text: &str) -> Vec<u16> {
    // 大部分 Win32 文本 API 都要求零结尾 UTF-16，这个转换在全项目复用最多。
    OsStr::new(text)
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

pub fn format_resource_string(template: &str, values: &[String]) -> String {
    // 这里实现的是 Task Manager 旧式资源格式里最常见的 `%d/%u/%s/%%` 子集，
    // 足够满足状态栏和托盘提示等场景，不必引入完整的 printf 解析器。
    let mut rendered =
        String::with_capacity(template.len() + values.iter().map(String::len).sum::<usize>());
    let mut chars = template.chars().peekable();
    let mut index = 0usize;

    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek().copied() {
                Some('%') => {
                    rendered.push('%');
                    chars.next();
                }
                Some('d' | 'u' | 's') => {
                    chars.next();
                    if let Some(value) = values.get(index) {
                        rendered.push_str(value);
                        index += 1;
                    }
                }
                _ => rendered.push(ch),
            }
        } else {
            rendered.push(ch);
        }
    }

    rendered
}

fn set_window_long_ptr_value(hwnd: HWND, index: i32, value: isize) -> isize {
    // 安全性: the caller supplies the target HWND/index/value tuple; this helper performs the
    // raw Win32 slot write and returns the previous value without creating references.
    unsafe { SetWindowLongPtrW(hwnd, index, value as _) as isize }
}

fn get_window_long_ptr_value(hwnd: HWND, index: i32) -> isize {
    // 安全性: reading a Win32 long-ptr slot does not create Rust references; callers interpret
    // the integer according to the slot they requested.
    unsafe { GetWindowLongPtrW(hwnd, index) as isize }
}

pub fn set_window_userdata(hwnd: HWND, value: isize) {
    let _ = set_window_long_ptr_value(hwnd, GWLP_USERDATA, value);
}

pub fn get_window_userdata(hwnd: HWND) -> isize {
    get_window_long_ptr_value(hwnd, GWLP_USERDATA)
}

pub fn set_window_userdata_ptr<T>(hwnd: HWND, value: *mut T) {
    set_window_userdata(hwnd, value as isize);
}

pub fn get_window_userdata_ptr<T>(hwnd: HWND) -> *mut T {
    get_window_userdata(hwnd) as *mut T
}

pub fn window_userdata_non_null<T>(hwnd: HWND) -> Option<NonNull<T>> {
    NonNull::new(get_window_userdata_ptr(hwnd))
}

pub fn set_style(hwnd: HWND, style: u32) {
    let _ = set_window_long_ptr_value(hwnd, GWL_STYLE, style as isize);
}

pub fn set_dialog_msg_result(hwnd: HWND, value: isize) {
    let _ = set_window_long_ptr_value(hwnd, DWLP_MSGRESULT as i32, value);
}

pub fn width(rect: &RECT) -> i32 {
    rect.right - rect.left
}

pub fn height(rect: &RECT) -> i32 {
    rect.bottom - rect.top
}

pub fn loword(value: usize) -> u16 {
    (value & 0xFFFF) as u16
}

pub fn hiword(value: usize) -> u16 {
    ((value >> 16) & 0xFFFF) as u16
}

pub fn sanitize_task_manager_menu(menu: HMENU, processor_count: usize) {
    // 某些菜单项是否可见由系统策略和 CPU 数量决定。
    // 这里在每次加载菜单后做一次裁剪，避免资源文件里维护多套变体。
    if menu.is_null() {
        return;
    }

    // 安全性: `menu` is checked non-null and remains owned by the caller while we remove items.
    unsafe {
        if SHRestricted(REST_NORUN) != 0 {
            DeleteMenu(menu, u32::from(IDM_RUN), MF_BYCOMMAND);
        }

        if processor_count <= 1 {
            DeleteMenu(menu, u32::from(IDM_ALLCPUS), MF_BYCOMMAND);
        }
        EnableMenuItem(
            menu,
            u32::from(IDM_MULTIGRAPH),
            MF_BYCOMMAND
                | if processor_count <= 1 || processor_count > STATIC_CPU_GRAPH_COUNT {
                    MF_GRAYED
                } else {
                    MF_ENABLED
                },
        );
    }
}

pub fn subclass_list_view(hwnd: HWND) {
    // 统一给列表启用双缓冲和自定义背景擦除逻辑，减少自动刷新时的闪烁。
    if hwnd.is_null() {
        return;
    }

    // 安全性: all operations target a live ListView HWND supplied by the caller; ComCtl32 keeps
    // the boxed state pointer as subclass ref-data until WM_NCDESTROY.
    unsafe {
        SendMessageW(
            hwnd,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            LVS_EX_DOUBLEBUFFER,
            LVS_EX_DOUBLEBUFFER as isize,
        );

        let mut existing_ref_data = 0usize;
        if GetWindowSubclass(
            hwnd,
            Some(list_view_wnd_proc),
            LIST_VIEW_SUBCLASS_ID,
            &mut existing_ref_data,
        ) != 0
        {
            return;
        }

        let state = Box::into_raw(Box::new(ListViewPaintState::new()));
        if SetWindowSubclass(
            hwnd,
            Some(list_view_wnd_proc),
            LIST_VIEW_SUBCLASS_ID,
            state as usize,
        ) == 0
        {
            drop(Box::from_raw(state));
        }
    }
}

fn finish_list_view_update_internal(hwnd: HWND, invalidate: bool) {
    if hwnd.is_null() {
        return;
    }

    // 安全性: all calls target the provided ListView HWND; null handles were rejected above.
    unsafe {
        SendMessageW(hwnd, WM_SETREDRAW, 1, 0);
        if invalidate {
            InvalidateRect(hwnd, null(), 0);
        }
    }
}

pub fn finish_list_view_update(hwnd: HWND) {
    // 恢复重绘并安排一次异步刷新，避免采样/提交消息被同步 GDI 绘制阻塞。
    finish_list_view_update_internal(hwnd, true);
}

pub fn is_32_bit_process_handle(handle: HANDLE) -> bool {
    if handle.is_null() {
        return false;
    }

    let mut wow64 = 0;
    // 安全性: `handle` is checked non-null and only queried; `wow64` is a valid out parameter.
    unsafe { IsWow64Process(handle, &raw mut wow64) != 0 && wow64 != 0 }
}

pub fn is_32_bit_process_pid(pid: u32) -> bool {
    // 只为了查询位数时，打开最低限度的查询句柄即可，减少权限失败的概率。
    // 安全性: opening a process for query-only access; failure is represented as `None`.
    OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
        .is_some_and(|handle| is_32_bit_process_handle(handle.as_raw()))
}

pub fn append_32_bit_suffix(label: &str, is_32_bit: bool) -> Cow<'_, str> {
    if !is_32_bit {
        return Cow::Borrowed(label);
    }

    Cow::Owned(format!("{label} {}", text(TextKey::Bitness32Suffix)))
}

pub unsafe fn call_window_proc(
    wndproc: WNDPROC,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    // 安全性: callers pass a WNDPROC previously returned by Win32, and the message parameters
    // are forwarded unchanged.
    wndproc.map_or(0, |proc| {
        // 安全性: see the function-level safety note above.
        unsafe { CallWindowProcW(Some(proc), hwnd, msg, wparam, lparam) }
    })
}

fn set_rect_rgn_indirect(region: HRGN, rect: &RECT) {
    // 安全性: `region` is a caller-owned HRGN and `rect` is a valid borrowed RECT.
    unsafe { SetRectRgn(region, rect.left, rect.top, rect.right, rect.bottom) };
}

fn list_view_get_view_rgn(hwnd: HWND, state: &mut ListViewPaintState) {
    // 这里把”所有可视项区域”合成为一个区域，
    // 让背景擦除只覆盖真正的空白区，而不是先把选中行也擦掉。
    state.ensure_resources();
    if state.view_rgn.is_null() || state.clip_rgn.is_null() {
        return;
    }

    // 安全性: all GDI/message calls target the live ListView currently being subclassed; the
    // stack RECT is sized for LVM_GETITEMRECT and the regions are owned by `state`.
    unsafe {
        SetRectRgn(state.view_rgn, 0, 0, 0, 0);
    }
    // 安全性: read-only ListView query for item count.
    let item_count = unsafe { SendMessageW(hwnd, LVM_GETITEMCOUNT, 0, 0) as usize };
    let top_index = unsafe { SendMessageW(hwnd, LVM_GETTOPINDEX, 0, 0).max(0) as usize };
    let visible_count = unsafe { SendMessageW(hwnd, LVM_GETCOUNTPERPAGE, 0, 0).max(0) as usize };
    let end_index = top_index
        .saturating_add(visible_count)
        .saturating_add(1)
        .min(item_count);
    // 安全性: querying a process-global system metric has no caller-side invariants.
    let edge_width = unsafe { GetSystemMetrics(SM_CXEDGE) };

    for index in top_index..end_index {
        let mut item_rect = RECT {
            left: LVIR_BOUNDS as i32,
            // 安全性: RECT is a plain old data Win32 struct where all-zero is valid.
            ..unsafe { zeroed() }
        };
        // 安全性: LVM_GETITEMRECT writes into `item_rect`, whose pointer remains valid during
        // the synchronous SendMessage call.
        if unsafe {
            SendMessageW(
                hwnd,
                LVM_GETITEMRECT,
                index,
                &mut item_rect as *mut _ as LPARAM,
            )
        } == 0
        {
            continue;
        }

        item_rect.left += edge_width;
        set_rect_rgn_indirect(state.clip_rgn, &item_rect);
        // 安全性: all regions are valid GDI regions owned by `state`.
        unsafe { CombineRgn(state.view_rgn, state.view_rgn, state.clip_rgn, RGN_OR) };
    }
}

unsafe extern "system" fn list_view_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> isize {
    // 自定义 ListView 子类只接管背景擦除相关消息，其余消息继续走 ComCtl32 子类链。
    // 安全性: SetWindowSubclass stores a live `ListViewPaintState` pointer in ref-data until
    // WM_NCDESTROY; the mutable borrow is limited to this message dispatch.
    let state_ptr = ref_data as *mut ListViewPaintState;
    // 安全性: `state_ptr` either points to that live state object or is null.
    let Some(state) = (unsafe { state_ptr.as_mut() }) else {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    };

    match msg {
        WM_SYSCOLORCHANGE => {
            if !state.brush.is_null() {
                // 安全性: `brush` was created by GDI and is owned by `state`.
                unsafe { DeleteObject(state.brush as _) };
                state.brush = null_mut();
            }
            // 安全性: resources belong to this subclass state and `hwnd` is the current window.
            unsafe {
                state.ensure_resources();
                InvalidateRect(hwnd, null_mut(), 1);
            }
        }
        WM_ERASEBKGND => {
            // 安全性: the erase message supplies a valid HDC in WPARAM and all GDI resources
            // are owned by the per-window subclass state.
            unsafe {
                state.ensure_resources();
                if !state.brush.is_null() && !state.view_rgn.is_null() && !state.clip_rgn.is_null()
                {
                    let hdc = wparam as HDC;
                    let mut client_rect = zeroed::<RECT>();
                    GetClientRect(hwnd, &mut client_rect);
                    list_view_get_view_rgn(hwnd, state);
                    set_rect_rgn_indirect(state.clip_rgn, &client_rect);
                    CombineRgn(state.clip_rgn, state.clip_rgn, state.view_rgn, RGN_DIFF);
                    FillRgn(hdc, state.clip_rgn, state.brush);
                    return 1;
                }
            }
        }
        WM_NCDESTROY => {
            unsafe {
                RemoveWindowSubclass(hwnd, Some(list_view_wnd_proc), LIST_VIEW_SUBCLASS_ID);
            }
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            unsafe { drop(Box::from_raw(state_ptr)) };
            return result;
        }
        _ => {}
    }

    // 安全性: unhandled messages continue through the system-managed subclass chain.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

pub fn window_rect_relative_to_page(hwnd: HWND, page_hwnd: HWND) -> RECT {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut rect = zeroed::<RECT>();
        GetWindowRect(hwnd, &mut rect);
        MapWindowPoints(null_mut(), page_hwnd, &mut rect as *mut _ as _, 2);
        rect
    }
}

pub fn copy_text_to_callback_buffer(buffer: *mut u16, capacity: usize, text: &str) {
    if buffer.is_null() || capacity == 0 {
        return;
    }

    let max_len = capacity.saturating_sub(1);
    let mut written = 0usize;
    for code_unit in text.encode_utf16().take(max_len) {
        // 安全性: `written` is bounded by capacity - 1.
        unsafe { *buffer.add(written) = code_unit };
        written += 1;
    }
    // 安全性: one slot was reserved for the terminator.
    unsafe { *buffer.add(written) = 0 };
}

pub unsafe fn widestr_ptr_to_string(ptr: *const u16) -> String {
    // 安全性: 调用方必须传入有效的、以 NUL 结尾的 UTF-16 字符串指针。
    // 函数最多读取 MAX_WIDE_CHARS 个编码单元后停止。
    {
        if ptr.is_null() {
            return String::new();
        }
        const MAX_WIDE_CHARS: usize = 32 * 1024;
        let mut length = 0usize;
        while length < MAX_WIDE_CHARS && *ptr.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(ptr, length))
    }
}
