# GUI

`lh-gui`, milestone M3 in `PLAN.md` — the last v0.1 piece besides packaging (M4). Everything
in `PLAN.md` §2's scope table is implemented and exposed through `lh-cli` already (M1/M2
done, §0); this crate is a second front end onto the same `lh-core`, per Principle 4. It adds
no domain logic of its own — a Verify button calls `lh_core::analysis::verify` exactly as
`lh verify` does, and the job queue it runs through is the one `docs/job-queue.md` built for
`lh-cli`'s own batch commands.

---

## 0. What already exists

*Established 2026-08-30 by reading the current tree and building a real Iced 0.14 program
against it, not by guessing at what a job-queue GUI usually looks like.*

* **`lh-gui` is a placeholder.** `lh-gui/src/main.rs` prints "not implemented yet" and exits;
  `lh-gui/Cargo.toml` depends on nothing but `lh-core`. No GUI framework is in the workspace
  yet — `iced` is named in `PLAN.md` §5 but not in `Cargo.toml`.
* **`lh-core::scan::scan` already produces exactly PLAN.md §4's working set**: a
  `WorkingSet { files: Vec<AudioFile>, skipped: Vec<(PathBuf, String)> }`, walked recursively,
  never silently dropping a file it could not read. The file table's rows are `AudioFile`
  as-is — path, `AudioFormat`, `file_size`, `StreamInfo` (rate/bits/channels/duration via
  `duration_secs()`), and the encoder vendor string. Nothing needs inventing here.
* **`lh-core::job::Queue<T>` (`docs/job-queue.md`) is done and already UI-agnostic** — J1/J2
  built it, wired into `lh-cli`, with no GUI-specific type anywhere in it. `Queue<T>::events()`
  today returns `&Receiver<Event<T>>` (`lh-core/src/job/mod.rs:194`), borrowed from `&self`;
  every existing caller (`lh-cli/src/main.rs:69,588`, `lh-core/tests/job.rs`) only ever calls
  `.recv()`/`.recv_timeout()`/iterates it, methods that take `&self` on `Receiver` — so an
  owned clone works at every one of those call sites exactly as the borrow does today. §3
  below is why the GUI needs that.
* **`lh-core::tools::Registry`** (`lh-core/src/tools/mod.rs`) already has `entries()` and
  `missing_required()` — the Tools panel (PLAN §4) is a direct read of it, no new plumbing.
* **`lh-cli/src/main.rs`'s `run_batch`** is the reference implementation for how a batch of
  jobs becomes user-visible output — submit one job per file, drain `events()`, buffer
  per-file results to print in submission order, stream the progress *count* live
  (`docs/job-queue.md` §7). The GUI's job-queue panel is the same shape rendered as widgets
  instead of printed lines, not a redesign.
* **`Cargo.toml` resolves `iced = "0.14.0"`, released and current** (checked in-session via
  `cargo add --dry-run`); its default features pull `wgpu` + `winit`. A real Iced 0.14
  program was built and run against this machine's X11 display (§G0) rather than trusting
  crate docs, because the last two features this repo depended on sight-unseen — `flac`'s
  stderr progress (`docs/job-queue.md` §8) and `WAVE_FORMAT_EXTENSIBLE` support in shntool
  3.0.4 (`PLAN.md` §0.3) — both turned out not to work the way they were assumed to.

---

## G0. The Iced/job-queue spike

*Run 2026-08-30, in a scratch crate outside this repo, against real `iced = "0.14.0"`.*

The one real unknown going into M3 was `docs/job-queue.md` §4/§6 Q4's own hedge: "the
expectation (**unverified until M3 exists**) is that the Iced `Subscription` is a thin
adapter over `events()`, not a redesign." That was checked directly: a background thread
sent five `Progress` events plus a `Done` over a `crossbeam_channel`, an Iced 0.14 app
subscribed to it, and `update` received all five in order before `done` — confirmed by
`eprintln!` output alongside the running window, not just a successful compile.

**What it proved:**

