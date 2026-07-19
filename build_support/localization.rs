// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 构建期本地化生成
//
//   文件:       build_support/localization.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 在编译期读取 TOML 语言文件并生成静态本地化查表代码。
//!
//! 生成结果只写入 Cargo 的 `OUT_DIR`。所有语言必须提供完全相同的键集合，缺失、
//! 多余或格式错误的键都会终止构建，避免在运行时退回到另一种语言。

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::text_key_parser::parse_text_keys_from_source;

const LOCALES: &[(&str, &str)] = &[
    ("en_us", "EnUs"),
    ("zh_cn", "ZhCn"),
    ("zh_tw", "ZhTw"),
    ("ru", "Ru"),
    ("de", "De"),
    ("fr", "Fr"),
    ("pt", "Pt"),
    ("es", "Es"),
];

const MENU_STATUS_HELP_KEYS: &[(&str, &str)] = &[
    ("IDM_RUN", "crate::ui::resource_ids::IDM_RUN"),
    ("IDM_EXIT", "crate::ui::resource_ids::IDM_EXIT"),
    (
        "IDM_ALWAYSONTOP",
        "crate::ui::resource_ids::IDM_ALWAYSONTOP",
    ),
    (
        "IDM_MINIMIZEONUSE",
        "crate::ui::resource_ids::IDM_MINIMIZEONUSE",
    ),
    ("IDM_LARGEICONS", "crate::ui::resource_ids::IDM_LARGEICONS"),
    ("IDM_SMALLICONS", "crate::ui::resource_ids::IDM_SMALLICONS"),
    ("IDM_DETAILS", "crate::ui::resource_ids::IDM_DETAILS"),
    ("IDM_ALLCPUS", "crate::ui::resource_ids::IDM_ALLCPUS"),
    ("IDM_MULTIGRAPH", "crate::ui::resource_ids::IDM_MULTIGRAPH"),
    ("IDM_ABOUT", "crate::ui::resource_ids::IDM_ABOUT"),
    ("IDM_HIGH", "crate::ui::resource_ids::IDM_HIGH"),
    ("IDM_NORMAL", "crate::ui::resource_ids::IDM_NORMAL"),
    ("IDM_LOW", "crate::ui::resource_ids::IDM_LOW"),
    ("IDM_PAUSED", "crate::ui::resource_ids::IDM_PAUSED"),
    (
        "IDM_CONFIRMATIONS",
        "crate::ui::resource_ids::IDM_CONFIRMATIONS",
    ),
    ("IDM_PROC_DEBUG", "crate::ui::resource_ids::IDM_PROC_DEBUG"),
    (
        "IDM_PROC_TERMINATE",
        "crate::ui::resource_ids::IDM_PROC_TERMINATE",
    ),
    (
        "IDM_PROC_ENDTREE",
        "crate::ui::resource_ids::IDM_PROC_ENDTREE",
    ),
    ("IDM_HELP", "crate::ui::resource_ids::IDM_HELP"),
    ("IDM_PROCCOLS", "crate::ui::resource_ids::IDM_PROCCOLS"),
    ("IDM_REFRESH", "crate::ui::resource_ids::IDM_REFRESH"),
    ("IDM_AFFINITY", "crate::ui::resource_ids::IDM_AFFINITY"),
    (
        "IDM_KERNELTIMES",
        "crate::ui::resource_ids::IDM_KERNELTIMES",
    ),
    (
        "IDM_TASK_MINIMIZE",
        "crate::ui::resource_ids::IDM_TASK_MINIMIZE",
    ),
    (
        "IDM_TASK_MAXIMIZE",
        "crate::ui::resource_ids::IDM_TASK_MAXIMIZE",
    ),
    (
        "IDM_TASK_CASCADE",
        "crate::ui::resource_ids::IDM_TASK_CASCADE",
    ),
    (
        "IDM_TASK_TILEHORZ",
        "crate::ui::resource_ids::IDM_TASK_TILEHORZ",
    ),
    (
        "IDM_TASK_TILEVERT",
        "crate::ui::resource_ids::IDM_TASK_TILEVERT",
    ),
    (
        "IDM_TASK_BRINGTOFRONT",
        "crate::ui::resource_ids::IDM_TASK_BRINGTOFRONT",
    ),
];

pub(crate) fn generate() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let localization_dir = manifest_dir.join("localization");
    let text_key_path = manifest_dir
        .join("src")
        .join("ui")
        .join("localization")
        .join("text_key.rs");
    let output_path =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("localization_generated.rs");

    println!("cargo:rerun-if-changed={}", text_key_path.display());
    for (locale_name, _) in LOCALES {
        println!(
            "cargo:rerun-if-changed={}",
            localization_dir
                .join(format!("{locale_name}.toml"))
                .display()
        );
    }

    let text_keys = parse_text_keys(&text_key_path);
    let menu_keys = MENU_STATUS_HELP_KEYS
        .iter()
        .map(|(key, _)| (*key).to_string())
        .collect::<Vec<_>>();

    let mut localized_text = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut localized_menu_help = BTreeMap::<String, BTreeMap<String, String>>::new();

    for (locale_name, _) in LOCALES {
        let locale_path = localization_dir.join(format!("{locale_name}.toml"));
        let source = fs::read_to_string(&locale_path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", locale_path.display()));
        let table = toml::from_str::<toml::Table>(&source)
            .unwrap_or_else(|error| panic!("invalid TOML in {}: {error}", locale_path.display()));

        let text = read_string_table(
            &table,
            "text",
            &text_keys,
            &locale_path,
            RejectStyle::TextKey,
        );
        let menu_status_help = read_string_table(
            &table,
            "menu_status_help",
            &menu_keys,
            &locale_path,
            RejectStyle::CommandKey,
        );

        localized_text.insert((*locale_name).to_string(), text);
        localized_menu_help.insert((*locale_name).to_string(), menu_status_help);
    }

    let generated =
        render_generated_localization(&text_keys, &localized_text, &localized_menu_help);
    fs::write(&output_path, generated)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output_path.display()));
}

