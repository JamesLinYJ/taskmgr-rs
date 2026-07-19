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
mod config;
mod infrastructure;
mod pages;
mod system;
mod ui;

fn main() {
    // `app::run` 内部负责 Win32 初始化，并返回最终的进程退出码。
    std::process::exit(app::run());
}
