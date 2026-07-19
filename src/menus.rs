//! 菜单规格与构建入口。
//! 这里集中描述主菜单、右键菜单和子菜单的结构，让业务层只关心“要什么菜单”，
//! 不直接操作 `AppendMenuW` 这样的 Win32 细节。

use windows_sys::Win32::Foundation::ERROR_RESOURCE_DATA_NOT_FOUND;

use crate::language::{TextKey, text};
use crate::resource::*;
use crate::runtime_menu::{MenuBar, MenuItemState, PopupMenu};
use crate::winutil::sanitize_task_manager_menu;

type MenuResult<T> = Result<T, u32>;

fn append_item(menu: &mut PopupMenu, command_id: u16, key: TextKey) -> MenuResult<()> {
    menu.append_item(command_id, text(key), MenuItemState::ENABLED)
}

fn append_separator(menu: &mut PopupMenu) -> MenuResult<()> {
    menu.append_separator()
}

fn build_update_speed_menu() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_HIGH, TextKey::High),
        (IDM_NORMAL, TextKey::Normal),
        (IDM_LOW, TextKey::Low),
        (IDM_PAUSED, TextKey::Paused),
    ] {
        append_item(&mut menu, command_id, key)?;
    }
    Ok(menu)
}

fn build_file_menu() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_RUN, TextKey::NewTaskMenu)?;
    append_separator(&mut menu)?;
    append_item(&mut menu, IDM_EXIT, TextKey::ExitTaskManager)?;
    Ok(menu)
}

fn build_options_menu(base_items: &[(u16, TextKey)]) -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    for (command_id, key) in base_items.iter().copied() {
        if command_id == 0 {
            append_separator(&mut menu)?;
        } else {
            append_item(&mut menu, command_id, key)?;
        }
    }
    Ok(menu)
}

fn build_task_view_popup() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_RUN, TextKey::NewTaskMenu)?;
    append_separator(&mut menu)?;
    for (command_id, key) in [
        (IDM_LARGEICONS, TextKey::LargeIcons),
        (IDM_SMALLICONS, TextKey::SmallIcons),
        (IDM_DETAILS, TextKey::Details),
    ] {
        append_item(&mut menu, command_id, key)?;
    }
    Ok(menu)
}

fn build_task_context_popup() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    for (command_id, key) in [
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
    ] {
        if command_id == 0 {
            append_separator(&mut menu)?;
        } else {
            append_item(&mut menu, command_id, key)?;
        }
    }
    Ok(menu)
}

fn build_perf_view_menu() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_REFRESH, TextKey::RefreshNow)?;
    menu.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_separator(&mut menu)?;

    let mut cpu_history = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_ALLCPUS, TextKey::OneGraphAllCpus),
        (IDM_MULTIGRAPH, TextKey::OneGraphPerCpu),
    ] {
        append_item(&mut cpu_history, command_id, key)?;
    }
    menu.append_submenu(text(TextKey::CpuHistory), cpu_history)?;
    append_item(&mut menu, IDM_KERNELTIMES, TextKey::ShowKernelTimes)?;
    Ok(menu)
}

fn build_common_help_menu() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_HELP, TextKey::HelpTopics)?;
    append_separator(&mut menu)?;
    append_item(&mut menu, IDM_ABOUT, TextKey::AboutTaskManager)?;
    Ok(menu)
}

