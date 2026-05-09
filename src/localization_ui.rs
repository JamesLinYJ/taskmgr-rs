// 界面本地化辅助模块。
// 这里负责在对话框资源创建后，按当前语言把可见文本替换成对应翻译。

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetDlgItem, SetDlgItemTextW, SetWindowTextW, IDCANCEL, IDOK,
};

use crate::language::{text, TextKey};
use crate::resource::*;
use crate::winutil::to_wide_null;

pub fn localize_dialog(hwnd: HWND, dialog_id: u16) {
    // 对话框本地化按资源 ID 分发，确保同一个模板在不同语言下仍复用相同控件编号。
    if hwnd.is_null() {
        return;
    }

    match dialog_id {
        IDD_TASKPAGE => {
            set_dialog_item_text(hwnd, IDC_SWITCHTO, TextKey::SwitchTo);
            set_dialog_item_text(hwnd, IDC_ENDTASK, TextKey::EndTask);
            set_dialog_item_text(hwnd, i32::from(IDM_RUN), TextKey::NewTaskButton);
        }
        IDD_PROCPAGE => {
            set_dialog_item_text(hwnd, IDC_TERMINATE, TextKey::EndProcess);
        }
        IDD_NETPAGE => {
            set_dialog_item_text(hwnd, IDC_NOADAPTERS, TextKey::NoActiveNetworkAdaptersFound);
        }
        IDD_USERSPAGE => {
            set_dialog_item_text(hwnd, i32::from(IDM_DISCONNECT), TextKey::Disconnect);
            set_dialog_item_text(hwnd, i32::from(IDM_LOGOFF), TextKey::Logoff);
            set_dialog_item_text(hwnd, i32::from(IDM_SENDMESSAGE), TextKey::SendMessage);
        }
        IDD_PERFPAGE => {
            set_dialog_item_text(hwnd, IDC_STATIC14, TextKey::Handles);
            set_dialog_item_text(hwnd, IDC_STATIC15, TextKey::Threads);
            set_dialog_item_text(hwnd, IDC_STATIC16, TextKey::ProcessesLabel);
            set_dialog_item_text(hwnd, IDC_STATIC2, TextKey::Total);
            set_dialog_item_text(hwnd, IDC_STATIC3, TextKey::Available);
            set_dialog_item_text(hwnd, IDC_STATIC4, TextKey::FileCache);
            set_dialog_item_text(hwnd, IDC_STATIC6, TextKey::Total);
            set_dialog_item_text(hwnd, IDC_STATIC8, TextKey::Limit);
            set_dialog_item_text(hwnd, IDC_STATIC9, TextKey::Peak);
            set_dialog_item_text(hwnd, IDC_STATIC11, TextKey::Total);
            set_dialog_item_text(hwnd, IDC_STATIC12, TextKey::Paged);
            set_dialog_item_text(hwnd, IDC_STATIC17, TextKey::Nonpaged);
            set_control_text(dlg_item(hwnd, IDC_CPUFRAME), TextKey::CpuUsageHistory);
            set_control_text(dlg_item(hwnd, IDC_CPUUSAGEFRAME), TextKey::CpuUsage);
            set_control_text(dlg_item(hwnd, IDC_MEMBARFRAME), TextKey::MemUsage);
            set_control_text(dlg_item(hwnd, IDC_MEMFRAME), TextKey::MemoryUsageHistory);
            set_control_text(dlg_item(hwnd, IDC_STATIC1), TextKey::PhysicalMemoryK);
            set_control_text(dlg_item(hwnd, IDC_STATIC5), TextKey::CommitChargeK);
            set_control_text(dlg_item(hwnd, IDC_STATIC10), TextKey::KernelMemoryK);
            set_control_text(dlg_item(hwnd, IDC_STATIC13), TextKey::Totals);
        }
        IDD_SELECTPROCCOLS => {
            set_window_text(hwnd, TextKey::SelectColumnsTitle);
            set_dialog_item_text(hwnd, IDOK, TextKey::Ok);
            set_dialog_item_text(hwnd, IDCANCEL, TextKey::Cancel);
            set_dialog_item_text(
                hwnd,
                IDC_SELECTPROCCOLS_DESC,
                TextKey::SelectProcessColumnsDescription,
            );
            set_dialog_item_text(hwnd, IDC_IMAGENAME, TextKey::ImageName);
            set_dialog_item_text(hwnd, IDC_PID, TextKey::PidProcessIdentifier);
            set_dialog_item_text(hwnd, IDC_USERNAME, TextKey::UserName);
            set_dialog_item_text(hwnd, IDC_SESSIONID, TextKey::SessionId);
            set_dialog_item_text(hwnd, IDC_CPU, TextKey::CpuUsage);
            set_dialog_item_text(hwnd, IDC_CPUTIME, TextKey::CpuTime);
            set_dialog_item_text(hwnd, IDC_MEMUSAGE, TextKey::MemoryUsage);
            set_dialog_item_text(hwnd, IDC_MEMUSAGEDIFF, TextKey::MemoryUsageDelta);
            set_dialog_item_text(hwnd, IDC_PAGEFAULTS, TextKey::PageFaults);
            set_dialog_item_text(hwnd, IDC_PAGEFAULTSDIFF, TextKey::PageFaultsDelta);
            set_dialog_item_text(hwnd, IDC_COMMITCHARGE, TextKey::VirtualMemorySize);
            set_dialog_item_text(hwnd, IDC_PAGEDPOOL, TextKey::PagedPool);
            set_dialog_item_text(hwnd, IDC_NONPAGEDPOOL, TextKey::NonPagedPool);
            set_dialog_item_text(hwnd, IDC_BASEPRIORITY, TextKey::BasePriority);
            set_dialog_item_text(hwnd, IDC_HANDLECOUNT, TextKey::HandleCount);
            set_dialog_item_text(hwnd, IDC_THREADCOUNT, TextKey::ThreadCount);
        }
        IDD_AFFINITY => {
            set_window_text(hwnd, TextKey::ProcessorAffinity);
            set_dialog_item_text(hwnd, IDOK, TextKey::Ok);
            set_dialog_item_text(hwnd, IDCANCEL, TextKey::Cancel);
            set_dialog_item_text(hwnd, IDC_AFFINITY_GROUP, TextKey::Processors);
            set_dialog_item_text(
                hwnd,
                IDC_AFFINITY_DESC,
                TextKey::ProcessorAffinityDescription,
            );
        }
        IDD_MESSAGE => {
            set_window_text(hwnd, TextKey::SendMessageTitle);
            set_dialog_item_text(hwnd, IDOK, TextKey::Ok);
            set_dialog_item_text(hwnd, IDCANCEL, TextKey::Cancel);
            set_dialog_item_text(hwnd, IDC_MESSAGE_TITLE_LABEL, TextKey::MessageTitleLabel);
            set_dialog_item_text(hwnd, IDC_MESSAGE_BODY_LABEL, TextKey::MessageLabel);
        }
        _ => {}
    }
}

