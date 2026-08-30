# Job queue

A bounded worker pool that runs the operations `lh-core` already has — checksum, verify,
sbe, convert, torrent create — over a batch of files, reporting progress and accepting
cancellation through a UI-agnostic event stream. PLAN.md §3 names the shape (`crossbeam-channel`
plus a shared cancellation token, no async runtime) and §4 depends on it: the GUI's job queue
panel is this module's `Event` stream adapted into an Iced `Subscription`. This is the last
piece of M1.

---

## 0. What already exists

*Established 2026-08-30 by reading the current tree, not by guessing at what a job queue
usually looks like.*

* **One place already has intra-operation progress**, and it is the only evidence of what
  a real caller needs: `torrent::create_with_progress` takes `progress: &mut dyn FnMut(u32, u32)`
  and calls it once per piece with (pieces done, pieces total) from inside `hash_pieces`.
  It has no cancellation check anywhere in that loop — a `torrent create` cannot be stopped
  today short of killing the process.
* **Everywhere else is one blocking call with nothing in between.** `checksum::compute`,
  `analysis::verify`, `analysis::sbe`, `convert::to_wav`, `convert::to_flac` each run start
  to finish with no progress and no way in to stop them.
* **`tools::run` cannot be interrupted either.** It calls `Command::output()`, which blocks
  until the child exits and only then hands back stdout/stderr. `flac` writes its own
  progress to stderr as it runs, but we do not read it until the process has already
  finished, so there is no incremental signal to relay even if a caller wanted one.
* **`lh-cli`'s batch commands (`cmd_info`, `cmd_verify`, `cmd_sbe`, `cmd_checksum`) are a
  `for f in &files` loop on the invoking thread.** A hundred-file batch blocks silently
  until it is done; there is no per-file feedback and Ctrl-C has no handler to catch, so it
  kills the process outright.
* **That last point is a real correctness gap, not just a UX one.** `TempOutput`'s `Drop`
  is what removes a half-written `.lh-<pid>.part` file (Principle 1) — but Rust destructors
  do not run when the default SIGINT disposition kills the process. A `convert` interrupted
  today with Ctrl-C can leave debris beside the destination. A queue with an explicit,
  cooperative cancel path that lets the current job finish and unwind normally is a fix for
  that, independent of anything the GUI needs.
* **`crossbeam-channel` and `rayon` have been workspace dependencies since M0** (PLAN.md §5)
  and are used nowhere in the tree — `grep -rn "rayon\|crossbeam" lh-core lh-cli lh-gui`
  returns nothing outside `Cargo.toml`. This milestone is what they were added for.

---

## 1. What "job" means here

A job is one call to an operation `lh-core` already exposes — one file's checksum, one
verify, one sbe check, one conversion, one torrent create — run on a worker thread, able to
report progress through the same channel every other job uses, and able to notice a
cancellation request at its own natural checkpoints. It is not a general task scheduler and
it does not persist across runs: the process exiting is the queue's whole lifetime, matching
every other piece of `lh-core` being a library called from a short-lived CLI invocation or a
long-lived but single-process GUI.

---

## 2. Design

### Cancellation is cooperative, and says so

`CancelToken` wraps `Arc<AtomicBool>`. `cancel()` sets it; `is_cancelled()` reads it. The
queue does not — cannot, with `Command::output()` as it stands — forcibly stop a running
job. What it can do:

* refuse to *start* a queued job once cancelled,
* let a job check the token at a checkpoint it already has (between files in a batch, always;
  between pieces in `hash_pieces`, by extending its existing progress callback to also see the
  token) and return early,
* for a job that shells out to a reference tool, finish the current invocation before the
  gap where the next one would start — a WAV → FLAC job cannot be stopped mid-`flac`
  in this milestone.

That last case is a real, named limitation, not a silent one: it is exactly why §0 called out
that `run()` blocks on `Command::output()`. Making an in-flight `flac` killable needs
`spawn()` plus tracking the child's PID to `kill()`, which is J2 (§5).

