// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 本地化入口
//
//   文件:       src/ui/localization/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

use std::sync::OnceLock;

// 语言入口模块。
// 其它模块只通过这里按资源 ID 或文本键取字符串，不必关心当前语言表
// 实际来自哪个语言文件。

use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

#[path = "terms.rs"]
mod language_terms;
#[path = "ui.rs"]
mod language_ui;
#[path = "text_key.rs"]
mod text_key;

pub use language_terms::{
    adapter_state, network_column_titles, session_state, user_column_titles,
    user_session_column_title,
};
pub use language_ui::localize_dialog;
pub use text_key::TextKey;

const LANG_CHINESE: u16 = 0x04;
const LANG_GERMAN: u16 = 0x07;
const LANG_SPANISH: u16 = 0x0a;
const LANG_FRENCH: u16 = 0x0c;
const LANG_PORTUGUESE: u16 = 0x16;
const LANG_RUSSIAN: u16 = 0x19;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UiLanguage {
    EnUs,
    ZhCn,
    ZhTw,
    Ru,
    De,
    Fr,
    Pt,
    Es,
}

static UI_LANGUAGE: OnceLock<UiLanguage> = OnceLock::new();

include!(concat!(env!("OUT_DIR"), "/localization_generated.rs"));

pub fn current_language() -> UiLanguage {
    // 语言探测只做一次，后续都走缓存，避免每次查字符串都调用系统 API。
    *UI_LANGUAGE.get_or_init(|| {
        // 安全性: `GetUserDefaultUILanguage` is a process-local query with no pointer inputs.
        let lang_id = unsafe { GetUserDefaultUILanguage() };
        let primary = lang_id & 0x03ff;
        let sub = (lang_id >> 10) & 0x003f;
        match primary {
            LANG_CHINESE => match sub {
                0x02 | 0x04 => UiLanguage::ZhCn,
                0x01 | 0x03 | 0x05 => UiLanguage::ZhTw,
                _ => UiLanguage::ZhCn,
            },
            LANG_RUSSIAN => UiLanguage::Ru,
            LANG_GERMAN => UiLanguage::De,
            LANG_FRENCH => UiLanguage::Fr,
            LANG_PORTUGUESE => UiLanguage::Pt,
            LANG_SPANISH => UiLanguage::Es,
            _ => UiLanguage::EnUs,
        }
    })
}

pub fn text(key: TextKey) -> &'static str {
    // 文本键是新的唯一入口；底层静态表由 build.rs 在编译期生成。
    generated_text(current_language(), key)
}

pub fn menu_status_help(command_id: u16) -> Option<&'static str> {
    generated_menu_status_help(current_language(), command_id)
}
