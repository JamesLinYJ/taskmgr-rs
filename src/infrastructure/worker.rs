// +-------------------------------------------------------------------------
//
//   taskmgr-rs - 持久后台任务执行器
//
//   文件:       src/infrastructure/worker.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! 页面采样使用的持久单消费者 worker。
//!
//! `BackgroundWorker` 只拥有线程、容量为一的通道和完成通知；`SingleFlightWorker`
//! 在其上保证最多一个在途请求，并把刷新期间的请求合并为一个后续请求。UI 仍负责
//! 判断完成快照能否提交，worker 不接触页面状态。

use std::thread::{self, JoinHandle};
use std::time::Instant;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_NOT_ENOUGH_MEMORY, GetLastError, HWND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::infrastructure::diagnostics::{self, Field, Level};
use crate::infrastructure::native::record_win32_error;

enum WorkerCommand<Request> {
    Run {
        request: Request,
        notify_hwnd: isize,
        operation_id: u64,
    },
    Shutdown,
    #[cfg(test)]
    Disconnect,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CorrelatedCompletion<Completion> {
    pub(crate) operation_id: u64,
    pub(crate) value: Completion,
}

#[derive(Debug, PartialEq, Eq)]
enum WorkerEvent<Completion> {
    Completed(CorrelatedCompletion<Completion>),
    CollectorPanicked { operation_id: u64 },
}

struct SubmitError<Request> {
    code: u32,
    request: Request,
}

pub(crate) struct BackgroundWorker<Request, Completion> {
    command_sender: Sender<WorkerCommand<Request>>,
    completion_receiver: Option<Receiver<WorkerEvent<Completion>>>,
    thread: Option<JoinHandle<()>>,
    name: String,
}

impl<Request, Completion> BackgroundWorker<Request, Completion>
where
    Request: Send + 'static,
    Completion: Send + 'static,
{
    #[cfg(test)]
    pub(crate) fn spawn<Collect>(
        thread_name: &str,
        completion_message: u32,
        collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: Fn(Request) -> Completion + Send + Sync + 'static,
    {
        let collect = Arc::new(collect);
        Self::spawn_initialized(
            thread_name,
            completion_message,
            Arc::new(move || {
                let collect = Arc::clone(&collect);
                move |request| collect(request)
            }),
        )
    }

    /// Constructs collector state on the worker thread before receiving requests.
    fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        initialize: Arc<Initialize>,
    ) -> Result<Self, u32>
    where
        Initialize: Fn() -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        let (command_sender, command_receiver) = bounded::<WorkerCommand<Request>>(1);
        let (completion_sender, completion_receiver) = bounded::<WorkerEvent<Completion>>(1);
        let name = thread_name.to_string();
        let worker_name = name.clone();
        let thread = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                diagnostics::event(
                    Level::Debug,
                    "worker.started",
                    "worker",
                    "background worker started",
                    &[Field::text("worker", &worker_name)],
                );
                let mut collect = None;
                let mut restarting = false;
                while let Ok(command) = command_receiver.recv() {
                    match command {
                        WorkerCommand::Run {
                            request,
                            notify_hwnd,
                            operation_id,
                        } => {
                            let started = Instant::now();
                            diagnostics::event_with(
                                Level::Trace,
                                "worker.request_started",
                                "worker",
                                "background worker request started",
                                Some(operation_id),
                                None,
                                &[Field::text("worker", &worker_name)],
                            );
                            let completion = catch_unwind(AssertUnwindSafe(|| {
                                let collect = collect.get_or_insert_with(|| {
                                    let initialized = initialize();
                                    if restarting {
                                        diagnostics::event_with(
                                            Level::Info,
                                            "worker.collector_restarted",
                                            "worker",
                                            "background worker collector restarted after a panic",
                                            Some(operation_id),
                                            None,
                                            &[Field::text("worker", &worker_name)],
                                        );
                                    }
                                    initialized
                                });
                                diagnostics::with_operation_id(operation_id, || collect(request))
                            }));
                            let duration_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            let event = match completion {
                                Ok(value) => {
                                    restarting = false;
                                    diagnostics::event_with(
                                        Level::Trace,
                                        "worker.request_completed",
                                        "worker",
                                        "background worker request completed",
                                        Some(operation_id),
                                        Some(duration_ms),
                                        &[Field::text("worker", &worker_name)],
                                    );
                                    WorkerEvent::Completed(CorrelatedCompletion {
                                        operation_id,
                                        value,
                                    })
                                }
                                Err(_) => {
                                    collect = None;
                                    restarting = true;
                                    diagnostics::event_with(
                                        Level::Error,
                                        "worker.collector_panicked",
                                        "worker",
                                        "background worker collector panicked; its state was discarded",
                                        Some(operation_id),
                                        Some(duration_ms),
                                        &[Field::text("worker", &worker_name)],
                                    );
                                    WorkerEvent::CollectorPanicked { operation_id }
                                }
                            };
                            if completion_sender.send(event).is_err() {
                                break;
                            }
                            let notify_hwnd = notify_hwnd as HWND;
                            if !notify_hwnd.is_null()
                                && unsafe { PostMessageW(notify_hwnd, completion_message, 0, 0) }
                                    == 0
                            {
                                let error = unsafe { GetLastError() };
                                record_win32_error(
                                    "background worker completion notification",
                                    if error == 0 { ERROR_GEN_FAILURE } else { error },
                                );
                            }
                        }
                        WorkerCommand::Shutdown => {
                            diagnostics::event(
                                Level::Debug,
                                "worker.stopping",
                                "worker",
                                "background worker stopping",
                                &[Field::text("worker", &worker_name)],
                            );
                            break;
                        }
                        #[cfg(test)]
                        WorkerCommand::Disconnect => break,
                    }
                }
            })
            .map_err(thread_spawn_error)?;

        Ok(Self {
            command_sender,
            completion_receiver: Some(completion_receiver),
            thread: Some(thread),
            name,
        })
    }

    #[cfg(test)]
    pub(crate) fn submit(&self, request: Request, notify_hwnd: HWND) -> Result<u64, u32> {
        let operation_id = diagnostics::next_operation_id();
        self.submit_with_operation_id(request, notify_hwnd, operation_id)
            .map_err(|error| error.code)?;
        Ok(operation_id)
    }

    fn submit_with_operation_id(
        &self,
        request: Request,
        notify_hwnd: HWND,
        operation_id: u64,
    ) -> Result<(), SubmitError<Request>> {
        self.command_sender
            .send(WorkerCommand::Run {
                request,
                notify_hwnd: notify_hwnd as isize,
                operation_id,
            })
            .map_err(|error| {
                let WorkerCommand::Run { request, .. } = error.0 else {
                    unreachable!("only a run command is submitted through this path");
                };
                SubmitError {
                    code: ERROR_BROKEN_PIPE,
                    request,
                }
            })?;
        let parent_operation_id = diagnostics::current_operation_id();
        let mut fields = vec![Field::text("worker", &self.name)];
        if let Some(parent_operation_id) =
            parent_operation_id.filter(|parent| *parent != operation_id)
        {
            fields.push(Field::unsigned("parent_operation_id", parent_operation_id));
        }
        diagnostics::event_with(
            Level::Trace,
            "worker.request_submitted",
            "worker",
            "background worker request submitted",
            Some(operation_id),
            None,
            &fields,
        );
        Ok(())
    }

    fn try_recv(&self) -> Result<WorkerEvent<Completion>, TryRecvError> {
        self.completion_receiver
            .as_ref()
            .ok_or(TryRecvError::Disconnected)?
            .try_recv()
    }

    #[cfg(test)]
    fn disconnect(&self) {
        self.command_sender
            .send(WorkerCommand::Disconnect)
            .expect("test worker should accept disconnect");
    }
}

