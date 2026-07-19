// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 应用程序图标管线
//
//   文件:       src/pages/applications/icons.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Owns ImageLists and the bounded, long-lived pool used to collect window icons.
//! A batch is generation-tagged and is committed only after every dispatched worker responds.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering as AtomicOrdering},
};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded};
use windows_sys::Win32::Foundation::{ERROR_INVALID_DATA, HWND};
use windows_sys::Win32::UI::Controls::{
    HIMAGELIST, ImageList_Create, ImageList_Destroy, ImageList_Remove, ImageList_ReplaceIcon,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CopyIcon, GCL_HICON, GCL_HICONSM, GetClassLongPtrW, HICON, SMTO_ABORTIFHUNG, SMTO_NORMAL,
    SendMessageTimeoutW, WM_GETICON,
};

use super::{TaskIdentity, last_error_or_gen_failure, window_matches_identity};
use crate::infrastructure::native::{destroy_icon_handle, record_win32_error};
use crate::ui::assets::{DEFAULT_PROCESS_ICON_RESOURCE, load_icon_resource};

const MAX_TASK_ICON_WORKERS: usize = 8;
const ICON_FETCH_TIMEOUT_MS: u32 = 100;
const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;
const ICON_SMALL2: usize = 2;

#[derive(Clone, Copy)]
pub(super) struct TaskIconRequest {
    pub(super) identity: TaskIdentity,
    pub(super) is_hung: bool,
}

pub(super) struct TaskIconResult {
    pub(super) identity: TaskIdentity,
    small_icon: isize,
    large_icon: isize,
}

pub(super) struct TaskIconCompletion {
    pub(super) generation: u64,
    pub(super) requested_identities: Vec<TaskIdentity>,
    pub(super) result: Result<Vec<TaskIconResult>, u32>,
}

pub(super) struct TaskIconBatchRequest {
    pub(super) generation: u64,
    pub(super) requests: Vec<TaskIconRequest>,
}

impl TaskIconResult {
    pub(super) fn take_small_icon(&mut self) -> HICON {
        let icon = self.small_icon as HICON;
        self.small_icon = 0;
        icon
    }

    pub(super) fn take_large_icon(&mut self) -> HICON {
        let icon = self.large_icon as HICON;
        self.large_icon = 0;
        icon
    }
}

impl Drop for TaskIconResult {
    fn drop(&mut self) {
        if self.small_icon != 0 {
            destroy_icon_handle(self.small_icon as HICON);
        }
        if self.large_icon != 0 {
            destroy_icon_handle(self.large_icon as HICON);
        }
    }
}

#[derive(Default)]
pub(super) struct TaskIconStore {
    small: HIMAGELIST,
    large: HIMAGELIST,
    default_small: HICON,
    default_large: HICON,
    pub(super) free_slots: Vec<usize>,
}

