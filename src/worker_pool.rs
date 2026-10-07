//! Threads that share one job queue, so whichever is idle takes the next job.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

pub struct WorkerPool<J, T> {
    job_tx: Sender<J>,
    res_rx: Receiver<(J, Result<T, String>)>,
    workers: usize,
}

impl<J: Send + 'static, T: Send + 'static> WorkerPool<J, T> {
    /// Starts up to `workers` threads named `<name>-worker-<i>`. Each calls
    /// `setup` once, then `run` on every job it takes. Callers count results
    /// to know when their work is done, so a job whose `run` panics still
    /// yields one, `Err(panicked(&job))`, and its thread lives on.
    pub fn spawn<F>(
        name: &str,
        workers: usize,
        setup: fn(),
        run: F,
        panicked: fn(&J) -> String,
    ) -> Self
    where
        F: Fn(&J) -> Result<T, String> + Send + Sync + 'static,
    {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<J>();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let run = Arc::new(run);
        let mut running = 0;
        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let run = Arc::clone(&run);
            let spawned = thread::Builder::new()
                .name(format!("{name}-worker-{i}"))
                .spawn(move || {
                    setup();
                    loop {
                        // Hold the lock only while receiving, not during the job.
                        let job = {
                            let Ok(rx) = job_rx.lock() else { return };
                            let Ok(job) = rx.recv() else { return };
                            job
                        };
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&job)))
                                .unwrap_or_else(|_| Err(panicked(&job)));
                        if res_tx.send((job, result)).is_err() {
                            return;
                        }
                    }
                });
            match spawned {
                Ok(_) => running += 1,
                // wasm32 can't spawn threads this way. Log instead of
                // crashing at startup.
                Err(e) => eprintln!("[{name}] could not spawn worker {i}: {e}"),
            }
        }
        Self {
            job_tx,
            res_rx,
            workers: running,
        }
    }

    /// How many threads started.
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// Queues `job`. False once the workers are gone.
    pub fn submit(&self, job: J) -> bool {
        self.job_tx.send(job).is_ok()
    }

    /// Finished jobs with their results, without blocking.
    pub fn poll(&self) -> Vec<(J, Result<T, String>)> {
        std::iter::from_fn(|| self.res_rx.try_recv().ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn drain(pool: &WorkerPool<u32, u32>, n: usize) -> Vec<(u32, Result<u32, String>)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        while out.len() < n {
            assert!(Instant::now() < deadline, "pool stalled at {}", out.len());
            out.extend(pool.poll());
            std::thread::sleep(Duration::from_millis(1));
        }
        out.sort_by_key(|(job, _)| *job);
        out
    }

    #[test]
    fn a_panicking_job_still_reports_and_its_worker_takes_the_next() {
        let pool = WorkerPool::spawn(
            "test",
            1,
            || {},
            |&n: &u32| {
                if n == 2 {
                    panic!("bad job");
                }
                Ok(n * 10)
            },
            |n| format!("job {n} panicked"),
        );
        assert_eq!(pool.workers(), 1);
        for n in 1..=3 {
            assert!(pool.submit(n));
        }
        assert_eq!(
            drain(&pool, 3),
            vec![(1, Ok(10)), (2, Err("job 2 panicked".into())), (3, Ok(30))]
        );
    }
}