fn dlg_item(hwnd: HWND, control_id: i32) -> HWND {
    // 安全性: lookup only borrows the dialog handle; failure is represented as a null HWND.
    unsafe { GetDlgItem(hwnd, control_id) }
}

fn set_window_text(hwnd: HWND, text_key: TextKey) {
    // 窗口标题和普通控件文本共用同一套 `TextKey -> UTF-16` 转换。
    let wide = to_wide_null(text(text_key));
    // 安全性: `hwnd` is supplied by Win32 dialog creation and `wide` lives for the call.
    unsafe { SetWindowTextW(hwnd, wide.as_ptr()) };
}

fn set_dialog_item_text(hwnd: HWND, control_id: i32, text_key: TextKey) {
    // 按控件 ID 设置文本，适合按钮、标签和输入框标题等标准对话框子控件。
    let wide = to_wide_null(text(text_key));
    // 安全性: `hwnd` is a dialog window and `wide` lives for the call.
    unsafe { SetDlgItemTextW(hwnd, control_id, wide.as_ptr()) };
}

fn set_control_text(hwnd: HWND, text_key: TextKey) {
    // 有些自定义控件只能先拿到 `HWND` 再设文本，所以单独保留这个辅助函数。
    if hwnd.is_null() {
        return;
    }
    let wide = to_wide_null(text(text_key));
    // 安全性: `hwnd` is checked non-null and `wide` lives for the call.
    unsafe { SetWindowTextW(hwnd, wide.as_ptr()) };
}
