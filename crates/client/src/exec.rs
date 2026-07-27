//! One background thread owning a small tokio runtime. The UI hands it
//! futures (credential checks, region probes, launches, teardown) and polls
//! the results once per frame; the paint thread itself never blocks on the
//! network. This replaces the old noop-waker `block_on` hack, which could
//! only ever drive futures that never actually parked.

use std::future::Future;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

type Task = Box<dyn FnOnce(&tokio::runtime::Runtime) + Send>;

/// Handle to the executor thread. Cheap to clone via `Arc`; dropping the
/// last handle closes the channel and the thread exits after the job in
/// flight, if any, completes.
pub struct Executor {
    tx: Sender<Task>,
}

impl Executor {
    /// Spawns the worker thread with a current-thread tokio runtime. Jobs
    /// run one at a time in submission order, which is exactly the wizard's
    /// shape: it never has two network jobs outstanding.
    pub fn new() -> Executor {
        let (tx, rx) = channel::<Task>();
        std::thread::Builder::new()
            .name("jamstream-exec".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("executor tokio runtime");
                while let Ok(task) = rx.recv() {
                    task(&rt);
                }
            })
            .expect("spawn executor thread");
        Executor { tx }
    }

    /// Submits a future; the returned [`Job`] yields its output once.
    pub fn run<T, F>(&self, fut: F) -> Job<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = channel();
        let submitted = self.tx.send(Box::new(move |rt: &tokio::runtime::Runtime| {
            let out = rt.block_on(fut);
            let _ = tx.send(out);
        }));
        Job {
            rx,
            lost: submitted.is_err(),
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

/// A pending result. `poll` is nonblocking and yields the value exactly
/// once; callers replace the job with its output when it lands.
pub struct Job<T> {
    rx: Receiver<T>,
    lost: bool,
}

impl<T> Job<T> {
    pub fn poll(&mut self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(out) => Some(out),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // The task panicked or the executor is gone; surface as a
                // permanently pending job the UI can time out on its own
                // terms. `lost` lets tests distinguish this.
                self.lost = true;
                None
            }
        }
    }

    /// True once the sending side is gone without a value.
    pub fn lost(&self) -> bool {
        self.lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait<T>(job: &mut Job<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = job.poll() {
                return v;
            }
            assert!(Instant::now() < deadline, "job never completed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn runs_futures_off_the_calling_thread() {
        let exec = Executor::new();
        let caller = std::thread::current().id();
        let mut job = exec.run(async move { std::thread::current().id() != caller });
        assert!(wait(&mut job), "future ran on the calling thread");
    }

    #[test]
    fn jobs_complete_in_submission_order() {
        let exec = Executor::new();
        let mut first = exec.run(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            1u32
        });
        let mut second = exec.run(async { 2u32 });
        // The runtime is single-lane: the second job cannot finish first.
        assert_eq!(wait(&mut first), 1);
        assert_eq!(wait(&mut second), 2);
    }

    #[test]
    fn timers_work_on_the_executor() {
        let exec = Executor::new();
        let mut job = exec.run(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            "done"
        });
        assert_eq!(wait(&mut job), "done");
    }
}