impl TaskIconStore {
    pub(super) fn initialize(&mut self) -> Result<(), u32> {
        let mut next = Self::default();
        unsafe {
            next.small = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0x21,
                1,
                1,
            );
            next.large = ImageList_Create(
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0x21,
                1,
                1,
            );
            if next.small == 0 || next.large == 0 {
                return Err(last_error_or_gen_failure());
            }

            next.default_small = load_icon_resource(
                DEFAULT_PROCESS_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYSMICON,
                ),
                0,
            );
            next.default_large = load_icon_resource(
                DEFAULT_PROCESS_ICON_RESOURCE,
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CXICON,
                ),
                windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                    windows_sys::Win32::UI::WindowsAndMessaging::SM_CYICON,
                ),
                0,
            );
            if next.default_small.is_null() || next.default_large.is_null() {
                return Err(last_error_or_gen_failure());
            }
            next.reset()?;
        }
        *self = next;
        Ok(())
    }

    pub(super) fn small(&self) -> HIMAGELIST {
        self.small
    }

    pub(super) fn large(&self) -> HIMAGELIST {
        self.large
    }

    pub(super) fn allocate(&mut self, small_icon: HICON, large_icon: HICON) -> Result<usize, u32> {
        if small_icon.is_null() && large_icon.is_null() {
            return Ok(0);
        }
        let requested_slot = self.free_slots.last().copied();
        let appended = requested_slot.is_none();
        let target = match requested_slot {
            Some(slot) => match i32::try_from(slot) {
                Ok(slot) => slot,
                Err(_) => {
                    destroy_icon_handle(small_icon);
                    destroy_icon_handle(large_icon);
                    return Err(ERROR_INVALID_DATA);
                }
            },
            None => -1,
        };

        let small_index =
            match replace_owned_icon(self.small, target, small_icon, self.default_small) {
                Ok(index) => index,
                Err(error) => {
                    destroy_icon_handle(large_icon);
                    return Err(error);
                }
            };
        let large_index =
            match replace_owned_icon(self.large, target, large_icon, self.default_large) {
                Ok(index) => index,
                Err(error) => {
                    rollback_icon_slot(
                        self.small,
                        small_index,
                        self.default_small,
                        appended,
                        "task small icon rollback",
                    );
                    return Err(error);
                }
            };

        if small_index != large_index || requested_slot.is_some_and(|slot| slot != small_index) {
            rollback_icon_slot(
                self.small,
                small_index,
                self.default_small,
                appended,
                "task small icon rollback",
            );
            rollback_icon_slot(
                self.large,
                large_index,
                self.default_large,
                appended,
                "task large icon rollback",
            );
            return Err(ERROR_INVALID_DATA);
        }
        if requested_slot.is_some() {
            self.free_slots.pop();
        }
        Ok(small_index)
    }

    pub(super) fn release(&mut self, slot: usize) -> Result<(), u32> {
        if slot == 0 {
            return Ok(());
        }
        let Ok(slot_i32) = i32::try_from(slot) else {
            return Err(ERROR_INVALID_DATA);
        };
        let small_result =
            unsafe { ImageList_ReplaceIcon(self.small, slot_i32, self.default_small) };
        let small_error = (small_result < 0).then(last_error_or_gen_failure);
        let large_result =
            unsafe { ImageList_ReplaceIcon(self.large, slot_i32, self.default_large) };
        let large_error = (large_result < 0).then(last_error_or_gen_failure);
        if let Some(error) = small_error.or(large_error) {
            return Err(error);
        }
        self.free_slots.push(slot);
        Ok(())
    }

    unsafe fn reset(&mut self) -> Result<(), u32> {
        unsafe {
            ImageList_Remove(self.small, -1);
            ImageList_Remove(self.large, -1);
            let small_index = ImageList_ReplaceIcon(self.small, -1, self.default_small);
            let large_index = ImageList_ReplaceIcon(self.large, -1, self.default_large);
            if small_index != 0 || large_index != 0 {
                let error = last_error_or_gen_failure();
                ImageList_Remove(self.small, -1);
                ImageList_Remove(self.large, -1);
                return Err(error);
            }
            self.free_slots.clear();
            Ok(())
        }
    }

    pub(super) fn destroy(&mut self) {
        unsafe {
            if self.small != 0 {
                ImageList_Destroy(self.small);
                self.small = 0;
            }
            if self.large != 0 {
                ImageList_Destroy(self.large);
                self.large = 0;
            }
        }
        destroy_icon_handle(self.default_small);
        destroy_icon_handle(self.default_large);
        self.default_small = null_mut();
        self.default_large = null_mut();
        self.free_slots.clear();
    }
}

impl Drop for TaskIconStore {
    fn drop(&mut self) {
        self.destroy();
    }
}

struct TaskIconBatchWork {
    requests: Arc<[TaskIconRequest]>,
    next_request: AtomicUsize,
}

enum TaskIconPoolCommand {
    Run(Arc<TaskIconBatchWork>),
    Shutdown,
}

type TaskIconPoolResult = Result<Vec<TaskIconResult>, u32>;

pub(super) struct TaskIconExecutor {
    command_senders: Vec<Sender<TaskIconPoolCommand>>,
    result_receiver: Receiver<TaskIconPoolResult>,
    threads: Vec<JoinHandle<()>>,
}

impl TaskIconExecutor {
    pub(super) fn new() -> Result<Self, u32> {
        let worker_count = thread::available_parallelism()
            .map_err(io_error_code)
            .map(usize::from)?
            .clamp(1, MAX_TASK_ICON_WORKERS);
        let (result_sender, result_receiver) = bounded(worker_count);
        let mut command_senders = Vec::with_capacity(worker_count);
        let mut threads = Vec::<JoinHandle<()>>::with_capacity(worker_count);

        for worker_index in 0..worker_count {
            let (command_sender, command_receiver) = bounded(1);
            let worker_result_sender = result_sender.clone();
            let thread = match thread::Builder::new()
                .name(format!("taskmgr-rs-task-icon-{worker_index}"))
                .spawn(move || run_task_icon_pool_worker(command_receiver, worker_result_sender))
            {
                Ok(thread) => thread,
                Err(error) => {
                    drop(command_sender);
                    drop(command_senders);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(io_error_code(error));
                }
            };
            command_senders.push(command_sender);
            threads.push(thread);
        }
        drop(result_sender);

        Ok(Self {
            command_senders,
            result_receiver,
            threads,
        })
    }

