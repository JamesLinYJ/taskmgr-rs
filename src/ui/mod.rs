// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 共享 UI 基础模块入口
//
//   文件:       src/ui/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 页面共享的资源、图表、对话框、菜单和本地化基础设施。

pub(crate) mod assets;
pub(crate) mod charts;
pub(crate) mod dialogs;
pub(crate) mod drawing;
pub(crate) mod localization;
pub(crate) mod menus;
#[allow(dead_code)]
pub(crate) mod resource_ids;
pub(crate) mod runtime_menu;