impl<Request, Completion> Drop for BackgroundWorker<Request, Completion> {
    fn drop(&mut self) {
        // Disconnect completion first so a worker blocked on the capacity-one result channel can
        // leave before `join`. This makes teardown independent of whether the UI drained the last
        // completion.
        self.completion_receiver.take();
        let _ = self.command_sender.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            record_win32_error("background worker shutdown join", ERROR_GEN_FAILURE);
        }
        diagnostics::event(
            Level::Debug,
            "worker.stopped",
            "worker",
            "background worker stopped",
            &[Field::text("worker", &self.name)],
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDisposition {
    Submitted(u64),
    Coalesced(u64),
}

impl RequestDisposition {
    pub(crate) const fn operation_id(self) -> u64 {
        match self {
            Self::Submitted(operation_id) | Self::Coalesced(operation_id) => operation_id,
        }
    }

    pub(crate) const fn was_coalesced(self) -> bool {
        matches!(self, Self::Coalesced(_))
    }
}

pub(crate) struct WorkerDrain<Completion> {
    pub(crate) completions: Vec<CorrelatedCompletion<Completion>>,
    pub(crate) error: Option<u32>,
}

type WorkerSpawner<Request, Completion> =
    Box<dyn Fn() -> Result<BackgroundWorker<Request, Completion>, u32> + Send + Sync>;

pub(crate) struct SingleFlightWorker<Request, Completion> {
    worker: Option<BackgroundWorker<Request, Completion>>,
    restart: WorkerSpawner<Request, Completion>,
    name: String,
    merge_pending: fn(&mut Request, Request),
    pending: Option<Request>,
    pending_operation_id: Option<u64>,
    in_flight: bool,
}

impl<Request, Completion> SingleFlightWorker<Request, Completion>
where
    Request: Send + 'static,
    Completion: Send + 'static,
{
    pub(crate) fn spawn<Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: Fn(Request) -> Completion + Send + Sync + 'static,
    {
        let collect = Arc::new(collect);
        Self::spawn_initialized(thread_name, completion_message, merge_pending, move || {
            let collect = Arc::clone(&collect);
            move |request| collect(request)
        })
    }

    pub(crate) fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        initialize: Initialize,
    ) -> Result<Self, u32>
    where
        Initialize: Fn() -> Collect + Send + Sync + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        let initialize = Arc::new(initialize);
        let name = thread_name.to_string();
        let restart_name = name.clone();
        let restart_initialize = Arc::clone(&initialize);
        let restart: WorkerSpawner<Request, Completion> = Box::new(move || {
            BackgroundWorker::spawn_initialized(
                &restart_name,
                completion_message,
                Arc::clone(&restart_initialize),
            )
        });
        let worker =
            BackgroundWorker::spawn_initialized(thread_name, completion_message, initialize)?;
        Ok(Self::new(worker, restart, name, merge_pending))
    }

