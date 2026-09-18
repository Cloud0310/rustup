//! Bounded, archive-scoped workers on Tokio's blocking pool.
//!
//! Submit many file operations per blocking task. A per-file spawn_blocking
//! prototype added substantial CPU overhead on the docs workload. The async
//! supervisor awaits the worker group once per extraction dependency phase.
//! Blocking submission/join only happen on the dedicated tar producer thread.

use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};

use tokio::runtime::Handle;

type Work = Box<dyn FnOnce() + Send>;

struct Phase {
    sender: SyncSender<Work>,
    finished: Receiver<bool>,
}

pub(super) struct TokioPool {
    runtime: Handle,
    limit: usize,
    queued: Arc<AtomicUsize>,
    phase: RefCell<Option<Phase>>,
}

impl TokioPool {
    pub(super) fn new(limit: usize, runtime: Handle) -> Self {
        assert!(limit > 0);
        Self {
            runtime,
            limit,
            queued: Arc::new(AtomicUsize::new(0)),
            phase: RefCell::new(None),
        }
    }

    fn start_phase(&self) -> Phase {
        let (sender, receiver) = mpsc::sync_channel::<Work>(5);
        let receiver = Arc::new(Mutex::new(receiver));
        let (finished_tx, finished) = mpsc::channel();
        let mut workers = Vec::with_capacity(self.limit);
        for _ in 0..self.limit {
            let receiver = receiver.clone();
            let queued = self.queued.clone();
            workers.push(self.runtime.spawn_blocking(move || {
                loop {
                    // Release the receiver lock before running any filesystem
                    // operation, especially write/close or a chunk receive.
                    let work = receiver.lock().unwrap().recv();
                    let Ok(work) = work else { break };
                    queued.fetch_sub(1, Ordering::Relaxed);
                    work();
                }
            }));
        }
        self.runtime.spawn(async move {
            let mut success = true;
            for worker in workers {
                // Drain every worker even if an earlier one failed.
                success &= worker.await.is_ok();
            }
            let _ = finished_tx.send(success);
        });
        Phase { sender, finished }
    }

    pub(super) fn execute(&self, work: impl FnOnce() + Send + 'static) {
        let mut phase = self.phase.borrow_mut();
        let phase = phase.get_or_insert_with(|| self.start_phase());
        self.queued.fetch_add(1, Ordering::Relaxed);
        phase
            .sender
            .send(Box::new(work))
            .expect("disk workers should be running");
    }

    pub(super) fn queued_count(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub(super) fn join(&self) {
        if let Some(Phase { sender, finished }) = self.phase.borrow_mut().take() {
            // Closing admission drains the queue and ends each bounded worker.
            // A later directory completion may begin another extraction phase.
            drop(sender);
            assert!(
                finished.recv().expect("disk supervisor should finish"),
                "disk worker stopped unexpectedly"
            );
        }
    }
}

impl Drop for TokioPool {
    fn drop(&mut self) {
        self.join();
    }
}
