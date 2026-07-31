// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 程序入口
//
//   文件:       src/main.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 程序入口文件。
//! 这里本身不承载业务逻辑，只负责组织模块并把启动流程委托给 `app::run`。

#![windows_subsystem = "windows"]

mod app;
mod capabilities;
mod config;
mod infrastructure;
mod pages;
mod system;
mod ui;

fn main() {
    // 诊断必须先于权限申请和 Win32 UI 初始化，才能覆盖完整启动链路。
    infrastructure::diagnostics::initialize_from_env();
    if let Some(exit_code) = capabilities::run_from_environment() {
        infrastructure::diagnostics::shutdown();
        std::process::exit(exit_code);
    }
    let exit_code = app::run();
    infrastructure::diagnostics::shutdown();
    std::process::exit(exit_code);
}
