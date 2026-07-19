// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 页面宿主
//
//   文件:       src/app/page_host.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

use std::mem::zeroed;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_GEN_FAILURE, ERROR_INVALID_WINDOW_HANDLE, GetLastError, HINSTANCE, HWND, LPARAM, RECT,
    WPARAM,
};

// 页面宿主层。
// 该模块把资源对话框与各页面状态对象粘合起来，统一处理页面的创建、
// 激活、焦点切换、菜单切换以及 Win32 消息分发。

use windows_sys::Win32::Graphics::Gdi::{
    BLACK_BRUSH, COLOR_3DFACE, FillRect, GetStockObject, GetSysColorBrush, HDC, InvalidateRect,
    UpdateWindow,
};
use windows_sys::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, DrawMenuBar, GWL_STYLE, GetClientRect, GetDlgCtrlID, GetDlgItem, GetWindowLongW,
    HMENU, HTCAPTION, SW_HIDE, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetMenu,
    SetWindowLongW, SetWindowPos, ShowWindow, WM_COMMAND, WM_CONTEXTMENU, WM_CTLCOLORBTN,
    WM_DRAWITEM, WM_ERASEBKGND, WM_INITDIALOG, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEWHEEL, WM_NCDESTROY, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NOTIFY,
    WM_SHOWWINDOW, WM_SIZE, WM_VSCROLL, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

use crate::app::page_registry::{PAGE_DESCRIPTORS, PageFocus, PageId};
use crate::config::options::Options;
use crate::infrastructure::native::{
    get_window_userdata, redraw_window_tree, set_window_userdata, set_window_userdata_ptr,
};
use crate::pages::applications::TaskPageState;
use crate::pages::cpu::CpuPageState;
use crate::pages::cpu::model::CpuDetailRefresh;
use crate::pages::gpu::GpuPageState;
use crate::pages::network::NetworkPageState;
use crate::pages::performance::PerformancePageState;
use crate::pages::processes::ProcessPageState;
use crate::pages::users::UserPageState;
use crate::system::process_identity::ProcIdentity;
use crate::system::sampler::SystemSample;
use crate::ui::dialogs::create_dialog;
use crate::ui::localization::{localize_dialog, text};
use crate::ui::menus::build_main_menu;
use crate::ui::resource_ids::{
    IDC_CPU_DETAIL_GRAPH, IDC_CPUMETER, IDC_GPU_SELECTOR, IDC_MEMGRAPH, IDC_MEMMETER,
    IDC_NICTOTALS, IDC_PROCLIST, IDC_TASKLIST, IDC_USERLIST, PWM_CPU_FIRMWARE_WORKER_COMPLETE,
    PWM_CPU_WORKER_COMPLETE, PWM_GPU_METADATA_WORKER_COMPLETE, PWM_GPU_WORKER_COMPLETE,
    PWM_NET_WORKER_COMPLETE, PWM_PROC_WORKER_COMPLETE, PWM_TASK_WORKER_COMPLETE,
    PWM_USER_WORKER_COMPLETE,
};
use crate::ui::runtime_menu::MenuBar;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    Prewarm,
    Activation,
    Periodic,
    User,
}

impl RefreshReason {
    fn forces_list_refresh(self) -> bool {
        self != Self::Periodic
    }

    fn cpu_detail_refresh(self) -> CpuDetailRefresh {
        match self {
            Self::Prewarm => CpuDetailRefresh::Prewarm,
            Self::Activation => CpuDetailRefresh::Activation,
            Self::Periodic => CpuDetailRefresh::Periodic,
            Self::User => CpuDetailRefresh::User,
        }
    }
}

enum PageState {
    Task(Box<TaskPageState>),
    Process(Box<ProcessPageState>),
    Performance(Box<PerformancePageState>),
    Cpu(Box<CpuPageState>),
    Gpu(Box<GpuPageState>),
    Network(Box<NetworkPageState>),
    Users(Box<UserPageState>),
}

