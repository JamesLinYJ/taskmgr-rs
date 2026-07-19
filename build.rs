// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 构建流程组合入口
//
//   文件:       build.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 构建期组合入口。
//!
//! 本文件只安排本地化生成和 Windows 资源编译的顺序；具体校验与生成逻辑分别由
//! `build_support` 中的模块拥有。

#[path = "build_support/icon_pipeline.rs"]
mod icon_pipeline;
#[path = "build_support/localization.rs"]
mod localization;
#[allow(dead_code)]
#[path = "src/ui/resource_ids.rs"]
mod resource;
#[path = "build_support/resources.rs"]
mod resources;
#[path = "build_support/text_key_parser.rs"]
mod text_key_parser;

fn main() {
    localization::generate();
    resources::compile();
}
