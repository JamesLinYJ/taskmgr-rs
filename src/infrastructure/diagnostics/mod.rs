// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 结构化诊断日志
//
//   文件:       src/infrastructure/diagnostics/mod.rs
//
//   日期:       2026年07月27日
//   环境:       Fedora Linux 45 x86_64；Linux 内核 7.2.0-0.rc4.260725g0ce37745d4bf.39.fc45.x86_64；Rust 1.97.1；MinGW GCC 16.1.1；Wine 11.14 (Staging)
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 进程级结构化诊断运行时。
//!
//! 调用线程只构造一条拥有所有数据的记录并尝试放入有界通道。单独的长期写入线程
//! 负责 JSONL 序列化、轮转和刷新；错误/警告使用保留通道，普通队列拥塞时丢弃并
//! 形成可见计数。诊断级别、敏感字段和 minidump 都只存在于当前进程会话。

mod archive;
mod crash;
mod secure_fs;

use std::cell::Cell;
use std::env;
use std::ffi::{OsStr, OsString};
#[cfg(test)]
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased, tick};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{ERROR_GEN_FAILURE, GetLastError, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::SystemInformation::{GetSystemTime, OSVERSIONINFOW};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, IsWow64Process,
};

use self::archive::StoredZipWriter;
use self::secure_fs::{SecureDirectory, mark_delete, random_bytes, rename_within_directory};
use crate::infrastructure::native::process_is_elevated;

const LOG_SCHEMA_VERSION: u16 = 1;
const BUNDLE_SCHEMA_VERSION: u16 = 1;
const REGULAR_QUEUE_CAPACITY: usize = 8_192;
const PRIORITY_QUEUE_CAPACITY: usize = 256;
const CONTROL_QUEUE_CAPACITY: usize = 16;
const LOG_PART_LIMIT_BYTES: u64 = 10 * 1024 * 1024;
const LOG_ROOT_LIMIT_BYTES: u64 = 200 * 1024 * 1024;
const LOG_SESSION_LIMIT: usize = 10;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const SAMPLING_FAULT_ENVIRONMENT: &str = "TASKMGR_RS_DIAGNOSTIC_INJECT_SAMPLING_ERROR";

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();

thread_local! {
    static CURRENT_OPERATION_ID: Cell<Option<u64>> = const { Cell::new(None) };
}

struct OperationContextGuard {
    previous: Option<u64>,
}