* The bridge is real and is a thin adapter, confirming the hedge. `crossbeam_channel::Receiver`
  is not `Hash`, and `Subscription::run_with<D, S>(data: D, builder: fn(&D) -> S)` requires
  `D: Hash + 'static` (`iced_futures-0.14.0/src/subscription.rs:198`) — so the adapter is a
  small wrapper struct carrying a stable numeric id (hashed) alongside the cloned `Receiver`
  (not hashed), not a queue redesign.
  ```rust
  struct QueueEvents<T> { id: u64, rx: crossbeam_channel::Receiver<Event<T>> }
  impl<T> Hash for QueueEvents<T> {
      fn hash<H: Hasher>(&self, state: &mut H) { self.id.hash(state); }
  }
  ```
  The `id` must stay **constant** across `view` calls for one logical subscription — Iced
  diffs subscriptions by this hash each frame and tears down / restarts the stream when it
  changes (`run_with`'s own doc: "data... will be used to uniquely identify the
  Subscription"). A changing id would silently drop events on every redraw; §2 below is why
  a single long-lived queue makes this trivial to get right (one id, forever) rather than
  something to recompute per job.
* `builder: fn(&D) -> S` is a plain function pointer, not a capturing closure — confirmed by
  the compiler (a closure that captured anything from its environment failed to coerce). The
  non-capturing closure literal used in the spike (`|data: &QueueEvents| { ... }`) captures
  nothing itself; everything it needs comes through `data`. This matches
  `application(boot, update, view)`'s `boot: impl BootFn<State, Message>` having the same
  shape one level up (`iced-0.14.0/src/application.rs:535`).
* `iced::stream::channel(size, async move |output| { ... })` (`iced_futures-0.14.0/src/stream.rs:11`)
  is the documented way in, and a **blocking** `for event in rx.iter() { output.send(...).await }`
  inside that async block did not stall the window — five 150ms-apart events rendered live
  and the app stayed responsive throughout. Subscription streams run on Iced's own
  executor, off the render/event-loop thread; this was the thing worth confirming rather
  than assuming, since it is the same kind of "blocks the wrong thread" mistake
  `docs/job-queue.md` §8 found in `flac`'s own `Command::output()`.
* `iced::event::listen_with(fn(Event, Status, window::Id) -> Option<Message>)`
  (`iced_futures-0.14.0/src/event.rs:26`) exists and surfaces
  `window::Event::FileDropped(PathBuf)` / `FileHovered` / `FilesHoveredLeft`
  (`iced_core-0.14.0/src/window/event.rs:56,66,76`, reached via winit's `DroppedFile`/
  `HoveredFile` — `iced_winit-0.14.0/src/conversion.rs:334-341`). `PLAN.md` §4's "whole
  window as a drag-and-drop target" was a claim about Iced's capability that had not been
  checked before now; it holds, batched alongside the job-queue subscription via
  `Subscription::batch` (`iced_futures-0.14.0/src/subscription.rs:212`).

**What it did not prove — real limits, not oversights:**

* Only run under X11 on Linux, in a 3-second window, with one worker thread and five
  events. Behavior under real rayon-pool load (a 30-file batch, several queues' worth of
  concurrent progress), on Wayland, or on macOS/Windows, is unchecked.
* Nothing about `wgpu` vs `tiny-skia` rendering backend choice was evaluated — the spike
  built with the `tiny-skia` feature for a smaller/faster compile, not because `wgpu` was
  ruled out.
* No cancel button was wired — the spike only receives events, it does not send a
  cancellation back through `CancelToken`, which is a plain method call
  (`Queue::cancel()`/`cancel_token()`) needing no Subscription machinery, so it was not
  the open question here.

---

## 1. What "the GUI" means here

One process, one window, one long-lived `Queue<JobOutcome>` (§2) for as long as the window
is open — a real change from every existing `Queue<T>` user, which is a single CLI
invocation that builds a queue, runs a batch, and exits. `docs/job-queue.md` §6 Q3 asked
"one `Queue` per batch command, or one long-lived queue for the whole process?" and
deferred it to "when M3 exists." M3 now exists: the GUI needs one queue so that starting a
convert while a verify is still running shows both in one job-queue panel, which is
PLAN.md §4's own picture ("Job queue — per-file and aggregate progress"), and that requires
either a single shared queue or a merge step over several — the single queue is simpler and
there is no caller yet who needs more than one rayon pool.

Everything the GUI does, `lh-cli` already does (Principle 4): scan a folder, verify,
checksum (ffp/md5/st5), convert, create/check a torrent, discover tools. `lh-gui` adds a
window, a working set held in memory, and a queue that outlives any single operation; it
adds no new operation `lh-core` does not already expose.

---

## 2. Design

### One `Queue<JobOutcome>`, not one `Queue<T>` per operation

`job::Queue<T>` is generic on purpose so the `job` module never imports an operation's
result type (`docs/job-queue.md` §2). That coupling rule still holds here: `JobOutcome` is
defined in `lh-gui`, not `lh-core`, as the one closed enum the GUI's single queue needs:

```rust
// lh-gui/src/job.rs
enum JobOutcome {
    Verify(lh_core::Result<Verification>),
    Checksum(ChecksumKind, lh_core::Result<[u8; 16]>),
    Sbe(lh_core::Result<Sbe>),
    Convert(lh_core::Result<Conversion>),
    TorrentCreate(lh_core::Result<torrent::Created>),
    TorrentCheck(lh_core::Result<torrent::CheckReport>),
}
```

`App` owns one `job::Queue<JobOutcome>` for the process's lifetime. Every button (Verify,
Convert, Create torrent, ...) submits jobs whose closures wrap their operation's real return
value in the matching `JobOutcome` variant — the same pattern `lh-cli`'s batch commands use
today, just against one shared queue instead of one built per command.

