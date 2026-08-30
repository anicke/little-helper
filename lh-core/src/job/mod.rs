//! A bounded worker pool that runs `lh-core`'s own operations over a batch of files,
//! reporting progress and accepting cooperative cancellation through a UI-agnostic event
//! stream. See `docs/job-queue.md` for why this is shaped the way it is (§2) and what it
//! deliberately does not do yet (§4).
//!
//! Cancellation is cooperative: the queue never kills a running job, it only refuses to
//! start a queued one and lets a running one notice, at whatever checkpoint it already
//! has, via [`Progress::is_cancelled`].

use crossbeam_channel::{Receiver, Sender, unbounded};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Shared cancellation flag. Cheap to clone; every job in a queue holds one.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Identifies one submitted job within its queue, in submission order starting at 0 — a
/// fresh `Queue` per batch (docs/job-queue.md §2 open question 3) makes that a dense index
/// a caller can use to put results back in submission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

impl JobId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// What a running job can do with the queue: report how far along it is, and check
/// whether it should stop. Handed to the job closure; nothing else constructs one.
pub struct Progress<T> {
    id: JobId,
    cancel: CancelToken,
    tx: Sender<Event<T>>,
}

impl<T> Progress<T> {
    /// Sub-item progress, e.g. (pieces done, pieces total) — the one shape a real caller
    /// has today, in `torrent::create_with_progress`. A job with no natural sub-items just
    /// never calls this; job-level `Started`/`Finished` still fires around it either way.
    pub fn report(&self, done: u32, total: u32) {
        let _ = self.tx.send(Event::Progress {
            id: self.id,
            done,
            total,
        });
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// A `Progress` with no `Queue` behind it, for a caller that runs one job directly
    /// instead of submitting it — `lh-cli`'s single-file fast path (docs/job-queue.md §3),
    /// which skips the queue entirely but still wants `is_cancelled()` wired to its own
    /// Ctrl-C handler. `report`s go nowhere: nothing is draining `events()` for a job the
    /// queue never heard about, but sending into a channel with no receiver is harmless,
    /// the same as it is when a `Queue` is torn down out from under a slow job.
    pub fn detached(cancel: CancelToken) -> Self {
        let (tx, _rx) = unbounded();
        Progress {
            id: JobId(0),
            cancel,
            tx,
        }
    }
}

/// One thing that happened to a job. `T` is the operation's own result type — the queue
/// does not interpret it, so a batch of fallible work carries `T = lh_core::Result<X>` and
/// the caller tells success from failure itself; the queue only tells "ran" from "did not".
#[derive(Debug)]
pub enum Event<T> {
    Started {
        id: JobId,
        label: String,
    },
    Progress {
        id: JobId,
        done: u32,
        total: u32,
    },
    Finished {
        id: JobId,
        label: String,
        output: T,
    },
    /// Never started, because the queue was cancelled before a worker got to it. Carries
    /// no `T`: nothing ran to produce one. A caller that needs a value per submitted job
    /// (lh-cli's batch commands do) supplies its own placeholder for this case.
    Cancelled {
        id: JobId,
        label: String,
    },
}

/// A bounded pool of workers running jobs that all produce the same result type `T`, and
/// the channel their events arrive on.
pub struct Queue<T: Send + 'static> {
    pool: rayon::ThreadPool,
    tx: Sender<Event<T>>,
    rx: Receiver<Event<T>>,
    next_id: AtomicU64,
    cancel: CancelToken,
    outstanding: Arc<(Mutex<u64>, Condvar)>,
}

impl<T: Send + 'static> Queue<T> {
    /// One worker per available core.
    pub fn new() -> Self {
        Self::with_workers(std::thread::available_parallelism().map_or(1, |n| n.get()))
    }

    pub fn with_workers(n: usize) -> Self {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n.max(1))
            .build()
            .expect("building the job queue's thread pool");
        let (tx, rx) = unbounded();
        Self {
            pool,
            tx,
            rx,
            next_id: AtomicU64::new(0),
            cancel: CancelToken::new(),
            outstanding: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    /// Queue `job` to run on a worker. Returns immediately with the id it was assigned, in
    /// submission order; `events()` is how the result comes back.
    pub fn submit(
        &self,
        label: impl Into<String>,
        job: impl FnOnce(&Progress<T>) -> T + Send + 'static,
    ) -> JobId {
        let id = JobId(self.next_id.fetch_add(1, Ordering::SeqCst));
        let label = label.into();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let outstanding = self.outstanding.clone();

        {
            let (count, _) = &*outstanding;
            *count.lock().expect("outstanding lock poisoned") += 1;
        }
        let _ = tx.send(Event::Started {
            id,
            label: label.clone(),
        });

        self.pool.spawn(move || {
            if cancel.is_cancelled() {
                let _ = tx.send(Event::Cancelled { id, label });
            } else {
                let progress = Progress {
                    id,
                    cancel,
                    tx: tx.clone(),
                };
                let output = job(&progress);
                let _ = tx.send(Event::Finished { id, label, output });
            }
            let (count, done) = &*outstanding;
            let mut n = count.lock().expect("outstanding lock poisoned");
            *n -= 1;
            if *n == 0 {
                done.notify_all();
            }
        });

        id
    }

    /// The receiving end of the event stream: every `submit`'s `Started`, zero or more
    /// `Progress`, and exactly one `Finished` or `Cancelled`, arrive here. `Started` for a
    /// given job always precedes its own terminal event, but events from different jobs
    /// interleave in whatever order the workers finish them.
    pub fn events(&self) -> &Receiver<Event<T>> {
        &self.rx
    }

    /// A clone of the token this queue cancels on, for wiring up an external signal (a
    /// Ctrl-C handler, a GUI cancel button) without exposing the queue itself past its
    /// current scope.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Ask every job to stop: a queued job that has not started yet will not start, and a
    /// running job sees [`Progress::is_cancelled`] return true at its next checkpoint.
    /// Does not kill anything already running (docs/job-queue.md §2).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Block until every submitted job has produced its terminal event. Callers that also
    /// want to see progress along the way should drain `events()` themselves instead —
    /// this is for the case where only completion matters.
    pub fn wait(&self) {
        let (count, done) = &*self.outstanding;
        let mut n = count.lock().expect("outstanding lock poisoned");
        while *n > 0 {
            n = done.wait(n).expect("outstanding lock poisoned");
        }
    }
}

impl<T: Send + 'static> Default for Queue<T> {
    fn default() -> Self {
        Self::new()
    }
}