impl Drop for OperationContextGuard {
    fn drop(&mut self) {
        CURRENT_OPERATION_ID.set(self.previous);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Error,
            2 => Self::Warn,
            4 => Self::Debug,
            5 => Self::Trace,
            _ => Self::Info,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiagnosticConfig {
    level: Level,
    sensitive: bool,
    minidump: bool,
    root_override: Option<PathBuf>,
    session_id: String,
    parse_warnings: Vec<String>,
}

impl DiagnosticConfig {
    fn from_environment() -> Self {
        Self::parse(env::args_os().skip(1))
    }

    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Self {
        let mut level = Level::Info;
        let mut sensitive = false;
        let mut minidump = false;
        let mut root_override = None;
        let mut requested_session = None;
        let mut warnings = Vec::new();

        for argument in arguments {
            let value = argument.to_string_lossy();
            match value.as_ref() {
                "--diagnostic" => level = Level::Trace,
                "--diagnostic-sensitive" => {
                    sensitive = true;
                    level = Level::Trace;
                }
                "--diagnostic-minidump" => {
                    minidump = true;
                    level = Level::Trace;
                }
                _ if value.starts_with("--diagnostic=") => {
                    match value.trim_start_matches("--diagnostic=") {
                        "debug" => level = Level::Debug,
                        "trace" => level = Level::Trace,
                        invalid => warnings.push(format!(
                            "ignored invalid diagnostic level {invalid:?}; expected debug or trace"
                        )),
                    }
                }
                _ if value.starts_with("--diagnostic-dir=") => {
                    let path = PathBuf::from(value.trim_start_matches("--diagnostic-dir="));
                    if path.is_absolute() {
                        root_override = Some(path);
                    } else {
                        warnings.push(
                            "ignored relative --diagnostic-dir path; an absolute path is required"
                                .to_string(),
                        );
                    }
                }
                _ if value.starts_with("--diagnostic-session=") => {
                    let candidate = value.trim_start_matches("--diagnostic-session=");
                    if valid_session_id(candidate) {
                        requested_session = Some(candidate.to_string());
                    } else {
                        warnings.push(
                            "ignored invalid internal diagnostic session identifier".to_string(),
                        );
                    }
                }
                _ => {}
            }
        }

        Self {
            level,
            sensitive,
            minidump,
            root_override,
            session_id: requested_session.unwrap_or_else(generate_session_id),
            parse_warnings: warnings,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum FieldValue {
    Text(String),
    Unsigned(u64),
    Signed(i64),
    Boolean(bool),
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    name: &'static str,
    value: FieldValue,
    sensitive: bool,
}

impl Field {
    pub(crate) fn text(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: FieldValue::Text(value.into()),
            sensitive: false,
        }
    }

    pub(crate) fn sensitive_text(name: &'static str, value: impl Into<String>) -> Self {
        Self {
            name,
            value: FieldValue::Text(value.into()),
            sensitive: true,
        }
    }

    pub(crate) const fn unsigned(name: &'static str, value: u64) -> Self {
        Self {
            name,
            value: FieldValue::Unsigned(value),
            sensitive: false,
        }
    }

    pub(crate) const fn signed(name: &'static str, value: i64) -> Self {
        Self {
            name,
            value: FieldValue::Signed(value),
            sensitive: false,
        }
    }

    pub(crate) const fn boolean(name: &'static str, value: bool) -> Self {
        Self {
            name,
            value: FieldValue::Boolean(value),
            sensitive: false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticStatus {
    pub(crate) level: Level,
    pub(crate) sensitive: bool,
    pub(crate) minidump: bool,
    pub(crate) session_id: String,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) file_active: bool,
    pub(crate) sink_error: Option<String>,
    pub(crate) dropped_events: u64,
}

#[derive(Serialize)]
struct SourceLocation {
    file: String,
    line: u32,
}

#[derive(Serialize)]
struct LogRecord {
    schema_version: u16,
    timestamp_utc: String,
    timestamp_unix_ms: u64,
    monotonic_ms: u64,
    sequence: u64,
    session_id: String,
    pid: u32,
    tid: u32,
    thread_name: Option<String>,
    level: &'static str,
    target: String,
    event: String,
    message: String,
    source: SourceLocation,
    operation_id: Option<u64>,
    duration_ms: Option<u64>,
    privacy: &'static str,
    fields: Map<String, Value>,
}

struct SharedState {
    started: Instant,
    sequence: AtomicU64,
    operation_sequence: AtomicU64,
    level: AtomicU8,
    sensitive: AtomicBool,
    sensitive_ever_enabled: AtomicBool,
    minidump: AtomicBool,
    dropped_regular: AtomicU64,
    dropped_priority: AtomicU64,
    dropped_total: AtomicU64,
    file_active: AtomicBool,
    shutdown: AtomicBool,
    session_id: String,
    root_directory: Option<PathBuf>,
    session_directory: Option<PathBuf>,
    sink_error: Mutex<Option<String>>,
}

impl SharedState {
    fn allows(&self, level: Level) -> bool {
        level as u8 <= self.level.load(Ordering::Relaxed)
    }
}

struct Diagnostics {
    shared: Arc<SharedState>,
    regular_sender: Sender<LogRecord>,
    priority_sender: Sender<LogRecord>,
    control_sender: Sender<WriterControl>,
    writer_thread: Mutex<Option<JoinHandle<()>>>,
    sampling_fault: SamplingFaultInjection,
}

enum WriterControl {
    Flush(Sender<Result<(), String>>),
    Shutdown(Sender<()>),
}

struct SamplingFaultInjection {
    requested: AtomicBool,
    armed: AtomicBool,
}

impl SamplingFaultInjection {
    fn new(requested: bool) -> Self {
        Self {
            requested: AtomicBool::new(requested),
            armed: AtomicBool::new(false),
        }
    }

    fn arm_once(&self) -> bool {
        if !self.requested.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.armed.store(true, Ordering::Release);
        true
    }

    fn take(&self) -> bool {
        self.armed.swap(false, Ordering::AcqRel)
    }
}

struct FileSink {
    file: File,
    root_directory: SecureDirectory,
    session_directory: SecureDirectory,
    high_integrity: bool,
    process_id: u32,
    part: u32,
    bytes_written: u64,
}

pub(crate) fn initialize_from_env() {
    if DIAGNOSTICS.get().is_some() {
        return;
    }
    let config = DiagnosticConfig::from_environment();
    let parse_warnings = config.parse_warnings.clone();
    let sampling_fault_requested = sampling_fault_requested(config.level);
    let diagnostics = Diagnostics::new(config, sampling_fault_requested);
    let session_directory = diagnostics.shared.session_directory.clone();
    if DIAGNOSTICS.set(diagnostics).is_err() {
        return;
    }

    if let Some(directory) = session_directory
        && let Err(error) = crash::install(&directory, status().minidump)
    {
        set_sink_error(format!("native crash diagnostics unavailable: {error}"));
    }
    install_panic_hook();

    event(
        Level::Info,
        "diagnostics.session_started",
        "diagnostics",
        "diagnostic session started",
        &[
            Field::text("app_version", env!("CARGO_PKG_VERSION")),
            Field::text("target_arch", env::consts::ARCH),
            Field::text("target_os", env::consts::OS),
            Field::text(
                "target_env",
                if cfg!(target_env = "gnu") {
                    "gnu"
                } else if cfg!(target_env = "msvc") {
                    "msvc"
                } else {
                    "unknown"
                },
            ),
            Field::unsigned("pointer_width", usize::BITS.into()),
        ],
    );
    record_runtime_environment();
    if let Some(error) = status().sink_error {
        event(
            Level::Warn,
            "diagnostics.storage_degraded",
            "diagnostics",
            &error,
            &[Field::boolean("file_logging_active", status().file_active)],
        );
    }
    for warning in parse_warnings {
        event(
            Level::Warn,
            "diagnostics.argument_ignored",
            "diagnostics",
            &warning,
            &[],
        );
    }
    if crash::test_crash_requested(enabled(Level::Debug), status().minidump) {
        event(
            Level::Warn,
            "diagnostics.test_crash_triggered",
            "diagnostics",
            "controlled access violation requested for crash diagnostics validation",
            &[Field::text("fault", "access-violation")],
        );
        let _ = flush();
        crash::trigger_test_access_violation();
    }
}

impl Diagnostics {
    fn new(mut config: DiagnosticConfig, sampling_fault_requested: bool) -> Self {
        let mut startup_errors = Vec::new();
        let (root_directory, session_directory) =
            prepare_directories(&mut config, &mut startup_errors);
        let file_sink = session_directory.as_ref().and_then(|session| {
            let root = root_directory.as_ref()?;
            match FileSink::open(root.clone(), session.clone(), process::id()) {
                Ok(sink) => Some(sink),
                Err(error) => {
                    startup_errors.push(format!("unable to open diagnostic log: {error}"));
                    None
                }
            }
        });

        let shared = Arc::new(SharedState {
            started: Instant::now(),
            sequence: AtomicU64::new(0),
            operation_sequence: AtomicU64::new(u64::from(process::id()) << 32),
            level: AtomicU8::new(config.level as u8),
            sensitive: AtomicBool::new(config.sensitive),
            sensitive_ever_enabled: AtomicBool::new(config.sensitive),
            minidump: AtomicBool::new(config.minidump),
            dropped_regular: AtomicU64::new(0),
            dropped_priority: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            file_active: AtomicBool::new(file_sink.is_some()),
            shutdown: AtomicBool::new(false),
            session_id: config.session_id,
            root_directory,
            session_directory,
            sink_error: Mutex::new(startup_errors.first().cloned()),
        });

        let (regular_sender, regular_receiver) = bounded(REGULAR_QUEUE_CAPACITY);
        let (priority_sender, priority_receiver) = bounded(PRIORITY_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = bounded(CONTROL_QUEUE_CAPACITY);
        let writer_shared = Arc::clone(&shared);
        let writer_thread = match thread::Builder::new()
            .name("taskmgr-rs-diagnostics".to_string())
            .spawn(move || {
                writer_loop(
                    writer_shared,
                    file_sink,
                    regular_receiver,
                    priority_receiver,
                    control_receiver,
                )
            }) {
            Ok(thread) => Some(thread),
            Err(error) => {
                shared.file_active.store(false, Ordering::Release);
                if let Ok(mut sink_error) = shared.sink_error.lock() {
                    *sink_error = Some(format!("unable to start diagnostic writer: {error}"));
                }
                None
            }
        };

        Self {
            shared,
            regular_sender,
            priority_sender,
            control_sender,
            writer_thread: Mutex::new(writer_thread),
            sampling_fault: SamplingFaultInjection::new(sampling_fault_requested),
        }
    }

    fn enqueue(&self, record: LogRecord, level: Level) {
        if self.shared.shutdown.load(Ordering::Acquire) {
            return;
        }
        let (sender, dropped) = if level <= Level::Warn {
            (&self.priority_sender, &self.shared.dropped_priority)
        } else {
            (&self.regular_sender, &self.shared.dropped_regular)
        };
        match sender.try_send(record) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                dropped.fetch_add(1, Ordering::Relaxed);
                self.shared.dropped_total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn flush(&self, timeout: Duration) -> Result<(), String> {
        let (sender, receiver) = bounded(1);
        self.control_sender
            .send_timeout(WriterControl::Flush(sender), timeout)
            .map_err(|_| "diagnostic writer did not accept a flush request".to_string())?;
        receiver
            .recv_timeout(timeout)
            .map_err(|_| "diagnostic writer flush timed out".to_string())?
    }
}

pub(crate) fn enabled(level: Level) -> bool {
    DIAGNOSTICS
        .get()
        .is_some_and(|runtime| runtime.shared.allows(level))
}

pub(crate) fn next_operation_id() -> u64 {
    DIAGNOSTICS
        .get()
        .map(|runtime| {
            runtime
                .shared
                .operation_sequence
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1)
        })
        .unwrap_or(0)
}

pub(crate) fn with_operation_id<T>(operation_id: u64, action: impl FnOnce() -> T) -> T {
    let previous = CURRENT_OPERATION_ID.replace(Some(operation_id));
    let _guard = OperationContextGuard { previous };
    action()
}

pub(crate) fn without_operation_id<T>(action: impl FnOnce() -> T) -> T {
    let previous = CURRENT_OPERATION_ID.replace(None);
    let _guard = OperationContextGuard { previous };
    action()
}

pub(crate) fn current_operation_id() -> Option<u64> {
    CURRENT_OPERATION_ID.get()
}

/// Arms a single controlled sampler failure on the next forced refresh.
///
/// This is deliberately available only through a developer environment variable and only when
/// detailed logging was already enabled at process startup. It never changes normal sampling.
pub(crate) fn arm_requested_sampling_fault() -> bool {
    let armed = DIAGNOSTICS
        .get()
        .is_some_and(|runtime| runtime.sampling_fault.arm_once());
    if armed {
        event(
            Level::Warn,
            "diagnostics.sampling_fault_armed",
            "diagnostics",
            "controlled sampling failure armed for the next system sample",
            &[Field::text("fault", "ntstatus-unsuccessful-once")],
        );
    }
    armed
}

pub(crate) fn take_requested_sampling_fault() -> bool {
    DIAGNOSTICS
        .get()
        .is_some_and(|runtime| runtime.sampling_fault.take())
}

#[track_caller]
pub(crate) fn event(
    level: Level,
    event_name: &'static str,
    target: &str,
    message: &str,
    fields: &[Field],
) {
    event_with(level, event_name, target, message, None, None, fields);
}

#[track_caller]
pub(crate) fn event_with(
    level: Level,
    event_name: &'static str,
    target: &str,
    message: &str,
    operation_id: Option<u64>,
    duration_ms: Option<u64>,
    fields: &[Field],
) {
    let location = Location::caller();
    let operation_id = operation_id.or_else(|| CURRENT_OPERATION_ID.get());
    let Some(runtime) = DIAGNOSTICS.get() else {
        debug_output(&format!(
            "taskmgr-rs [{}] {event_name}: {message}\r\n",
            level.as_str()
        ));
        return;
    };
    if !runtime.shared.allows(level) {
        return;
    }

    let record = build_record(
        &runtime.shared,
        level,
        event_name,
        target,
        message,
        operation_id,
        duration_ms,
        fields,
        location.file(),
        location.line(),
    );
    if level <= Level::Warn {
        debug_output(&format!(
            "taskmgr-rs [{}] {event_name}: {message}\r\n",
            level.as_str()
        ));
    }
    runtime.enqueue(record, level);
}

#[allow(clippy::too_many_arguments)]
fn build_record(
    shared: &SharedState,
    level: Level,
    event_name: &str,
    target: &str,
    message: &str,
    operation_id: Option<u64>,
    duration_ms: Option<u64>,
    fields: &[Field],
    source_file: &str,
    source_line: u32,
) -> LogRecord {
    let sensitive = shared.sensitive.load(Ordering::Relaxed);
    let mut serialized_fields = Map::new();
    for field in fields {
        if field.sensitive && !sensitive {
            continue;
        }
        let value = match &field.value {
            FieldValue::Text(value) => Value::String(value.clone()),
            FieldValue::Unsigned(value) => Value::from(*value),
            FieldValue::Signed(value) => Value::from(*value),
            FieldValue::Boolean(value) => Value::from(*value),
        };
        serialized_fields.insert(field.name.to_string(), value);
    }

    let (timestamp_utc, timestamp_unix_ms) = utc_timestamp();
    let monotonic_ms = u64::try_from(shared.started.elapsed().as_millis()).unwrap_or(u64::MAX);
    LogRecord {
        schema_version: LOG_SCHEMA_VERSION,
        timestamp_utc,
        timestamp_unix_ms,
        monotonic_ms,
        // The writer assigns the persisted sequence immediately before serialization. Keeping
        // producer-side timestamps here preserves occurrence time without allowing the priority
        // lane to make the JSONL sequence run backwards.
        sequence: 0,
        session_id: shared.session_id.clone(),
        pid: process::id(),
        // Safety: this is a value-only query for the calling thread.
        tid: unsafe { GetCurrentThreadId() },
        thread_name: thread::current().name().map(ToOwned::to_owned),
        level: level.as_str(),
        target: target.to_string(),
        event: event_name.to_string(),
        message: message.to_string(),
        source: SourceLocation {
            file: normalized_source_path(source_file),
            line: source_line,
        },
        operation_id,
        duration_ms,
        privacy: if sensitive { "sensitive" } else { "redacted" },
        fields: serialized_fields,
    }
}

fn writer_loop(
    shared: Arc<SharedState>,
    mut sink: Option<FileSink>,
    regular_receiver: Receiver<LogRecord>,
    priority_receiver: Receiver<LogRecord>,
    control_receiver: Receiver<WriterControl>,
) {
    let ticker = tick(FLUSH_INTERVAL);
    loop {
        select_biased! {
            recv(control_receiver) -> command => {
                match command {
                    Ok(WriterControl::Flush(reply)) => {
                        for record in priority_receiver.try_iter() {
                            write_record(&shared, &mut sink, record);
                        }
                        for record in regular_receiver.try_iter() {
                            write_record(&shared, &mut sink, record);
                        }
                        report_dropped_events(&shared, &mut sink);
                        let _ = reply.send(flush_sink(&shared, &mut sink));
                    }
                    Ok(WriterControl::Shutdown(reply)) => {
                        for record in priority_receiver.try_iter() {
                            write_record(&shared, &mut sink, record);
                        }
                        for record in regular_receiver.try_iter() {
                            write_record(&shared, &mut sink, record);
                        }
                        report_dropped_events(&shared, &mut sink);
                        let _ = flush_sink(&shared, &mut sink);
                        let _ = reply.send(());
                        break;
                    }
                    Err(_) => break,
                }
            }
            recv(priority_receiver) -> record => {
                if let Ok(record) = record {
                    write_record(&shared, &mut sink, record);
                    let _ = flush_sink(&shared, &mut sink);
                }
            }
            recv(regular_receiver) -> record => {
                if let Ok(record) = record {
                    write_record(&shared, &mut sink, record);
                }
            }
            recv(ticker) -> _ => {
                report_dropped_events(&shared, &mut sink);
                let _ = flush_sink(&shared, &mut sink);
            }
        }
    }
    shared.file_active.store(false, Ordering::Release);
}

fn write_record(shared: &SharedState, sink: &mut Option<FileSink>, mut record: LogRecord) {
    let Some(active_sink) = sink.as_mut() else {
        return;
    };
    record.sequence = shared
        .sequence
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let mut bytes = match serde_json::to_vec(&record) {
        Ok(bytes) => bytes,
        Err(error) => {
            disable_sink(
                shared,
                sink,
                format!("unable to serialize diagnostic event: {error}"),
            );
            return;
        }
    };
    bytes.push(b'\n');
    if let Err(error) = active_sink.write_line(&bytes) {
        disable_sink(
            shared,
            sink,
            format!("unable to write diagnostic log: {error}"),
        );
    }
}

fn report_dropped_events(shared: &SharedState, sink: &mut Option<FileSink>) {
    let regular = shared.dropped_regular.swap(0, Ordering::AcqRel);
    let priority = shared.dropped_priority.swap(0, Ordering::AcqRel);
    if regular == 0 && priority == 0 {
        return;
    }
    let record = build_record(
        shared,
        Level::Warn,
        "diagnostics.events_dropped",
        "diagnostics",
        "diagnostic events were dropped because a bounded queue was unavailable",
        None,
        None,
        &[
            Field::unsigned("regular", regular),
            Field::unsigned("priority", priority),
        ],
        "src/infrastructure/diagnostics/mod.rs",
        line!(),
    );
    write_record(shared, sink, record);
    debug_output(&format!(
        "taskmgr-rs [warn] diagnostics.events_dropped: regular={regular}, priority={priority}\r\n"
    ));
}

fn flush_sink(shared: &SharedState, sink: &mut Option<FileSink>) -> Result<(), String> {
    let Some(active_sink) = sink.as_mut() else {
        return shared
            .sink_error
            .lock()
            .ok()
            .and_then(|error| error.clone())
            .map_or(Ok(()), Err);
    };
    if let Err(error) = active_sink
        .file
        .flush()
        .and_then(|_| active_sink.file.sync_data())
    {
        let message = format!("unable to flush diagnostic log: {error}");
        disable_sink(shared, sink, message.clone());
        return Err(message);
    }
    Ok(())
}

fn disable_sink(shared: &SharedState, sink: &mut Option<FileSink>, message: String) {
    debug_output(&format!("taskmgr-rs [error] {message}\r\n"));
    *sink = None;
    shared.file_active.store(false, Ordering::Release);
    if let Ok(mut error) = shared.sink_error.lock() {
        *error = Some(message);
    }
}

impl FileSink {
    fn open(
        root_directory: PathBuf,
        session_directory: PathBuf,
        process_id: u32,
    ) -> io::Result<Self> {
        if session_directory.parent() != Some(root_directory.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic session is not a direct child of its root",
            ));
        }
        let root = SecureDirectory::open_absolute(&root_directory)?;
        let session_name = session_directory.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session name is missing")
        })?;
        let session = root.open_directory(session_name)?;
        let high_integrity = process_is_elevated().unwrap_or(false);
        let part = 0;
        let file = session.create_file(
            OsStr::new(&part_file_name(process_id, part)),
            FILE_SHARE_READ,
            high_integrity,
        )?;
        Ok(Self {
            file,
            root_directory: root,
            session_directory: session,
            high_integrity,
            process_id,
            part,
            bytes_written: 0,
        })
    }

    fn write_line(&mut self, bytes: &[u8]) -> io::Result<()> {
        let incoming = u64::try_from(bytes.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "diagnostic event is too large")
        })?;
        if incoming > LOG_PART_LIMIT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "one diagnostic event exceeds the 10 MiB part limit",
            ));
        }
        if self.bytes_written != 0
            && self.bytes_written.saturating_add(incoming) > LOG_PART_LIMIT_BYTES
        {
            self.rotate()?;
        }
        self.file.write_all(bytes)?;
        self.bytes_written = self.bytes_written.saturating_add(incoming);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()?;
        let root_budget_before_next_part =
            LOG_ROOT_LIMIT_BYTES.saturating_sub(LOG_PART_LIMIT_BYTES);
        cleanup_retention_in(
            &self.root_directory,
            self.session_directory.path().file_name(),
            root_budget_before_next_part,
        )
        .map_err(|error| io::Error::new(error.kind(), format!("rotation retention: {error}")))?;
        if directory_size_in(&self.root_directory)
            .map_err(|error| io::Error::new(error.kind(), format!("rotation sizing: {error}")))?
            > root_budget_before_next_part
        {
            return Err(io::Error::other(
                "the diagnostic log root cannot reserve another part within the 200 MiB limit",
            ));
        }
        self.part = self
            .part
            .checked_add(1)
            .ok_or_else(|| io::Error::other("diagnostic log part number overflow"))?;
        self.file = self
            .session_directory
            .create_file(
                OsStr::new(&part_file_name(self.process_id, self.part)),
                FILE_SHARE_READ,
                self.high_integrity,
            )
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("creating rotated diagnostic part: {error}"),
                )
            })?;
        self.bytes_written = 0;
        Ok(())
    }
}

