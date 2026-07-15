//! 程序入口文件。
//! 这里本身不承载业务逻辑，只负责组织模块并把启动流程委托给 `app::run`。

#![windows_subsystem = "windows"]

mod app;
mod app_controllers;
mod assets;
mod background_worker;
mod chart_renderer;
mod cpu_sampler;
mod dialog_templates;
mod drawing;
mod language;
mod menus;
mod netpage;
mod options;
mod pages;
mod perf_drawing;
mod perf_layout;
mod perfpage;
mod procpage;
#[allow(dead_code)]
mod resource;
mod runtime_menu;
mod taskpage;
mod userpage;
mod winutil;

fn main() {
    // `app::run` 内部负责 Win32 初始化，并返回最终的进程退出码。
    std::process::exit(app::run());
}