### Two levels of progress, and only one of them is real everywhere

* **Job-level** — started / finished / failed, and *N of M* for a batch. The queue provides
  this uniformly for every job kind, because it only needs to know when a closure returns.
* **Sub-item level** — progress *inside* one job. Only torrent create has this today (pieces
  done/total). Checksum, verify, sbe and convert are opaque single calls. J1 does not fake
  sub-item progress for them by, say, guessing from file size and a clock — a batch of five
  FLACs reports as five job-level events, not five fabricated progress bars. Giving convert
  real byte-level progress means threading a progress-aware writer/hasher through
  `format::flac::decode_to_wav` and reading `flac`'s stderr incrementally instead of after
  the fact; that is J2, not a JobQueue-module concern to smuggle in here.

### Concurrency: a rayon pool, no async runtime

Matches PLAN.md §3's own reasoning: the workload is CPU-bound (hashing, decoding) or
process-bound (an external `flac`), never IO-concurrent, so there is nothing an async
runtime buys here. One job is one rayon task; the pool is bounded by
`std::thread::available_parallelism()` unless overridden. `crossbeam-channel` carries the
`Event` stream to whoever is watching — the CLI's progress line today, an Iced `Subscription`
once M3 exists. The queue itself renders nothing (Principle 4): it produces events, it does
not print them.

### Shape

```rust
// lh-core/src/job/mod.rs

pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn new() -> Self;
    pub fn cancel(&self);
    pub fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(u64);

/// What a running job can do with the queue: say how far along it is, and check whether
/// it should stop. Handed to the job closure, not constructed by it.
pub struct Progress<'a, T> {
    id: JobId,
    label: &'a str,
    cancel: CancelToken,
    tx: &'a crossbeam_channel::Sender<Event<T>>,
}
impl<'a, T> Progress<'a, T> {
    /// Sub-item progress, e.g. (pieces done, pieces total). A job that has no natural
    /// sub-items just never calls this — job-level Started/Finished still fires.
    pub fn report(&self, done: u32, total: u32);
    pub fn is_cancelled(&self) -> bool;
}

pub enum Event<T> {
    Started { id: JobId, label: String },
    Progress { id: JobId, done: u32, total: u32 },
    Finished { id: JobId, label: String, output: T },
    Cancelled { id: JobId, label: String },
}

pub struct Queue<T: Send + 'static> {
    // rayon::ThreadPool + crossbeam_channel::{Sender, Receiver}<Event<T>> + CancelToken
    // + an AtomicU64 for JobId
}
impl<T: Send + 'static> Queue<T> {
    pub fn new() -> Self;                     // available_parallelism() workers
    pub fn with_workers(n: usize) -> Self;
    pub fn submit(
        &self,
        label: impl Into<String>,
        job: impl FnOnce(&Progress<T>) -> T + Send + 'static,
    ) -> JobId;
    pub fn events(&self) -> &crossbeam_channel::Receiver<Event<T>>;
    pub fn cancel(&self);                     // sets the shared CancelToken
    pub fn wait(&self);                       // blocks until every submitted job has an Event
}
```

`T` is generic rather than tied to `lh_core::Error` so the same `Queue<T>` serves a batch of
`Result<Conversion>` and a batch of `Result<[u8; 16]>` without the `job` module importing
every operation's result type — the coupling runs one way, from call sites into `job`, never
back. A job that can fail submits `job: impl FnOnce(&Progress<T>) -> T` with
`T = lh_core::Result<Conversion>` and inspects `Ok`/`Err` itself; the queue does not
special-case failure, because "the job finished" and "the job succeeded" are different
questions and only the caller knows how to tell them apart for its own operation.

---

## 3. Wiring into the CLI — J1's proof

A design that only a test exercises is not proven; §0 already found the real gap
(`cmd_verify`/`cmd_sbe`/`cmd_checksum` looping serially with no feedback and no cancel), so J1
closes it there rather than leaving the queue to sit beside the code it was meant to replace.