pub(crate) fn status() -> DiagnosticStatus {
    let Some(runtime) = DIAGNOSTICS.get() else {
        return DiagnosticStatus {
            level: Level::Info,
            sensitive: false,
            minidump: false,
            session_id: "not-initialized".to_string(),
            directory: None,
            file_active: false,
            sink_error: Some("diagnostics are not initialized".to_string()),
            dropped_events: 0,
        };
    };
    DiagnosticStatus {
        level: Level::from_u8(runtime.shared.level.load(Ordering::Relaxed)),
        sensitive: runtime.shared.sensitive.load(Ordering::Relaxed),
        minidump: runtime.shared.minidump.load(Ordering::Relaxed),
        session_id: runtime.shared.session_id.clone(),
        directory: runtime.shared.session_directory.clone(),
        file_active: runtime.shared.file_active.load(Ordering::Acquire),
        sink_error: runtime
            .shared
            .sink_error
            .lock()
            .ok()
            .and_then(|error| error.clone()),
        dropped_events: runtime.shared.dropped_total.load(Ordering::Relaxed),
    }
}

pub(crate) fn set_detailed(enabled: bool) {
    if let Some(runtime) = DIAGNOSTICS.get() {
        runtime.shared.level.store(
            if enabled {
                Level::Trace as u8
            } else {
                Level::Info as u8
            },
            Ordering::Release,
        );
        event(
            Level::Info,
            "diagnostics.level_changed",
            "diagnostics",
            if enabled {
                "detailed diagnostic logging enabled"
            } else {
                "detailed diagnostic logging disabled"
            },
            &[Field::text("level", if enabled { "trace" } else { "info" })],
        );
    }
}

pub(crate) fn set_sensitive(enabled: bool) {
    if let Some(runtime) = DIAGNOSTICS.get() {
        runtime.shared.sensitive.store(enabled, Ordering::Release);
        if enabled {
            runtime
                .shared
                .sensitive_ever_enabled
                .store(true, Ordering::Release);
            runtime
                .shared
                .level
                .store(Level::Trace as u8, Ordering::Release);
        }
        event(
            Level::Warn,
            "diagnostics.privacy_changed",
            "diagnostics",
            if enabled {
                "sensitive diagnostic fields enabled for future events"
            } else {
                "sensitive diagnostic fields disabled"
            },
            &[Field::boolean("sensitive", enabled)],
        );
    }
}

pub(crate) fn set_minidump(enabled: bool) {
    if let Some(runtime) = DIAGNOSTICS.get() {
        runtime.shared.minidump.store(enabled, Ordering::Release);
        if enabled {
            runtime
                .shared
                .level
                .store(Level::Trace as u8, Ordering::Release);
        }
        crash::set_minidump_enabled(enabled);
        event(
            Level::Warn,
            "diagnostics.minidump_changed",
            "diagnostics",
            if enabled {
                "minidump capture enabled for this process"
            } else {
                "minidump capture disabled"
            },
            &[Field::boolean("minidump", enabled)],
        );
    }
}

pub(crate) fn flush() -> Result<(), String> {
    DIAGNOSTICS
        .get()
        .ok_or_else(|| "diagnostics are not initialized".to_string())?
        .flush(CONTROL_TIMEOUT)
}

pub(crate) fn shutdown() {
    let Some(runtime) = DIAGNOSTICS.get() else {
        return;
    };
    if runtime.shared.shutdown.load(Ordering::Acquire) {
        return;
    }

    event(
        Level::Info,
        "diagnostics.session_stopping",
        "diagnostics",
        "diagnostic session stopping",
        &[],
    );
    if runtime.shared.shutdown.swap(true, Ordering::AcqRel) {
        return;
    }
    let (sender, receiver) = bounded(1);
    let _ = runtime
        .control_sender
        .send_timeout(WriterControl::Shutdown(sender), CONTROL_TIMEOUT);
    let _ = receiver.recv_timeout(CONTROL_TIMEOUT);
    if let Ok(mut thread) = runtime.writer_thread.lock()
        && let Some(thread) = thread.take()
    {
        let _ = thread.join();
    }
}