fn build_common_options_menu(include_confirmations: bool) -> MenuResult<PopupMenu> {
    if include_confirmations {
        build_options_menu(&[
            (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
            (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
            (IDM_CONFIRMATIONS, TextKey::Confirmations),
            (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
        ])
    } else {
        build_options_menu(&[
            (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
            (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
            (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
        ])
    }
}

fn append_common_main_menus(
    bar: &mut MenuBar,
    file: PopupMenu,
    options: PopupMenu,
    view: PopupMenu,
) -> MenuResult<()> {
    bar.append_submenu(text(TextKey::File), file)?;
    bar.append_submenu(text(TextKey::Options), options)?;
    bar.append_submenu(text(TextKey::View), view)?;
    Ok(())
}

fn build_task_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_separator(&mut view)?;
    append_item(&mut view, IDM_LARGEICONS, TextKey::LargeIcons)?;
    append_item(&mut view, IDM_SMALLICONS, TextKey::SmallIcons)?;
    append_item(&mut view, IDM_DETAILS, TextKey::Details)?;

    let mut windows = PopupMenu::new()?;
    for (command_id, key) in [
        (IDM_TASK_TILEHORZ, TextKey::TileHorizontally),
        (IDM_TASK_TILEVERT, TextKey::TileVertically),
        (IDM_TASK_MINIMIZE, TextKey::Minimize),
        (IDM_TASK_MAXIMIZE, TextKey::Maximize),
        (IDM_TASK_CASCADE, TextKey::Cascade),
        (IDM_TASK_BRINGTOFRONT, TextKey::BringToFront),
    ] {
        append_item(&mut windows, command_id, key)?;
    }

    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(true)?,
        view,
    )?;
    bar.append_submenu(text(TextKey::Windows), windows)?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_process_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_separator(&mut view)?;
    append_item(&mut view, IDM_PROCCOLS, TextKey::SelectColumnsMenu)?;
    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(true)?,
        view,
    )?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_perf_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(true)?,
        build_perf_view_menu()?,
    )?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_cpu_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_separator(&mut view)?;
    append_item(&mut view, IDM_KERNELTIMES, TextKey::ShowKernelTimes)?;
    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(false)?,
        view,
    )?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_network_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(false)?,
        view,
    )?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_gpu_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_common_main_menus(
        &mut bar,
        build_file_menu()?,
        build_common_options_menu(false)?,
        view,
    )?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_users_main_menu() -> MenuResult<MenuBar> {
    let mut bar = MenuBar::new()?;
    let options = build_options_menu(&[
        (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
        (IDM_MINIMIZEONUSE, TextKey::MinimizeOnUse),
        (IDM_HIDEWHENMIN, TextKey::HideWhenMinimized),
        (IDM_SHOWDOMAINNAMES, TextKey::ShowFullAccountName),
    ])?;
    let mut view = PopupMenu::new()?;
    append_item(&mut view, IDM_REFRESH, TextKey::RefreshNow)?;
    view.append_submenu(text(TextKey::UpdateSpeed), build_update_speed_menu()?)?;
    append_common_main_menus(&mut bar, build_file_menu()?, options, view)?;
    bar.append_submenu(text(TextKey::Help), build_common_help_menu()?)?;
    Ok(bar)
}

fn build_tray_popup() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_RESTORETASKMAN, TextKey::RestoreTaskManager)?;
    append_item(&mut menu, IDM_EXIT, TextKey::ExitTaskManager)?;
    append_separator(&mut menu)?;
    append_item(&mut menu, IDM_ALWAYSONTOP, TextKey::AlwaysOnTop)?;
    Ok(menu)
}

fn build_user_context_popup() -> MenuResult<PopupMenu> {
    let mut menu = PopupMenu::new()?;
    append_item(&mut menu, IDM_SENDMESSAGE, TextKey::SendMessage)?;
    append_separator(&mut menu)?;
    append_item(&mut menu, IDM_DISCONNECT, TextKey::Disconnect)?;
    append_item(&mut menu, IDM_LOGOFF, TextKey::Logoff)?;
    Ok(menu)
}

pub fn build_main_menu(resource_id: u16, processor_count: usize) -> MenuResult<MenuBar> {
    let menu = match resource_id {
        IDR_MAINMENU_TASK => build_task_main_menu()?,
        IDR_MAINMENU_PROC => build_process_main_menu()?,
        IDR_MAINMENU_PERF => build_perf_main_menu()?,
        IDR_MAINMENU_CPU => build_cpu_main_menu()?,
        IDR_MAINMENU_GPU => build_gpu_main_menu()?,
        IDR_MAINMENU_NET => build_network_main_menu()?,
        IDR_MAINMENU_USER => build_users_main_menu()?,
        _ => return Err(ERROR_RESOURCE_DATA_NOT_FOUND),
    };
    sanitize_task_manager_menu(menu.as_raw(), processor_count);
    Ok(menu)
}

pub fn build_popup_menu(resource_id: u16) -> MenuResult<PopupMenu> {
    match resource_id {
        IDR_TASK_CONTEXT => build_task_context_popup(),
        IDR_TASKVIEW => build_task_view_popup(),
        IDR_USER_CONTEXT => build_user_context_popup(),
        IDR_TRAYMENU => build_tray_popup(),
        _ => Err(ERROR_RESOURCE_DATA_NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuItemInfoW, MENUITEMINFOW, MFT_SEPARATOR, MIIM_FTYPE,
    };

    #[test]
    fn options_menu_only_uses_explicit_separator_entries() {
        let menu = build_options_menu(&[
            (IDM_ALWAYSONTOP, TextKey::AlwaysOnTop),
            (0, TextKey::TaskManager),
            (IDM_CONFIRMATIONS, TextKey::Confirmations),
        ])
        .expect("menu creation should succeed");

        unsafe {
            assert_eq!(GetMenuItemCount(menu.as_raw()), 3);
            for (position, should_be_separator) in [false, true, false].into_iter().enumerate() {
                let mut info = zeroed::<MENUITEMINFOW>();
                info.cbSize = size_of::<MENUITEMINFOW>() as u32;
                info.fMask = MIIM_FTYPE;
                assert_ne!(
                    GetMenuItemInfoW(menu.as_raw(), position as u32, 1, &mut info),
                    0
                );
                assert_eq!(info.fType & MFT_SEPARATOR != 0, should_be_separator);
            }
        }
    }
}