    pub(super) fn collect(&mut self, request: TaskIconBatchRequest) -> TaskIconCompletion {
        let TaskIconBatchRequest {
            generation,
            requests,
        } = request;
        let requested_identities = requests
            .iter()
            .map(|request| request.identity)
            .collect::<Vec<_>>();
        if requests.is_empty() {
            return TaskIconCompletion {
                generation,
                requested_identities,
                result: Ok(Vec::new()),
            };
        }

        let active_workers = requests.len().min(self.command_senders.len());
        let work = Arc::new(TaskIconBatchWork {
            requests: requests.into(),
            next_request: AtomicUsize::new(0),
        });
        let mut dispatched = 0usize;
        let mut batch_error = None;
        for sender in self.command_senders.iter().take(active_workers) {
            if sender
                .send(TaskIconPoolCommand::Run(Arc::clone(&work)))
                .is_err()
            {
                batch_error = Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                break;
            }
            dispatched += 1;
        }

        let mut results = Vec::with_capacity(work.requests.len());
        for _ in 0..dispatched {
            match self.result_receiver.recv() {
                Ok(Ok(mut worker_results)) => results.append(&mut worker_results),
                Ok(Err(error)) => {
                    batch_error.get_or_insert(error);
                }
                Err(_) => {
                    batch_error.get_or_insert(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE);
                    break;
                }
            }
        }

        TaskIconCompletion {
            generation,
            requested_identities,
            result: match batch_error {
                Some(error) => Err(error),
                None => Ok(results),
            },
        }
    }
}

impl Drop for TaskIconExecutor {
    fn drop(&mut self) {
        for sender in &self.command_senders {
            let _ = sender.send(TaskIconPoolCommand::Shutdown);
        }
        self.command_senders.clear();
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                record_win32_error(
                    "task icon worker shutdown",
                    windows_sys::Win32::Foundation::ERROR_GEN_FAILURE,
                );
            }
        }
    }
}

fn run_task_icon_pool_worker(
    command_receiver: Receiver<TaskIconPoolCommand>,
    result_sender: Sender<TaskIconPoolResult>,
) {
    while let Ok(command) = command_receiver.recv() {
        match command {
            TaskIconPoolCommand::Run(work) => {
                let result = catch_unwind(AssertUnwindSafe(|| collect_task_icon_work(&work)))
                    .unwrap_or(Err(windows_sys::Win32::Foundation::ERROR_GEN_FAILURE));
                if result_sender.send(result).is_err() {
                    break;
                }
            }
            TaskIconPoolCommand::Shutdown => break,
        }
    }
}

fn collect_task_icon_work(work: &TaskIconBatchWork) -> TaskIconPoolResult {
    let mut results = Vec::new();
    loop {
        let index = work.next_request.fetch_add(1, AtomicOrdering::Relaxed);
        let Some(request) = work.requests.get(index).copied() else {
            break;
        };
        if !window_matches_identity(request.identity) {
            continue;
        }

        let (small_icon, large_icon) = fetch_window_icons(request.identity.hwnd(), request.is_hung);
        if window_matches_identity(request.identity) {
            results.push(TaskIconResult {
                identity: request.identity,
                small_icon: small_icon as isize,
                large_icon: large_icon as isize,
            });
        } else {
            if !small_icon.is_null() {
                destroy_icon_handle(small_icon);
            }
            if !large_icon.is_null() {
                destroy_icon_handle(large_icon);
            }
        }
    }
    Ok(results)
}

pub(super) fn merge_task_icon_batches(
    current: &mut TaskIconBatchRequest,
    incoming: TaskIconBatchRequest,
) {
    current.generation = current.generation.max(incoming.generation);
    let mut positions = current
        .requests
        .iter()
        .enumerate()
        .map(|(index, request)| (request.identity, index))
        .collect::<HashMap<_, _>>();
    for request in incoming.requests {
        if let Some(index) = positions.get(&request.identity).copied() {
            current.requests[index] = request;
        } else {
            positions.insert(request.identity, current.requests.len());
            current.requests.push(request);
        }
    }
}

