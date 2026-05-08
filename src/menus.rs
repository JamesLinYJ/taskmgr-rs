//! 菜单规格与构建入口。
//! 这里集中描述主菜单、右键菜单和子菜单的结构，让业务层只关心“要什么菜单”，
//! 不直接操作 `AppendMenuW` 这样的 Win32 细节。

use crate::language::{text, TextKey};
use crate::resource::*;
use crate::runtime_menu::{MenuBar, MenuItemState, PopupMenu};
use crate::winutil::sanitize_task_manager_menu;
use windows_sys::Win32::UI::WindowsAndMessaging::HMENU;

fn append_item(menu: &mut PopupMenu, command_id: u16, key: TextKey) -> bool {
    // 菜单项统一走语言键，避免调用方自己取文本。
    menu.append_item(command_id, text(key), MenuItemState::ENABLED)
}

fn append_separator(menu: &mut PopupMenu) -> bool {
    menu.append_separator()
}

fn build_update_speed_menu() -> Option<PopupMenu> {
    // 刷新速度子菜单会被多个主菜单复用。
    let mut menu = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_HIGH, TextKey::High),
        (IDM_NORMAL, TextKey::Normal),
        (IDM_LOW, TextKey::Low),
        (IDM_PAUSED, TextKey::Paused),
    ] {
        if !append_item(&mut menu, command_id, key) {
            return None;
        }
    }
    Some(menu)
}

fn build_file_menu() -> Option<PopupMenu> {
    // 文件菜单保持经典任务管理器的精简结构：新任务 + 退出。
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_RUN, TextKey::NewTaskMenu) {
        return None;
    }
    if !append_separator(&mut menu) {
        return None;
    }
    if !append_item(&mut menu, IDM_EXIT, TextKey::ExitTaskManager) {
        return None;
    }
    Some(menu)
}

fn build_options_menu(base_items: &[(u16, TextKey)]) -> Option<PopupMenu> {
    // `command_id == 0` 代表分隔线，其余项则按顺序展开成普通菜单项。
    let mut menu = PopupMenu::new()?;
    for (index, (command_id, key)) in base_items.iter().copied().enumerate() {
        if index > 0 && !append_separator(&mut menu) && command_id == 0 {
            return None;
        }
        if command_id == 0 {
            if !append_separator(&mut menu) {
                return None;
            }
            continue;
        }
        if !append_item(&mut menu, command_id, key) {
            return None;
        }
    }
    Some(menu)
}

fn build_task_view_popup() -> Option<PopupMenu> {
    // 任务页视图菜单只负责应用程序页自己的显示模式。
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_RUN, TextKey::NewTaskMenu) {
        return None;
    }
    if !append_separator(&mut menu) {
        return None;
    }
    for (command_id, key) in [
        (IDM_LARGEICONS, TextKey::LargeIcons),
        (IDM_SMALLICONS, TextKey::SmallIcons),
        (IDM_DETAILS, TextKey::Details),
    ] {
        if !append_item(&mut menu, command_id, key) {
            return None;
        }
    }
    Some(menu)
}

fn build_task_context_popup() -> Option<PopupMenu> {
    // 任务页右键菜单仍然保持老 Task Manager 的命令顺序。
    let mut menu = PopupMenu::new()?;
    for (index, (command_id, key)) in [
        (IDM_TASK_SWITCHTO, TextKey::SwitchTo),
        (IDM_TASK_BRINGTOFRONT, TextKey::BringToFront),
        (0, TextKey::TaskManager),
        (IDM_TASK_MINIMIZE, TextKey::Minimize),
        (IDM_TASK_MAXIMIZE, TextKey::Maximize),
        (IDM_TASK_CASCADE, TextKey::Cascade),
        (IDM_TASK_TILEHORZ, TextKey::TileHorizontally),
        (IDM_TASK_TILEVERT, TextKey::TileVertically),
        (0, TextKey::TaskManager),
        (IDM_TASK_ENDTASK, TextKey::EndTask),
        (IDM_TASK_FINDPROCESS, TextKey::GoToProcess),
    ]
    .iter()
    .copied()
    .enumerate()
    {
        if command_id == 0 {
            if !append_separator(&mut menu) {
                return None;
            }
            continue;
        }
        let _ = index;
        if !append_item(&mut menu, command_id, key) {
            return None;
        }
    }
    Some(menu)
}

fn build_perf_view_menu() -> Option<PopupMenu> {
    // 性能页视图菜单额外承载 CPU 历史模式等性能页专属选项。
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_REFRESH, TextKey::RefreshNow) {
        return None;
    }
    if !menu.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?) {
        return None;
    }
    if !append_separator(&mut menu) {
        return None;
    }

    let mut cpu_history = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_ALLCPUS, TextKey::OneGraphAllCpus),
        (IDM_MULTIGRAPH, TextKey::OneGraphPerCpu),
    ] {
        if !append_item(&mut cpu_history, command_id, key) {
            return None;
        }
    }
    if !menu.append_submenu(text(TextKey::CpuHistory), cpu_history) {
        return None;
    }
    if !append_item(&mut menu, IDM_KERNELTIMES, TextKey::ShowKernelTimes) {
        return None;
    }
    Some(menu)
}

fn build_common_help_menu() -> Option<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_HELP, TextKey::HelpTopics) {
        return None;
    }
    if !append_separator(&mut menu) {
        return None;
    }
    if !append_item(&mut menu, IDM_ABOUT, TextKey::AboutTaskManager) {
        return None;
    }
    Some(menu)
}