impl PageState {
    fn prepare_initialize(
        &mut self,
        hinstance: HINSTANCE,
        main_hwnd: HWND,
        processor_count: usize,
    ) -> Result<(), u32> {
        match self {
            Self::Task(state) => state.prepare_initialize(hinstance, main_hwnd),
            Self::Performance(state) => state.initialize(hinstance, processor_count),
            _ => Ok(()),
        }
    }

    fn complete_initialize(
        &mut self,
        hinstance: HINSTANCE,
        hwnd: HWND,
        main_hwnd: HWND,
        hwnd_tabs: HWND,
    ) -> Result<(), u32> {
        let required_control = match self {
            Self::Task(_) => IDC_TASKLIST,
            Self::Process(_) => IDC_PROCLIST,
            Self::Performance(_) => IDC_CPUMETER,
            Self::Cpu(_) => IDC_CPU_DETAIL_GRAPH,
            Self::Gpu(_) => IDC_GPU_SELECTOR,
            Self::Network(_) => IDC_NICTOTALS,
            Self::Users(_) => IDC_USERLIST,
        };
        if unsafe { GetDlgItem(hwnd, required_control) }.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }

        match self {
            Self::Task(state) => state.complete_initialize()?,
            Self::Process(state) => unsafe {
                state.initialize(hinstance, hwnd, main_hwnd)?;
            },
            Self::Performance(state) => state.complete_initialize(hwnd)?,
            Self::Cpu(state) => state.initialize(hwnd)?,
            Self::Gpu(state) => state.initialize(hwnd, main_hwnd, hwnd_tabs)?,
            Self::Network(state) => unsafe {
                state.initialize(hwnd, main_hwnd, hwnd_tabs)?;
            },
            Self::Users(state) => state.initialize(hwnd)?,
        }
        Ok(())
    }

    fn apply_options(
        &mut self,
        hwnd: HWND,
        main_hwnd: HWND,
        options: &Options,
        processor_count: usize,
    ) {
        let redraw_owner_draw_page = match self {
            Self::Task(state) => {
                state.apply_options(options);
                false
            }
            Self::Process(state) => unsafe {
                state.apply_options(options, processor_count);
                false
            },
            Self::Performance(state) => {
                if state.apply_options(hwnd, options, processor_count) {
                    state.size_page(hwnd, main_hwnd);
                    true
                } else {
                    false
                }
            }
            Self::Cpu(state) => {
                if state.apply_options(options) {
                    state.size_page()
                } else {
                    false
                }
            }
            Self::Gpu(state) => {
                if state.apply_options(options) {
                    state.size_page()
                } else {
                    false
                }
            }
            Self::Network(state) => unsafe {
                state.apply_options(options);
                false
            },
            Self::Users(state) => {
                state.apply_options(options);
                false
            }
        };
        if redraw_owner_draw_page {
            redraw_window_tree(hwnd);
        }
    }

    fn timer_event(&mut self, options: &Options, processor_count: usize, reason: RefreshReason) {
        let force = reason.forces_list_refresh();
        match self {
            Self::Task(state) => state.timer_event(options, force),
            Self::Process(state) => unsafe {
                state.apply_options(options, processor_count);
                state.timer_event(options, force);
            },
            Self::Performance(_) => {}
            Self::Cpu(state) => state.timer_event(reason.cpu_detail_refresh()),
            Self::Gpu(state) => {
                let _ = state.apply_options(options);
                state.timer_event();
            }
            Self::Network(state) => unsafe {
                state.apply_options(options);
                state.timer_event();
            },
            Self::Users(state) => {
                state.apply_options(options);
                state.timer_event();
            }
        }
    }

    fn deactivate(&mut self, options: &mut Options) {
        if let Self::Process(state) = self {
            unsafe {
                state.deactivate(options);
            }
        }
    }

    fn destroy(&mut self) {
        match self {
            Self::Task(state) => state.destroy(),
            Self::Process(state) => unsafe { state.destroy() },
            Self::Performance(state) => state.destroy(),
            Self::Cpu(state) => state.destroy(),
            Self::Gpu(state) => state.destroy(),
            Self::Network(state) => unsafe { state.destroy() },
            Self::Users(state) => state.destroy(),
        }
    }

    fn apply_system_sample(
        &mut self,
        hwnd: HWND,
        sample: &SystemSample,
        redraw: bool,
    ) -> Result<(), u32> {
        match self {
            Self::Performance(state) => state.apply_system_sample(hwnd, sample, redraw),
            Self::Cpu(state) => state.apply_system_sample(sample, redraw),
            _ => Ok(()),
        }
    }

    fn mark_system_sample_error(&mut self) {
        if let Self::Cpu(state) = self {
            state.mark_system_sample_error();
        }
    }

    fn handle_process_command(&mut self, command_id: u16, options: Option<&mut Options>) -> bool {
        match self {
            Self::Process(state) => unsafe {
                state.handle_command(command_id, options);
                true
            },
            _ => false,
        }
    }

    fn handle_user_command(&mut self, command_id: u16) -> bool {
        match self {
            Self::Users(state) => state.handle_command(command_id),
            _ => false,
        }
    }

    fn user_show_domain_names(&self) -> Option<bool> {
        match self {
            Self::Users(state) => Some(state.show_domain_names()),
            _ => None,
        }
    }

    fn find_process(&mut self, identity: ProcIdentity) -> bool {
        match self {
            Self::Process(state) => unsafe { state.find_process(identity) },
            _ => false,
        }
    }

    fn no_title(&self) -> bool {
        match self {
            Self::Task(state) => state.no_title(),
            Self::Process(state) => unsafe { state.no_title() },
            Self::Performance(state) => state.no_title(),
            Self::Cpu(state) => state.no_title(),
            Self::Gpu(state) => state.no_title(),
            Self::Network(state) => unsafe { state.no_title() },
            Self::Users(state) => state.no_title(),
        }
    }

    fn handle_init_dialog(
        &mut self,
        _hinstance: HINSTANCE,
        hwnd: HWND,
        _main_hwnd: HWND,
        _hwnd_tabs: HWND,
    ) -> isize {
        match self {
            Self::Task(state) => state.handle_init_dialog(hwnd),
            Self::Process(_) => 1,
            Self::Network(_) => 1,
            Self::Users(_) => 1,
            Self::Performance(_) => 1,
            Self::Cpu(_) => 1,
            Self::Gpu(_) => 1,
        }
    }

    fn handle_page_command(&mut self, command_id: u16) -> isize {
        match self {
            Self::Task(state) => {
                state.handle_command(command_id);
                1
            }
            Self::Process(state) => unsafe {
                state.handle_command(command_id, None);
                1
            },
            Self::Users(state) => isize::from(state.handle_command(command_id)),
            _ => 0,
        }
    }

    fn handle_notify(&mut self, lparam: LPARAM) -> isize {
        match self {
            Self::Task(state) => state.handle_notify(lparam),
            Self::Process(state) => unsafe { state.handle_notify(lparam) },
            Self::Cpu(state) => state.handle_notify(lparam),
            Self::Users(state) => state.handle_notify(lparam),
            _ => 0,
        }
    }

    fn handle_context_menu(&mut self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> Option<isize> {
        match self {
            Self::Task(state) if wparam as HWND == unsafe { GetDlgItem(hwnd, IDC_TASKLIST) } => {
                state.show_context_menu(
                    i32::from((lparam & 0xFFFF) as i16),
                    i32::from(((lparam >> 16) & 0xFFFF) as i16),
                );
                Some(1)
            }
            Self::Process(state) if wparam as HWND == unsafe { GetDlgItem(hwnd, IDC_PROCLIST) } => {
                unsafe {
                    state.show_context_menu(
                        i32::from((lparam & 0xFFFF) as i16),
                        i32::from(((lparam >> 16) & 0xFFFF) as i16),
                    );
                }
                Some(1)
            }
            Self::Users(state) if wparam as HWND == unsafe { GetDlgItem(hwnd, IDC_USERLIST) } => {
                state.show_context_menu(
                    i32::from((lparam & 0xFFFF) as i16),
                    i32::from(((lparam >> 16) & 0xFFFF) as i16),
                );
                Some(1)
            }
            _ => None,
        }
    }

    fn handle_size_or_show(&mut self, hwnd: HWND, main_hwnd: HWND) -> isize {
        let redraw_owner_draw_page = match self {
            Self::Task(state) => {
                state.size_page();
                false
            }
            Self::Process(state) => unsafe {
                state.size_page();
                false
            },
            Self::Performance(state) => {
                state.size_page(hwnd, main_hwnd);
                true
            }
            Self::Cpu(state) => state.size_page(),
            Self::Gpu(state) => state.size_page(),
            Self::Network(state) => unsafe {
                state.size_page();
                false
            },
            Self::Users(state) => {
                state.size_page();
                false
            }
        };
        if redraw_owner_draw_page {
            redraw_window_tree(hwnd);
        }
        1
    }

    fn redraw_after_layout(&self, hwnd: HWND) {
        match self {
            Self::Performance(_) | Self::Cpu(_) | Self::Gpu(_) => redraw_window_tree(hwnd),
            _ => redraw_plain_page(hwnd),
        }
    }

    fn host_style_flags(&self) -> u32 {
        match self {
            Self::Process(_) | Self::Network(_) => WS_CLIPCHILDREN,
            Self::Performance(_) | Self::Cpu(_) | Self::Gpu(_) => WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            Self::Task(_) | Self::Users(_) => 0,
        }
    }

    fn forwards_title_double_click(&self) -> bool {
        !matches!(self, Self::Task(_))
    }

    unsafe fn initialize_dialog_host(
        &mut self,
        hinstance: HINSTANCE,
        hwnd: HWND,
        main_hwnd: HWND,
        hwnd_tabs: HWND,
    ) -> isize {
        let style_flags = self.host_style_flags();
        if style_flags != 0 {
            let current_style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
            unsafe {
                SetWindowLongW(hwnd, GWL_STYLE, (current_style | style_flags) as i32);
            }
        }
        self.handle_init_dialog(hinstance, hwnd, main_hwnd, hwnd_tabs)
    }

    unsafe fn handle_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<isize> {
        match msg {
            WM_COMMAND => match self {
                Self::Gpu(state) => Some(state.handle_command(wparam)),
                Self::Task(_) | Self::Process(_) | Self::Users(_) => {
                    Some(self.handle_page_command((wparam & 0xFFFF) as u16))
                }
                _ => None,
            },
            WM_NOTIFY => match self {
                Self::Task(_) | Self::Process(_) | Self::Cpu(_) | Self::Users(_) => {
                    Some(self.handle_notify(lparam))
                }
                _ => None,
            },
            WM_CONTEXTMENU => self.handle_context_menu(hwnd, wparam, lparam),
            WM_ERASEBKGND if matches!(self, Self::Performance(_) | Self::Cpu(_) | Self::Gpu(_)) => {
                Some(unsafe { erase_performance_page_background(hwnd, wparam) })
            }
            WM_CTLCOLORBTN => {
                let control_id = unsafe { GetDlgCtrlID(lparam as HWND) };
                let is_graph = match self {
                    Self::Performance(state) => state.is_graph_control(control_id),
                    Self::Cpu(state) => state.is_graph_control(control_id),
                    Self::Gpu(state) => state.is_graph_control(control_id),
                    _ => false,
                };
                is_graph.then(|| unsafe { GetStockObject(BLACK_BRUSH) } as isize)
            }
            WM_DRAWITEM => {
                let Some(draw_item) = (unsafe { (lparam as *const DRAWITEMSTRUCT).as_ref() })
                else {
                    return Some(0);
                };
                let result = match self {
                    Self::Performance(state) => {
                        let control_id = wparam as i32;
                        if let Some(pane_index) = state.cpu_graph_pane_index(control_id) {
                            state.draw_cpu_graph(draw_item.hDC, draw_item.rcItem, pane_index);
                            1
                        } else {
                            match control_id {
                                IDC_CPUMETER => {
                                    state.draw_cpu_meter(draw_item.hDC, draw_item.rcItem);
                                    1
                                }
                                IDC_MEMMETER => {
                                    state.draw_mem_meter(draw_item.hDC, draw_item.rcItem);
                                    1
                                }
                                IDC_MEMGRAPH => {
                                    state.draw_mem_graph(draw_item.hDC, draw_item.rcItem);
                                    1
                                }
                                _ => 0,
                            }
                        }
                    }
                    Self::Cpu(state) if state.is_graph_control(draw_item.CtlID as i32) => {
                        state.draw_graph(draw_item.hDC, draw_item.rcItem);
                        1
                    }
                    Self::Gpu(state) if state.is_graph_control(draw_item.CtlID as i32) => {
                        state.draw_graph(draw_item.hDC, draw_item.rcItem, draw_item.CtlID as i32);
                        1
                    }
                    Self::Network(state) => {
                        if let Some(pane_index) = state.graph_pane_index(draw_item.CtlID as i32) {
                            unsafe {
                                state.draw_graph(draw_item.hDC, draw_item.rcItem, pane_index);
                            }
                            1
                        } else {
                            0
                        }
                    }
                    _ => 0,
                };
                Some(result)
            }
            WM_VSCROLL => match self {
                Self::Gpu(state) => Some(state.handle_vscroll(wparam)),
                Self::Network(state) => Some(unsafe { state.handle_vscroll(wparam) }),
                _ => None,
            },
            WM_MOUSEWHEEL => match self {
                Self::Gpu(state) => Some(state.handle_mouse_wheel(wparam)),
                Self::Network(state) => Some(unsafe { state.handle_mouse_wheel(wparam) }),
                _ => None,
            },
            PWM_TASK_WORKER_COMPLETE => match self {
                Self::Task(state) => {
                    state.handle_worker_completion();
                    Some(1)
                }
                _ => None,
            },
            PWM_PROC_WORKER_COMPLETE => match self {
                Self::Process(state) => {
                    unsafe {
                        state.handle_worker_completion();
                    }
                    Some(1)
                }
                _ => None,
            },
            PWM_CPU_WORKER_COMPLETE => match self {
                Self::Cpu(state) => {
                    state.handle_native_worker_completion();
                    Some(1)
                }
                _ => None,
            },
            PWM_CPU_FIRMWARE_WORKER_COMPLETE => match self {
                Self::Cpu(state) => {
                    state.handle_firmware_worker_completion();
                    Some(1)
                }
                _ => None,
            },
            PWM_GPU_WORKER_COMPLETE => match self {
                Self::Gpu(state) => {
                    state.handle_worker_completion();
                    Some(1)
                }
                _ => None,
            },
            PWM_GPU_METADATA_WORKER_COMPLETE => match self {
                Self::Gpu(state) => {
                    state.handle_metadata_worker_completion();
                    Some(1)
                }
                _ => None,
            },
            PWM_NET_WORKER_COMPLETE => match self {
                Self::Network(state) => {
                    unsafe {
                        state.handle_worker_completion();
                    }
                    Some(0)
                }
                _ => None,
            },
            PWM_USER_WORKER_COMPLETE => match self {
                Self::Users(state) => {
                    state.handle_worker_completion();
                    Some(0)
                }
                _ => None,
            },
            _ => None,
        }
    }
}