pub(super) fn failed_task_icon_completion(
    request: TaskIconBatchRequest,
    error: u32,
) -> TaskIconCompletion {
    TaskIconCompletion {
        generation: request.generation,
        requested_identities: request
            .requests
            .iter()
            .map(|request| request.identity)
            .collect(),
        result: Err(error),
    }
}

fn io_error_code(error: std::io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(windows_sys::Win32::Foundation::ERROR_GEN_FAILURE)
}

fn fetch_window_icons(hwnd: HWND, is_hung: bool) -> (HICON, HICON) {
    let (small2, big) = if is_hung {
        (null_mut(), null_mut())
    } else {
        (
            query_window_icon_source(hwnd, ICON_SMALL2),
            query_window_icon_source(hwnd, ICON_BIG),
        )
    };
    let small = if is_hung || (!small2.is_null() && !big.is_null()) {
        null_mut()
    } else {
        query_window_icon_source(hwnd, ICON_SMALL)
    };

    let mut small_source = [small2, small, big]
        .into_iter()
        .find(|icon| !icon.is_null())
        .unwrap_or(null_mut());
    let mut large_source = [big, small, small2]
        .into_iter()
        .find(|icon| !icon.is_null())
        .unwrap_or(null_mut());

    if small_source.is_null() || large_source.is_null() {
        let class_small = query_class_icon_source(hwnd, GCL_HICONSM);
        let class_large = query_class_icon_source(hwnd, GCL_HICON);
        if small_source.is_null() {
            small_source = if !class_small.is_null() {
                class_small
            } else {
                class_large
            };
        }
        if large_source.is_null() {
            large_source = if !class_large.is_null() {
                class_large
            } else {
                class_small
            };
        }
    }

    unsafe {
        (
            if small_source.is_null() {
                null_mut()
            } else {
                CopyIcon(small_source)
            },
            if large_source.is_null() {
                null_mut()
            } else {
                CopyIcon(large_source)
            },
        )
    }
}

// 通过 SendMessageTimeoutW(WM_GETICON) 查询窗口图标。
// 超时使用 SMTO_ABORTIFHUNG 防止阻塞在挂起窗口上。
fn query_window_icon_source(hwnd: HWND, icon_type: usize) -> HICON {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe {
        let mut result = 0usize;
        SendMessageTimeoutW(
            hwnd,
            WM_GETICON,
            icon_type,
            0,
            SMTO_NORMAL | SMTO_ABORTIFHUNG,
            ICON_FETCH_TIMEOUT_MS,
            &mut result,
        );
        result as HICON
    }
}

// 通过 GetClassLongPtrW 查询窗口类默认图标。
fn query_class_icon_source(hwnd: HWND, class_index: i32) -> HICON {
    // 安全性: this function is a safe facade over Win32/FFI work; all callers run it on the owning UI thread and the existing body preserves its original handle/pointer invariants.
    unsafe { GetClassLongPtrW(hwnd, class_index) as HICON }
}

fn replace_owned_icon(
    imagelist: HIMAGELIST,
    target: i32,
    owned_icon: HICON,
    default_icon: HICON,
) -> Result<usize, u32> {
    let source = if owned_icon.is_null() {
        default_icon
    } else {
        owned_icon
    };
    let index = unsafe { ImageList_ReplaceIcon(imagelist, target, source) };
    destroy_icon_handle(owned_icon);
    if index < 0 {
        Err(last_error_or_gen_failure())
    } else {
        Ok(index as usize)
    }
}

fn rollback_icon_slot(
    imagelist: HIMAGELIST,
    slot: usize,
    default_icon: HICON,
    appended: bool,
    context: &str,
) {
    let Ok(slot) = i32::try_from(slot) else {
        record_win32_error(context, ERROR_INVALID_DATA);
        return;
    };
    let succeeded = unsafe {
        if appended {
            ImageList_Remove(imagelist, slot) != 0
        } else {
            ImageList_ReplaceIcon(imagelist, slot, default_icon) >= 0
        }
    };
    if !succeeded {
        record_win32_error(context, last_error_or_gen_failure());
    }
}

// 将工作线程采集的 WorkerTaskEntry 转换为 UI 线程的 TaskEntry。