### The subscription

```rust
struct QueueEvents { id: u64, rx: crossbeam_channel::Receiver<job::Event<JobOutcome>> }
impl Hash for QueueEvents { /* hash id only, per §G0 */ }

fn subscription(app: &App) -> Subscription<Message> {
    Subscription::batch([
        Subscription::run_with(
            QueueEvents { id: 0, rx: app.queue.events() },
            |data| iced::stream::channel(64, async move |mut output| {
                for event in data.rx.iter() { let _ = output.send(Message::Job(event)).await; }
            }),
        ),
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::PathDropped(path))
            }
            _ => None,
        }),
    ])
}
```

`id: 0` is fixed because there is exactly one queue for the app's whole life (§1) — no
per-job or per-batch id to keep synchronized, which is exactly the failure mode §G0 flagged
for a changing hash. `Message::Job(job::Event<JobOutcome>)` folds into `App` state the way
`run_batch` folds events into printed lines: `Started`/`Progress` update a row's status in
the file table and the aggregate counter in the job-queue panel; `Finished`/`Cancelled`
match on the `JobOutcome` variant to know which result type to render and log.

### The one `lh-core` touch this needs

`Queue::events(&self) -> &Receiver<Event<T>>` (`lh-core/src/job/mod.rs:194`) returns owned
instead: `pub fn events(&self) -> Receiver<Event<T>>` (a cheap `Receiver` clone — crossbeam's
`Receiver` is designed to be shared this way, and every message still goes to exactly one
of a channel's live receivers, which is fine here since the GUI is the only consumer of its
own queue). Confirmed call-site compatible (§0): every current caller only calls `&self`
methods on the result, which work identically on a clone. This is additive to
`docs/job-queue.md`'s design, not a revision of it.

### Layout

Follows `PLAN.md` §4 §4 directly — five regions, no new ones invented:

* **Path bar / Add folder** — a text input plus button calling `scan::scan`, and the
  drag-and-drop target from §G0 doing the same.
* **File table** — a `scrollable` `Column` of rows from `WorkingSet.files`
  (`PLAN.md` §4: no virtualization needed at working-set size). Each row shows name, format,
  duration, rate/bits/channels, SBE (from `analysis::sbe`, computed at scan time since it is
  header-derived and cheap), and a per-row status cell driven by `job::Event`s addressed to
  that row's `JobId`.
* **Operation panel** — pick an operation and options (e.g. overwrite, `--tracker` for
  torrent create), Run submits one job per selected file to the shared queue.
* **Job queue panel** — aggregate `N of M done` plus per-job rows, a Cancel button calling
  `Queue::cancel()` directly (a plain method call — no subscription needed to send a
  cancellation, only to receive progress).
