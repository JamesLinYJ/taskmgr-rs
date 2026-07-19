// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 系统采样模块入口
//
//   文件:       src/system/mod.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 系统级采样、处理器拓扑和稳定进程身份。

pub(crate) mod cpu_sampler;
pub(crate) mod cpu_topology;
pub(crate) mod process_identity;
pub(crate) mod sampler;