pub struct DialogPage {
    // `DialogPage` 是资源对话框与具体页面状态对象之间的适配层。
    // 不同页面共享同一套激活/隐藏/菜单切换流程，只把真正的业务状态交给子页面实现。
    hinstance: HINSTANCE,
    hwnd: HWND,
    hwnd_tabs: HWND,
    main_hwnd: HWND,
    id: PageId,
    menu: Option<MenuBar>,
    state: PageState,
}

impl DialogPage {
    pub(crate) fn new(id: PageId) -> Self {
        // 页面构造阶段只声明稳定身份和状态类型，静态 UI 元数据来自页面注册表。
        let state = match id {
            PageId::Applications => PageState::Task(Box::new(TaskPageState::new())),
            PageId::Processes => PageState::Process(Box::new(ProcessPageState::new())),
            PageId::Performance => PageState::Performance(Box::new(PerformancePageState::new())),
            PageId::Cpu => PageState::Cpu(Box::default()),
            PageId::Gpu => PageState::Gpu(Box::default()),
            PageId::Network => PageState::Network(Box::new(NetworkPageState::new())),
            PageId::Users => PageState::Users(Box::new(UserPageState::new())),
        };
        Self {
            hinstance: null_mut(),
            hwnd: null_mut(),
            hwnd_tabs: null_mut(),
            main_hwnd: null_mut(),
            id,
            menu: None,
            state,
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn title(&self, _hinstance: HINSTANCE) -> String {
        text(self.id.descriptor().title_key).to_string()
    }

    pub fn initialize(
        &mut self,
        hinstance: HINSTANCE,
        main_hwnd: HWND,
        hwnd_tabs: HWND,
        processor_count: usize,
    ) -> Result<(), u32> {
        // 页面初始化分成两段：
        // 先准备纯状态资源，再创建对话框，最后补上依赖真实 HWND 的后置初始化。
        self.hinstance = hinstance;
        self.main_hwnd = main_hwnd;
        self.hwnd_tabs = hwnd_tabs;

        self.state
            .prepare_initialize(hinstance, main_hwnd, processor_count)?;

        let descriptor = self.id.descriptor();

        self.hwnd = match create_dialog(
            hinstance,
            descriptor.dialog_id,
            main_hwnd,
            Some(page_dialog_proc),
            self as *mut DialogPage as LPARAM,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                self.state.destroy();
                return Err(error);
            }
        };

        localize_dialog(self.hwnd, descriptor.dialog_id);
        if let Err(error) = self.state.complete_initialize(
            self.hinstance,
            self.hwnd,
            self.main_hwnd,
            self.hwnd_tabs,
        ) {
            // 安全性: `self.hwnd` is the just-created dialog owned by this page.
            unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = null_mut();
            self.state.destroy();
            return Err(error);
        }
        let menu = match build_main_menu(descriptor.menu_id, processor_count) {
            Ok(menu) => menu,
            Err(error) => {
                // 安全性: `self.hwnd` is still the page dialog owned by this object.
                unsafe { DestroyWindow(self.hwnd) };
                self.hwnd = null_mut();
                self.state.destroy();
                return Err(error);
            }
        };
        self.menu = Some(menu);
        Ok(())
    }

    pub fn activate(
        &mut self,
        _hinstance: HINSTANCE,
        main_hwnd: HWND,
        options: &Options,
        processor_count: usize,
        current_menu: &mut HMENU,
    ) -> Result<(), u32> {
        // 激活页面时顺带切换主菜单和焦点目标，
        // 这样每个页面都能看起来像自己“拥有”一套独立菜单。
        if self.hwnd.is_null() {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        }

        let Some(menu) = self.menu.as_ref() else {
            return Err(ERROR_INVALID_WINDOW_HANDLE);
        };
        let menu = menu.as_raw();

        if !options.no_title() {
            // 安全性: the page owns `menu` for the full lifetime of the main window.
            unsafe {
                if SetMenu(main_hwnd, menu) == 0 {
                    let error = GetLastError();
                    return Err(if error == 0 { ERROR_GEN_FAILURE } else { error });
                }
                DrawMenuBar(main_hwnd);
            }
        }

        *current_menu = menu;

        self.apply_options(options, processor_count);

        Ok(())
    }

    pub fn show_and_focus(&self) {
        if self.hwnd.is_null() {
            return;
        }

        // 页面在隐藏状态完成布局和刷新后才显示，避免首次切页暴露中间绘制状态。
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
            SetWindowPos(self.hwnd, null_mut(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
        if matches!(
            &self.state,
            PageState::Performance(_) | PageState::Cpu(_) | PageState::Gpu(_)
        ) {
            // Hidden-page prewarming cannot paint owner-draw children. Commit the complete chart
            // tree synchronously after it becomes visible so no template-layout frame escapes.
            redraw_window_tree(self.hwnd);
        }

        match self.id.descriptor().initial_focus {
            PageFocus::None => {}
            PageFocus::Tabs => {
                if !self.hwnd_tabs.is_null() {
                    unsafe { SetFocus(self.hwnd_tabs) };
                }
            }
            PageFocus::Control(control_id) => {
                let focus_hwnd = unsafe { GetDlgItem(self.hwnd, control_id) };
                if !focus_hwnd.is_null() {
                    unsafe { SetFocus(focus_hwnd) };
                }
            }
        }
    }

    pub fn apply_options(&mut self, options: &Options, processor_count: usize) {
        // 宿主层只负责把全局选项广播到实际持有状态的那一页，
        // 页面内部再决定哪些控件需要重排、重绘或重建列。
        self.state
            .apply_options(self.hwnd, self.main_hwnd, options, processor_count);
    }

    pub fn timer_event(
        &mut self,
        options: &Options,
        processor_count: usize,
        reason: RefreshReason,
    ) {
        // 定时刷新同样走统一入口，避免主框架需要知道每个页面各自的刷新细节。
        self.state.timer_event(options, processor_count, reason);
    }

    pub fn apply_system_sample(&mut self, sample: &SystemSample, redraw: bool) -> Result<(), u32> {
        self.state.apply_system_sample(self.hwnd, sample, redraw)
    }

    pub fn mark_system_sample_error(&mut self) {
        self.state.mark_system_sample_error();
    }

    pub fn redraw_after_layout(&self) {
        // 主窗口在无标题模式切换后只重绘自己的边框/背景；页面按自身规则刷新，
        // 避免主窗口递归刷新隐藏控件导致残留文字盖到图表上。
        if self.hwnd.is_null() {
            return;
        }
        self.state.redraw_after_layout(self.hwnd);
    }

    pub fn deactivate(&mut self, options: &mut Options) {
        // 页面切走前只保存必要的易失状态，比如进程页列宽；
        // 其它页面如果没有额外状态，就只需要隐藏窗口。
        self.state.deactivate(options);
        if !self.hwnd.is_null() {
            // 安全性: hiding this page dialog HWND.
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }

    pub fn destroy(&mut self) {
        // 页面销毁分为“业务资源销毁”和“窗口销毁”两层，前者有些并不依赖窗口仍然存在。
        self.state.destroy();
        if !self.hwnd.is_null() {
            // 安全性: destroying this page dialog HWND exactly once.
            unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = null_mut();
        }
        self.menu = None;
    }

    pub fn handle_process_command(
        &mut self,
        command_id: u16,
        options: Option<&mut Options>,
    ) -> bool {
        self.state.handle_process_command(command_id, options)
    }

    pub fn handle_user_command(&mut self, command_id: u16) -> bool {
        self.state.handle_user_command(command_id)
    }

    pub fn user_show_domain_names(&self) -> Option<bool> {
        self.state.user_show_domain_names()
    }

    pub fn find_process(&mut self, identity: ProcIdentity) -> bool {
        self.state.find_process(identity)
    }
}

pub fn default_pages() -> [DialogPage; PageId::COUNT] {
    std::array::from_fn(|index| DialogPage::new(PAGE_DESCRIPTORS[index].id))
}

fn page_pointer_for_message(msg: u32, stored_userdata: isize, lparam: LPARAM) -> *mut DialogPage {
    if msg == WM_INITDIALOG {
        lparam as *mut DialogPage
    } else {
        stored_userdata as *mut DialogPage
    }
}

fn page_from_hwnd(hwnd: HWND, msg: u32, lparam: LPARAM) -> *mut DialogPage {
    // Only WM_INITDIALOG carries the application-defined initialization pointer. Messages sent
    // while the dialog manager is still constructing the HWND use lParam for their own contracts.
    page_pointer_for_message(msg, get_window_userdata(hwnd), lparam)
}

fn bind_page(hwnd: HWND, page: *mut DialogPage) {
    if !page.is_null() {
        // 安全性: WM_INITDIALOG supplies the DialogPage pointer passed to CreateDialogParam.
        unsafe {
            (*page).hwnd = hwnd;
            set_window_userdata_ptr(hwnd, page);
        }
    }
}

unsafe fn forward_no_title_drag(page: *mut DialogPage, msg: u32, lparam: LPARAM) -> bool {
    if page.is_null() || !unsafe { (*page).state.no_title() } {
        return false;
    }

    let forwarded_msg = if msg == WM_LBUTTONUP {
        WM_NCLBUTTONUP
    } else {
        WM_NCLBUTTONDOWN
    };
    unsafe {
        SendMessageW((*page).main_hwnd, forwarded_msg, HTCAPTION as usize, lparam);
    }
    true
}

unsafe fn forward_main_double_click(
    page: *mut DialogPage,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> bool {
    if page.is_null() {
        return false;
    }

    unsafe {
        SendMessageW((*page).main_hwnd, msg, wparam, lparam);
    }
    true
}

fn redraw_plain_page(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }

    // 安全性: the page HWND is owned by the current dialog page; ordinary pages can use a normal
    // erased invalidation because they do not overlay hidden statistic controls onto owner-draw graphs.
    unsafe {
        InvalidateRect(hwnd, null_mut(), 1);
        UpdateWindow(hwnd);
    }
}

unsafe extern "system" fn page_dialog_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    let page = page_from_hwnd(hwnd, msg, lparam);

    // Safety: the dialog and every page state live on the UI thread. The pointer is installed
    // during WM_INITDIALOG and cleared before the HWND can receive post-destruction callbacks.
    unsafe {
        match msg {
            WM_INITDIALOG => {
                if page.is_null() {
                    return 0;
                }
                bind_page(hwnd, page);
                (*page).state.initialize_dialog_host(
                    (*page).hinstance,
                    hwnd,
                    (*page).main_hwnd,
                    (*page).hwnd_tabs,
                )
            }
            WM_NCDESTROY => {
                if !page.is_null() {
                    set_window_userdata(hwnd, 0);
                    if (*page).hwnd == hwnd {
                        (*page).hwnd = null_mut();
                    }
                }
                0
            }
            WM_LBUTTONUP | WM_LBUTTONDOWN => {
                forward_no_title_drag(page, msg, lparam);
                0
            }
            WM_NCLBUTTONDBLCLK | WM_LBUTTONDBLCLK => {
                if !page.is_null() && (*page).state.forwards_title_double_click() {
                    forward_main_double_click(page, msg, wparam, lparam);
                }
                0
            }
            WM_SHOWWINDOW => 1,
            WM_SIZE => {
                if page.is_null() {
                    1
                } else {
                    (*page).state.handle_size_or_show(hwnd, (*page).main_hwnd)
                }
            }
            _ if page.is_null() => 0,
            _ => (*page)
                .state
                .handle_message(hwnd, msg, wparam, lparam)
                .unwrap_or(0),
        }
    }
}

unsafe fn erase_performance_page_background(hwnd: HWND, wparam: WPARAM) -> isize {
    // Owner-draw pages use the system face color while graph controls are temporarily paused.
    unsafe {
        let hdc = wparam as HDC;
        if hdc.is_null() {
            return 1;
        }

        let mut rect = zeroed::<RECT>();
        GetClientRect(hwnd, &mut rect);
        FillRect(hdc, &rect, GetSysColorBrush(COLOR_3DFACE));
        1
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::WM_SETFONT;

    #[test]
    fn messages_before_init_never_interpret_their_lparam_as_page_state() {
        assert!(page_pointer_for_message(WM_SETFONT, 0, 1).is_null());
    }

    #[test]
    fn init_pointer_is_replaced_by_bound_userdata_after_initialization() {
        let page = NonNull::<DialogPage>::dangling().as_ptr();
        assert_eq!(
            page_pointer_for_message(WM_INITDIALOG, 0, page as LPARAM),
            page
        );
        assert_eq!(page_pointer_for_message(WM_SIZE, page as isize, 1), page);
    }
}
