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
| ~~**J1**~~ | ~~Core queue~~ | **Done** — `CancelToken`, `JobId`, `Progress<T>`, `Event<T>`, `Queue<T>` on a bounded rayon pool. Wired into `lh verify`/`sbe`/`ffp`/`md5`/`st5`/`convert` for multi-file batches; `Ctrl-C` cancels cleanly via `ctrlc`. Torrent create runs as a single queued job reusing its existing progress callback. 5 tests in `lh-core/tests/job.rs`. See §7. |
| ~~**J2**~~ | ~~Fine-grained + killable~~ | **Done, with one sketch item cut on evidence** — frame-level progress for FLAC → WAV (in-process); `run()` grows `run_cancellable`, a `spawn()` + `kill()` variant, so a WAV → FLAC job actually stops mid-`flac`. Byte-level progress *from `flac`'s own stderr* is not implemented — §8 found it is not available at all over a pipe. See §8. |
| **J3** | GUI adapter | Iced `Subscription` wrapping `Queue<T>::events()` for the job-queue panel in PLAN.md §4. Planned in [docs/gui.md](docs/gui.md) (§G0 spike done; G2 is where this lands). |

---

## 6. Open questions

1. **Worker count: `available_parallelism()`, or one fewer to leave a core free?** Matters
   more once the GUI (M3) is a process running the queue continuously in the background of an
   interactive UI than it does for a CLI invocation that has the machine to itself.
2. **A `--jobs N` CLI flag now, or wait for someone to ask?** Nothing in §0's evidence says
   anyone has hit the default's limits yet.
3. ~~**One `Queue` per batch command, or one long-lived queue for the whole process?**~~
   **Resolved in [docs/gui.md](docs/gui.md) §1/§2**: the GUI uses one long-lived
   `Queue<JobOutcome>` for the process's life, which breaks `JobId::index()`'s dense-index
   assumption from §7 below — see gui.md §5 open question 2.
4. **Does `Progress::report`'s (done, total) shape generalize past pieces?** It is exactly
   what `hash_pieces` already produces. J2's byte-level convert progress may want (bytes,
   total_bytes) instead — same shape, different unit — or may want to report elapsed time too.
   Leave it alone until J2 has a second real caller to check it against, the way C2's span
   walk was only generalized once creation needed it too.

---

## 7. J1 notes

*Landed 2026-08-30.*

Four things the implementation forced, none of them visible from the sketch in §2:

* **`Progress<'a, T>` could not stay borrowed.** `Queue::submit` requires `job: impl FnOnce(&Progress<T>) -> T + Send + 'static` because rayon's `spawn` needs a `'static` closure, and nothing borrowed from `&self` can cross into one. `Progress<T>` ended up owning a cloned `Sender` and a cloned `CancelToken` instead of borrowing them — both are cheap to clone by design (an `Arc` and a channel handle), so this cost nothing but the lifetime parameter.
* **`wait()` needed its own bookkeeping, not a rayon trick.** `ThreadPool::broadcast` runs a closure on every worker thread, but it does not wait for work already sitting in rayon's own injector queue — a `spawn`'d job can still be pending when a `broadcast` closure runs on the thread that would have picked it up next. `Queue` instead carries a plain `Mutex<u64>` + `Condvar` outstanding-job counter, incremented in `submit` and decremented when a job's terminal event is sent. Correct, and it is what `wait()` in the CLI's `run_batch` never actually needed to call — draining `events()` for as many terminal events as jobs submitted already implies "done" without a second synchronization primitive, which is what `run_batch` does. `wait()` exists for a caller that wants completion without watching progress.
* **§3's "streams results as they land" oversold the CLI change, and got corrected here.** A script piping `lh verify`'s stdout must see the same file order on every run; printing in completion order — the literal reading of that sentence — makes the output nondeterministic across runs on a multi-file batch, since worker threads finish in whatever order the OS schedules them. `run_batch` keeps per-file report lines in submission order, buffered until every job has a terminal event, and only the *progress counter* ("N of M done") streams live, to stderr, where a script is not reading anyway. Real, visible feedback during a long batch; unchanged, reproducible stdout.
* **The cancellation checkpoint inside `hash_pieces` needed the existing callback to grow a return value, not a second parameter.** `create_with_progress`'s `progress: &mut dyn FnMut(u32, u32)` became `FnMut(u32, u32) -> bool`, where `false` stops the walk. That keeps `torrent::create` free of any dependency on the `job` module, exactly as §2 requires — the CLI's job closure is the only place that knows both types, forwarding `Progress::report` in and `Progress::is_cancelled` out through the one function `hash_pieces` already called every piece. A stopped walk returns the new `Error::Cancelled` rather than a partial `Created`, matching Principle 1.
* **`JobId::index()` is honest about leaning on a design choice from open question 3, not a general property.** It only maps to "position in the file list" because each CLI batch command builds one fresh `Queue` and submits every file in one dense, ordered pass — true today, and it would silently stop being true the moment two batches ever shared one `Queue`. Worth remembering if J3's GUI adapter reaches for a single long-lived queue across operations, per that same open question.

---

## 8. J2 notes

*Landed 2026-08-30.*

**§2's plan to read `flac`'s stderr incrementally does not work, and this was checked
before writing any parsing code, not after.** `flac 1.5.0` (the reference binary this repo
already uses — PLAN.md's `flac -d` / `--show-md5sum` parity claims are against the same
binary) gates its `\r`-updated `N% complete` line on `isatty(stderr)`. Piped through
`Command`'s `Stdio::piped()`, stderr carries only the four-line banner and one final summary
line (`test.wav: Verify OK, wrote N bytes, ratio=R`) — confirmed both on a small fixture and
on a 60-second synthetic WAV encoded at `-8` (finishes in ~50ms either way, so file size
was not masking anything). `flac --help` has no flag to force the percentage display for a
non-terminal, and no alternate progress channel (no `--progress-fd` or similar). Allocating
a pty to get it would be a real new dependency for a number that, per §2's own reasoning,
nothing currently consumes — cut, rather than built and left unused.

What that leaves for WAV → FLAC: `run_cancellable` still turns cancellation from "wait for
this whole file's `flac` to finish" into "kill it now", which was the actual named gap
(§2, §0). The job just reports `Started` / `Finished` like any other opaque single-call
job (§2's "sub-item level" category) — that was already true for checksum/verify/sbe in J1;
WAV → FLAC joins them instead of getting a number nothing can produce.

**`run_cancellable` drains the child's stdout and stderr on their own threads, not after
`wait()`.** `Command::output()` (`run`'s old body, now `run_cancellable(tool, args, &mut ||
true)`) reads both pipes only once the child has exited, which is fine when nothing is
watching in between. Polling `try_wait()` in a loop instead means the child can sit blocked
on a full stderr pipe while the loop is busy sleeping between polls — `flac`'s own banner is
small enough that this was never observed in testing here, but a reader thread per pipe
removes the possibility rather than relying on the message staying short forever.

**Frame-level progress for FLAC → WAV reused `hash_pieces`'s exact shape**
(`FnMut(u32, u32) -> bool`, called once per unit with cumulative-done and total, `false`
means stop) rather than inventing a `(bytes, total_bytes)` variant — open question 4 asked
whether the shape generalizes past pieces, and decoding one FLAC block at a time onto
`done += block.duration()` against `total_frames` is the same shape with a different unit,
exactly as guessed. `total_frames` comes from STREAMINFO and is usually present; when it is
not (`probed.stream_info.total_frames == None`, legal but rare — a streaming encoder that
never learned the sample count), `to_wav_with_progress` reports total `0` and the CLI shows
a bare done-count instead of a fraction, rather than lying about a denominator it does not
have.

**`Progress<T>` grew a `detached()` constructor** so `run_batch`'s single-file fast path
(§3 — deliberately skips the queue) can still hand `to_wav_with_progress` /
`to_flac_cancellable` a real `Progress` to check `is_cancelled()` against, with its own
fresh `CancelToken` wired to the same Ctrl-C handler either path installs. It sends into a
channel nobody ever reads (the receiver is dropped immediately), which is fine — `submit`'s
own `Progress` already tolerates a closed channel the same way, since a queue can be torn
down out from under a slow job. This is the one place `job` gained API surface *for* a
caller pattern rather than for the queue's own bookkeeping; it is a general enough shape
(a cancel token plus a progress sink) that it did not seem worth gating behind `cfg(test)`
or similar.
