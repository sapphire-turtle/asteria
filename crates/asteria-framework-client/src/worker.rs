//! Background threads that run a session's blocking requests.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::rpc::RpcClient;

/// A unit of work. It gets the session's client and is responsible for reporting its own
/// result (see [`Session`](crate::session::Session)).
pub(crate) type Job = Box<dyn FnOnce(&RpcClient) + Send>;

/// A pool of threads taking [`Job`]s in order from one queue.
///
/// Dropping it never waits: the queue is closed and the flag returned by
/// [`cancelled`](Self::cancelled) is set, so queued jobs are skipped and each thread exits once
/// the job it is running (if any) returns. A job that is mid-request must check the flag
/// before reporting, so its result is discarded.
pub(crate) struct Worker {
    jobs: mpsc::Sender<Job>,
    cancelled: Arc<AtomicBool>,
}

impl Worker {
    /// Starts `threads` threads (at least one) named `name`. With one thread, jobs run in the
    /// order they were submitted.
    pub(crate) fn spawn(name: &str, threads: usize, client: Arc<RpcClient>) -> io::Result<Worker> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let cancelled = Arc::new(AtomicBool::new(false));
        for _ in 0..threads.max(1) {
            let rx = rx.clone();
            let client = client.clone();
            let cancelled = cancelled.clone();
            std::thread::Builder::new()
                .name(name.into())
                .spawn(move || {
                    loop {
                        // The guard is dropped at the end of the statement, before the job runs.
                        let job = match rx.lock() {
                            Ok(rx) => rx.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        if cancelled.load(Ordering::Acquire) {
                            return;
                        }
                        job(&client);
                    }
                })?;
        }
        Ok(Worker { jobs, cancelled })
    }

    pub(crate) fn submit(&self, job: Job) {
        // Fails only if every thread has exited, which happens only after `drop`.
        let _ = self.jobs.send(job);
    }

    /// Set once the worker is dropped.
    pub(crate) fn cancelled(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // `jobs` is dropped right after this, which ends the threads' `recv`.
    }
}
