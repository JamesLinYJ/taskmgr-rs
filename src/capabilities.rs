// +-------------------------------------------------------------------------
//
//   taskmgr-rs - Hardware capability report command
//
//   File:       src/capabilities.rs
//
//   Date:       2026-07-31
//   Environment: Windows 10 Pro Dev (Build 29634.1000) x86_64; Rust 1.97.0 (MSVC target)
//   Author:     OpenAI Codex
// --------------------------------------------------------------------------

//! Implements the non-interactive compatibility-report command.
//!
//! The report asks the same Windows interfaces used by the application and preserves native error
//! domains and codes. Output is written through the diagnostic subsystem's handle-anchored,
//! exclusive, atomic attachment path; an existing destination is never overwritten.

use std::env;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_GEN_FAILURE, ERROR_NOT_SUPPORTED, ERROR_PROC_NOT_FOUND,
    GetLastError,
};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount, GetCurrentProcess,
};

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::pages::gpu;
use crate::system::cpu_sets::{CpuSetError, CpuSetTopology, query_process_default_cpu_sets};

const COMMAND_PREFIX: &str = "--diagnostic-capabilities=";
const COMMAND_NAME: &str = "--diagnostic-capabilities";
const REPORT_SCHEMA_VERSION: u16 = 1;

enum CapabilityCommand {
    NotRequested,
    Export(PathBuf),
    Invalid(String),
}

pub(crate) fn run_from_environment() -> Option<i32> {
    let destination = match parse_arguments(env::args_os()) {
        CapabilityCommand::NotRequested => return None,
        CapabilityCommand::Export(destination) => destination,
        CapabilityCommand::Invalid(reason) => {
            diagnostics::event(
                Level::Error,
                "capabilities.command_invalid",
                "capabilities",
                "capability report command is invalid",
                &[Field::text("reason", reason)],
            );
            let _ = diagnostics::flush();
            return Some(2);
        }
    };

    let result = build_report()
        .and_then(|report| serde_json::to_vec_pretty(&report).map_err(|error| error.to_string()))
        .and_then(|mut bytes| {
            bytes.push(b'\n');
            diagnostics::write_secure_attachment(&destination, &bytes)
                .map_err(|error| error.to_string())
        });
    match result {
        Ok(()) => {
            diagnostics::event(
                Level::Info,
                "capabilities.report_exported",
                "capabilities",
                "hardware capability report exported",
                &[Field::sensitive_text(
                    "destination",
                    destination.to_string_lossy(),
                )],
            );
            let _ = diagnostics::flush();
            Some(0)
        }
        Err(error) => {
            diagnostics::event(
                Level::Error,
                "capabilities.report_failed",
                "capabilities",
                "hardware capability report export failed",
                &[
                    Field::sensitive_text("destination", destination.to_string_lossy()),
                    Field::text("error", error),
                ],
            );
            let _ = diagnostics::flush();
            Some(2)
        }
    }
}

fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> CapabilityCommand {
    let mut destination = None;
    for argument in arguments.into_iter().skip(1) {
        if argument == OsStr::new(COMMAND_NAME) {
            return CapabilityCommand::Invalid(format!("{COMMAND_NAME} requires =<absolute-path>"));
        }
        let Some(value) = strip_wide_prefix(&argument, COMMAND_PREFIX) else {
            continue;
        };
        if value.is_empty() {
            return CapabilityCommand::Invalid(
                "capability report destination cannot be empty".to_string(),
            );
        }
        if destination.is_some() {
            return CapabilityCommand::Invalid(
                "capability report destination was specified more than once".to_string(),
            );
        }
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return CapabilityCommand::Invalid(
                "capability report destination must be an absolute path".to_string(),
            );
        }
        destination = Some(path);
    }
    destination.map_or(CapabilityCommand::NotRequested, CapabilityCommand::Export)
}

fn strip_wide_prefix(value: &OsStr, prefix: &str) -> Option<OsString> {
    let value = value.encode_wide().collect::<Vec<_>>();
    let prefix = OsStr::new(prefix).encode_wide().collect::<Vec<_>>();
    value
        .strip_prefix(prefix.as_slice())
        .map(OsString::from_wide)
}

fn build_report() -> Result<Value, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?;
    Ok(json!({
        "schema_version": REPORT_SCHEMA_VERSION,
        "generated_unix_time": {
            "seconds": now.as_secs(),
            "nanoseconds": now.subsec_nanos(),
        },
        "application": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "target_arch": env::consts::ARCH,
            "target_os": env::consts::OS,
            "target_abi": if cfg!(target_env = "msvc") {
                "msvc"
            } else if cfg!(target_env = "gnu") {
                "gnu"
            } else {
                "unknown"
            },
            "pointer_width": usize::BITS,
        },
        "environment": diagnostics::runtime_environment_manifest(),
        "cpu": cpu_capability_report(),
        "gpu": gpu::diagnostic_capability_report(),
        "result_legend": {
            "supported": "the capability entry point was available and returned valid data",
            "unsupported": "Windows or the installed driver reported that the capability is absent",
            "error": "the capability should not be treated as supported because its query failed",
        },
        "privacy_notice": "This file includes CPU topology, GPU model identifiers, and display-driver versions. It contains no process list and is never uploaded automatically.",
    }))
}