* `cmd_verify`, `cmd_sbe`, `cmd_checksum` (ffp/md5/st5) and `cmd_convert`'s batch form each
  submit one job per file to a `Queue<Result<...>>` built for that command, and drain
  `events()` into the same per-file lines they print today — the *output* does not change,
  only how it is produced and that a large batch now streams results as they land instead of
  going quiet until the end.
* **Single-file invocations skip the queue.** `lh verify one.flac` has one job; spinning up a
  rayon pool and a channel for it is pure overhead with nothing to show for it, and the
  common case must not get slower. The threshold is "more than one file", not a flag.
- `Ctrl-C` installs a handler (new dependency: `ctrlc`, MIT/Apache-2.0, no transitive
  dependencies beyond `libc`/`winapi`) that calls `Queue::cancel()` instead of the default
  disposition that kills the process outright. This is the fix §0 named: a job already in
  flight finishes and its `TempOutput` commits or drops normally; queued-but-unstarted jobs
  never begin.
* `torrent create`'s own `create_with_progress` callback stays exactly as it is — the CLI
  submits the whole `create()` call as *one* queue job, and that job's closure forwards
  `Progress::report` into the existing `&mut dyn FnMut(u32, u32)` parameter. Two progress
  mechanisms were considered and rejected: teaching `torrent::create` to take a `job::Progress`
  directly would put a `job`-module type into `lh-core::torrent`, inverting the dependency
  §2 just drew one way on purpose.

---

## 4. Out of scope for J1

* Byte-level progress for checksum/verify/convert — file-level only (§2).
* Killing an in-flight external process on cancel — cooperative, between jobs, only (§2).
* Any persistence or resume — the queue's lifetime is the process's (§1).
* GUI wiring — M3 has not started. `Event<T>`/`Progress<T>`/`Queue<T>` carry no CLI-specific
  or GUI-specific type, so the expectation (unverified until M3 exists) is that the Iced
  `Subscription` is a thin adapter over `events()`, not a redesign.

---

## 5. Milestones

| # | Milestone | Contents |
|---|---|---|
| **J1** | Core queue | `CancelToken`, `JobId`, `Progress<T>`, `Event<T>`, `Queue<T>` on a bounded rayon pool. Wired into `lh verify`/`sbe`/`ffp`/`md5`/`st5`/`convert` for multi-file batches; `Ctrl-C` cancels cleanly via `ctrlc`. Torrent create runs as a single queued job reusing its existing progress callback. |
| **J2** | Fine-grained + killable | Byte-level progress for convert (progress-aware WAV writer; incremental read of `flac`'s stderr); `run()` grows a cancel-aware variant using `spawn()` + `kill()` so a WAV → FLAC job can actually be stopped mid-run. |
| **J3** | GUI adapter | Iced `Subscription` wrapping `Queue<T>::events()` for the job-queue panel in PLAN.md §4. Needs M3 to exist first. |

---

## 6. Open questions

1. **Worker count: `available_parallelism()`, or one fewer to leave a core free?** Matters
   more once the GUI (M3) is a process running the queue continuously in the background of an
   interactive UI than it does for a CLI invocation that has the machine to itself.
2. **A `--jobs N` CLI flag now, or wait for someone to ask?** Nothing in §0's evidence says
   anyone has hit the default's limits yet.
3. **One `Queue` per batch command, or one long-lived queue for the whole process?** J1 does
   the former — simplest, and a CLI invocation is one command anyway. A GUI wants the latter,
   a single pool and panel across every operation the user starts; revisit when M3 exists
   rather than guessing at its needs now.
4. **Does `Progress::report`'s (done, total) shape generalize past pieces?** It is exactly
   what `hash_pieces` already produces. J2's byte-level convert progress may want (bytes,
   total_bytes) instead — same shape, different unit — or may want to report elapsed time too.
   Leave it alone until J2 has a second real caller to check it against, the way C2's span
   walk was only generalized once creation needed it too.