pub(crate) fn export_bundle(destination: &Path) -> Result<(), String> {
    flush()?;
    let runtime = DIAGNOSTICS
        .get()
        .ok_or_else(|| "diagnostics are not initialized".to_string())?;
    let session_directory = runtime
        .shared
        .session_directory
        .as_ref()
        .ok_or_else(|| "no diagnostic log directory is available".to_string())?;
    let root_directory = runtime
        .shared
        .root_directory
        .as_ref()
        .ok_or_else(|| "no diagnostic log root is available".to_string())?;
    let export_sessions =
        select_export_sessions(root_directory, session_directory).map_err(io_error)?;
    let mut export_files = collect_export_files(&export_sessions).map_err(io_error)?;
    let executable_hash =
        executable_sha256().unwrap_or_else(|error| format!("unavailable: {error}"));
    let current_status = status();
    let sensitive_ever_enabled = runtime
        .shared
        .sensitive_ever_enabled
        .load(Ordering::Acquire);
    let contains_dump = export_files.iter().any(|file| {
        Path::new(&file.file_name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dmp"))
    });
    let contains_crash_record = export_files
        .iter()
        .any(|file| file.file_name.ends_with(".crash.json"));
    let (created_utc, _) = utc_timestamp();
    let environment_manifest = runtime_environment_manifest();
    let manifest = json!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "created_utc": created_utc,
        "application": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "target_arch": env::consts::ARCH,
            "target_env": if cfg!(target_env = "gnu") { "gnu" } else if cfg!(target_env = "msvc") { "msvc" } else { "unknown" },
            "executable_sha256": executable_hash,
        },
        "diagnostics": {
            "session_id": current_status.session_id,
            "level": current_status.level.as_str(),
            "sensitive_fields_enabled": current_status.sensitive,
            "sensitive_fields_ever_enabled": sensitive_ever_enabled,
            "contains_minidump": contains_dump,
            "contains_crash_record": contains_crash_record,
            "dropped_events_total": current_status.dropped_events,
        },
        "environment": environment_manifest,
        "sessions": export_sessions.iter().map(|session| session.name.as_str()).collect::<Vec<_>>(),
        "privacy_notice": "Process memory dumps and logs collected with sensitive mode may contain private information. No data is uploaded automatically."
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    let environment_bytes = serde_json::to_vec_pretty(&runtime_environment_manifest())
        .map_err(|error| error.to_string())?;
    let summary = format!(
        "taskmgr-rs diagnostic bundle\r\n\
         Version: {}\r\n\
         Session: {}\r\n\
         Log level: {}\r\n\
         Sensitive fields ever enabled: {}\r\n\
         Contains minidump: {}\r\n\
         Executable SHA-256: {}\r\n",
        env!("CARGO_PKG_VERSION"),
        current_status.session_id,
        current_status.level.as_str(),
        sensitive_ever_enabled,
        contains_dump,
        executable_hash,
    );

    let (destination_directory, destination_name) =
        open_export_destination(destination).map_err(io_error)?;
    let (mut temporary, temporary_name) =
        create_temporary_bundle(&destination_directory).map_err(io_error)?;
    let result = (|| -> Result<(), String> {
        let mut archive = StoredZipWriter::from_file(&mut temporary);
        archive
            .add_bytes("manifest.json", &manifest_bytes)
            .map_err(io_error)?;
        archive
            .add_bytes("summary.txt", summary.as_bytes())
            .map_err(io_error)?;
        archive
            .add_bytes("environment.json", &environment_bytes)
            .map_err(io_error)?;
        for export in &mut export_files {
            archive
                .add_open_file_prefix(&export.archive_name, &mut export.source, export.length)
                .map_err(io_error)?;
        }
        archive.finish().map_err(io_error)?;
        rename_within_directory(&temporary, &destination_directory, &destination_name, true)
            .map_err(io_error)
    })();
    if result.is_err() {
        let _ = mark_delete(&temporary);
    } else {
        event(
            Level::Info,
            "diagnostics.bundle_exported",
            "diagnostics",
            "diagnostic bundle exported",
            &[
                Field::sensitive_text("destination", destination.to_string_lossy()),
                Field::unsigned("file_count", export_files.len() as u64),
                Field::boolean("contains_minidump", contains_dump),
                Field::text("temporary_name", temporary_name.to_string_lossy()),
            ],
        );
    }
    result
}

/// Atomically writes one user-requested diagnostic attachment without following a pathname after
/// validation. The destination is never overwritten: callers must choose a fresh attachment name.
pub(crate) fn write_secure_attachment(destination: &Path, contents: &[u8]) -> io::Result<()> {
    let (destination_directory, destination_name) = open_export_destination(destination)?;
    let (mut temporary, _) = create_temporary_bundle(&destination_directory)?;
    let result = (|| {
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        rename_within_directory(&temporary, &destination_directory, &destination_name, false)
    })();
    if result.is_err() {
        let _ = mark_delete(&temporary);
    }
    result
}

struct ExportFile {
    source: File,
    file_name: String,
    archive_name: String,
    length: u64,
}

struct ExportSession {
    name: String,
    directory: SecureDirectory,
}

fn select_export_sessions(root: &Path, current: &Path) -> io::Result<Vec<ExportSession>> {
    if current.parent() != Some(root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "current diagnostic session is not a direct child of its root",
        ));
    }
    let current_name = current
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "session name is invalid"))?
        .to_string();
    let root = SecureDirectory::open_absolute(root)?;
    let current_directory = root.open_directory(OsStr::new(&current_name))?;
    let mut sessions = vec![ExportSession {
        name: current_name.clone(),
        directory: current_directory,
    }];
    let mut crashed = Vec::new();
    for entry in root.entries()? {
        let Some(name) = entry
            .name
            .to_str()
            .filter(|name| name.starts_with("session-"))
        else {
            continue;
        };
        if name.eq_ignore_ascii_case(&current_name) {
            continue;
        }
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
            || entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
                == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic export encountered an untrusted session entry",
            ));
        }
        let directory = root.open_directory(&entry.name)?;
        if directory_contains_crash(&directory)? {
            crashed.push((
                entry.last_write_time,
                ExportSession {
                    name: name.to_string(),
                    directory,
                },
            ));
        }
    }
    crashed.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    if let Some((_, previous_crash)) = crashed.into_iter().next() {
        sessions.push(previous_crash);
    }
    Ok(sessions)
}

fn collect_export_files(sessions: &[ExportSession]) -> io::Result<Vec<ExportFile>> {
    let mut files = Vec::new();
    for session in sessions {
        for entry in session.directory.entries()? {
            if entry.attributes
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "diagnostic export refused to follow a reparse point",
                ));
            }
            if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
                != 0
                || !allowed_export_file(&entry.name)
            {
                continue;
            }
            let Some(file_name) = entry.name.to_str().map(ToOwned::to_owned) else {
                continue;
            };
            let source = session.directory.open_file(&entry.name)?;
            let length = source.metadata()?.len();
            files.push(ExportFile {
                source,
                file_name: file_name.clone(),
                archive_name: format!("sessions/{}/{file_name}", session.name),
                length,
            });
        }
    }
    files.sort_by(|left, right| left.archive_name.cmp(&right.archive_name));
    Ok(files)
}

fn allowed_export_file(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("jsonl")
                || extension.eq_ignore_ascii_case("json")
                || extension.eq_ignore_ascii_case("dmp")
        })
}

fn directory_contains_crash(directory: &SecureDirectory) -> io::Result<bool> {
    for entry in directory.entries()? {
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "crash artifact discovery refused a reparse point",
            ));
        }
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
        {
            continue;
        }
        let path = Path::new(&entry.name);
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dmp"))
            || entry
                .name
                .to_str()
                .is_some_and(|name| name.ends_with(".crash.json"))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn export_contains_crash_artifact() -> bool {
    let Some(runtime) = DIAGNOSTICS.get() else {
        return false;
    };
    let (Some(root), Some(current)) = (
        runtime.shared.root_directory.as_ref(),
        runtime.shared.session_directory.as_ref(),
    ) else {
        return false;
    };
    select_export_sessions(root, current).is_ok_and(|sessions| {
        sessions
            .iter()
            .any(|session| directory_contains_crash(&session.directory).unwrap_or(false))
    })
}

pub(crate) fn export_requires_privacy_warning() -> bool {
    DIAGNOSTICS.get().is_some_and(|runtime| {
        runtime
            .shared
            .sensitive_ever_enabled
            .load(Ordering::Acquire)
    }) || export_contains_crash_artifact()
}

pub(crate) fn default_bundle_name() -> String {
    let mut system_time = SYSTEMTIME::default();
    // Safety: `system_time` is a valid writable structure.
    unsafe { GetSystemTime(&mut system_time) };
    format!(
        "taskmgr-rs-diagnostics-{:04}{:02}{:02}-{:02}{:02}{:02}-{}.zip",
        system_time.wYear,
        system_time.wMonth,
        system_time.wDay,
        system_time.wHour,
        system_time.wMinute,
        system_time.wSecond,
        status().session_id
    )
}

pub(crate) fn elevated_relaunch_parameters() -> Vec<u16> {
    let arguments = elevated_relaunch_arguments(env::args_os().skip(1), &status().session_id);
    quote_windows_arguments(&arguments)
}