fn build_task_main_menu() -> Option<MenuBar> {
    let mut bar = MenuBar::new()?;
    let file = build_file_menu()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_CONFIRMATIONS, TextKey::Confirmations),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
    ])?;
    let mut view = PopupMenu::new()?;
    if !append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)
        || !view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)
        || !append_separator(&mut view)
        || !append_item(&mut view, IDM_LARGEICONS, TextKey::LargeIcons)
        || !append_item(&mut view, IDM_SMALLICONS, TextKey::SmallIcons)
        || !append_item(&mut view, IDM_DETAILS, TextKey::Details)
    {
        return None;
    }
    let mut windows = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_TASK_TILEHORZ, TextKey::TileHorizontally),
        (IDM_TASK_TILEVERT, TextKey::TileVertically),
        (IDM_TASK_MINIMIZE, TextKey::Minimize),
        (IDM_TASK_MAXIMIZE, TextKey::Maximize),
        (IDM_TASK_CASCADE, TextKey::Cascade),
        (IDM_TASK_BRINGTOFRONT, TextKey::BringToFront),
    ] {
        if !append_item(&mut windows, command_id, key) {
            return None;
        }
    }
    if !bar.append_submenu(text(TextKey::File), file)
        || !bar.append_submenu(text(TextKey::Options), options)
        || !bar.append_submenu(text(TextKey::View), view)
        || !bar.append_submenu(text(TextKey::Windows), windows)
        || !bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)
    {
        return None;
    }
    Some(bar)
}

fn build_process_main_menu() -> Option<MenuBar> {
    let mut bar = MenuBar::new()?;
    let file = build_file_menu()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_CONFIRMATIONS, TextKey::Confirmations),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
    ])?;
    let mut view = PopupMenu::new()?;
    if !append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)
        || !view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)
        || !append_separator(&mut view)
        || !append_item(&mut view, IDM_PROCCOLS, TextKey::SelectColumnsMenu)
    {
        return None;
    }
    if !bar.append_submenu(text(TextKey::File), file)
        || !bar.append_submenu(text(TextKey::Options), options)
        || !bar.append_submenu(text(TextKey::View), view)
        || !bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)
    {
        return None;
    }
    Some(bar)
}

fn build_perf_main_menu() -> Option<MenuBar> {
    let mut bar = MenuBar::new()?;
    let file = build_file_menu()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_CONFIRMATIONS, TextKey::Confirmations),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
    ])?;
    let view = build_perf_view_menu()?;
    if !bar.append_submenu(text(TextKey::File), file)
        || !bar.append_submenu(text(TextKey::Options), options)
        || !bar.append_submenu(text(TextKey::View), view)
        || !bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)
    {
        return None;
    }
    Some(bar)
}

fn build_network_main_menu() -> Option<MenuBar> {
    let mut bar = MenuBar::new()?;
    let file = build_file_menu()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
    ])?;
    let mut view = PopupMenu::new()?;
    if !append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)
        || !view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)
        || !append_separator(&mut view)
        || !append_item(&mut view, IDM_USERCOLS, TextKey::SelectColumnsMenu)
    {
        return None;
    }
    if !bar.append_submenu(text(TextKey::File), file)
        || !bar.append_submenu(text(TextKey::Options), options)
        || !bar.append_submenu(text(TextKey::View), view)
        || !bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)
    {
        return None;
    }
    Some(bar)
}

fn build_users_main_menu() -> Option<MenuBar> {
    let mut bar = MenuBar::new()?;
    let file = build_file_menu()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
        (IDM_SHOWDOMAINNAMES, TextKey::ShowFullAccountName),
    ])?;
    let mut view = PopupMenu::new()?;
    if !append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)
        || !view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)
    {
        return None;
    }
    if !bar.append_submenu(text(TextKey::File), file)
        || !bar.append_submenu(text(TextKey::Options), options)
        || !bar.append_submenu(text(TextKey::View), view)
        || !bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)
    {
        return None;
    }
    Some(bar)
}

fn build_tray_popup() -> Option<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_RESTORETASKMAN, TextKey::RestoreTaskManager)
        || !append_item(&mut menu, IDM_EXIT, TextKey::ExitTaskManager)
        || !append_separator(&mut menu)
        || !append_item(&mut menu, IDM_ALWAYSONTOP, TextKey::AlwaysOnTop)
    {
        return None;
    }
    Some(menu)
}

fn build_user_context_popup() -> Option<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    if !append_item(&mut menu, IDM_SENDMESSAGE, TextKey::SendMessage)
        || !append_separator(&mut menu)
        || !append_item(&mut menu, IDM_DISCONNECT, TextKey::Disconnect)
        || !append_item(&mut menu, IDM_LOGOFF, TextKey::Logoff)
    {
        return None;
    }
    Some(menu)
}

pub fn build_main_menu(resource_id: u16, processor_count: usize) -> Option<HMENU> {
    let menu = match resource_id {
        IDR_MAINMENU_TASK => build_task_main_menu()?.into_raw(),
        IDR_MAINMENU_PROC => build_process_main_menu()?.into_raw(),
        IDR_MAINMENU_PERF => build_perf_main_menu()?.into_raw(),
        IDR_MAINMENU_NET => build_network_main_menu()?.into_raw(),
        IDR_MAINMENU_USER => build_users_main_menu()?.into_raw(),
        _ => return None,
    };
    sanitize_task_manager_menu(menu, processor_count);
    Some(menu)
}

pub fn build_popup_menu(resource_id: u16, processor_count: usize) -> Option<HMENU> {
    let menu = match resource_id {
        IDR_TASK_CONTEXT => build_task_context_popup()?.into_raw(),
        IDR_TASKVIEW => build_task_view_popup()?.into_raw(),
        IDR_USER_CONTEXT => build_user_context_popup()?.into_raw(),
        IDR_TRAYMENU => build_tray_popup()?.into_raw(),
        _ => return None,
    };
    sanitize_task_manager_menu(menu, processor_count);
    Some(menu)
}