fn parse_text_keys(path: &Path) -> Vec<String> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    parse_text_keys_from_source(&source, path)
}

#[derive(Clone, Copy)]
enum RejectStyle {
    TextKey,
    CommandKey,
}

fn read_string_table(
    root: &toml::map::Map<String, toml::Value>,
    table_name: &str,
    expected_keys: &[String],
    path: &Path,
    reject_style: RejectStyle,
) -> BTreeMap<String, String> {
    let table = root
        .get(table_name)
        .and_then(toml::Value::as_table)
        .unwrap_or_else(|| panic!("{} is missing [{table_name}]", path.display()));

    let expected = expected_keys.iter().cloned().collect::<BTreeSet<_>>();
    let actual = table.keys().cloned().collect::<BTreeSet<_>>();

    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        panic!(
            "{} is missing keys in [{table_name}]: {}",
            path.display(),
            missing.join(", ")
        );
    }

    let unknown = actual.difference(&expected).cloned().collect::<Vec<_>>();
    if !unknown.is_empty() {
        panic!(
            "{} contains unknown keys in [{table_name}]: {}",
            path.display(),
            unknown.join(", ")
        );
    }

    let mut output = BTreeMap::new();
    for key in expected_keys {
        reject_non_symbolic_key(key, path, table_name, reject_style);
        let value = table
            .get(key)
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "{} key [{table_name}].{key} must be a string",
                    path.display()
                )
            });
        output.insert(key.clone(), value.to_string());
    }
    output
}

fn reject_non_symbolic_key(key: &str, path: &Path, table_name: &str, reject_style: RejectStyle) {
    if key.chars().all(|ch| ch.is_ascii_digit()) || key.starts_with("IDS_") {
        panic!(
            "{} contains forbidden numeric/resource-style key [{table_name}].{}",
            path.display(),
            key
        );
    }

    if matches!(reject_style, RejectStyle::CommandKey)
        && !key.starts_with("IDM_")
        && !key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch == '_' || ch.is_ascii_digit())
    {
        panic!(
            "{} contains invalid command-style key [{table_name}].{}",
            path.display(),
            key
        );
    }
}

fn render_generated_localization(
    text_keys: &[String],
    localized_text: &BTreeMap<String, BTreeMap<String, String>>,
    localized_menu_help: &BTreeMap<String, BTreeMap<String, String>>,
) -> String {
    let mut output = String::new();
    output.push_str("// @generated by build.rs\n");
    writeln!(output, "const LOCALE_COUNT: usize = {};", LOCALES.len()).unwrap();
    writeln!(output, "const TEXT_KEY_COUNT: usize = {};", text_keys.len()).unwrap();
    writeln!(
        output,
        "const MENU_STATUS_HELP_COUNT: usize = {};",
        MENU_STATUS_HELP_KEYS.len()
    )
    .unwrap();
    output.push('\n');

    output.push_str("fn language_index(language: UiLanguage) -> usize {\n");
    output.push_str("    match language {\n");
    for (index, (_, ui_variant)) in LOCALES.iter().enumerate() {
        writeln!(output, "        UiLanguage::{ui_variant} => {index},").unwrap();
    }
    output.push_str("    }\n");
    output.push_str("}\n\n");

    output.push_str("static TEXTS: [[&str; TEXT_KEY_COUNT]; LOCALE_COUNT] = [\n");
    for (locale_name, _) in LOCALES {
        let text_map = localized_text
            .get(*locale_name)
            .unwrap_or_else(|| panic!("missing generated text locale: {locale_name}"));
        output.push_str("    [\n");
        for key in text_keys {
            let value = text_map
                .get(key)
                .unwrap_or_else(|| panic!("missing generated text value: {locale_name}.{key}"));
            writeln!(output, "        {:?},", value).unwrap();
        }
        output.push_str("    ],\n");
    }
    output.push_str("];\n\n");

    output
        .push_str("static MENU_STATUS_HELP: [[&str; LOCALE_COUNT]; MENU_STATUS_HELP_COUNT] = [\n");
    for (key, _) in MENU_STATUS_HELP_KEYS {
        output.push_str("    [\n");
        for (locale_name, _) in LOCALES {
            let help_map = localized_menu_help
                .get(*locale_name)
                .unwrap_or_else(|| panic!("missing generated menu-help locale: {locale_name}"));
            let value = help_map.get(*key).unwrap_or_else(|| {
                panic!("missing generated menu-help value: {locale_name}.{key}")
            });
            writeln!(output, "        {:?},", value).unwrap();
        }
        output.push_str("    ],\n");
    }
    output.push_str("];\n\n");

    output.push_str(
        "pub(crate) fn generated_text(language: UiLanguage, key: TextKey) -> &'static str {\n",
    );
    output.push_str("    TEXTS[language_index(language)][key as usize]\n");
    output.push_str("}\n\n");

    output.push_str(
        "pub(crate) fn generated_menu_status_help(language: UiLanguage, command_id: u16) -> Option<&'static str> {\n",
    );
    output.push_str("    let help_index = match command_id {\n");
    for (index, (_, command_expr)) in MENU_STATUS_HELP_KEYS.iter().enumerate() {
        writeln!(output, "        {command_expr} => {index},").unwrap();
    }
    output.push_str("        _ => return None,\n");
    output.push_str("    };\n");
    output.push_str("    Some(MENU_STATUS_HELP[help_index][language_index(language)])\n");
    output.push_str("}\n");

    output
}