fn elevated_relaunch_arguments(
    arguments: impl IntoIterator<Item = OsString>,
    session_id: &str,
) -> Vec<OsString> {
    let mut arguments = arguments.into_iter().collect::<Vec<_>>();
    arguments.retain(|argument| {
        !argument
            .to_string_lossy()
            .starts_with("--diagnostic-session=")
    });
    arguments.push(OsString::from(format!("--diagnostic-session={session_id}")));
    arguments
}

pub(crate) fn detailed_restart_parameters() -> Vec<u16> {
    let current = status();
    let arguments = detailed_restart_arguments(
        env::args_os().skip(1),
        &current.session_id,
        current.sensitive,
        current.minidump,
    );
    quote_windows_arguments(&arguments)
}

fn detailed_restart_arguments(
    arguments: impl IntoIterator<Item = OsString>,
    session_id: &str,
    sensitive: bool,
    minidump: bool,
) -> Vec<OsString> {
    let mut arguments = arguments
        .into_iter()
        .filter(|argument| {
            let argument = argument.to_string_lossy();
            argument.starts_with("--diagnostic-dir=") || !is_diagnostic_argument(&argument)
        })
        .collect::<Vec<_>>();
    arguments.push(OsString::from("--diagnostic=trace"));
    if sensitive {
        arguments.push(OsString::from("--diagnostic-sensitive"));
    }
    if minidump {
        arguments.push(OsString::from("--diagnostic-minidump"));
    }
    arguments.push(OsString::from(format!("--diagnostic-session={session_id}")));
    arguments
}

fn is_diagnostic_argument(argument: &str) -> bool {
    argument == "--diagnostic"
        || argument == "--diagnostic-sensitive"
        || argument == "--diagnostic-minidump"
        || argument.starts_with("--diagnostic=")
        || argument.starts_with("--diagnostic-dir=")
        || argument.starts_with("--diagnostic-session=")
}

fn quote_windows_arguments(arguments: &[OsString]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            output.push(b' ' as u16);
        }
        quote_windows_argument(argument, &mut output);
    }
    output.push(0);
    output
}

fn quote_windows_argument(argument: &OsStr, output: &mut Vec<u16>) {
    let value = argument.encode_wide().collect::<Vec<_>>();
    let needs_quotes = value.is_empty()
        || value
            .iter()
            .any(|unit| matches!(*unit, 0x20 | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x22));
    if !needs_quotes {
        output.extend_from_slice(&value);
        return;
    }

    output.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in value {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(unit);
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            output.push(unit);
        }
        backslashes = 0;
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
}

fn prepare_directories(
    config: &mut DiagnosticConfig,
    errors: &mut Vec<String>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    let default_root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("taskmgr-rs").join("logs"));
    let preferred = config.root_override.clone().or(default_root);
    let fallback = env::temp_dir().join("taskmgr-rs").join("logs");
    let candidates = preferred
        .into_iter()
        .chain(std::iter::once(fallback))
        .collect::<Vec<_>>();
    prepare_directories_from_candidates(config, errors, candidates)
}

fn prepare_directories_from_candidates(
    config: &mut DiagnosticConfig,
    errors: &mut Vec<String>,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> (Option<PathBuf>, Option<PathBuf>) {
    for root in candidates {
        let root_handle = match SecureDirectory::open_or_create_absolute(&root) {
            Ok(root) => root,
            Err(error) => {
                errors.push(format!(
                    "unable to securely create diagnostic root {}: {error}",
                    root.display()
                ));
                continue;
            }
        };
        if let Err(error) = cleanup_retention_in(&root_handle, None, LOG_ROOT_LIMIT_BYTES) {
            errors.push(format!(
                "unable to apply diagnostic retention in {}: {error}",
                root.display()
            ));
        }
        let high_integrity = process_is_elevated().unwrap_or(false);
        let mut session_id = config.session_id.clone();
        let mut session_handle = None;
        for _ in 0..16 {
            match root_handle.create_directory(OsStr::new(&session_id), high_integrity) {
                Ok(session) => {
                    session_handle = Some(session);
                    break;
                }
                Err(error) if is_name_collision(&error) => {
                    session_id = generate_session_id();
                }
                Err(error) => {
                    errors.push(format!(
                        "unable to securely create diagnostic session {}: {error}",
                        root.join(&session_id).display()
                    ));
                    break;
                }
            }
        }
        let Some(session_handle) = session_handle else {
            errors.push(format!(
                "unable to allocate an exclusive diagnostic session in {}",
                root.display()
            ));
            continue;
        };
        config.session_id = session_id;
        if let Err(error) = cleanup_retention_in(
            &root_handle,
            session_handle.path().file_name(),
            LOG_ROOT_LIMIT_BYTES.saturating_sub(LOG_PART_LIMIT_BYTES),
        ) {
            errors.push(format!(
                "unable to finalize diagnostic retention in {}: {error}",
                root.display()
            ));
        }
        return (Some(root), Some(session_handle.path().to_path_buf()));
    }
    (None, None)
}

#[cfg(test)]
fn cleanup_retention(root: &Path, active_session: Option<&Path>) -> io::Result<()> {
    cleanup_retention_with_limit(root, active_session, LOG_ROOT_LIMIT_BYTES)
}

#[cfg(test)]
fn cleanup_retention_with_limit(
    root: &Path,
    active_session: Option<&Path>,
    byte_limit: u64,
) -> io::Result<()> {
    if active_session.is_some_and(|active| active.parent() != Some(root)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "active diagnostic session is not a child of the retention root",
        ));
    }
    let root_handle = SecureDirectory::open_absolute(root)?;
    cleanup_retention_in(
        &root_handle,
        active_session.and_then(Path::file_name),
        byte_limit,
    )
}

fn cleanup_retention_in(
    root: &SecureDirectory,
    active_session: Option<&OsStr>,
    byte_limit: u64,
) -> io::Result<()> {
    let mut sessions = Vec::new();
    for entry in root.entries()? {
        let Some(name) = entry
            .name
            .to_str()
            .filter(|name| name.starts_with("session-"))
        else {
            continue;
        };
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
            || entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
                == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic session entry is not a trusted directory",
            ));
        }
        let session = root.open_directory(&entry.name)?;
        let size = directory_size_in(&session)?;
        sessions.push((entry.last_write_time, size, OsString::from(name)));
    }
    sessions.sort_by_key(|entry| entry.0);
    let mut total = sessions.iter().map(|entry| entry.1).sum::<u64>();
    let mut remaining = sessions.len();
    for (_, size, name) in sessions {
        if remaining <= LOG_SESSION_LIMIT && total <= byte_limit {
            break;
        }
        if active_session.is_some_and(|active| os_names_equal(active, &name)) {
            continue;
        }
        delete_directory_child(root, &name)?;
        total = total.saturating_sub(size);
        remaining = remaining.saturating_sub(1);
    }
    Ok(())
}

#[cfg(test)]
fn directory_size(path: &Path) -> io::Result<u64> {
    let directory = SecureDirectory::open_absolute(path)?;
    directory_size_in(&directory)
}

fn directory_size_in(directory: &SecureDirectory) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in directory.entries()? {
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic retention encountered a reparse point",
            ));
        }
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
        {
            let child = directory.open_directory(&entry.name)?;
            total = total.saturating_add(directory_size_in(&child)?);
        } else {
            let file = directory.open_file(&entry.name)?;
            total = total.saturating_add(file.metadata()?.len());
        }
    }
    Ok(total)
}

fn delete_directory_child(parent: &SecureDirectory, name: &OsStr) -> io::Result<()> {
    let directory = parent.open_directory_for_delete(name)?;
    for entry in directory.entries()? {
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "diagnostic retention refused to delete through a reparse point",
            ));
        }
        if entry.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY != 0
        {
            delete_directory_child(&directory, &entry.name)?;
        } else {
            let file = directory.open_file_for_delete(&entry.name)?;
            mark_delete(&file)?;
        }
    }
    directory.delete_empty()
}

fn is_name_collision(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(80) | Some(183))
}

fn os_names_equal(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn part_file_name(process_id: u32, part: u32) -> String {
    format!("taskmgr-{process_id}-{part:04}.jsonl")
}

#[cfg(test)]
fn part_path(session: &Path, process_id: u32, part: u32) -> PathBuf {
    session.join(part_file_name(process_id, part))
}

fn valid_session_id(value: &str) -> bool {
    value.starts_with("session-")
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn generate_session_id() -> String {
    let mut random = [0u8; 16];
    if random_bytes(&mut random).is_ok() {
        let mut session = String::from("session-");
        for byte in random {
            use std::fmt::Write as _;
            write!(session, "{byte:02x}").expect("writing into String cannot fail");
        }
        return session;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("session-{nanos:032x}-{:08x}", process::id())
}

fn utc_timestamp() -> (String, u64) {
    let mut system_time = SYSTEMTIME::default();
    // Safety: `system_time` is a valid writable structure.
    unsafe { GetSystemTime(&mut system_time) };
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    (
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            system_time.wYear,
            system_time.wMonth,
            system_time.wDay,
            system_time.wHour,
            system_time.wMinute,
            system_time.wSecond,
            system_time.wMilliseconds,
        ),
        u64::try_from(unix_ms).unwrap_or(u64::MAX),
    )
}

fn normalized_source_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(index) = normalized.rfind("/src/") {
        return normalized[index + 1..].to_string();
    }
    if let Some(index) = normalized.rfind("/tests/") {
        return normalized[index + 1..].to_string();
    }
    normalized
}

fn debug_output(message: &str) {
    let mut wide = message.encode_utf16().collect::<Vec<_>>();
    wide.push(0);
    // Safety: `wide` is NUL-terminated and remains valid for the synchronous call.
    unsafe { OutputDebugStringW(wide.as_ptr()) };
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        let location = information.location();
        let message = information
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                information
                    .payload()
                    .downcast_ref::<String>()
                    .map(String::as_str)
            })
            .unwrap_or("non-string panic payload");
        let fields = location
            .map(|location| {
                vec![
                    Field::text("panic_file", normalized_source_path(location.file())),
                    Field::unsigned("panic_line", u64::from(location.line())),
                    Field::unsigned("panic_column", u64::from(location.column())),
                ]
            })
            .unwrap_or_default();
        event(Level::Error, "process.panic", "runtime", message, &fields);
        if thread::current().name() != Some("taskmgr-rs-diagnostics") {
            let _ = flush();
        }
        previous(information);
    }));
}