* **Log / audit pane** — renders `Provenance::render()` (`lh-core/src/tools/runner.rs:55`,
  already exists) for every finished job, exportable to a text file.
* **Tools panel** — `Registry::entries()` / `missing_required()`, already exists (§0).

`PLAN.md` §10 Q1 (soft cap on working-set size) stays answered as "deferred, no cap in
v0.1" — nothing here reopens it; the file table is exactly the bounded case that decision
already covers.

---

## 3. Out of scope for G1

* Byte-level or frame-level progress bars beyond what `job::Event::Progress` already
  carries (`docs/job-queue.md` §2, §8) — a convert job with no `total_frames` shows a
  bare count, same as the CLI.
* Torrent check/create panels (T4/C5 in their own docs) — G1 is file-table + verify/checksum
  first, matching how M1/M2 built read-only analysis before conversion and torrent work.
* Anything about packaging, bundled `flac` sidecars, or notarization — M4.
* Resolving `docs/job-queue.md` §6 Q1 (worker count) or Q2 (`--jobs` flag) — no evidence yet
  that the GUI's default load differs from the CLI's.

---

## 4. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**G0**~~ | ~~Iced/job-queue spike~~ | **Done** — confirmed the `Subscription` bridge, the `Hash`-identity requirement, that blocking `rx.iter()` inside `stream::channel` does not stall the window, and that window-level drag-and-drop is available. See §G0. |
| ~~**G1**~~ | ~~Scaffold + file table~~ | **Done** — `iced` + `rfd` in the workspace, path bar (text input, Browse via `rfd::AsyncFileDialog`, Scan), window-wide drag-and-drop, `WorkingSet` → file table (name, format, duration, rate/bits/channels, SBE), Tools panel off `Registry::entries()`. Read-only: no queue yet. 4 tests in `lh-gui/src/main.rs`. See §G1 notes. |
| ~~**G2**~~ | ~~Job queue wired~~ | **Done** — one long-lived `Queue<JobOutcome>`, the subscription adapter, an operation panel (verify / ffp / md5 / st5 / sbe) that runs every file in the working set, per-row status via `App::latest_job_by_path`, an aggregate `N of M done` and per-job job-queue panel, and a working Cancel button. 2 new tests against the real fixture corpus and a real `Queue`, plus a real (non-interactive) run under X11. See §G2 notes. |
| ~~**G3**~~ | ~~Convert + log pane~~ | **Done** — convert (both directions) joins the operation panel with a direction picker and an Overwrite checkbox, through the same queue, with real per-file progress (`to_wav_with_progress`) and a real mid-run cancel (`to_flac_cancellable`, J2). A file already in the target format submits no job at all rather than showing FAILED for a no-op. A log/audit pane renders `Provenance::render()` for every finished job that produced one, with an Export button. 4 new tests: two through the real queue (WAV write + FLAC write via the real reference `flac` binary), one proving the already-in-format skip submits nothing, none against a mocked queue. See §G3 notes. |
| **G4** | Torrent panels | Create and check, per `docs/torrent-creation.md` C5 and `docs/torrent-verification.md` T4 — both already named the job queue as their dependency; it now exists. |

---

## 5. Open questions

1. **`wgpu` or `tiny-skia` as the shipped renderer?** §G0 built with `tiny-skia` for compile
   speed, not as a decision — `wgpu` is Iced's default and likely the safer cross-platform
   choice for packaging (M4); revisit with real measurements once G1 exists to measure.
2. **Per-row `JobId` bookkeeping** — `JobId::index()`'s "position in submission order"
   guarantee (`docs/job-queue.md` §7) held because every existing caller builds one dense
   `Queue` per batch. A single long-lived queue (§1) breaks that: `JobId`s are still unique
   but no longer a dense 0..N index into "the current batch," so the file table needs its
   own `HashMap<JobId, row index>` rather than reusing that shortcut. Worth confirming this
   is the only place the dense-index assumption was leaned on before G2 is written.