fn cpu_capability_report() -> Value {
    let processor_groups = active_processor_groups();
    let process = unsafe { GetCurrentProcess() };
    let cpu_sets = match CpuSetTopology::query(process) {
        Ok(topology) => {
            let default_sets = unsafe { query_process_default_cpu_sets(process) };
            json!({
                "status": "supported",
                "groups": topology.groups().iter().map(|group| json!({
                    "number": group.number,
                    "logical_processor_count": group.processor_mask.count_ones(),
                    "assignable_processor_count": group.assignable_mask.count_ones(),
                    "processor_mask": format!("0x{:X}", group.processor_mask),
                    "assignable_mask": format!("0x{:X}", group.assignable_mask),
                    "sets": group.cpu_sets().iter().map(|cpu_set| json!({
                        "id": cpu_set.id,
                        "group": cpu_set.group,
                        "logical_processor": cpu_set.logical_processor,
                        "assignable": cpu_set.assignable,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "current_process_default": match default_sets {
                    Ok(ids) => json!({
                        "status": "supported",
                        "unrestricted": ids.is_empty(),
                        "cpu_set_ids": ids,
                    }),
                    Err(error) => cpu_set_error_capability(error),
                },
            })
        }
        Err(error) => cpu_set_error_capability(error),
    };
    json!({
        "processor_groups": processor_groups,
        "cpu_sets": cpu_sets,
    })
}

fn active_processor_groups() -> Value {
    let count = unsafe { GetActiveProcessorGroupCount() };
    if count == 0 {
        let error = unsafe { GetLastError() };
        return json!({
            "status": "error",
            "error": {
                "domain": "win32",
                "code": if error == 0 { ERROR_GEN_FAILURE } else { error },
                "context": "GetActiveProcessorGroupCount",
            },
        });
    }
    let groups = (0..count)
        .map(|number| {
            let processor_count = unsafe { GetActiveProcessorCount(number) };
            json!({
                "number": number,
                "active_logical_processor_count": processor_count,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": "supported",
        "group_count": count,
        "active_logical_processor_count": groups
            .iter()
            .filter_map(|group| group["active_logical_processor_count"].as_u64())
            .sum::<u64>(),
        "groups": groups,
    })
}

fn cpu_set_error_capability(error: CpuSetError) -> Value {
    let code = error.win32_code();
    json!({
        "status": if matches!(
            code,
            ERROR_NOT_SUPPORTED | ERROR_CALL_NOT_IMPLEMENTED | ERROR_PROC_NOT_FOUND
        ) {
            "unsupported"
        } else {
            "error"
        },
        "error": {
            "domain": "win32",
            "code": code,
            "code_hex": format!("0x{code:08X}"),
            "context": error.context(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{CapabilityCommand, parse_arguments};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn parse(arguments: &[&str]) -> CapabilityCommand {
        parse_arguments(arguments.iter().map(OsString::from))
    }

    #[test]
    fn unrelated_arguments_do_not_request_a_report() {
        assert!(matches!(
            parse(&["taskmgr.exe", "--diagnostic=trace"]),
            CapabilityCommand::NotRequested
        ));
    }

    #[test]
    fn report_destination_preserves_an_absolute_unicode_path() {
        let CapabilityCommand::Export(path) = parse(&[
            "taskmgr.exe",
            r"--diagnostic-capabilities=C:\reports\兼容能力.json",
        ]) else {
            panic!("valid capability report argument should be accepted");
        };
        assert_eq!(path, PathBuf::from(r"C:\reports\兼容能力.json"));
    }

    #[test]
    fn missing_relative_and_duplicate_destinations_are_rejected() {
        assert!(matches!(
            parse(&["taskmgr.exe", "--diagnostic-capabilities"]),
            CapabilityCommand::Invalid(_)
        ));
        assert!(matches!(
            parse(&["taskmgr.exe", "--diagnostic-capabilities=capabilities.json"]),
            CapabilityCommand::Invalid(_)
        ));
        assert!(matches!(
            parse(&[
                "taskmgr.exe",
                r"--diagnostic-capabilities=C:\one.json",
                r"--diagnostic-capabilities=C:\two.json",
            ]),
            CapabilityCommand::Invalid(_)
        ));
    }
}