fn set_sink_error(message: String) {
    if let Some(runtime) = DIAGNOSTICS.get()
        && let Ok(mut error) = runtime.shared.sink_error.lock()
    {
        *error = Some(message);
    }
}

fn record_runtime_environment() {
    let mut fields = vec![
        Field::text("runtime", "windows"),
        Field::unsigned(
            "argument_count",
            env::args_os().count().saturating_sub(1) as u64,
        ),
        Field::sensitive_text(
            "command_line",
            env::args_os()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ];
    if let Some((major, minor, build)) = windows_runtime_version() {
        fields.push(Field::unsigned("windows_major", u64::from(major)));
        fields.push(Field::unsigned("windows_minor", u64::from(minor)));
        fields.push(Field::unsigned("windows_build", u64::from(build)));
    }
    match wow64_status() {
        Ok(wow64) => fields.push(Field::boolean("wow64", wow64)),
        Err(error) => fields.push(Field::unsigned("wow64_query_error", u64::from(error))),
    }
    match process_is_elevated() {
        Ok(elevated) => fields.push(Field::boolean("administrator", elevated)),
        Err(error) => fields.push(Field::unsigned(
            "administrator_query_error",
            u64::from(error),
        )),
    }
    if let Ok(executable) = env::current_exe()
        && let Some(process_name) = executable.file_name()
    {
        fields.push(Field::sensitive_text(
            "process_name",
            process_name.to_string_lossy(),
        ));
    }
    if let Some(user_name) = env::var_os("USERNAME") {
        fields.push(Field::sensitive_text(
            "user_name",
            user_name.to_string_lossy(),
        ));
    }
    event(
        Level::Info,
        "runtime.environment",
        "runtime",
        "runtime environment detected",
        &fields,
    );
}

pub(crate) fn runtime_environment_manifest() -> Value {
    let windows = windows_runtime_version();
    let wow64 = wow64_status();
    let administrator = process_is_elevated();
    json!({
        "runtime": "windows",
        "target": {
            "architecture": env::consts::ARCH,
            "os": env::consts::OS,
            "abi": if cfg!(target_env = "gnu") { "gnu" } else if cfg!(target_env = "msvc") { "msvc" } else { "unknown" },
            "pointer_width": usize::BITS,
        },
        "windows": windows.map(|(major, minor, build)| json!({
            "major": major,
            "minor": minor,
            "build": build,
        })),
        "process": {
            "wow64": wow64.as_ref().ok(),
            "wow64_query_error": wow64.err(),
            "administrator": administrator.as_ref().ok(),
            "administrator_query_error": administrator.err(),
        },
    })
}

fn wow64_status() -> Result<bool, u32> {
    let mut wow64 = 0;
    // Safety: GetCurrentProcess returns a process-wide pseudo handle and `wow64` is writable.
    if unsafe { IsWow64Process(GetCurrentProcess(), &mut wow64) } != 0 {
        Ok(wow64 != 0)
    } else {
        let error = unsafe { GetLastError() };
        Err(if error == 0 { ERROR_GEN_FAILURE } else { error })
    }
}

fn windows_runtime_version() -> Option<(u32, u32, u32)> {
    let ntdll = "ntdll.dll\0".encode_utf16().collect::<Vec<_>>();
    // Safety: the module name is NUL-terminated and ntdll is loaded in every process.
    let module = unsafe { GetModuleHandleW(ntdll.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // Safety: the export name is NUL-terminated.
    let procedure = unsafe { GetProcAddress(module, c"RtlGetVersion".as_ptr().cast()) }?;
    type RtlGetVersionFunction = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;
    // Safety: the exact ntdll export has the declared stable signature.
    let rtl_get_version: RtlGetVersionFunction = unsafe { std::mem::transmute(procedure) };
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    // Safety: `version` is correctly sized and writable for the duration of the call.
    if unsafe { rtl_get_version(&mut version) } != 0 {
        return None;
    }
    Some((
        version.dwMajorVersion,
        version.dwMinorVersion,
        version.dwBuildNumber,
    ))
}

fn executable_sha256() -> Result<String, String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let mut file = File::open(executable).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing into String cannot fail");
    }
    Ok(output)
}

fn open_export_destination(destination: &Path) -> io::Result<(SecureDirectory, OsString)> {
    if !destination.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "diagnostic export destination must be absolute",
        ));
    }
    let name = destination
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "destination name is missing"))?
        .to_os_string();
    let parent = destination.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "destination directory is missing",
        )
    })?;
    Ok((SecureDirectory::open_absolute(parent)?, name))
}