    fn new(
        worker: BackgroundWorker<Request, Completion>,
        restart: WorkerSpawner<Request, Completion>,
        name: String,
        merge_pending: fn(&mut Request, Request),
    ) -> Self {
        Self {
            worker: Some(worker),
            restart,
            name,
            merge_pending,
            pending: None,
            pending_operation_id: None,
            in_flight: false,
        }
    }

    fn ensure_worker(&mut self) -> Result<(), u32> {
        if self.worker.is_some() {
            return Ok(());
        }
        self.worker = Some((self.restart)()?);
        diagnostics::event(
            Level::Info,
            "worker.thread_restarted",
            "worker",
            "background worker thread restarted after channel disconnection",
            &[Field::text("worker", &self.name)],
        );
        Ok(())
    }

    fn submit_request(
        &mut self,
        request: Request,
        notify_hwnd: HWND,
        operation_id: u64,
    ) -> Result<(), SubmitError<Request>> {
        if let Err(code) = self.ensure_worker() {
            return Err(SubmitError { code, request });
        }
        let result = self
            .worker
            .as_ref()
            .expect("worker was ensured")
            .submit_with_operation_id(request, notify_hwnd, operation_id);
        match result {
            Ok(()) => Ok(()),
            Err(first_error) => {
                self.worker = None;
                if let Err(code) = self.ensure_worker() {
                    return Err(SubmitError {
                        code,
                        request: first_error.request,
                    });
                }
                self.worker
                    .as_ref()
                    .expect("worker was restarted")
                    .submit_with_operation_id(first_error.request, notify_hwnd, operation_id)
            }
        }
    }

    pub(crate) fn request(
        &mut self,
        request: Request,
        notify_hwnd: HWND,
    ) -> Result<RequestDisposition, u32> {
        if self.in_flight {
            let already_pending = self.pending.is_some();
            if let Some(pending) = self.pending.as_mut() {
                (self.merge_pending)(pending, request);
            } else {
                self.pending = Some(request);
                self.pending_operation_id = Some(diagnostics::next_operation_id());
            }
            let operation_id = self
                .pending_operation_id
                .unwrap_or_else(diagnostics::next_operation_id);
            let mut fields = vec![
                Field::text("worker", &self.name),
                Field::boolean("replaced_existing_pending", already_pending),
            ];
            if let Some(parent_operation_id) =
                diagnostics::current_operation_id().filter(|parent| *parent != operation_id)
            {
                fields.push(Field::unsigned("parent_operation_id", parent_operation_id));
            }
            diagnostics::event_with(
                Level::Trace,
                "worker.request_coalesced",
                "worker",
                "background worker request coalesced",
                Some(operation_id),
                None,
                &fields,
            );
            return Ok(RequestDisposition::Coalesced(operation_id));
        }

        let operation_id = diagnostics::next_operation_id();
        self.submit_request(request, notify_hwnd, operation_id)
            .map_err(|error| error.code)?;
        self.in_flight = true;
        Ok(RequestDisposition::Submitted(operation_id))
    }

