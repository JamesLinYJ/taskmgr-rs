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

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};
use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_NOT_ENOUGH_MEMORY, GetLastError, HWND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::infrastructure::native::record_win32_error;

enum WorkerCommand<Request> {
    Run {
        request: Request,
        notify_hwnd: isize,
    },
    Shutdown,
}

pub(crate) struct BackgroundWorker<Request, Completion> {
    command_sender: Sender<WorkerCommand<Request>>,
    completion_receiver: Option<Receiver<Completion>>,
    thread: Option<JoinHandle<()>>,
}

impl<Request, Completion> BackgroundWorker<Request, Completion>
where
    Request: Send + 'static,
    Completion: Send + 'static,
{
    pub(crate) fn spawn<Collect>(
        thread_name: &str,
        completion_message: u32,
        collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: FnMut(Request) -> Completion + Send + 'static,
    {
        Self::spawn_initialized(thread_name, completion_message, move || collect)
    }

    /// Constructs collector state on the worker thread before receiving requests.
    pub(crate) fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        initialize: Initialize,
    ) -> Result<Self, u32>
    where
        Initialize: FnOnce() -> Collect + Send + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        let (command_sender, command_receiver) = bounded::<WorkerCommand<Request>>(1);
        let (completion_sender, completion_receiver) = bounded::<Completion>(1);
        let thread = thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let mut collect = initialize();
                while let Ok(command) = command_receiver.recv() {
                    match command {
                        WorkerCommand::Run {
                            request,
                            notify_hwnd,
                        } => {
                            let completion = collect(request);
                            if completion_sender.send(completion).is_err() {
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
                        WorkerCommand::Shutdown => break,
                    }
                }
            })
            .map_err(thread_spawn_error)?;

        Ok(Self {
            command_sender,
            completion_receiver: Some(completion_receiver),
            thread: Some(thread),
        })
    }

    pub(crate) fn submit(&self, request: Request, notify_hwnd: HWND) -> Result<(), u32> {
        self.command_sender
            .send(WorkerCommand::Run {
                request,
                notify_hwnd: notify_hwnd as isize,
            })
            .map_err(|_| ERROR_BROKEN_PIPE)
    }

    pub(crate) fn try_recv(&self) -> Result<Completion, TryRecvError> {
        self.completion_receiver
            .as_ref()
            .ok_or(TryRecvError::Disconnected)?
            .try_recv()
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
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestDisposition {
    Submitted,
    Coalesced,
}

pub(crate) struct WorkerDrain<Completion> {
    pub(crate) completions: Vec<Completion>,
    pub(crate) error: Option<u32>,
}

pub(crate) struct SingleFlightWorker<Request, Completion> {
    worker: BackgroundWorker<Request, Completion>,
    merge_pending: fn(&mut Request, Request),
    pending: Option<Request>,
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
        Collect: FnMut(Request) -> Completion + Send + 'static,
    {
        Ok(Self::new(
            BackgroundWorker::spawn(thread_name, completion_message, collect)?,
            merge_pending,
        ))
    }

    pub(crate) fn spawn_initialized<Initialize, Collect>(
        thread_name: &str,
        completion_message: u32,
        merge_pending: fn(&mut Request, Request),
        initialize: Initialize,
    ) -> Result<Self, u32>
    where
        Initialize: FnOnce() -> Collect + Send + 'static,
        Collect: FnMut(Request) -> Completion + 'static,
    {
        Ok(Self::new(
            BackgroundWorker::spawn_initialized(thread_name, completion_message, initialize)?,
            merge_pending,
        ))
    }

    fn new(
        worker: BackgroundWorker<Request, Completion>,
        merge_pending: fn(&mut Request, Request),
    ) -> Self {
        Self {
            worker,
            merge_pending,
            pending: None,
            in_flight: false,
        }
    }

    pub(crate) fn request(
        &mut self,
        request: Request,
        notify_hwnd: HWND,
    ) -> Result<RequestDisposition, u32> {
        if self.in_flight {
            if let Some(pending) = self.pending.as_mut() {
                (self.merge_pending)(pending, request);
            } else {
                self.pending = Some(request);
            }
            return Ok(RequestDisposition::Coalesced);
        }

        self.worker.submit(request, notify_hwnd)?;
        self.in_flight = true;
        Ok(RequestDisposition::Submitted)
    }

    pub(crate) fn drain(&mut self, notify_hwnd: HWND) -> WorkerDrain<Completion> {
        let mut completions = Vec::new();
        let mut error = None;
        loop {
            match self.worker.try_recv() {
                Ok(completion) => {
                    self.in_flight = false;
                    completions.push(completion);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = false;
                    self.pending = None;
                    error = Some(ERROR_BROKEN_PIPE);
                    break;
                }
            }
        }

        if error.is_none()
            && !self.in_flight
            && let Some(request) = self.pending.take()
        {
            match self.worker.submit(request, notify_hwnd) {
                Ok(()) => self.in_flight = true,
                Err(submit_error) => error = Some(submit_error),
            }
        }

        WorkerDrain { completions, error }
    }

    pub(crate) fn is_in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
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
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    #[test]
    fn worker_delivers_completion_even_when_notification_window_is_gone() {
        let worker = BackgroundWorker::spawn("taskmgr-rs-worker-test", 0, |value: u32| value * 2)
            .expect("worker should start");

        worker.submit(21, null_mut()).expect("request should queue");

        assert_eq!(
            worker
                .completion_receiver
                .as_ref()
                .expect("receiver should exist")
                .recv_timeout(Duration::from_secs(2))
                .expect("completion should arrive"),
            42
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

        assert_eq!(
            worker.request(1, null_mut()),
            Ok(RequestDisposition::Submitted)
        );
        assert_eq!(
            worker.request(2, null_mut()),
            Ok(RequestDisposition::Coalesced)
        );
        assert_eq!(
            worker.request(3, null_mut()),
            Ok(RequestDisposition::Coalesced)
        );
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
        assert_eq!(first, vec![1]);

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
        assert_eq!(second, vec![3]);
        assert!(!worker.is_in_flight());
        assert!(!worker.has_pending());
    }

    #[test]
    fn worker_panic_is_reported_as_a_disconnected_completion_channel() {
        let mut worker = SingleFlightWorker::spawn(
            "taskmgr-rs-worker-panic-test",
            0,
            keep_pending,
            |(): ()| -> () { panic!("synthetic collector panic") },
        )
        .expect("worker should start");
        worker
            .request((), null_mut())
            .expect("request should queue");
        worker
            .request((), null_mut())
            .expect("follow-up request should coalesce");
        assert!(worker.has_pending());

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let drained = worker.drain(null_mut());
            if drained.error == Some(ERROR_BROKEN_PIPE) {
                assert!(!worker.is_in_flight());
                assert!(!worker.has_pending());
                break;
            }
            assert!(Instant::now() < deadline, "disconnect timed out");
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