fn create_temporary_bundle(directory: &SecureDirectory) -> io::Result<(File, OsString)> {
    for _ in 0..16 {
        let mut random = [0u8; 16];
        random_bytes(&mut random)?;
        let mut name = String::from(".taskmgr-rs-export-");
        for byte in random {
            use std::fmt::Write as _;
            write!(name, "{byte:02x}").expect("writing into String cannot fail");
        }
        name.push_str(".tmp");
        let name = OsString::from(name);
        match directory.create_user_attachment_file(&name, 0) {
            Ok(file) => return Ok((file, name)),
            Err(error) if is_name_collision(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a unique diagnostic export file",
    ))
}

fn io_error(error: io::Error) -> String {
    error.to_string()
}

fn sampling_fault_requested(level: Level) -> bool {
    level >= Level::Debug
        && env::var_os(SAMPLING_FAULT_ENVIRONMENT)
            .is_some_and(|value| value == OsStr::new("ntstatus-once"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::{symlink_dir, symlink_file};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn parse(arguments: &[&str]) -> DiagnosticConfig {
        DiagnosticConfig::parse(arguments.iter().map(OsString::from))
    }

    fn test_shared(sensitive: bool) -> SharedState {
        SharedState {
            started: Instant::now(),
            sequence: AtomicU64::new(0),
            operation_sequence: AtomicU64::new(0),
            level: AtomicU8::new(Level::Info as u8),
            sensitive: AtomicBool::new(sensitive),
            sensitive_ever_enabled: AtomicBool::new(sensitive),
            minidump: AtomicBool::new(false),
            dropped_regular: AtomicU64::new(0),
            dropped_priority: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            file_active: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            session_id: "session-test".to_string(),
            root_directory: None,
            session_directory: None,
            sink_error: Mutex::new(None),
        }
    }

    fn temporary_test_directory(label: &str) -> PathBuf {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        env::temp_dir().join(format!(
            "taskmgr-rs-diagnostics-test-{label}-{}-{sequence}",
            process::id()
        ))
    }

    fn create_test_directory_link(link: &Path, target: &Path) -> bool {
        match symlink_dir(target, link) {
            Ok(()) => true,
            Err(error) if matches!(error.raw_os_error(), Some(5) | Some(1314)) => false,
            Err(error) => panic!("directory symlink fixture should be created: {error}"),
        }
    }

    fn create_test_file_link(link: &Path, target: &Path) -> bool {
        match symlink_file(target, link) {
            Ok(()) => true,
            Err(error) if matches!(error.raw_os_error(), Some(5) | Some(1314)) => false,
            Err(error) => panic!("file symlink fixture should be created: {error}"),
        }
    }

    #[test]
    fn diagnostic_flags_are_one_session_configuration() {
        let config = parse(&[
            "--diagnostic=debug",
            "--diagnostic-sensitive",
            "--diagnostic-minidump",
            "--diagnostic-session=session-test_1",
        ]);
        assert_eq!(config.level, Level::Trace);
        assert!(config.sensitive);
        assert!(config.minidump);
        assert_eq!(config.session_id, "session-test_1");
    }

    #[test]
    fn invalid_diagnostic_values_fall_back_safely() {
        let config = parse(&[
            "--diagnostic=verbose",
            "--diagnostic-dir=relative",
            "--diagnostic-session=../escape",
        ]);
        assert_eq!(config.level, Level::Info);
        assert!(config.root_override.is_none());
        assert!(valid_session_id(&config.session_id));
        assert_eq!(config.parse_warnings.len(), 3);
    }

    #[test]
    fn ordinary_start_does_not_persist_sensitive_or_minidump_choices() {
        let config = parse(&[]);
        assert_eq!(config.level, Level::Info);
        assert!(!config.sensitive);
        assert!(!config.minidump);
    }

    #[test]
    fn sensitive_fields_are_removed_before_serialization() {
        let shared = test_shared(false);
        let record = build_record(
            &shared,
            Level::Info,
            "test.event",
            "test",
            "message",
            None,
            None,
            &[
                Field::text("safe", "visible"),
                Field::sensitive_text("secret", "hidden"),
            ],
            "src/test.rs",
            12,
        );
        assert_eq!(record.fields.get("safe"), Some(&Value::from("visible")));
        assert!(!record.fields.contains_key("secret"));
    }

    #[test]
    fn json_record_escapes_text_and_keeps_schema_and_correlation_fields() {
        let shared = test_shared(true);
        let record = build_record(
            &shared,
            Level::Warn,
            "test.escaped",
            "test",
            "line\n\"quoted\"",
            Some(0x1234),
            Some(87),
            &[Field::sensitive_text("path", "C:\\private\\file")],
            r"C:\build\taskmgr-rs\src\test.rs",
            44,
        );
        let encoded = serde_json::to_string(&record).expect("record should serialize");
        assert!(encoded.contains(r#"line\n\"quoted\""#));
        let value: Value = serde_json::from_str(&encoded).expect("record should be valid JSON");
        assert_eq!(value["schema_version"], LOG_SCHEMA_VERSION);
        assert_eq!(value["operation_id"], 0x1234u64);
        assert_eq!(value["duration_ms"], 87u64);
        assert_eq!(value["source"]["file"], "src/test.rs");
        assert_eq!(value["source"]["line"], 44u64);
        assert_eq!(value["fields"]["path"], r"C:\private\file");
    }

    #[test]
    fn dynamic_level_changes_take_effect_without_restarting_the_writer() {
        let shared = test_shared(false);
        assert!(shared.allows(Level::Info));
        assert!(!shared.allows(Level::Debug));
        shared.level.store(Level::Trace as u8, Ordering::Release);
        assert!(shared.allows(Level::Debug));
        assert!(shared.allows(Level::Trace));
        shared.level.store(Level::Info as u8, Ordering::Release);
        assert!(!shared.allows(Level::Trace));
    }

    #[test]
    fn operation_context_can_be_suspended_and_restores_the_parent() {
        assert_eq!(current_operation_id(), None);
        with_operation_id(10, || {
            assert_eq!(current_operation_id(), Some(10));
            with_operation_id(20, || {
                assert_eq!(current_operation_id(), Some(20));
            });
            assert_eq!(current_operation_id(), Some(10));
            without_operation_id(|| {
                assert_eq!(current_operation_id(), None);
                with_operation_id(30, || {
                    assert_eq!(current_operation_id(), Some(30));
                });
                assert_eq!(current_operation_id(), None);
            });
            assert_eq!(current_operation_id(), Some(10));
        });
        assert_eq!(current_operation_id(), None);
    }

    #[test]
    fn bounded_queue_counts_every_dropped_regular_event() {
        let shared = Arc::new(test_shared(false));
        let (regular_sender, regular_receiver) = bounded(1);
        let (priority_sender, _priority_receiver) = bounded(1);
        let (control_sender, _control_receiver) = bounded(1);
        let runtime = Diagnostics {
            shared: Arc::clone(&shared),
            regular_sender,
            priority_sender,
            control_sender,
            writer_thread: Mutex::new(None),
            sampling_fault: SamplingFaultInjection::new(false),
        };
        for sequence in 0..3 {
            let record = build_record(
                &shared,
                Level::Info,
                "test.queue",
                "test",
                "queue event",
                Some(sequence),
                None,
                &[],
                "src/test.rs",
                1,
            );
            runtime.enqueue(record, Level::Info);
        }
        assert_eq!(regular_receiver.len(), 1);
        assert_eq!(shared.dropped_regular.load(Ordering::Relaxed), 2);
        assert_eq!(shared.dropped_total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn asynchronous_writer_flushes_a_complete_json_line() {
        let root = temporary_test_directory("flush");
        let session_id = "session-flush".to_string();
        let runtime = Diagnostics::new(
            DiagnosticConfig {
                level: Level::Trace,
                sensitive: false,
                minidump: false,
                root_override: Some(root.clone()),
                session_id: session_id.clone(),
                parse_warnings: Vec::new(),
            },
            false,
        );
        let record = build_record(
            &runtime.shared,
            Level::Info,
            "test.flushed",
            "test",
            "flush event",
            None,
            None,
            &[],
            "src/test.rs",
            2,
        );
        runtime.enqueue(record, Level::Info);
        runtime.flush(CONTROL_TIMEOUT).expect("writer should flush");

        let session = root.join(session_id);
        let log = fs::read_dir(&session)
            .expect("session should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .expect("writer should create a JSONL part");
        let contents = fs::read_to_string(log).expect("log should be readable");
        assert!(contents.ends_with('\n'));
        assert!(contents.contains(r#""event":"test.flushed""#));

        let (sender, receiver) = bounded(1);
        runtime
            .control_sender
            .send(WriterControl::Shutdown(sender))
            .expect("writer should accept shutdown");
        receiver
            .recv_timeout(CONTROL_TIMEOUT)
            .expect("writer should acknowledge shutdown");
        if let Some(thread) = runtime
            .writer_thread
            .lock()
            .expect("writer mutex should be available")
            .take()
        {
            thread.join().expect("writer should stop cleanly");
        }
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn persisted_sequence_follows_jsonl_line_order_across_priority_lanes() {
        let root = temporary_test_directory("sequence-order");
        let session_id = "session-sequence-order".to_string();
        let runtime = Diagnostics::new(
            DiagnosticConfig {
                level: Level::Trace,
                sensitive: false,
                minidump: false,
                root_override: Some(root.clone()),
                session_id: session_id.clone(),
                parse_warnings: Vec::new(),
            },
            false,
        );
        for (level, event_name) in [
            (Level::Info, "test.regular_first"),
            (Level::Error, "test.priority"),
            (Level::Debug, "test.regular_last"),
        ] {
            let record = build_record(
                &runtime.shared,
                level,
                event_name,
                "test",
                "sequence event",
                None,
                None,
                &[],
                "src/test.rs",
                3,
            );
            runtime.enqueue(record, level);
        }

        let (sender, receiver) = bounded(1);
        runtime
            .control_sender
            .send(WriterControl::Shutdown(sender))
            .expect("writer should accept shutdown");
        receiver
            .recv_timeout(CONTROL_TIMEOUT)
            .expect("writer should acknowledge shutdown");
        if let Some(thread) = runtime
            .writer_thread
            .lock()
            .expect("writer mutex should be available")
            .take()
        {
            thread.join().expect("writer should stop cleanly");
        }

        let session = root.join(session_id);
        let log = fs::read_dir(&session)
            .expect("session should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .expect("writer should create a JSONL part");
        let records = fs::read_to_string(log)
            .expect("log should be readable")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("line should be valid JSON"))
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        let sequences = records
            .iter()
            .map(|record| {
                record["sequence"]
                    .as_u64()
                    .expect("sequence should be numeric")
            })
            .collect::<Vec<_>>();
        assert_eq!(sequences, [1, 2, 3]);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn file_sink_rotates_before_crossing_the_part_limit() {
        let root = temporary_test_directory("rotation");
        let session = root.join("session-rotation");
        fs::create_dir_all(&session).expect("session directory should be created");
        let mut sink = FileSink::open(root.clone(), session.clone(), 42).expect("sink should open");
        sink.bytes_written = LOG_PART_LIMIT_BYTES - 1;
        sink.write_line(b"xx").expect("sink should rotate");
        assert_eq!(sink.part, 1);
        assert!(part_path(&session, 42, 0).is_file());
        assert!(part_path(&session, 42, 1).is_file());
        drop(sink);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn retention_keeps_no_more_than_ten_sessions() {
        let root = temporary_test_directory("retention");
        fs::create_dir_all(&root).expect("root should be created");
        for index in 0..12 {
            let session = root.join(format!("session-{index:02}"));
            fs::create_dir(&session).expect("session should be created");
            fs::write(session.join("log.jsonl"), [index as u8]).expect("log should be written");
        }
        cleanup_retention(&root, None).expect("retention should succeed");
        let remaining = fs::read_dir(&root)
            .expect("root should be readable")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count();
        assert_eq!(remaining, LOG_SESSION_LIMIT);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn retention_reserves_space_for_the_next_active_log_part() {
        let root = temporary_test_directory("retention-reserve");
        let active = root.join("session-active");
        let previous = root.join("session-previous");
        fs::create_dir_all(&active).expect("active session should be created");
        fs::create_dir_all(&previous).expect("previous session should be created");
        fs::write(active.join("active.jsonl"), [0u8; 12])
            .expect("active fixture should be written");
        fs::write(previous.join("previous.jsonl"), [0u8; 9])
            .expect("previous fixture should be written");

        cleanup_retention_with_limit(&root, Some(&active), 12)
            .expect("retention should reserve the requested budget");
        assert!(active.is_dir());
        assert!(!previous.exists());
        assert!(directory_size(&root).expect("root size should be readable") <= 12);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn directory_setup_falls_back_after_a_path_failure() {
        let outer = temporary_test_directory("fallback");
        fs::create_dir_all(&outer).expect("outer directory should be created");
        let blocked = outer.join("blocked");
        fs::write(&blocked, b"not a directory").expect("blocking file should be created");
        let fallback = outer.join("fallback");
        let mut config = DiagnosticConfig {
            level: Level::Info,
            sensitive: false,
            minidump: false,
            root_override: None,
            session_id: "session-fallback".to_string(),
            parse_warnings: Vec::new(),
        };
        let mut errors = Vec::new();
        let (root, session) = prepare_directories_from_candidates(
            &mut config,
            &mut errors,
            [blocked, fallback.clone()],
        );
        let expected_session = fallback.join("session-fallback");
        assert_eq!(root.as_deref(), Some(fallback.as_path()));
        assert_eq!(session.as_deref(), Some(expected_session.as_path()));
        assert_eq!(errors.len(), 1);
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn bundle_file_selection_excludes_options_screenshots_and_symbols() {
        let root = temporary_test_directory("bundle");
        let session = root.join("session-bundle");
        fs::create_dir_all(&session).expect("session directory should be created");
        for name in [
            "taskmgr-1-0000.jsonl",
            "crash-1-2.crash.json",
            "crash-1-2.dmp",
            "options.bin",
            "screenshot.png",
            "taskmgr.pdb",
        ] {
            fs::write(session.join(name), name).expect("fixture should be written");
        }
        let export_session = ExportSession {
            name: "session-bundle".to_string(),
            directory: SecureDirectory::open_absolute(&session)
                .expect("session should open securely"),
        };
        let files = collect_export_files(&[export_session]).expect("files should be collected");
        let names = files
            .iter()
            .map(|file| file.archive_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "sessions/session-bundle/crash-1-2.crash.json",
                "sessions/session-bundle/crash-1-2.dmp",
                "sessions/session-bundle/taskmgr-1-0000.jsonl",
            ]
        );
        drop(files);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn secure_root_rejects_a_directory_reparse_point() {
        let outer = temporary_test_directory("root-reparse");
        let target = outer.join("target");
        let redirected = outer.join("redirected");
        fs::create_dir_all(&target).expect("target should be created");
        if !create_test_directory_link(&redirected, &target) {
            fs::remove_dir_all(outer).expect("unsupported fixture should be removable");
            return;
        }

        let error = SecureDirectory::open_or_create_absolute(&redirected)
            .err()
            .expect("redirected root must be rejected");
        assert!(
            error.to_string().contains("reparse"),
            "unexpected error: {error}"
        );

        fs::remove_dir(&redirected).expect("directory link should be removable");
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn redirected_session_is_not_reused_and_its_target_is_untouched() {
        let outer = temporary_test_directory("session-reparse");
        let root = outer.join("root");
        let target = outer.join("target");
        let requested = root.join("session-requested");
        fs::create_dir_all(&root).expect("root should be created");
        fs::create_dir_all(&target).expect("target should be created");
        fs::write(target.join("sentinel"), b"unchanged").expect("sentinel should be written");
        if !create_test_directory_link(&requested, &target) {
            fs::remove_dir_all(outer).expect("unsupported fixture should be removable");
            return;
        }
        let mut config = DiagnosticConfig {
            level: Level::Info,
            sensitive: false,
            minidump: false,
            root_override: None,
            session_id: "session-requested".to_string(),
            parse_warnings: Vec::new(),
        };
        let mut errors = Vec::new();

        let (_, session) =
            prepare_directories_from_candidates(&mut config, &mut errors, [root.clone()]);
        let session = session.expect("a fresh exclusive session should be created");
        assert_ne!(session, requested);
        assert_eq!(
            fs::read(target.join("sentinel")).expect("sentinel should remain"),
            b"unchanged"
        );

        fs::remove_dir(&session).expect("fresh session should be removable");
        fs::remove_dir(&requested).expect("directory link should be removable");
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn precreated_log_part_is_never_opened_or_truncated() {
        let root = temporary_test_directory("precreated-part");
        let session = root.join("session-precreated");
        fs::create_dir_all(&session).expect("session should be created");
        let part = part_path(&session, 42, 0);
        fs::write(&part, b"attacker-owned").expect("precreated part should be written");

        assert!(
            FileSink::open(root.clone(), session, 42).is_err(),
            "exclusive log creation must reject an existing name"
        );
        assert_eq!(
            fs::read(&part).expect("precreated part should remain readable"),
            b"attacker-owned"
        );

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn retention_refuses_a_redirected_session_without_touching_its_target() {
        let outer = temporary_test_directory("retention-reparse");
        let root = outer.join("root");
        let target = outer.join("target");
        let redirected = root.join("session-redirected");
        fs::create_dir_all(&root).expect("root should be created");
        fs::create_dir_all(&target).expect("target should be created");
        fs::write(target.join("sentinel"), b"unchanged").expect("sentinel should be written");
        if !create_test_directory_link(&redirected, &target) {
            fs::remove_dir_all(outer).expect("unsupported fixture should be removable");
            return;
        }

        assert!(
            cleanup_retention_with_limit(&root, None, 0).is_err(),
            "retention must reject a redirected session"
        );
        assert_eq!(
            fs::read(target.join("sentinel")).expect("sentinel should remain"),
            b"unchanged"
        );

        fs::remove_dir(&redirected).expect("directory link should be removable");
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn export_rejects_file_reparse_points_and_hard_links() {
        let outer = temporary_test_directory("export-source-links");
        let session_path = outer.join("session-links");
        let outside = outer.join("outside.json");
        fs::create_dir_all(&session_path).expect("session should be created");
        fs::write(&outside, b"outside").expect("outside file should be written");
        let hard_link = session_path.join("hard-link.json");
        fs::hard_link(&outside, &hard_link).expect("hard-link fixture should be created");
        let session = ExportSession {
            name: "session-links".to_string(),
            directory: SecureDirectory::open_absolute(&session_path)
                .expect("session should open securely"),
        };
        assert!(
            collect_export_files(&[session]).is_err(),
            "hard-linked source must be rejected"
        );
        fs::remove_file(&hard_link).expect("hard link should be removable");

        let file_link = session_path.join("file-link.json");
        if create_test_file_link(&file_link, &outside) {
            let session = ExportSession {
                name: "session-links".to_string(),
                directory: SecureDirectory::open_absolute(&session_path)
                    .expect("session should open securely"),
            };
            assert!(
                collect_export_files(&[session]).is_err(),
                "file reparse point must be rejected"
            );
            fs::remove_file(&file_link).expect("file link should be removable");
        }
        assert_eq!(
            fs::read(&outside).expect("outside file should remain"),
            b"outside"
        );
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn export_temp_files_are_random_exclusive_and_rename_by_directory_handle() {
        let root = temporary_test_directory("export-temp");
        fs::create_dir_all(&root).expect("destination directory should be created");
        let directory =
            SecureDirectory::open_absolute(&root).expect("destination should open securely");
        let precreated = root.join("precreated.tmp");
        fs::write(&precreated, b"unchanged").expect("precreated file should be written");
        assert!(
            directory
                .create_user_attachment_file(OsStr::new("precreated.tmp"), 0)
                .is_err(),
            "exclusive creation must reject a precreated file"
        );
        assert_eq!(
            fs::read(&precreated).expect("precreated file should remain"),
            b"unchanged"
        );

        let (mut first, first_name) =
            create_temporary_bundle(&directory).expect("first temp should be created");
        let (second, second_name) =
            create_temporary_bundle(&directory).expect("second temp should be created");
        assert_ne!(first_name, second_name);
        assert!(
            fs::remove_file(root.join(&first_name)).is_err(),
            "an open no-share temp cannot be replaced by pathname"
        );
        first
            .write_all(b"new bundle")
            .expect("temp should be writable");
        first.sync_all().expect("temp should flush");
        mark_delete(&second).expect("unused temp should be marked for deletion");
        drop(second);

        let destination = root.join("bundle.zip");
        fs::write(&destination, b"old bundle").expect("destination should be precreated");
        assert!(
            rename_within_directory(&first, &directory, OsStr::new("bundle.zip"), false).is_err(),
            "a no-replace rename must reject an existing destination"
        );
        assert_eq!(
            fs::read(&destination).expect("existing destination should remain readable"),
            b"old bundle"
        );
        rename_within_directory(&first, &directory, OsStr::new("bundle.zip"), true)
            .expect("handle-relative replace should succeed");
        drop(first);
        drop(directory);
        assert_eq!(
            fs::read(&destination).expect("destination should be readable"),
            b"new bundle"
        );

        fs::remove_file(precreated).expect("precreated fixture should be removable");
        fs::remove_file(destination).expect("destination should be removable");
        fs::remove_dir(root).expect("destination directory should be removable");
    }

    #[test]
    fn export_destination_rejects_a_redirected_parent() {
        let outer = temporary_test_directory("export-parent-reparse");
        let target = outer.join("target");
        let redirected = outer.join("redirected");
        fs::create_dir_all(&target).expect("target should be created");
        if !create_test_directory_link(&redirected, &target) {
            fs::remove_dir_all(outer).expect("unsupported fixture should be removable");
            return;
        }

        assert!(
            open_export_destination(&redirected.join("bundle.zip")).is_err(),
            "redirected export parent must be rejected"
        );

        fs::remove_dir(&redirected).expect("directory link should be removable");
        fs::remove_dir_all(outer).expect("test directory should be removable");
    }

    #[test]
    fn relaunch_arguments_replace_session_and_preserve_custom_directory() {
        let elevated = elevated_relaunch_arguments(
            [
                OsString::from("--ordinary"),
                OsString::from("--diagnostic-session=invalid-old"),
            ],
            "session-new",
        );
        assert_eq!(
            elevated,
            [
                OsString::from("--ordinary"),
                OsString::from("--diagnostic-session=session-new"),
            ]
        );

        let detailed = detailed_restart_arguments(
            [
                OsString::from("--ordinary"),
                OsString::from("--diagnostic=debug"),
                OsString::from("--diagnostic-sensitive"),
                OsString::from("--diagnostic-dir=C:\\logs"),
                OsString::from("--diagnostic-session=session-old"),
            ],
            "session-new",
            false,
            true,
        );
        assert_eq!(
            detailed,
            [
                OsString::from("--ordinary"),
                OsString::from("--diagnostic-dir=C:\\logs"),
                OsString::from("--diagnostic=trace"),
                OsString::from("--diagnostic-minidump"),
                OsString::from("--diagnostic-session=session-new"),
            ]
        );
    }

    #[test]
    fn windows_quoting_preserves_spaces_quotes_and_trailing_backslashes() {
        let arguments = [
            OsString::from("plain"),
            OsString::from("two words"),
            OsString::from("quote\"inside"),
            OsString::from(r"C:\path with space\"),
        ];
        let encoded = quote_windows_arguments(&arguments);
        let rendered = String::from_utf16_lossy(&encoded[..encoded.len() - 1]);
        assert_eq!(
            rendered,
            r#"plain "two words" "quote\"inside" "C:\path with space\\""#,
        );
    }

    #[test]
    fn source_paths_do_not_expose_build_machine_prefixes() {
        assert_eq!(
            normalized_source_path(r"C:\build\taskmgr-rs\src\app\mod.rs"),
            "src/app/mod.rs"
        );
    }

    #[test]
    fn sampling_fault_injection_is_single_use() {
        let injection = SamplingFaultInjection::new(true);
        assert!(injection.arm_once());
        assert!(!injection.arm_once());
        assert!(injection.take());
        assert!(!injection.take());
    }
}