3. **Wayland and macOS/Windows** — §G0 only ran on X11/Linux. `PLAN.md` §9 already flags
   Windows path/Unicode handling as a risk; the drag-and-drop event path in particular
   (`DroppedFile`/`HoveredFile`) should be re-checked on each platform once CI (M4) can run
   the GUI crate, not assumed to be identical everywhere winit compiles.
4. ~~**Does the log pane need `report/`?**~~ **Resolved in G3: no.** `App::log` is
   `Vec<String>` — `Provenance::render()`'s text from every finished job that produced one,
   appended in the order jobs finish, exported by joining with `\n`. Nothing G3 built
   (Export button, the per-line scroll, a real multi-operation session mixing verify and
   convert) needed more structure than that. Revisit only if a real caller needs to filter,
   sort, or re-render the log some other way than "read top to bottom" — none has yet.

---

## G1 notes

*Landed 2026-08-30.*

* **`iced::application`'s `boot` argument being `impl BootFn<State, Message>` (a `Fn() -> C`)
  meant `App::boot() -> (App, Task<Message>)` as an associated function, not a closure** —
  `App::boot` coerces to the right shape directly (§G0's non-capturing-closure finding, one
  level up: an ordinary top-level or associated `fn` always satisfies it, which is simpler
  than the wrapped-in-a-closure phrasing in Iced's own doc example).
* **`Task::perform` needed none of `Subscription::run_with`'s `Hash`-identity ceremony.**
  Its `f: impl FnOnce(A) -> T + MaybeSend + 'static` is an ordinary capturing closure
  (`iced_runtime-0.14.0/src/task.rs:48-51`) — a one-shot future (the Browse button's
  `rfd::AsyncFileDialog::pick_folder()`) is not something Iced needs to diff across `view`
  calls the way a long-running subscription stream is, so the wrapper struct §G0 needed for
  the job-queue bridge does not generalize to every async boundary. G2's actual queue
  subscription will still need it.
* **A bare `.map(...).unwrap_or_else(...)` building an `Element` failed to infer `Theme`**
  (`iced::widget::text::Catalog` unsatisfied) until the branch was annotated
  `let error: Element<'_, Message> = ...`. Cosmetic, but worth naming: Iced 0.14's default
  `Theme` type parameter only resolves inside a context that already commits to a concrete
  `Element<Message>`, not through an inferred closure return.
* **What this did not check, and could not in this environment**: the window ran for
  several seconds under this machine's X11 display without panicking, and `App::scan`,
  `sbe_label`, `format_duration`, and `tool_line` are covered by tests against the real
  fixture corpus (`lh-core/tests/fixtures`) and a live `Registry::discover()` — but nobody
  clicked Browse, dropped a file, or looked at the rendered table. No screenshot tool was
  available in this sandbox to confirm layout, spacing, or that the drag-and-drop path
  fires end to end outside of the isolated §G0 spike. That is a real gap the plan's own
  standard (`docs/job-queue.md` and this doc's own §G0 both distinguish "compiles" from
  "was run") says not to paper over — worth a manual pass before G1 is trusted further.

---

## G2 notes

*Landed 2026-08-30.*

Four things the implementation forced or corrected, none visible from §2's sketch:

* **`Sbe` carries `Sbe` directly, not `Result<Sbe>`.** §2's `JobOutcome` sketch guessed
  `Sbe(lh_core::Result<Sbe>)` before checking `analysis::sbe`'s real signature
  (`lh-core/src/analysis/sbe.rs:18`): it takes an already-probed `&StreamInfo`, not a path,
  so it cannot fail the way `verify`/`compute` can. `JobOutcome::Sbe(Sbe)` is what actually
  got built (`lh-gui/src/job.rs`).
* **`Message` needing `Clone` forced a second, render-at-the-boundary type.** Iced's own
  widgets (`text_input`, `pick_list`, `button::on_press`, ...) require `Message: Clone`
  wherever they are used in `view` — not just at the one call site that builds a
  `Message::Job` — and `JobOutcome`'s `lh_core::Result`s carry `lh_core::Error`, which is
  not `Clone` (it wraps `std::io::Error`, `claxon::Error`, `metaflac::Error`, none of which
  are). Rather than making `Error` fake-Clone or dropping `Clone` from every other widget's
  requirements, `job::JobUpdate` was added: `Event<JobOutcome>` renders into it (via the
  same `render()` that produces the file table's/job-queue panel's strings) the moment it
  comes off the queue's channel, inside the subscription — so `Message` only ever carries
  `String`/`Result<String, String>`, never the raw `lh_core::Error`. §2's sketch of
  `Message::Job(job::Event<JobOutcome>)` does not compile as written; this is the fix, not
  a redesign of the bridge itself.
* **The queue's `CancelToken` needed a `reset()` that nothing before G2 had a reason to
  add.** `Queue::submit` checks one `CancelToken` shared for the *queue's* whole life, and
  every existing caller — every `lh-cli` batch command, `torrent create` — builds a fresh
  `Queue` per invocation and exits, so a one-way cancel flag was never a problem before now.
  `lh-gui`'s single long-lived `Queue<JobOutcome>` (§1) is the first caller that outlives
  its own cancellation: without a reset, one Cancel press — even with nothing in flight —
  would silently stop every *future* Run from ever executing a job, for the rest of the
  window's life, since the flag is checked again on every later `submit`. `CancelToken`
  grew `reset()` (`lh-core/src/job/mod.rs`), and `App::run_operation` calls it before
  submitting each batch. `cancelling_with_nothing_in_flight_does_not_disable_the_next_run`
  (`lh-gui/src/main.rs`) is there because this is exactly the kind of bug that compiles
  clean and only shows up the second time a user clicks Run after Cancel — worth catching
  in a test rather than in the field, per this repo's own standard for what counts as
  checked.
* **`Subscription::run_with`'s builder needed the `Receiver` cloned out of `&QueueEvents`
  before entering the `async move` block, not borrowed from it.** §2's sketch (copied from
  §G0's spike) wrote `for event in data.rx.iter() { ... }` directly inside
  `async move |mut output| { ... }` where `data: &QueueEvents` is the builder's own
  parameter — that fails to compile here with "lifetime may not live long enough": the
  async block would be holding a borrow tied to the builder call's own stack frame, but the
  stream it returns has to outlive that call. Cloning first —
  `let rx = data.rx.clone(); iced::stream::channel(64, async move |mut output| { for event
  in rx.iter() { ... } })` — moves an owned, `'static` `Receiver` into the block instead
  (crossbeam's `Receiver` clone is cheap and shares the same channel, per `events()`'s own
  doc). Unclear whether §G0's spike avoided this because its own `QueueEvents` was
  constructed differently or because the exact code was never actually copy-pasted verbatim
  — worth noting so a future doc sketch is not trusted byte-for-byte without compiling it.

**What this did and did not check.** `App::run_operation` and `App::handle_job_event` are
now exercised against a real `Queue<JobOutcome>` and the real fixture corpus
(`lh-core/tests/fixtures`) in two tests — `cdda-aligned.flac` verifies OK, `wrong-md5.flac`
reports a mismatch, and a Cancel-with-nothing-in-flight followed by a real Run produces no
`Cancelled` rows — draining `queue.events()` directly rather than through Iced's own
`Subscription`, since nothing outside a running window can drive that. The compiled binary
was also run for real under this machine's X11 display (`timeout 6
./target/debug/little-helper`, exit 124 — still running, no panic) as a smoke check, the
same bar §G0 and G1 used. What is still unchecked, same gap G1 already named: nobody
clicked Browse, Run, or Cancel, or watched the job-queue panel update live — no screenshot
or input-automation tool is available for a native window in this sandbox, only for a
Chrome tab. A manual pass (or a future in-repo integration harness) is still owed before
the operation panel is trusted end to end.

---

## G3 notes

*Landed 2026-08-30.*

* **`destination()` moved from `lh-cli` into `lh_core::convert`, generalized from
  `&AudioFile` to `&Path`.** It was a private fn in `lh-cli/src/main.rs` building "same
  stem, new extension, beside the source" as an `OsString` (not `with_extension`, which
  eats everything after the last dot — `gd77-05-08.d1t01.flac` — and the fixture corpus
  carries non-UTF-8 names on purpose). `lh-gui` needed the exact same logic for its own
  convert destinations; duplicating a correctness-sensitive detail like non-ASCII path
  handling across both front ends is exactly what Principle 4 exists to avoid, so it was
  lifted rather than copied. `lh-cli/src/main.rs`'s own `destination()` is now a call to
  `lh_core::convert::destination`; no behavior changed, confirmed by the existing
  `lh-core/tests/convert.rs` and `lh-cli` suites still passing unmodified.
* **The "already in the target format" case is a skip, not a job — matching `lh-cli`'s own
  `ConvertOutcome::Skipped`, which was not obvious from §2's sketch.** The first draft had
  `convert_to_wav`/`convert_to_flac` return `Err(Error::malformed(path, "already WAV"))`
  for this case, which `job::render` would have shown as `FAILED: already WAV` in the
  job-queue panel — wrong, since nothing about the file is actually wrong. `lh-cli`'s
  `cmd_convert` treats this as `ConvertOutcome::Skipped`, printed `SKIPPED ... (already
  {want})`, never touching the error path. Fixed by filtering in `run_operation` before
  `submit` is ever called: a file already in the target format gets no `JobId`, no
  job-queue row, and its file-table status is whatever it already was — not by inventing a
  third `JobOutcome` variant for "ran but did nothing."
  `converting_to_the_format_a_file_is_already_in_submits_no_job` is the regression test for
  the bug that was caught before it was committed, not after.
* **The log pane's provenance has to be extracted from `JobOutcome` at the same boundary
  `Message: Clone` already forced in G2, not later.** `JobUpdate::Finished` grew a
  `provenance: Option<String>` field, filled by a new `provenance_of(&JobOutcome) ->
  Option<String>` called inside `From<Event<JobOutcome>> for JobUpdate` — the same place
  `render()` already turns the outcome into its short status line, for the same reason:
  once `Message::Job` exists, the raw `JobOutcome` (and the `lh_core::Error` it can carry,
  which is not `Clone`) must not cross into it. `App::handle_job_event` just pushes
  `Some(text)` onto `App::log`; only `JobOutcome::Convert(Ok(_))` produces one today, per
  §5 open question 4's resolution.
* **`iced::widget::checkbox` is a builder, not a struct literal or a helper with inline
  args.** `checkbox(is_checked).label("...").on_toggle(Message::OverwriteToggled)` —
  consistent with every other Iced 0.14 widget already in this file (`text_input`,
  `pick_list`, `button`), but worth confirming against `iced_widget-0.14.2/src/checkbox.rs`
  rather than assumed, per this doc's own standard.

**What this did and did not check.** Four tests run `App::run_operation` against a real
`Queue<JobOutcome>` and real files in a `tempfile::tempdir()` (never the read-only fixtures
dir, since convert actually writes): FLAC → WAV against `cdda-aligned.flac` writes a real
`.wav`, is reported "checked against source" (the fixture carries a STREAMINFO MD5), and
logs one `FLAC → WAV` provenance entry; WAV → FLAC against `cdda-aligned.wav` runs the real
`flac` binary found on this machine (skips with a note if `flac` is absent, the convention
`lh-core/tests/convert.rs` already uses) and writes a real, independently-checked `.flac`;
and the already-in-format case submits zero jobs. This is the first `lh-gui` milestone
whose tests actually invoke the reference `flac` binary, not just in-process code — G2's
real-`Queue` tests only ever exercised verify/checksum/sbe, none of which shell out.
Cancellation mid-convert (`Progress::is_cancelled` reaching `to_flac_cancellable`'s killed
child, J2's whole point) is exercised by `lh-core/tests/convert.rs` already, not re-tested
here — `lh-gui`'s own wiring only needed proving that pressing Cancel calls
`Queue::cancel()` (G2) and that `is_cancelled()` is threaded into both closures (this
milestone), not that killing a real `flac` child works, which is `lh-core`'s job to prove
and already does. Same unchecked gap as G1/G2: nobody clicked the real Convert button, the
Overwrite checkbox, or Export in a running window — no input-automation tool exists here
for a native window, only for a Chrome tab. The compiled binary was run for real under X11
(`timeout 6 ./target/debug/little-helper`, exit 124, no panic) as the same smoke check.
