from __future__ import annotations

from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text(encoding="utf-8")


def write(path: str, text: str) -> None:
    Path(path).write_text(text, encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = read(path)
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one match, found {count}: {old[:120]!r}")
    write(path, text.replace(old, new, 1))


def replace_between(path: str, start: str, end: str, replacement: str) -> None:
    text = read(path)
    start_index = text.find(start)
    if start_index < 0:
        raise RuntimeError(f"{path}: start marker not found: {start!r}")
    end_index = text.find(end, start_index + len(start))
    if end_index < 0:
        raise RuntimeError(f"{path}: end marker not found: {end!r}")
    if text.find(start, start_index + len(start)) >= 0:
        raise RuntimeError(f"{path}: start marker is not unique: {start!r}")
    write(path, text[:start_index] + replacement + text[end_index:])


# ---------------------------------------------------------------------------
# #19: isolate taskmgr-rs options from the native Task Manager Preferences.
# ---------------------------------------------------------------------------

OPTIONS = "src/config/options.rs"

replace_once(
    OPTIONS,
    'use windows_sys::Win32::Foundation::{ERROR_REVISION_MISMATCH, ERROR_SUCCESS, RECT};',
    '''use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA, ERROR_REVISION_MISMATCH, ERROR_SUCCESS, RECT,
};''',
)

replace_once(
    OPTIONS,
    '''const TASKMAN_KEY: &str = "Software\\\\Microsoft\\\\Windows NT\\\\CurrentVersion\\\\TaskManager";
const OPTIONS_KEY: &str = "Preferences";
const OPTIONS_SCHEMA_VERSION: i32 = 2;''',
    '''const OPTIONS_KEY: &str = "Software\\\\taskmgr-rs\\\\TaskManager";
const OPTIONS_VALUE: &str = "OptionsV1";
const LEGACY_TASKMAN_KEY: &str =
    "Software\\\\Microsoft\\\\Windows NT\\\\CurrentVersion\\\\TaskManager";
const LEGACY_OPTIONS_VALUE: &str = "Preferences";
const OPTIONS_STORAGE_MAGIC: [u8; 8] = *b"TMGRRS01";
const OPTIONS_STORAGE_VERSION: u32 = 1;
const OPTIONS_SCHEMA_VERSION: i32 = 2;''',
)

replace_once(
    OPTIONS,
    '''pub struct Options {
    // 该结构体会按二进制整体落盘到注册表，因此字段顺序和类型都需要保持稳定。
    pub cb_size: u32,
    pub timer_interval: u32,
    pub view_mode: i32,
    pub cpu_history_mode: i32,
    pub update_speed: i32,
    pub window_rect: RECT,
    pub current_page: i32,
    pub active_process_columns: [i32; NUM_COLUMN + 1],
    pub column_widths: [i32; NUM_COLUMN + 1],
    flags: u32,
    pub unused: i32,
    pub unused2: i32,
}

impl Default for Options {''',
    '''pub struct Options {
    // 该结构体会按二进制整体落盘到注册表，因此字段顺序和类型都需要保持稳定。
    pub cb_size: u32,
    pub timer_interval: u32,
    pub view_mode: i32,
    pub cpu_history_mode: i32,
    pub update_speed: i32,
    pub window_rect: RECT,
    pub current_page: i32,
    pub active_process_columns: [i32; NUM_COLUMN + 1],
    pub column_widths: [i32; NUM_COLUMN + 1],
    flags: u32,
    pub unused: i32,
    pub unused2: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StoredOptions {
    magic: [u8; 8],
    storage_version: u32,
    payload_size: u32,
    options: Options,
}

impl StoredOptions {
    fn new(options: Options) -> Self {
        Self {
            magic: OPTIONS_STORAGE_MAGIC,
            storage_version: OPTIONS_STORAGE_VERSION,
            payload_size: size_of::<Options>() as u32,
            options,
        }
    }

    fn into_options(self) -> Result<Options, u32> {
        if self.magic != OPTIONS_STORAGE_MAGIC
            || self.payload_size != size_of::<Options>() as u32
        {
            return Err(ERROR_INVALID_DATA);
        }
        if self.storage_version != OPTIONS_STORAGE_VERSION {
            return Err(ERROR_REVISION_MISMATCH);
        }
        Ok(self.options)
    }
}

/// Marker for fixed-layout registry values whose every bit pattern is a valid Rust value.
///
/// # Safety
///
/// Implementors must be `Copy`, have a stable layout, contain no references, and permit every
/// possible byte pattern. This lets the registry reader initialize the value as zeroed storage
/// before Windows fills every byte.
unsafe trait RegistryPod: Copy {}

unsafe impl RegistryPod for Options {}
unsafe impl RegistryPod for StoredOptions {}

impl Default for Options {''',
)

replace_between(
    OPTIONS,
    '    pub fn load(&mut self, min_width: i32, min_height: i32) -> bool {',
    '    pub fn minimize_on_use(&self) -> bool {',
    r'''    pub fn load(&mut self, min_width: i32, min_height: i32) -> bool {
        // taskmgr-rs owns a versioned application-specific value. The Microsoft Task Manager key
        // is read only for a one-time migration of values that carry taskmgr-rs' exact schema
        // marker; it is never written or deleted.
        if modifiers_force_defaults() {
            self.set_default_values(min_width, min_height);
            return false;
        }

        match read_registry_binary::<StoredOptions>(OPTIONS_KEY, OPTIONS_VALUE) {
            Ok(Some(stored)) => {
                let loaded = match stored.into_options() {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        record_win32_error("taskmgr-rs options envelope", error);
                        self.set_default_values(min_width, min_height);
                        return false;
                    }
                };
                return self.apply_loaded_options(loaded, min_width, min_height, false);
            }
            Ok(None) => {}
            Err(error) => {
                record_win32_error("reading taskmgr-rs options", error);
                self.set_default_values(min_width, min_height);
                return false;
            }
        }

        match read_registry_binary::<Options>(LEGACY_TASKMAN_KEY, LEGACY_OPTIONS_VALUE) {
            Ok(Some(legacy)) if legacy_options_is_taskmgr_rs(&legacy) => {
                self.apply_loaded_options(legacy, min_width, min_height, true)
            }
            Ok(Some(_)) | Ok(None) => {
                self.set_default_values(min_width, min_height);
                false
            }
            Err(error) => {
                record_win32_error("reading legacy taskmgr-rs options", error);
                self.set_default_values(min_width, min_height);
                false
            }
        }
    }

    fn apply_loaded_options(
        &mut self,
        mut loaded: Options,
        min_width: i32,
        min_height: i32,
        persist_migration: bool,
    ) -> bool {
        let migrated = match loaded.migrate_schema() {
            Ok(migrated) => migrated,
            Err(()) => {
                record_win32_error("unsupported options schema", ERROR_REVISION_MISMATCH);
                self.set_default_values(min_width, min_height);
                return false;
            }
        };
        let loaded_was_valid = loaded.is_valid(min_width, min_height);
        if !loaded_was_valid {
            loaded.normalize(min_width, min_height);
        }
        *self = loaded;
        if (persist_migration || migrated || !loaded_was_valid)
            && let Err(error) = self.save()
        {
            record_win32_error("normalized options persistence", error);
        }
        loaded_was_valid
    }

    pub fn save(&self) -> Result<(), u32> {
        let stored = StoredOptions::new(*self);
        write_registry_binary(OPTIONS_KEY, OPTIONS_VALUE, &stored)
    }

''',
)

replace_once(
    OPTIONS,
    'fn modifiers_force_defaults() -> bool {',
    r'''fn legacy_options_is_taskmgr_rs(options: &Options) -> bool {
    options.cb_size == size_of::<Options>() as u32
        && options.unused == OPTIONS_SCHEMA_VERSION
        && options.unused2 == 0
        && options.current_page >= -1
        && options.current_page < PageId::COUNT as i32
        && is_valid_view_mode(options.view_mode)
        && is_valid_cpu_history_mode(options.cpu_history_mode)
        && is_valid_update_speed(options.update_speed)
        && options.flags & !ALL_VALID_FLAGS == 0
        && process_columns_are_valid(&options.active_process_columns, &options.column_widths)
}

fn read_registry_binary<T: RegistryPod>(
    key_path: &str,
    value_name: &str,
) -> Result<Option<T>, u32> {
    unsafe {
        let key_path = to_wide_null(key_path);
        let value_name = to_wide_null(value_name);
        let mut key: HKEY = null_mut();
        let open_status =
            RegOpenKeyExW(HKEY_CURRENT_USER, key_path.as_ptr(), 0, KEY_READ, &mut key);
        if open_status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if open_status != ERROR_SUCCESS {
            return Err(open_status);
        }

        let mut value_type = 0u32;
        let mut value_size = 0u32;
        let size_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut value_size,
        );
        if size_status == ERROR_FILE_NOT_FOUND {
            let close_status = RegCloseKey(key);
            return if close_status == ERROR_SUCCESS {
                Ok(None)
            } else {
                Err(close_status)
            };
        }
        if size_status != ERROR_SUCCESS {
            RegCloseKey(key);
            return Err(size_status);
        }
        if value_type != REG_BINARY || value_size != size_of::<T>() as u32 {
            RegCloseKey(key);
            return Err(ERROR_INVALID_DATA);
        }

        // SAFETY: RegistryPod requires every bit pattern to be valid. The exact-size check above
        // and the second query's unchanged byte count guarantee that Windows initializes all bytes.
        let mut value = zeroed::<T>();
        let mut actual_size = value_size;
        let read_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            (&mut value as *mut T).cast::<u8>(),
            &mut actual_size,
        );
        let close_status = RegCloseKey(key);
        if read_status != ERROR_SUCCESS {
            return Err(read_status);
        }
        if close_status != ERROR_SUCCESS {
            return Err(close_status);
        }
        if value_type != REG_BINARY || actual_size != size_of::<T>() as u32 {
            return Err(ERROR_INVALID_DATA);
        }
        Ok(Some(value))
    }
}

fn write_registry_binary<T: RegistryPod>(
    key_path: &str,
    value_name: &str,
    value: &T,
) -> Result<(), u32> {
    unsafe {
        let key_path = to_wide_null(key_path);
        let value_name = to_wide_null(value_name);
        let mut key: HKEY = null_mut();
        let mut disposition = 0u32;
        let create_status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            null_mut(),
            &mut key,
            &mut disposition,
        );
        if create_status != ERROR_SUCCESS {
            return Err(create_status);
        }

        // SAFETY: RegistryPod has a fixed initialized representation and contains no references.
        let bytes =
            std::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>());
        let set_status = RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_BINARY,
            bytes.as_ptr(),
            bytes.len() as u32,
        );
        let close_status = RegCloseKey(key);
        if set_status != ERROR_SUCCESS {
            Err(set_status)
        } else if close_status != ERROR_SUCCESS {
            Err(close_status)
        } else {
            Ok(())
        }
    }
}

fn modifiers_force_defaults() -> bool {''',
)

replace_once(
    OPTIONS,
    '''    use super::{
        ColumnId, NUM_COLUMN, OPTIONS_SCHEMA_VERSION, Options, SCHEMA_0_NETWORK_PAGE,
        SCHEMA_0_USERS_PAGE, SCHEMA_1_GPU_PAGE, SCHEMA_1_NETWORK_PAGE, SCHEMA_1_USERS_PAGE,
        UpdateSpeed, normalize_process_columns, process_columns_are_valid,
        update_speed_timer_interval, window_rect_dimensions_are_valid, window_rect_is_valid,
    };''',
    '''    use super::{
        ColumnId, ERROR_INVALID_DATA, ERROR_REVISION_MISMATCH, LEGACY_OPTIONS_VALUE,
        LEGACY_TASKMAN_KEY, NUM_COLUMN, OPTIONS_KEY, OPTIONS_SCHEMA_VERSION,
        OPTIONS_STORAGE_MAGIC, OPTIONS_STORAGE_VERSION, OPTIONS_VALUE, Options,
        SCHEMA_0_NETWORK_PAGE, SCHEMA_0_USERS_PAGE, SCHEMA_1_GPU_PAGE, SCHEMA_1_NETWORK_PAGE,
        SCHEMA_1_USERS_PAGE, StoredOptions, UpdateSpeed, legacy_options_is_taskmgr_rs,
        normalize_process_columns, process_columns_are_valid, update_speed_timer_interval,
        window_rect_dimensions_are_valid, window_rect_is_valid,
    };''',
)

replace_once(
    OPTIONS,
    '''    #[test]
    fn process_columns_reject_missing_primary_and_duplicates() {''',
    r'''    #[test]
    fn stored_options_envelope_round_trips_and_rejects_foreign_data() {
        let options = Options::default();
        assert_eq!(
            StoredOptions::new(options).into_options().unwrap().unused,
            options.unused
        );

        let mut foreign = StoredOptions::new(options);
        foreign.magic = *b"NATIVE00";
        assert_eq!(foreign.into_options().err(), Some(ERROR_INVALID_DATA));

        let mut future = StoredOptions::new(options);
        future.storage_version = OPTIONS_STORAGE_VERSION + 1;
        assert_eq!(
            future.into_options().err(),
            Some(ERROR_REVISION_MISMATCH)
        );

        let mut wrong_size = StoredOptions::new(options);
        wrong_size.payload_size = 0;
        assert_eq!(wrong_size.into_options().err(), Some(ERROR_INVALID_DATA));
        assert_eq!(OPTIONS_STORAGE_MAGIC, *b"TMGRRS01");
    }

    #[test]
    fn legacy_migration_requires_the_exact_taskmgr_rs_schema_marker() {
        let current = Options::default();
        assert!(legacy_options_is_taskmgr_rs(&current));

        for schema in [0, 1, OPTIONS_SCHEMA_VERSION + 1] {
            let candidate = Options {
                unused: schema,
                ..current
            };
            assert!(!legacy_options_is_taskmgr_rs(&candidate));
        }

        let wrong_size = Options {
            cb_size: 0,
            ..current
        };
        assert!(!legacy_options_is_taskmgr_rs(&wrong_size));
    }

    #[test]
    fn application_options_namespace_is_distinct_from_windows_task_manager() {
        assert_ne!(OPTIONS_KEY, LEGACY_TASKMAN_KEY);
        assert_ne!(OPTIONS_VALUE, LEGACY_OPTIONS_VALUE);
    }

    #[test]
    fn process_columns_reject_missing_primary_and_duplicates() {''',
)

# ---------------------------------------------------------------------------
# #20: directional network capacities and explicit total-utilization semantics.
# ---------------------------------------------------------------------------

NETWORK = "src/pages/network.rs"

replace_once(
    NETWORK,
    '''    link_speed_bps: u64,
    bytes_sent: u64,''',
    '''    transmit_link_speed_bps: u64,
    receive_link_speed_bps: u64,
    bytes_sent: u64,''',
)

replace_once(
    NETWORK,
    '''            // A zero curve point marks an unavailable interval after first sight or counter
            // reset; the textual value remains "-" so it is not presented as measured idle.
            let total_delta = counter_delta.map_or(0, |delta| delta.2);
            let sent_util = counter_delta.map_or(0, |delta| {
                utilization_percent_for_history(delta.0, raw_adapter.link_speed_bps, elapsed_secs)
            });
            let received_util = counter_delta.map_or(0, |delta| {
                utilization_percent_for_history(delta.1, raw_adapter.link_speed_bps, elapsed_secs)
            });
            let total_util = counter_delta.map_or(0, |delta| {
                utilization_percent_for_history(delta.2, raw_adapter.link_speed_bps, elapsed_secs)
            });

            push_history(&mut sent_history, sent_util);
            push_history(&mut received_history, received_util);
            push_history(&mut total_history, total_util);''',
    '''            // Each direction owns an independent full-duplex capacity. "Total" is the
            // busiest direction's percentage, which remains meaningful for asymmetric links and
            // cannot fabricate a 200% value by summing independent capacities.
            let (sent_ratio, received_ratio, total_ratio) = counter_delta.map_or(
                (None, None, None),
                |delta| {
                    directional_utilization_ratios(
                        delta.0,
                        delta.1,
                        raw_adapter.transmit_link_speed_bps,
                        raw_adapter.receive_link_speed_bps,
                        elapsed_secs,
                    )
                },
            );

            push_history(
                &mut sent_history,
                utilization_percent_for_history(sent_ratio),
            );
            push_history(
                &mut received_history,
                utilization_percent_for_history(received_ratio),
            );
            push_history(
                &mut total_history,
                utilization_percent_for_history(total_ratio),
            );''',
)

replace_once(
    NETWORK,
    '''                link_speed: format_link_speed(raw_adapter.link_speed_bps),
                utilization: counter_delta
                    .map(|_| {
                        utilization_text(total_delta, raw_adapter.link_speed_bps, elapsed_secs)
                    })
                    .unwrap_or_else(|| "-".to_string()),''',
    '''                link_speed: format_link_speeds(
                    raw_adapter.transmit_link_speed_bps,
                    raw_adapter.receive_link_speed_bps,
                ),
                utilization: utilization_text(total_ratio),''',
)

replace_once(
    NETWORK,
    '''                link_speed_bps: row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed),
                bytes_sent: row.OutOctets,''',
    '''                transmit_link_speed_bps: row.TransmitLinkSpeed,
                receive_link_speed_bps: row.ReceiveLinkSpeed,
                bytes_sent: row.OutOctets,''',
)

replace_once(
    NETWORK,
    '        u8::from(adapter.link_speed_bps != 0),',
    '''        u8::from(
            adapter.transmit_link_speed_bps != 0 || adapter.receive_link_speed_bps != 0,
        ),''',
)

replace_between(
    NETWORK,
    '''fn utilization_ratio_percent(
    bytes_per_interval: u64,
    link_speed_bps: u64,
    elapsed_secs: f64,
) -> Option<f64> {''',
    'fn format_counter(value: u64) -> String {',
    r'''fn utilization_ratio_percent(
    bytes_per_interval: u64,
    link_speed_bps: u64,
    elapsed_secs: f64,
) -> Option<f64> {
    if link_speed_bps == 0 || !elapsed_secs.is_finite() || elapsed_secs <= 0.0 {
        return None;
    }
    if bytes_per_interval == 0 {
        return Some(0.0);
    }

    let bits_per_second = (bytes_per_interval as f64 * 8.0) / elapsed_secs;
    let percent = (bits_per_second * 100.0) / link_speed_bps as f64;
    percent.is_finite().then(|| percent.clamp(0.0, 100.0))
}

fn directional_utilization_ratios(
    sent_bytes: u64,
    received_bytes: u64,
    transmit_link_speed_bps: u64,
    receive_link_speed_bps: u64,
    elapsed_secs: f64,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let sent =
        utilization_ratio_percent(sent_bytes, transmit_link_speed_bps, elapsed_secs);
    let received =
        utilization_ratio_percent(received_bytes, receive_link_speed_bps, elapsed_secs);
    let total = match (sent, received) {
        (Some(sent), Some(received)) => Some(sent.max(received)),
        (Some(sent), None) => Some(sent),
        (None, Some(received)) => Some(received),
        (None, None) => None,
    };
    (sent, received, total)
}

fn utilization_percent_for_history(ratio_percent: Option<f64>) -> u8 {
    let Some(ratio_percent) = ratio_percent else {
        return 0;
    };

    let rounded = ratio_percent.round().clamp(0.0, 100.0) as u8;
    if rounded == 0 && ratio_percent > 0.0 {
        1
    } else {
        rounded
    }
}

fn utilization_text(ratio_percent: Option<f64>) -> String {
    let Some(ratio_percent) = ratio_percent else {
        return "-".to_string();
    };

    if ratio_percent > 0.0 && ratio_percent < 1.0 {
        "<1%".to_string()
    } else {
        format!("{}%", ratio_percent.round().clamp(0.0, 100.0) as u8)
    }
}

fn format_link_speeds(transmit_bits_per_second: u64, receive_bits_per_second: u64) -> String {
    match (transmit_bits_per_second, receive_bits_per_second) {
        (0, 0) => "-".to_string(),
        (transmit, receive) if transmit == receive => format_link_speed(transmit),
        (transmit, receive) => format!(
            "Tx {} / Rx {}",
            format_link_speed(transmit),
            format_link_speed(receive)
        ),
    }
}

fn format_link_speed(bits_per_second: u64) -> String {
    // 链路速率采用十进制网络单位显示，更符合网卡/交换机常见标注方式。
    if bits_per_second == 0 {
        return "-".to_string();
    }

    let units = ["bps", "Kbps", "Mbps", "Gbps", "Tbps"];
    let mut value = bits_per_second as f64;
    let mut unit = 0usize;
    while value >= 1000.0 && unit + 1 < units.len() {
        value /= 1000.0;
        unit += 1;
    }

    if value >= 100.0 || unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

''',
)

replace_once(
    NETWORK,
    '''    #[test]
    fn adapter_counter_delta_rejects_missing_regressed_and_overflowed_intervals() {''',
    r'''    #[test]
    fn asymmetric_links_use_each_direction_capacity() {
        let (sent, received, total) =
            directional_utilization_ratios(1_250_000, 0, 10_000_000, 100_000_000, 1.0);
        assert_eq!(sent, Some(100.0));
        assert_eq!(received, Some(0.0));
        assert_eq!(total, Some(100.0));

        let (sent, received, total) =
            directional_utilization_ratios(0, 1_250_000, 100_000_000, 10_000_000, 1.0);
        assert_eq!(sent, Some(0.0));
        assert_eq!(received, Some(100.0));
        assert_eq!(total, Some(100.0));
    }

    #[test]
    fn full_duplex_total_is_the_busiest_direction_not_a_sum() {
        let (_, _, total) =
            directional_utilization_ratios(12_500_000, 12_500_000, 100_000_000, 100_000_000, 1.0);
        assert_eq!(total, Some(100.0));
    }

    #[test]
    fn unavailable_direction_does_not_poison_the_other_direction() {
        let (sent, received, total) =
            directional_utilization_ratios(0, 1_250_000, 0, 10_000_000, 1.0);
        assert_eq!(sent, None);
        assert_eq!(received, Some(100.0));
        assert_eq!(total, Some(100.0));
        assert_eq!(utilization_text(None), "-");
        assert_eq!(utilization_text(Some(0.0)), "0%");
    }

    #[test]
    fn asymmetric_link_speed_text_preserves_both_capacities() {
        assert_eq!(
            format_link_speeds(10_000_000, 100_000_000),
            "Tx 10.0 Mbps / Rx 100 Mbps"
        );
        assert_eq!(
            format_link_speeds(1_000_000_000, 1_000_000_000),
            "1.0 Gbps"
        );
    }

    #[test]
    fn adapter_counter_delta_rejects_missing_regressed_and_overflowed_intervals() {''',
)

# ---------------------------------------------------------------------------
# #21: preserve AeDebug templates, choose the target registry view, and inherit
# only the debugger-ready event handle.
# ---------------------------------------------------------------------------

ACTIONS = "src/pages/processes/actions.rs"

replace_once(
    ACTIONS,
    '''use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};''',
    '''use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::Path;
use std::ptr::{null, null_mut};''',
)

replace_once(
    ACTIONS,
    '''use windows_sys::Win32::Foundation::{
    ERROR_BUSY, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_DATA, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
    ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND, FILETIME, GetLastError, HANDLE, HWND, LPARAM,
    WAIT_OBJECT_0, WPARAM,
};''',
    '''use windows_sys::Win32::Foundation::{
    ERROR_BUSY, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_DATA, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
    ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND, FILETIME, GetLastError, HANDLE,
    HWND, LPARAM, WAIT_OBJECT_0, WPARAM,
};''',
)

replace_once(
    ACTIONS,
    '''use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};''',
    '''use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_EXPAND_SZ, REG_SZ,
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
};''',
)

replace_once(
    ACTIONS,
    '''use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetSystemTimeAsFileTime, GetWindowsDirectoryW,
};''',
    '''use windows_sys::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetSystemTimeAsFileTime, GetWindowsDirectoryW, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_FILE_MACHINE_ARM, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_ARMNT,
    IMAGE_FILE_MACHINE_I386, IMAGE_FILE_MACHINE_IA64, IMAGE_FILE_MACHINE_THUMB,
    IMAGE_FILE_MACHINE_UNKNOWN,
};''',
)

replace_once(
    ACTIONS,
    '''use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, CreateProcessW,
    GetProcessAffinityMask, GetProcessGroupAffinity, GetProcessIdOfThread, GetThreadGroupAffinity,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenThread,
    PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SET_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS, STARTUPINFOW, SetPriorityClass,
    SetProcessAffinityMask, SetProcessDefaultCpuSets, SetThreadGroupAffinity,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_INFORMATION, TerminateProcess,
    WaitForSingleObject,
};''',
    '''use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, CREATE_NEW_CONSOLE, CreateEventW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetProcessAffinityMask, GetProcessGroupAffinity, GetProcessIdOfThread, GetThreadGroupAffinity,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, InitializeProcThreadAttributeList, IsWow64Process2,
    LPPROC_THREAD_ATTRIBUTE_LIST, NORMAL_PRIORITY_CLASS, OpenThread, PROCESS_INFORMATION,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_LIMITED_INFORMATION, PROCESS_TERMINATE, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS, STARTUPINFOEXW, STARTUPINFOW,
    SetPriorityClass, SetProcessAffinityMask, SetProcessDefaultCpuSets, SetThreadGroupAffinity,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SET_INFORMATION, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};''',
)

replace_once(
    ACTIONS,
    'use windows_sys::Win32::UI::Controls::{',
    '''use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::UI::Controls::{''',
)

replace_between(
    ACTIONS,
    '''    // 以 AeDebug 注册表配置的调试器启动并附加到目标进程。命令行传 -p <pid>。
    pub(super) fn attach_debugger(&mut self, identity: ProcIdentity) -> bool {''',
    '    // 通过 explorer.exe /select 命令在资源管理器中定位进程的可执行文件。',
    r'''    // 使用目标进程位数对应的 AeDebug 命令模板启动调试器。完整模板中的第一个
    // `%ld` 接收 PID，第二个接收唯一继承给调试器的 ready-event 句柄。
    pub(super) fn attach_debugger(&mut self, identity: ProcIdentity) -> bool {
        if !self.quick_confirm(&self.strings.warning, &self.strings.debug) {
            return false;
        }

        let target_handle =
            match open_process_for_identity(identity, PROCESS_QUERY_LIMITED_INFORMATION) {
                Ok(handle) => handle,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_debug, error);
                    return false;
                }
            };
        let registry_view = match debugger_registry_view_for_process(target_handle.as_raw()) {
            Ok(view) => view,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        let debugger = match load_debugger_command(registry_view) {
            Ok(Some(debugger)) => debugger,
            Ok(None) => {
                self.show_failure_message(&self.strings.cant_debug, ERROR_FILE_NOT_FOUND);
                return false;
            }
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };
        if !Path::new(&debugger.executable).is_file() {
            self.show_failure_message(&self.strings.cant_debug, ERROR_FILE_NOT_FOUND);
            return false;
        }

        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: `security` remains live for the synchronous call and requests one unnamed,
        // nonsignaled event whose returned handle is adopted immediately.
        let raw_event = unsafe { CreateEventW(&security, 0, 0, null()) };
        let Some(debugger_ready_event) = (unsafe { OwnedHandle::from_raw(raw_event) }) else {
            self.show_failure_message(&self.strings.cant_debug, nonzero_last_error());
            return false;
        };
        let command_line =
            match format_debugger_template(&debugger.template, identity.pid, debugger_ready_event.as_raw() as usize) {
                Ok(command_line) => command_line,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_debug, error);
                    return false;
                }
            };
        let attributes = match ProcThreadAttributeList::for_handle(debugger_ready_event.as_raw()) {
            Ok(attributes) => attributes,
            Err(error) => {
                self.show_failure_message(&self.strings.cant_debug, error);
                return false;
            }
        };

        let mut command_line_wide = to_wide_null(&command_line);
        let application_name = to_wide_null(&debugger.executable);
        let startup_info = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: size_of::<STARTUPINFOEXW>() as u32,
                ..unsafe { zeroed() }
            },
            lpAttributeList: attributes.as_ptr(),
        };
        let mut process_info = unsafe { zeroed::<PROCESS_INFORMATION>() };

        // SAFETY: the application/command buffers and extended startup information remain live
        // for the call. bInheritHandles is required by PROC_THREAD_ATTRIBUTE_HANDLE_LIST, which
        // restricts inheritance to the one event in `attributes`.
        let created = unsafe {
            CreateProcessW(
                application_name.as_ptr(),
                command_line_wide.as_mut_ptr(),
                null_mut(),
                null_mut(),
                1,
                CREATE_NEW_CONSOLE | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup_info.StartupInfo,
                &mut process_info,
            )
        };
        let create_error = if created == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        drop(target_handle);

        if created == 0 {
            self.show_failure_message(
                &self.strings.cant_debug,
                if create_error == 0 {
                    ERROR_GEN_FAILURE
                } else {
                    create_error
                },
            );
            false
        } else {
            // SAFETY: successful CreateProcessW returned two fresh handles and this is their only
            // ownership transfer. The child owns its inherited copy of the ready event.
            match unsafe { own_created_process_handles(process_info) } {
                Ok(_) => true,
                Err(error) => {
                    self.show_failure_message(&self.strings.cant_debug, error);
                    false
                }
            }
        }
    }

''',
)

replace_between(
    ACTIONS,
    'pub(super) fn load_debugger_path() -> Result<Option<String>, u32> {',
    '// 先构建完整的已验证句柄集合，再把不可逆的终止阶段与 UI 结果呈现分离。',
    r'''const AEDEBUG_KEY: &str = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AeDebug";
const AEDEBUG_VALUE: &str = "Debugger";
const MAX_DEBUGGER_COMMAND_BYTES: u32 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DebuggerRegistryView {
    Native,
    Registry32,
    Registry64,
}

impl DebuggerRegistryView {
    const fn access_mask(self) -> u32 {
        match self {
            Self::Native => 0,
            Self::Registry32 => KEY_WOW64_32KEY,
            Self::Registry64 => KEY_WOW64_64KEY,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DebuggerCommand {
    template: String,
    executable: String,
}

struct ProcThreadAttributeList {
    storage: Vec<usize>,
    handles: Box<[HANDLE; 1]>,
    initialized: bool,
}

impl ProcThreadAttributeList {
    fn for_handle(handle: HANDLE) -> Result<Self, u32> {
        if handle.is_null() {
            return Err(ERROR_INVALID_HANDLE);
        }

        let mut byte_count = 0usize;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut byte_count);
        }
        if byte_count == 0 {
            return Err(nonzero_last_error());
        }

        let word_count = byte_count.div_ceil(size_of::<usize>());
        let mut value = Self {
            storage: vec![0usize; word_count],
            handles: Box::new([handle]),
            initialized: false,
        };
        if unsafe {
            InitializeProcThreadAttributeList(value.as_ptr(), 1, 0, &mut byte_count)
        } == 0
        {
            return Err(nonzero_last_error());
        }
        value.initialized = true;
        if unsafe {
            UpdateProcThreadAttribute(
                value.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                value.handles.as_mut_ptr().cast::<c_void>(),
                size_of::<HANDLE>(),
                null_mut(),
                null_mut(),
            )
        } == 0
        {
            return Err(nonzero_last_error());
        }
        Ok(value)
    }

    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
        }
    }
}

pub(super) fn load_debugger_path() -> Result<Option<String>, u32> {
    // This is an availability probe for menu state. Launch-time selection is repeated against
    // the selected process' machine type so the command can never use the wrong registry view.
    let mut first_error = None;
    for view in [
        DebuggerRegistryView::Native,
        DebuggerRegistryView::Registry64,
        DebuggerRegistryView::Registry32,
    ] {
        match load_debugger_command(view) {
            Ok(Some(command)) if Path::new(&command.executable).is_file() => {
                return Ok(Some(command.executable));
            }
            Ok(_) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(None)
    }
}

fn load_debugger_command(view: DebuggerRegistryView) -> Result<Option<DebuggerCommand>, u32> {
    let Some((raw_command, value_type)) = read_aedebug_string(view)? else {
        return Ok(None);
    };
    parse_debugger_command(&raw_command, value_type)
}

fn read_aedebug_string(
    view: DebuggerRegistryView,
) -> Result<Option<(String, u32)>, u32> {
    unsafe {
        let key_name = to_wide_null(AEDEBUG_KEY);
        let value_name = to_wide_null(AEDEBUG_VALUE);
        let mut key: HKEY = null_mut();
        let open_status = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            key_name.as_ptr(),
            0,
            KEY_READ | view.access_mask(),
            &mut key,
        );
        if open_status == ERROR_FILE_NOT_FOUND || open_status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        if open_status != 0 {
            return Err(open_status);
        }

        let mut value_type = 0u32;
        let mut value_size = 0u32;
        let size_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            null_mut(),
            &mut value_size,
        );
        if size_status == ERROR_FILE_NOT_FOUND {
            let close_status = RegCloseKey(key);
            return if close_status == 0 {
                Ok(None)
            } else {
                Err(close_status)
            };
        }
        if size_status != 0 {
            RegCloseKey(key);
            return Err(size_status);
        }
        if !matches!(value_type, REG_SZ | REG_EXPAND_SZ)
            || value_size < size_of::<u16>() as u32
            || !value_size.is_multiple_of(size_of::<u16>() as u32)
            || value_size > MAX_DEBUGGER_COMMAND_BYTES
        {
            RegCloseKey(key);
            return Err(ERROR_INVALID_DATA);
        }

        let mut buffer = vec![0u16; value_size as usize / size_of::<u16>()];
        let mut actual_size = value_size;
        let read_status = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            null_mut(),
            &mut value_type,
            buffer.as_mut_ptr().cast::<u8>(),
            &mut actual_size,
        );
        let close_status = RegCloseKey(key);
        if read_status == ERROR_MORE_DATA {
            return Err(ERROR_MORE_DATA);
        }
        if read_status != 0 {
            return Err(read_status);
        }
        if close_status != 0 {
            return Err(close_status);
        }
        if !matches!(value_type, REG_SZ | REG_EXPAND_SZ)
            || actual_size < size_of::<u16>() as u32
            || !actual_size.is_multiple_of(size_of::<u16>() as u32)
            || actual_size > value_size
        {
            return Err(ERROR_INVALID_DATA);
        }
        let units = actual_size as usize / size_of::<u16>();
        let Some(length) = buffer[..units].iter().position(|value| *value == 0) else {
            return Err(ERROR_INVALID_DATA);
        };
        Ok(Some((
            String::from_utf16(&buffer[..length]).map_err(|_| ERROR_INVALID_DATA)?,
            value_type,
        )))
    }
}

fn debugger_registry_view_for_process(process: HANDLE) -> Result<DebuggerRegistryView, u32> {
    if process.is_null() {
        return Err(ERROR_INVALID_HANDLE);
    }
    let mut process_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    let mut native_machine = IMAGE_FILE_MACHINE_UNKNOWN;
    if unsafe { IsWow64Process2(process, &mut process_machine, &mut native_machine) } == 0 {
        return Err(nonzero_last_error());
    }
    debugger_registry_view_for_machines(process_machine, native_machine)
}

fn debugger_registry_view_for_machines(
    process_machine: u16,
    native_machine: u16,
) -> Result<DebuggerRegistryView, u32> {
    let effective_machine = if process_machine == IMAGE_FILE_MACHINE_UNKNOWN {
        native_machine
    } else {
        process_machine
    };
    let target_is_32_bit = match effective_machine {
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => true,
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => false,
        _ => return Err(ERROR_NOT_SUPPORTED),
    };
    let native_is_64_bit = match native_machine {
        IMAGE_FILE_MACHINE_AMD64 | IMAGE_FILE_MACHINE_ARM64 | IMAGE_FILE_MACHINE_IA64 => true,
        IMAGE_FILE_MACHINE_I386
        | IMAGE_FILE_MACHINE_ARM
        | IMAGE_FILE_MACHINE_ARMNT
        | IMAGE_FILE_MACHINE_THUMB => false,
        _ => return Err(ERROR_NOT_SUPPORTED),
    };
    Ok(if !native_is_64_bit {
        DebuggerRegistryView::Native
    } else if target_is_32_bit {
        DebuggerRegistryView::Registry32
    } else {
        DebuggerRegistryView::Registry64
    })
}

// 引用命令行参数。只在包含空格、制表符或引号时加引号，并正确处理反斜杠转义。
fn quote_command_line_arg(value: &str) -> String {
    if !value.contains([' ', '\t', '"']) {
        return value.to_string();
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;

    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }

        if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
            continue;
        }

        if backslashes > 0 {
            quoted.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
        }
        quoted.push(ch);
    }

    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

// 从命令行字符串中提取第一个 token（即可执行文件路径）。处理引号包裹和非引号两种格式。
pub(super) fn extract_first_command_token(command_line: &str) -> String {
    let trimmed = command_line.trim();
    if let Some(rest) = trimmed.strip_prefix('"') {
        rest.split('"').next().unwrap_or_default().to_string()
    } else {
        trimmed
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

fn parse_debugger_command(
    command_line: &str,
    value_type: u32,
) -> Result<Option<DebuggerCommand>, u32> {
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)?
    } else if value_type == REG_SZ {
        command_line.to_string()
    } else {
        return Err(ERROR_INVALID_DATA);
    };
    parse_expanded_debugger_command(&expanded)
}

fn parse_expanded_debugger_command(
    command_line: &str,
) -> Result<Option<DebuggerCommand>, u32> {
    let template = command_line.trim().to_string();
    let executable = extract_first_command_token(&template);
    let file_name = Path::new(&executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if executable.is_empty()
        || !Path::new(&executable).is_absolute()
        || file_name.eq_ignore_ascii_case("drwtsn32")
        || file_name.eq_ignore_ascii_case("drwtsn32.exe")
    {
        return Ok(None);
    }
    validate_debugger_template(&template)?;
    Ok(Some(DebuggerCommand {
        template,
        executable,
    }))
}

fn validate_debugger_template(template: &str) -> Result<(), u32> {
    let mut placeholders = 0usize;
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            continue;
        }
        match chars.next() {
            Some('%') => {}
            Some('l' | 'L') => {
                if !matches!(chars.next(), Some('d' | 'D')) {
                    return Err(ERROR_INVALID_DATA);
                }
                placeholders += 1;
            }
            Some('p' | 'P') => return Err(ERROR_NOT_SUPPORTED),
            Some(_) | None => return Err(ERROR_INVALID_DATA),
        }
    }
    if placeholders == 2 {
        Ok(())
    } else {
        Err(ERROR_INVALID_DATA)
    }
}

fn format_debugger_template(
    template: &str,
    process_id: u32,
    event_handle: usize,
) -> Result<String, u32> {
    validate_debugger_template(template)?;
    let replacements = [process_id.to_string(), event_handle.to_string()];
    let mut replacement_index = 0usize;
    let mut output = String::with_capacity(template.len() + 32);
    let mut chars = template.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            output.push(ch);
            continue;
        }
        match chars.next() {
            Some('%') => output.push('%'),
            Some('l' | 'L') => {
                if !matches!(chars.next(), Some('d' | 'D')) {
                    return Err(ERROR_INVALID_DATA);
                }
                output.push_str(
                    replacements
                        .get(replacement_index)
                        .ok_or(ERROR_INVALID_DATA)?,
                );
                replacement_index += 1;
            }
            Some('p' | 'P') => return Err(ERROR_NOT_SUPPORTED),
            Some(_) | None => return Err(ERROR_INVALID_DATA),
        }
    }
    if replacement_index == replacements.len() {
        Ok(output)
    } else {
        Err(ERROR_INVALID_DATA)
    }
}

// Compatibility helper used by the existing pure parsing tests.
pub(super) fn normalize_debugger_command_with<F>(
    command_line: &str,
    value_type: u32,
    expand_environment_variables: F,
) -> Option<String>
where
    F: Fn(&str) -> String,
{
    let expanded = if value_type == REG_EXPAND_SZ {
        expand_environment_variables(command_line)
    } else if value_type == REG_SZ {
        command_line.to_string()
    } else {
        return None;
    };
    let executable = extract_first_command_token(&expanded);
    let file_name = Path::new(&executable)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if executable.is_empty()
        || file_name.eq_ignore_ascii_case("drwtsn32")
        || file_name.eq_ignore_ascii_case("drwtsn32.exe")
    {
        None
    } else {
        Some(executable)
    }
}

// 展开字符串中的环境变量（如 %SystemRoot%）。
// 使用 Win32 ExpandEnvironmentStringsW API，正确处理 WOW64 重定向和 %% 转义。
fn expand_environment_variables(command_line: &str) -> Result<String, u32> {
    let wide_input = to_wide_null(command_line);
    let required = unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), null_mut(), 0) };
    if required == 0 {
        return Err(nonzero_last_error());
    }
    let mut buffer = vec![0u16; required as usize];
    let written =
        unsafe { ExpandEnvironmentStringsW(wide_input.as_ptr(), buffer.as_mut_ptr(), required) };
    if written == 0 || written > required {
        return Err(nonzero_last_error());
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(test)]
mod debugger_tests {
    use super::*;

    #[test]
    fn full_aedebug_template_preserves_debugger_specific_arguments() {
        let template = r#""C:\Debuggers\windbg.exe" -p %ld -e %ld -g"#;
        let command = parse_expanded_debugger_command(template)
            .unwrap()
            .unwrap();
        assert_eq!(command.executable, r"C:\Debuggers\windbg.exe");
        assert_eq!(
            format_debugger_template(&command.template, 1234, 5678).unwrap(),
            r#""C:\Debuggers\windbg.exe" -p 1234 -e 5678 -g"#
        );
    }

    #[test]
    fn visual_studio_jit_template_and_literal_percent_are_supported() {
        let template =
            r#""C:\Windows\System32\vsjitdebugger.exe" -p %ld -e %ld --label 100%%"#;
        assert_eq!(
            format_debugger_template(template, 42, 99).unwrap(),
            r#""C:\Windows\System32\vsjitdebugger.exe" -p 42 -e 99 --label 100%"#
        );
    }

    #[test]
    fn unsupported_or_ambiguous_templates_are_rejected() {
        assert_eq!(
            parse_expanded_debugger_command(
                r#""C:\Debuggers\dbg.exe" -p %ld -e %ld -j 0x%p"#
            ),
            Err(ERROR_NOT_SUPPORTED)
        );
        for template in [
            r#""C:\Debuggers\dbg.exe" -p %ld"#,
            r#""C:\Debuggers\dbg.exe" -p %ld -e %ld -x %ld"#,
            r#""C:\Debuggers\dbg.exe" -p %q -e %ld"#,
        ] {
            assert!(parse_expanded_debugger_command(template).is_err());
        }
        assert!(
            parse_expanded_debugger_command(
                r#""relative\dbg.exe" -p %ld -e %ld"#
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn target_machine_selects_the_matching_registry_view() {
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_I386,
                IMAGE_FILE_MACHINE_AMD64,
            )
            .unwrap(),
            DebuggerRegistryView::Registry32
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_UNKNOWN,
                IMAGE_FILE_MACHINE_AMD64,
            )
            .unwrap(),
            DebuggerRegistryView::Registry64
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_UNKNOWN,
                IMAGE_FILE_MACHINE_I386,
            )
            .unwrap(),
            DebuggerRegistryView::Native
        );
        assert_eq!(
            debugger_registry_view_for_machines(
                IMAGE_FILE_MACHINE_ARMNT,
                IMAGE_FILE_MACHINE_ARM64,
            )
            .unwrap(),
            DebuggerRegistryView::Registry32
        );
    }
}

''',
)

print("Applied root-cause fixes for issues #19, #20, and #21.")
