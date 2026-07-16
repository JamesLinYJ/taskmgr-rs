//! Persistent single-consumer workers used by pages that sample Win32 state.
//!
//! The UI owns the worker and remains responsible for deciding when a completed
//! snapshot becomes visible. This type only centralizes thread/channel lifetime,
//! completion notification, and orderly shutdown.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_GEN_FAILURE, ERROR_NOT_ENOUGH_MEMORY, GetLastError, HWND,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::winutil::record_win32_error;

enum WorkerCommand<Request> {
    Run {
        request: Request,
        notify_hwnd: isize,
    },
    Shutdown,
}

pub(crate) struct BackgroundWorker<Request, Completion> {
    command_sender: Sender<WorkerCommand<Request>>,
    completion_receiver: Receiver<Completion>,
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
        mut collect: Collect,
    ) -> Result<Self, u32>
    where
        Collect: FnMut(Request) -> Completion + Send + 'static,
    {
        let (command_sender, command_receiver) = channel::<WorkerCommand<Request>>();
        let (completion_sender, completion_receiver) = channel::<Completion>();
        let thread = thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                loop {
                    let command = match command_receiver.recv() {
                        Ok(command) => command,
                        Err(_) => break,
                    };
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
            completion_receiver,
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
        self.completion_receiver.try_recv()
    }
}

impl<Request, Completion> Drop for BackgroundWorker<Request, Completion> {
    fn drop(&mut self) {
        if self.command_sender.send(WorkerCommand::Shutdown).is_err() {
            record_win32_error("background worker shutdown request", ERROR_BROKEN_PIPE);
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            record_win32_error("background worker shutdown join", ERROR_GEN_FAILURE);
        }
    }
}

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
    use std::time::Duration;

    #[test]
    fn worker_delivers_completion_even_when_notification_window_is_gone() {
        let worker = BackgroundWorker::spawn("taskmgr-rs-worker-test", 0, |value: u32| value * 2)
            .expect("worker should start");

        worker.submit(21, null_mut()).expect("request should queue");

        assert_eq!(
            worker
                .completion_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("completion should arrive"),
            42
        );
    }
}