    pub(crate) fn drain(&mut self, notify_hwnd: HWND) -> WorkerDrain<Completion> {
        let mut completions = Vec::new();
        let mut error = None;
        loop {
            let event = match self.worker.as_ref() {
                Some(worker) => worker.try_recv(),
                None => Err(TryRecvError::Disconnected),
            };
            match event {
                Ok(WorkerEvent::Completed(completion)) => {
                    self.in_flight = false;
                    completions.push(completion);
                }
                Ok(WorkerEvent::CollectorPanicked { operation_id }) => {
                    self.in_flight = false;
                    error.get_or_insert(ERROR_GEN_FAILURE);
                    diagnostics::event_with(
                        Level::Warn,
                        "worker.request_failed_after_panic",
                        "worker",
                        "background worker request failed after a collector panic",
                        Some(operation_id),
                        None,
                        &[Field::text("worker", &self.name)],
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = false;
                    self.worker = None;
                    error.get_or_insert(ERROR_BROKEN_PIPE);
                    if let Err(restart_error) = self.ensure_worker() {
                        error = Some(restart_error);
                    }
                    break;
                }
            }
        }

        if !self.in_flight
            && let Some(request) = self.pending.take()
        {
            let operation_id = self
                .pending_operation_id
                .take()
                .unwrap_or_else(diagnostics::next_operation_id);
            match self.submit_request(request, notify_hwnd, operation_id) {
                Ok(()) => self.in_flight = true,
                Err(submit_error) => {
                    self.pending = Some(submit_error.request);
                    self.pending_operation_id = Some(operation_id);
                    error = Some(submit_error.code);
                }
            }
        }

        if !completions.is_empty() || error.is_some() {
            let operation_id = (completions.len() == 1).then(|| completions[0].operation_id);
            diagnostics::event_with(
                if error.is_some() {
                    Level::Warn
                } else {
                    Level::Trace
                },
                "worker.completions_drained",
                "worker",
                "background worker completions drained",
                operation_id,
                None,
                &[
                    Field::text("worker", &self.name),
                    Field::unsigned("completion_count", completions.len() as u64),
                    Field::boolean("request_failed", error.is_some()),
                    Field::boolean("follow_up_in_flight", self.in_flight),
                ],
            );
        }

        WorkerDrain { completions, error }
    }

    pub(crate) fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    fn disconnect(&self) {
        self.worker
            .as_ref()
            .expect("test worker should exist")
            .disconnect();
    }
}

pub(crate) fn replace_pending<Request>(current: &mut Request, incoming: Request) {
    *current = incoming;
}

pub(crate) fn keep_pending<Request>(_current: &mut Request, _incoming: Request) {}

fn thread_spawn_error(error: std::io::Error) -> u32 {
    match error
        .raw_os_error()
        .and_then(|value| u32::try_from(value).ok())
    {
        Some(error) => error,
        None => ERROR_NOT_ENOUGH_MEMORY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn worker_delivers_completion_even_when_notification_window_is_gone() {
        let worker = BackgroundWorker::spawn("taskmgr-rs-worker-test", 0, |value: u32| value * 2)
            .expect("worker should start");

        let operation_id = worker.submit(21, null_mut()).expect("request should queue");

        assert_eq!(
            worker
                .completion_receiver
                .as_ref()
                .expect("receiver should exist")
                .recv_timeout(Duration::from_secs(2))
                .expect("completion should arrive"),
            WorkerEvent::Completed(CorrelatedCompletion {
                operation_id,
                value: 42,
            })
        );
    }

    #[test]
    fn single_flight_coalesces_to_one_follow_up_request() {
        let mut worker = SingleFlightWorker::spawn(
            "taskmgr-rs-single-flight-test",
            0,
            replace_pending,
            |value: u32| value,
        )
        .expect("worker should start");

        let submitted = worker
            .request(1, null_mut())
            .expect("request should submit");
        assert!(matches!(submitted, RequestDisposition::Submitted(_)));
        let coalesced = worker
            .request(2, null_mut())
            .expect("request should coalesce");
        assert!(matches!(coalesced, RequestDisposition::Coalesced(_)));
        let coalesced_again = worker
            .request(3, null_mut())
            .expect("request should coalesce");
        assert_eq!(coalesced_again.operation_id(), coalesced.operation_id());
        assert!(worker.has_pending());

        let deadline = Instant::now() + Duration::from_secs(2);
        let first = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if !drained.completions.is_empty() {
                break drained.completions;
            }
            assert!(Instant::now() < deadline, "first completion timed out");
            thread::yield_now();
        };
        assert_eq!(
            first,
            vec![CorrelatedCompletion {
                operation_id: submitted.operation_id(),
                value: 1,
            }]
        );
        assert_eq!(diagnostics::current_operation_id(), None);
        diagnostics::with_operation_id(first[0].operation_id, || {
            assert_eq!(
                diagnostics::current_operation_id(),
                Some(submitted.operation_id())
            );
        });
        assert_eq!(diagnostics::current_operation_id(), None);

        let deadline = Instant::now() + Duration::from_secs(2);
        let second = loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if !drained.completions.is_empty() {
                break drained.completions;
            }
            assert!(Instant::now() < deadline, "follow-up completion timed out");
            thread::yield_now();
        };
        assert_eq!(
            second,
            vec![CorrelatedCompletion {
                operation_id: coalesced.operation_id(),
                value: 3,
            }]
        );
        assert!(!worker.is_in_flight());
        assert!(!worker.has_pending());
    }

