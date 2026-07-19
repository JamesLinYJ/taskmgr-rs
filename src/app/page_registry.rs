// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 页面身份与静态描述注册表
//
//   文件:       src/app/page_registry.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 定义页面持久身份和不随运行时变化的 UI 元数据。
//!
//! `PageId` 的数值写入现有 `Options` 二进制配置，因此不得重排。运行时页面数组、
//! 标签顺序和页面描述均使用 `PageId::ALL`，避免用多份裸整数表维持隐式一致性。

use crate::ui::localization::TextKey;
use crate::ui::resource_ids::{
    IDC_GPU_SELECTOR, IDC_NICTOTALS, IDC_TASKLIST, IDC_USERLIST, IDD_CPUPAGE, IDD_GPUPAGE,
    IDD_NETPAGE, IDD_PERFPAGE, IDD_PROCPAGE, IDD_TASKPAGE, IDD_USERSPAGE, IDR_MAINMENU_CPU,
    IDR_MAINMENU_GPU, IDR_MAINMENU_NET, IDR_MAINMENU_PERF, IDR_MAINMENU_PROC, IDR_MAINMENU_TASK,
    IDR_MAINMENU_USER,
};

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PageId {
    Applications = 0,
    Processes = 1,
    Performance = 2,
    Cpu = 3,
    Gpu = 4,
    Network = 5,
    Users = 6,
}

impl PageId {
    pub(crate) const ALL: [Self; 7] = [
        Self::Applications,
        Self::Processes,
        Self::Performance,
        Self::Cpu,
        Self::Gpu,
        Self::Network,
        Self::Users,
    ];
    pub(crate) const COUNT: usize = Self::ALL.len();

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub(crate) const fn persisted(self) -> i32 {
        self as i32
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub(crate) fn from_persisted(value: i32) -> Option<Self> {
        usize::try_from(value).ok().and_then(Self::from_index)
    }

    pub(crate) const fn descriptor(self) -> &'static PageDescriptor {
        &PAGE_DESCRIPTORS[self.index()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageFocus {
    None,
    Tabs,
    Control(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MinimumSizePolicy {
    CompactWhenBorderless,
    AlwaysNormal,
}

pub(crate) struct PageDescriptor {
    pub(crate) id: PageId,
    pub(crate) title_key: TextKey,
    pub(crate) dialog_id: u16,
    pub(crate) menu_id: u16,
    pub(crate) initial_focus: PageFocus,
    pub(crate) minimum_size: MinimumSizePolicy,
}

pub(crate) const PAGE_DESCRIPTORS: [PageDescriptor; PageId::COUNT] = [
    PageDescriptor {
        id: PageId::Applications,
        title_key: TextKey::ApplicationsPageTitle,
        dialog_id: IDD_TASKPAGE,
        menu_id: IDR_MAINMENU_TASK,
        initial_focus: PageFocus::Control(IDC_TASKLIST),
        minimum_size: MinimumSizePolicy::CompactWhenBorderless,
    },
    PageDescriptor {
        id: PageId::Processes,
        title_key: TextKey::ProcessesPageTitle,
        dialog_id: IDD_PROCPAGE,
        menu_id: IDR_MAINMENU_PROC,
        initial_focus: PageFocus::Tabs,
        minimum_size: MinimumSizePolicy::CompactWhenBorderless,
    },
    PageDescriptor {
        id: PageId::Performance,
        title_key: TextKey::PerformancePageTitle,
        dialog_id: IDD_PERFPAGE,
        menu_id: IDR_MAINMENU_PERF,
        initial_focus: PageFocus::None,
        minimum_size: MinimumSizePolicy::CompactWhenBorderless,
    },
    PageDescriptor {
        id: PageId::Cpu,
        title_key: TextKey::CpuPageTitle,
        dialog_id: IDD_CPUPAGE,
        menu_id: IDR_MAINMENU_CPU,
        initial_focus: PageFocus::None,
        minimum_size: MinimumSizePolicy::AlwaysNormal,
    },
    PageDescriptor {
        id: PageId::Gpu,
        title_key: TextKey::GpuPageTitle,
        dialog_id: IDD_GPUPAGE,
        menu_id: IDR_MAINMENU_GPU,
        initial_focus: PageFocus::Control(IDC_GPU_SELECTOR),
        minimum_size: MinimumSizePolicy::AlwaysNormal,
    },
    PageDescriptor {
        id: PageId::Network,
        title_key: TextKey::NetworkingPageTitle,
        dialog_id: IDD_NETPAGE,
        menu_id: IDR_MAINMENU_NET,
        initial_focus: PageFocus::Control(IDC_NICTOTALS),
        minimum_size: MinimumSizePolicy::CompactWhenBorderless,
    },
    PageDescriptor {
        id: PageId::Users,
        title_key: TextKey::UsersPageTitle,
        dialog_id: IDD_USERSPAGE,
        menu_id: IDR_MAINMENU_USER,
        initial_focus: PageFocus::Control(IDC_USERLIST),
        minimum_size: MinimumSizePolicy::CompactWhenBorderless,
    },
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::ui::dialogs::supports_page_dialog;
    use crate::ui::menus::supports_main_menu;

    #[test]
    fn persisted_page_values_and_descriptor_order_are_stable() {
        for (index, page_id) in PageId::ALL.into_iter().enumerate() {
            assert_eq!(page_id.index(), index);
            assert_eq!(page_id.persisted(), index as i32);
            assert_eq!(PageId::from_index(index), Some(page_id));
            assert_eq!(PageId::from_persisted(index as i32), Some(page_id));
            assert_eq!(page_id.descriptor().id, page_id);
        }
        assert_eq!(PageId::from_persisted(-1), None);
        assert_eq!(PageId::from_index(PageId::COUNT), None);
    }

    #[test]
    fn page_dialogs_and_menus_are_unique_within_their_resource_types() {
        let dialog_ids: HashSet<_> = PAGE_DESCRIPTORS
            .iter()
            .map(|descriptor| descriptor.dialog_id)
            .collect();
        let menu_ids: HashSet<_> = PAGE_DESCRIPTORS
            .iter()
            .map(|descriptor| descriptor.menu_id)
            .collect();

        assert_eq!(dialog_ids.len(), PageId::COUNT);
        assert_eq!(menu_ids.len(), PageId::COUNT);
        assert!(
            PAGE_DESCRIPTORS
                .iter()
                .all(|descriptor| supports_page_dialog(descriptor.dialog_id))
        );
        assert!(
            PAGE_DESCRIPTORS
                .iter()
                .all(|descriptor| supports_main_menu(descriptor.menu_id))
        );
    }
}
