// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 本地化键解析
//
//   文件:       build_support/text_key_parser.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

use std::path::Path;

pub fn parse_text_keys_from_source(source: &str, path: &Path) -> Vec<String> {
    let syntax = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    let text_key_enum = syntax
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Enum(item_enum) if item_enum.ident == "TextKey" => Some(item_enum),
            _ => None,
        })
        .unwrap_or_else(|| panic!("failed to find TextKey enum in {}", path.display()));

    let keys = text_key_enum
        .variants
        .iter()
        .map(|variant| variant.ident.to_string())
        .collect::<Vec<_>>();
    if keys.is_empty() {
        panic!("failed to parse TextKey variants from {}", path.display());
    }
    keys
}