    #[test]
    fn collector_panic_discards_state_and_runs_the_pending_request_after_reinitializing() {
        let initialize_count = Arc::new(AtomicUsize::new(0));
        let panic_once = Arc::new(AtomicBool::new(true));
        let initialize_count_by_worker = Arc::clone(&initialize_count);
        let panic_once_by_worker = Arc::clone(&panic_once);
        let mut worker = SingleFlightWorker::spawn_initialized(
            "taskmgr-rs-worker-panic-test",
            0,
            replace_pending,
            move || {
                initialize_count_by_worker.fetch_add(1, Ordering::AcqRel);
                let panic_once = Arc::clone(&panic_once_by_worker);
                move |value: u32| {
                    assert!(
                        !panic_once.swap(false, Ordering::AcqRel),
                        "synthetic collector panic"
                    );
                    value * 2
                }
            },
        )
        .expect("worker should start");
        worker.request(1, null_mut()).expect("request should queue");
        worker
            .request(2, null_mut())
            .expect("follow-up request should coalesce");
        assert!(worker.has_pending());

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            if drained.error == Some(ERROR_GEN_FAILURE) {
                assert!(drained.completions.is_empty());
                assert!(worker.is_in_flight());
                assert!(!worker.has_pending());
                break;
            }
            assert!(Instant::now() < deadline, "panic report timed out");
            thread::yield_now();
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.first() {
                assert_eq!(completion.value, 4);
                assert_eq!(initialize_count.load(Ordering::Acquire), 2);
                assert!(!worker.is_in_flight());
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reinitialized collector timed out"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn disconnected_worker_thread_is_recreated_before_the_next_request() {
        let mut worker = SingleFlightWorker::spawn(
            "taskmgr-rs-worker-restart-test",
            0,
            keep_pending,
            |value: u32| value + 1,
        )
        .expect("worker should start");
        worker.disconnect();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            if drained.error == Some(ERROR_BROKEN_PIPE) {
                break;
            }
            assert!(Instant::now() < deadline, "disconnect timed out");
            thread::yield_now();
        }

        worker
            .request(41, null_mut())
            .expect("replacement worker should accept a request");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            assert_eq!(drained.error, None);
            if let Some(completion) = drained.completions.first() {
                assert_eq!(completion.value, 42);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement worker completion timed out"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn dropping_worker_joins_an_accepted_request() {
        let completed = Arc::new(AtomicBool::new(false));
        {
            let completed_by_worker = Arc::clone(&completed);
            let mut worker = SingleFlightWorker::spawn(
                "taskmgr-rs-worker-close-test",
                0,
                keep_pending,
                move |(): ()| completed_by_worker.store(true, Ordering::Release),
            )
            .expect("worker should start");
            worker
                .request((), null_mut())
                .expect("request should be accepted");
        }

        assert!(completed.load(Ordering::Acquire));
    }
}
