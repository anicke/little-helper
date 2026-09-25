# TUI

`lh-tui`, not currently a milestone in `PLAN.md` at all — the two commits that created it
(`c26f810`, `50fed19`, `6ef4226`) went in without a plan doc, unlike every other feature in
this repo (`docs/gui.md`, `docs/gui-shell.md`, `docs/job-queue.md`, `docs/torrent-creation.md`,
`docs/torrent-verification.md` all precede or accompany their code). This doc exists to close
that gap: record what already shipped, decide what "the TUI" is for, and plan the screens
still missing.

Like `lh-gui`, it adds no domain logic of its own (Principle 4) — a screen calls the same
`lh_core` functions `lh` and `lh-gui` do, through the same `job::Queue`.

---

## 0. What already exists

*Established 2026-08-31 by reading `lh-tui/src/main.rs` and `lh-cli/src/lib.rs` directly.*

* **`lh-cli`'s command logic lives in `lib.rs`, `main.rs` is a thin wrapper.**
  `lh-cli/src/lib.rs`'s public surface is exactly `Cli`, `Command`, the arg structs
  (`Paths`, `ChecksumArgs`, `ConvertArgs`, `TorrentCommand`, ...) and `run`
  (`docs/architecture-cleanup.md` A2); every `cmd_*` function, plus `collect` and
  `checksum_kind_for`, is private to the crate. `lh-cli/src/main.rs` is just
  `Cli::parse()` + `run(cli)` + exit code. `lh-tui` depends on `lh-cli` as a library and
  parses the identical `Cli`, so every subcommand `lh` accepts, `lh-tui` accepts too, with
  identical flags — there is exactly one command grammar in the workspace, not two kept in
  sync by hand. Anything a screen needs beyond the grammar — collecting files, telling a
  checksum file's kind from its extension — it gets from `lh_core` directly (`scan::collect`,
  `ChecksumKind::from_path`) rather than reaching into `lh-cli`'s internals.
* **Only `Command::Verify` has a screen.** `main()` matches on `cli.command`: `Verify` goes
  to `run_verify`, everything else to `run_headless`, which calls `lh_cli::run(cli)` and
  prints exactly what `lh` would — no ratatui, no alternate screen. This is deliberate and
  stated in the module doc comment, not an oversight to route around silently.
* **The verify screen's shape**, which every future screen should default to unless a
  command's own result type says otherwise:
  * A local `collect(&paths)` (`lh_core::scan::collect` plus the same skip-reporting every
    `lh` command does) for the file list.
  * One `job::Queue<T>` for the screen's lifetime, one job per file, submitted before the
    draw loop starts.
  * A `FileRow { name, status }` table, `Status` a small enum matching the operation's own
    result shape (verify's: `Pending`/`Running`/`Ok`/`NoMd5`/`Mismatch{..}`/`Failed(String)`),
    updated by draining `queue.events()` (`Started`→`Running`, `Finished`→ match the output,
    `Cancelled`→`Failed("cancelled")`) once per frame before drawing.
  * A four-row layout: header (name + root + counts + elapsed), the table, a `Gauge`
    (ratio, colored by outcome, labelled with the same counts `lh`'s own batch summary
    would print), a one-line footer (`q / esc quit`).
  * `q`, `Esc`, and `Ctrl-C` all call `queue.cancel_token().cancel()` and break the loop —
    not `std::process::exit` — so `ratatui::restore()` always runs before the process ends.
  * The screen's own return value is "did everything come back clean," the same notion
    `lh verify`'s exit code already uses, so `$?` means the same thing whichever binary ran.
* **A shared `Theme`** (`lh-tui/src/main.rs:57`): six named `Style`s (`accent`, `ok`, `warn`,
  `error`, `dim`, `header`), built once per screen and threaded through every draw function
  as `&Theme`, rather than `Style`s inlined at each call site. Built deliberately without
  `.bg(...)` on any named color — ratatui's named colors resolve through the terminal's own
  ANSI palette, so an inverted background (`.bg(Color::Cyan)` for a header row, say) assumes
  a dark terminal and reads wrong on a light one; bold/underline read as "header" or "accent"
  regardless of background. Any new screen's colors belong in `Theme`, not inlined — that
  was the entire point of the refactor commit that produced it.
* **`SPINNER` is a free `const`**, shared by whichever screen wants a running-indicator glyph
  (`['⠋', '⠙', '⠸', '⠴']`, advanced one frame per `tick`). Not yet threaded through `Theme`
  since it is data, not a `Style`, but named here so a second screen reuses the constant
  instead of redeclaring it.
* **No test exists for anything in `lh-tui`.** `run_verify`/`run`'s draw loop takes a real
  `DefaultTerminal` and reads real terminal events, so it cannot be unit tested the way
  `lh-gui`'s `App::run_operation` is; `lh-core`'s own suite is what actually proves `verify`,
  `compute`, etc. are correct. What a screen's own code can be held to is the same bar
  `lh-gui`'s G-milestones use: compiled, and run for real (§0 below "real evidence" in every
  `gui.md`/`gui-shell.md` milestone) — a screenshot tool is unavailable for `lh-gui`'s native
  window, but a terminal screen can at least be captured as text; §6 records what has and
  has not actually been looked at.

---

## 1. What "the TUI" is for

Decided in this session, recorded rather than left implicit: **`lh-tui` stays a set of
independent per-command screens, not an interactive shell.** `lh-tui verify .` runs the
verify screen directly and exits; there is no menu, no rail, no mode where `lh-tui` with no
arguments lets you pick an operation and browse a working set interactively. This is the
opposite of `lh-gui`'s direction (`docs/gui-shell.md`'s whole point is one window, one shared
working set, switchable areas) and is a deliberate divergence, not a partial version of the
same idea:

* **A terminal invocation already names its command.** `lh-tui convert --to flac .` is not
  more work than opening a shell and picking "Convert" from a menu — the shell *is* the
  menu. `lh-gui`'s rail exists because a GUI has no argv to read; a TUI launched from a shell
  always does.
* **Every screen still needs its own view**, because the *result* shapes differ (a live
  per-file table plus gauge for a batch, a tree for `torrent info`, a diff-style table for
  `torrent check`) — sharing a shell would not remove that work, only add a picker in front
  of it that nobody asked for.
* **This can be revisited.** If a real interactive session (mixing several operations against
  one held-open working set, the way `lh-gui` does) turns out to be wanted, it is a new
  document, not a retrofit of this one — nothing in §2's per-screen design forecloses it,
  since each screen is already self-contained.

**Revisited 2026-09-23: the workspace.** Preparing a show is in practice always the same
sequence on one folder — rename → convert → tag — and quitting to retype
`lh-tui <command> <folder>` between each was the friction. `lh-tui <folder>` (no
subcommand; `lh-tui` alone means the current directory) now opens a small menu
(`screens/workspace.rs`) listing rename, convert → FLAC, tag, verify and convert → WAV.
Each entry opens the *same* screen its subcommand does, on an already-open terminal, and
quitting it returns to the menu. That is narrower than the "held-open working set" above: the
only shared state is the folder path, rescanned every time a step opens (a rename or a
conversion changes what is in it), plus each step's last result. Every subcommand works
exactly as before. To support it, a screen's setup is split from its draw loop
(`scan_folder`, `prepare_tag`, `find_encoder` return a `Refusal` rather than printing and
exiting), and rename/tag report `None` when left without applying, so the menu does not
mark them done.

The workspace's convert → FLAC runs with `--move-sources` on: once a WAV's FLAC has checked
out against it (`Conversion::move_source_to_originals`), the WAV moves into `_original/`
inside the show folder, so rename and tag after it see only the FLACs. Moved, never deleted;
a WAV that failed, was unchecked (8-bit), or would land on a file already in `_original/`
stays put and its row reads `KEPT`, which counts against a clean run. Standalone
`lh convert`/`lh-tui convert` only do this when asked, and refuse the flag with `--to wav`.

**Revisited 2026-09-24: every screen, not a workflow.** The menu listed only the five
steps of the usual sequence, so screens like checksum creation or the torrent ones were
unreachable from it. It now lists every screen, grouped — Prepare (rename, convert → FLAC,
convert → WAV, tag), Inspect (verify, sbe, sbe fix), Checksums (check, create
ffp/md5/st5), Torrent (create, info, check) — each with a one-line description, and no
longer advances the cursor after a clean step as if walking a sequence. What each opens on,
given only the folder:

* **check checksums** runs every `.ffp`/`.md5`/`.st5` in the folder, one screen after another.
* **create ffp/md5/st5** writes `<folder>.<ext>` inside the folder, only if every file
  computed (a partial list is never left looking complete) and never over an existing file.
* **sbe fix** is `sbe fix --in-place`: it opens on the plan (backward, no tail padding),
  `d` cycles the direction and `p` toggles tail padding, re-planning each time, and `a`
  applies it. Each file the plan changes is replaced under its own name and the file it
  replaced moves into `_original/sbe-fix/` — beside, not in, the `_original/` convert → FLAC
  puts its WAVs in, so a WAV show can be fixed first and converted after; files the plan
  leaves alone are not touched. A folder of WAVs is fixed as WAVs, with no encode. Once applying starts it can't be left until it's done
  (`fix_in_place` has no cancellation checkpoint).
* **create torrent** is `torrent create <folder>` (picker and all), writing
  `<folder>.torrent` beside it; **torrent info/check** use that same file, or else the one
  `.torrent` inside the folder.

Everything else about scope matches `lh-gui`'s own framing: `lh-tui` adds no operation
`lh-core` does not already expose (Principle 4), and a command with no screen yet keeps
working exactly as `lh` does — `run_headless` is not a placeholder to delete, it is the
permanent fallback for whichever commands never earn a screen of their own (`tools`,
`torrent info` and `info` are plausibly fine as plain text forever; §11 Q1).

---

## 2. The per-screen pattern

Distilled from §0 so the next screen does not have to reverse-engineer verify's:

```rust
fn run_x(args: XArgs) -> ExitCode {
    let (files, mut clean) = collect(&args.paths)?;   // same skip reporting as `lh`
    // ... early-return for an empty file list, exactly like run_verify ...
    let terminal = ratatui::init();
    let result = run(terminal, &label, files, /* screen-specific config */);
    ratatui::restore();
    // ... map `result` to an ExitCode the same way `lh x`'s own exit code works ...
}

fn run(mut terminal: DefaultTerminal, ..., files: Vec<AudioFile>) -> io::Result<bool> {
    let queue: Queue<T> = Queue::new();          // T = the operation's own result type
    let cancel = queue.cancel_token();
    for f in &files { /* queue.submit(...) one job per file, closing over what the
                          operation needs — verify's is `move |_| verify(&path)` */ }
    let events = queue.events();
    let mut rows: Vec<Row> = /* Pending, one per file */;
    loop {
        while let Ok(event) = events.try_recv() { /* fold into `rows` + running totals */ }
        terminal.draw(|frame| draw(frame, ...))?;
        if event::poll(Duration::from_millis(80))? {
            /* q / Esc / Ctrl-C -> cancel.cancel(); break */
        }
    }
    Ok(/* "everything came back clean," this operation's own definition of it */)
}
```

Fixed across every screen: the `80ms` poll interval (fast enough that quitting feels
immediate, slow enough not to burn a core spinning), the `q`/`Esc`/`Ctrl-C` triple all
mapping to cancel-then-break, draining every pending event before each draw rather than one
per frame (so a fast batch does not visibly lag the table behind the gauge), and using
`Theme` for every `Style` rather than inlining one. What varies per screen: `T`, the `Status`
enum's variants (they should mirror the operation's own result enum, the way verify's
mirrors `Verification`), and whatever the operation-specific header/footer needs to say.

**The header's elapsed time freezes once the work is done, not live `start.elapsed()`.**
Every screen keeps polling and redrawing at 80ms after its batch/job finishes too — that's
what lets `q` still quit a screen sitting on its final results — so a header computing
`start.elapsed()` directly on every frame would keep climbing for however long the user sits
there reading the table before quitting, which reads as the operation still running when it
is not. `header_elapsed(start, &mut finished_at, done)` (`lh-tui/src/main.rs:65`) is the fix
every screen shares: it returns a live, advancing value until `done` first turns true, then
returns that one frozen instant forever after, `finished_at` being the caller's own
`Option<Instant>` local to its run loop. `done` is each screen's own notion of "nothing left
to happen" — `done == total` for a per-file batch, `matches!(stage, ...Stage::Done(_))` for a
single-job screen, `matches!(stage, TagStage::Done)`/`RenameStage::Done` for the two editor
screens (their elapsed keeps advancing through `Editing`/`Writing` — that duration is real,
only the post-write idle wait should stop counting). `run_sbe_fix_plan_screen` (dry-run
preview, no job at all) and `run_sbe_fix_execute_screen` (which already `break`s the instant
its one job reaches `Done`, never idling on a finished screen) are the two exceptions with no
freeze — the first has no "done" to freeze at, the second is never idle long enough to matter.

A screen whose operation reports true sub-file progress (`convert`'s frame counts,
`torrent create`'s piece counts — both already flow through `job::Event::Progress` for
`lh-cli`'s own `run_batch`, §8 below) should render it; verify has no such screen because
`analysis::verify` never calls `Progress::report` (module doc, `lh-tui/src/main.rs:11`), not
because the pattern above cannot show one. §3 (Checksum) is the same shape as verify — no
sub-file progress exists for `checksum::compute` either.

**The editor screen**, added for Tag/Rename (§7) and not covered by anything above: every
screen through §6 only watches — it submits jobs to a `Queue` and folds events into a table,
taking no keyboard input beyond quitting. `tag`/`rename` edit, which forces two departures:

* **`q` cannot mean quit while a field has focus** — it is a letter someone might be typing.
  The key contract becomes: `Esc` leaves the focused field, and from no field leaves the
  screen; `Ctrl-C` always aborts, everywhere, unchanged; `q` (and, on these two screens, `a`
  to apply and `e` to enter the titles block) act only when nothing is focused. This is the
  one real break from the "`q`/`Esc`/`Ctrl-C` are identical" rule earlier in this section, and
  it is forced by having text fields at all, not chosen for its own sake.
* **Bracketed paste has to be enabled explicitly**, for the lifetime of these two screens
  only. `ratatui::init()` does not turn it on, so a pasted setlist would otherwise arrive as a
  stream of individual `Char` events racing the 80ms poll loop. `crossterm::execute!` toggles
  `EnableBracketedPaste`/`DisableBracketedPaste` around the screen's own `init`/`restore`, and
  `CtEvent::Paste(String)` is handled by splitting on newlines wherever a screen has a
  block worth pasting into (tag's titles list).

No new dependency for text entry: a `Field { value, cursor }` handling
`Char`/`Backspace`/`Delete`/`Left`/`Right`/`Home`/`End` (`lh-tui/src/main.rs`, shared by both
screens) covers every single-line field on both screens, and a `Vec<Field>` with a selected
index covers the titles block. The cursor is a glyph spliced into the rendered text, not a
real terminal cursor — that would need the widget to know its own screen coordinates inside
whatever layout is drawing it, which the table/list-heavy layouts here don't make cheap to
plumb through.

---

## 3. TUI2 — Checksum screen (ffp / md5 / st5) — done

One screen serves `Command::Ffp`/`Md5`/`St5`, parameterized by `ChecksumKind`, exactly as
`lh-cli`'s single `cmd_checksum(kind, args)` already serves all three (`lh-cli/src/lib.rs:376`)
— not three near-duplicate screens.

* **`Status`**: `Pending`, `Running`, `Ok([u8; 16])`, `Failed(String)`. No `Mismatch`/`NoMd5`
  variants — `checksum::compute` only ever succeeds with a digest or fails
  (`lh-core/src/checksum/mod.rs:76`), unlike `verify`'s three-way outcome.
* **Unlike verify, the digest itself belongs on screen.** Verify's detail column is blank for
  `Ok` (the interesting content is only in a mismatch); checksum's entire purpose is the
  digest, so the detail column shows `hex::encode(digest)` for every `Ok` row, not just
  failures. This is the one real content difference from verify's table, not just a
  find-and-replace of the status enum.
* **Order matters for the written file, same as `lh-gui`'s S3 checksum-create area**
  (`docs/gui-shell.md` §"S3 notes"): entries must land in submission order, not completion
  order, since a `.ffp` a person diffs across runs should not reorder itself because the
  queue's worker pool finished files in a different sequence. `ChecksumArgs.paths`'s file
  order (from `collect`) is the submission order; a `Vec` indexed the same way `rows` already
  is (by `JobId::index()`, dense per this screen's own single-batch queue — §0's per-row
  bookkeeping note in `gui.md` §5 Q2 about a *long-lived* queue losing dense indices does not
  apply here, since every TUI screen builds one queue per invocation and exits, same as every
  `lh-cli` batch command) is enough; no `HashMap<JobId, ...>` needed.
* **After the loop, same branch `cmd_checksum` already has**: build the `ChecksumFile` from
  every `Ok` row (in submission order), `write()` it if `--output` was given, otherwise
  render it to stdout — after `ratatui::restore()`, exactly where `run_verify` does nothing
  today because verify has no post-loop output. A quit-early (`q`/Ctrl-C) mid-batch still
  writes whatever finished before cancellation, matching `run_batch`'s own "partial output,
  not all-or-nothing" behavior for every other `lh` batch command.
* **Return value**: whether every file computed cleanly and (if `--output` was given) the
  write succeeded — same shape as `cmd_checksum`'s own `Ok(bool)`.

**Real evidence.** `cargo test --workspace` (136 tests, unchanged, plus `lh-cli`'s existing
suite covering `ChecksumArgs` now exposing public fields) and `cargo clippy --all-targets`
both clean. The compiled binary was run for real inside `tmux` (a real pty — this sandbox has
no screenshot tool for a native window, per every `gui.md`/`gui-shell.md` milestone, but a
terminal screen can be captured as text, which §6 above flagged as the actual bar): `lh-tui
ffp lh-core/tests/fixtures` showed the table filling in live (`running` → `OK`/`FAILED`),
each `OK` row's digest matching what `lh ffp` prints for the same files, the gauge coloring
red on the first failure, `q` restoring the terminal cleanly, and the post-quit stdout render
(`name:hash` lines) exactly matching `ChecksumFile::render()`'s FFP format. `-o /tmp/out.ffp`
against two fixtures wrote a file whose two lines matched submission order, not completion
order. Verify's own screen (§0, previously untested per this doc's own §6) was run the same
way over the same corpus: OK/NO MD5/MISMATCH/FAILED all rendered correctly and the gauge/exit
code (`1`, matching the real mismatch+failure) were correct — closing the gap §6 named.

---

## 4. TUI3 — Torrent create / check screens — done

Two screens, not one: `create` walks the whole payload as a single sequential
piece-hashing pass, so there is one job on a queue of one and one row of progress to show,
not a batch table; `check` produces a per-file `TorrentReport` only
once the whole pass finishes, so its screen shows a "hashing…" placeholder during the run
and the file table (the same shape as `lh-gui` G4's `TorrentFileRow`/`report_rows`, ported
to a ratatui `Table`) only after. Neither fits §2's per-file pattern, which is why they were
left out of it — a single job with sub-item progress, not N independent files.

* **Create's cancellation is real.** `create`'s progress callback returns a
  `bool` the same way `lh-cli`'s own `cmd_torrent_create` uses it — `false` stops the hash
  within one piece. So the create screen's `q`/`Esc`/`Ctrl-C` waits for the job's actual
  `Done` (bounded by one piece's hash time) instead of breaking the draw loop immediately
  the way every other screen does, so the reported outcome (cancelled vs. finished vs.
  errored) is the real one rather than a guess made before the job caught up.
* ~~**Check's cancellation is not.**~~ **Closed by docs/architecture-cleanup.md A3.**
  `check` (`lh-core/src/torrent/verify.rs`) gained the same cancellation checkpoint
  `create`'s progress callback already had — it returns a `bool` now, not `()` — closing the
  gap `lh-gui`'s G4 had hit and accepted rather than changing `lh-core`. The check screen now
  follows the same `want_quit`-then-wait-for-`Done` shape create's screen uses: `q`/`Esc`/
  `Ctrl-C` stops the hash within one piece instead of just abandoning the draw loop.
* **`--quick`** (`check_sizes`, no piece hashing) reuses the same screen and queue — it just
  produces a `Finished` event almost immediately, with no `Progress` events in between, so
  the "hashing…" placeholder is skipped in practice rather than needing its own code path.
* **Create's summary panel**, shown once the job finishes: file count, total size, piece
  count/length, infohash, and any excluded paths with `Skipped::reason()` — the same content
  `cmd_torrent_create` prints, plus the resolved tracker tiers and `private`/`source` flags
  shown above it throughout the run (known before hashing starts, unlike everything from
  `Created` itself). A cancelled or errored run replaces the summary with that outcome
  instead, colored `warn`/`error` on the gauge to match.
* **Check's table** skips `Padding` rows and appends `extra_local` as `EXTRA`, exactly like
  `lh-gui`'s `report_rows` and `lh-cli`'s `cmd_torrent_check` — the same three renderings of
  one `TorrentReport`, kept in step by eye since there is nowhere shared to put them
  (`lh-tui` does not depend on `lh-gui`, and vice versa).

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (136 tests, unchanged)
and `cargo clippy --all-targets` all clean; `cargo fmt --check` clean for `lh-cli`/`lh-tui`
(pre-existing `lh-gui` diffs are unrelated to this change, from a rustfmt version drift).
Run for real inside `tmux`: `lh-tui torrent create` against a two-file fixture folder wrote
a `.torrent` whose infohash on screen matched `lh torrent info` read back off disk;
`lh-tui torrent check` against that same torrent and payload reported both files `OK` with
`1 of 1 pieces verified`; corrupting one file's size (appending bytes) turned the same check
into `WRONG SIZE` for that file and `PARTIAL` for its piece-sharing neighbour, with the gauge
turning red and the label reporting `0 of 1 pieces verified, 0 failed, 1 unverifiable`
(matching `verify.rs`'s attribution rule: a shared piece convicts neither file outright);
`--quick` against the same corrupted payload reported `WRONG SIZE` / `SIZE OK` with no piece
count at all. `q` exited cleanly in every case (immediately for check, and — separately
confirmed by inspection of the cancellation-wait logic, since the fixture pieces hash faster
than a keypress — bounded by one piece for create).

## 5. TUI4 — Convert screen — done

Same per-file batch shape as verify/checksum (§2), not the single-job shape of torrent
create/check (§4) — `convert` decodes or encodes N independent files, one job each, same as
`cmd_convert`'s own `run_batch`. What makes it the richest screen (per the old note in this
section) is that a row's own progress is worth showing, unlike verify/checksum:

* **`ConvertOutcome`** mirrors `lh-cli`'s own (private) `ConvertOutcome` rather than
  exporting it — `Skipped`, `NoFileName`, `Done(Box<Conversion>)`, `Failed(lh_core::Error)`,
  the same small duplication every other screen's own `Status` enum already accepts (§0).
* **The encoder is discovered once, before the screen opens**, exactly like `cmd_convert`:
  `Target::Flac` with no `flac` found fails loudly (Principle 5) and exits before
  `ratatui::init()` runs, rather than after converting half a batch. Confirmed with
  `LH_FLAC=/nonexistent`: `lh-tui: encoding WAV to FLAC requires flac, which was not found
  (...)`, exit code 2, no screen drawn.
* **Rows show live sub-file progress where it exists.** `to_wav`'s
  `(frames done, frames total)` flows through `Progress::report` into `Event::Progress`,
  and a decoding row's status cell shows a live percentage instead of a bare spinner.
  `to_flac` has no such number — `flac` only draws its own percentage when
  stderr is a terminal, which piped through `Command` it never is (`convert/mod.rs`'s own
  doc comment), calling its progress with `(0, 0)` instead — so an encoding row just spins,
  the same as every other screen's `Running`.
* **The detail column carries the destination filename**, `-> name.ext`, and flags the
  weaker "unchecked" result (`checked_against_source == false`, source had nothing to
  compare against) in text rather than a separate status color — `OK` stays green either
  way, matching checksum's own choice to keep the status word simple and put the interesting
  content in the detail column (§3).
* **`--provenance` prints after the screen exits**, not inline — a table cell is nowhere
  near wide enough for `Provenance::render()`'s multi-line output. Every successful
  conversion's record is kept in submission order (same `Vec` indexed by `JobId::index()`
  pattern as checksum's entries, §3) purely so this post-loop dump can walk it.
* **Return value**: whether every file converted without failure — a skip counts as clean,
  the same notion `cmd_convert`'s own exit code uses.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (unchanged pass
count) and `cargo clippy --all-targets` all clean; `cargo fmt --check` clean for
`lh-cli`/`lh-tui`. Run for real inside `tmux`: `lh-tui convert --to wav` over a three-file
fixture folder (two FLACs, one WAV already in the target format) showed both FLACs decode
live to `OK` with the right `-> name.wav` detail and the WAV row as `SKIPPED (already WAV)`,
gauge `written:2 skipped:1 failed:0`, `q` exited 0. `--force --provenance` re-run over the
same now-mixed folder printed the full `Provenance::render()` block for each conversion
after `q`. `lh-tui convert --to flac` over two WAVs (one carrying a `LIST`/`INFO` chunk)
wrote both through the real `flac` binary and reported `OK` for both. Two known-bad fixtures
(`wrong-md5.flac`, `truncated.flac`) both reported `FAILED` with no WAV written for either —
Principle 1 held — and the screen's own exit code was `1`.

## 6. TUI5 — SBE screen — done

Same per-file batch shape as verify/checksum (§2), placed right after Checksum in
`lh-tui/src/main.rs` since both share the "no `Progress::report`" shape Convert's own section
(§5) calls out as its point of difference.

* **`T = Sbe`, not `Result<Sbe>`.** `analysis::sbe` is a pure, infallible function over a
  `StreamInfo` `collect` already probed — no decode, no I/O — so the queue's job type is the
  bare `Sbe` enum, mirroring `cmd_sbe`'s own `run_batch(&files, |f, _| sbe(&f.stream_info))`
  (`lh-cli/src/lib.rs:354`), which also returns bare `T`. `Status`'s only failure variant
  (`Failed(String)`) is reachable solely via `Event::Cancelled`, never from the job itself —
  the same asymmetry checksum/verify's own `Cancelled` handling already has, just total here
  since there is no other way for this operation to fail.
* **`NotApplicable` is neutral, not a warning.** Colored `dim` and left out of the "clean"
  check, the same treatment Convert gives `Skipped` rather than verify's `warn`-colored
  `NoMd5` — most non-CDDA files hit this by design (`sbe.rs`'s own doc comment: "never a pass,
  because 'pass' would imply we checked"), so it isn't something gone wrong.
* **`Misaligned` is the one status that counts against "clean,"** matching `cmd_sbe` setting
  `ok = false` for it; the detail column shows `+{remainder_frames} frames past a sector
  boundary`, the same number `cmd_sbe`'s own `(+N frames)` prints.
* **Return value**: `misaligned_count == 0 && failed_count == 0` — same "clean" contract
  `cmd_sbe`'s own `ok` flag uses, so `$?` matches between `lh sbe` and `lh-tui sbe`.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (unchanged pass count —
no `lh-core`/`lh-cli` logic changed) and `cargo clippy --all-targets` all clean; `cargo fmt`
was needed once (a multi-line `format!` call) and is clean after. Run for real against
`lh-core/tests/fixtures` (12 files): the table showed `ALIGNED` for 7 files, `MISALIGNED` for
`cdda-sbe.flac`/`.wav` with `+137 frames past a sector boundary` in the detail column, and
`N/A` for the three non-CDDA fixtures with the same reason text `lh sbe` prints; the gauge read
`12/12 aligned:7 n/a:3 misaligned:2 failed:0`, colored red for the misalignment. Quitting with
`q` under a real pty (Python's `pty.fork`, since `tmux`'s server did not survive between tool
calls in this sandbox) exited `1`, matching `lh sbe`'s own exit code against the same corpus
line for line.

---

## 7. TUI6 — Tag / Rename screens — done

Two screens, `lh-tui tag <dir>` and `lh-tui rename <dir>`, per docs/tagging.md §6. Both are
laid out as header / body / gauge / footer like every earlier screen, but the body during
editing is a live form plus a live diff, not a job table — the job table only appears once
`a` (apply) has been pressed, and only then does either screen touch a file.

* **Tag**: a `show` pane (the six show-level `tag::Field`s `Tab`/`Shift-Tab` cycle through,
  seeded from the first FLAC that already carries any tags, then from the folder's
  own `ShowName::parse` where that leaves `DATE` blank), a `titles` pane (`e` enters it, `↑`/
  `↓` selects a line, a paste replaces the whole block, `Esc` leaves it), and a `diff` pane
  recomputed every frame from `Tags::changes` — the exact function `cmd_tag` prints its own
  preview from, so the screen's diff and the CLI's are provably the same computation, not two
  hand-kept-in-step renderings of it. Only FLAC files are listed and numbered; the diff
  pane's title counts any other files left out (docs/tagging.md). Pressing `a` builds one
  write job per changed file (`ffp` before, `tag::apply`, `tag::assert_audio_unchanged` — docs/tagging.md
  §1 contract point 2, inside the job so a file that somehow changed fails on its own row);
  an unchanged file gets no job and is already `Unchanged` the moment `a` is pressed.
* **Rename**: a `spec` pane (`BAND`/`DATE`/`DISC`/`SHORT YEAR`, seeded from `ShowName::parse`
  the same way `cmd_rename`'s own defaults are) above a `plan` table recomputed every frame by
  `plan_rename` — pure arithmetic over names already in memory, the reason this is cheap
  enough to do on every keystroke. Collision rows render in `theme.error` and `a` refuses
  silently while any exist, the table already being the explanation. Applying is **one job**
  wrapping `execute_rename(&plan)` for the whole plan, not one job per file: unlike tagging,
  a rename is atomic (`execute_rename`'s own two-phase move with rollback, docs/tagging.md §1
  contract point 3), so there is no independent per-file write to submit — the table still
  shows one row per file, they just all resolve together from the one job's single result,
  the same way `run_sbe_fix_execute_screen` (§0) renders per-file rows from one `execute_fix`
  call. This is the one place these two screens diverge from a literal "one job per file" —
  correctness following `lh-core`'s own atomicity contract instead.
* **Both** return whichever "clean" notion the operation already has (tag: no `Failed` row;
  rename: every row `Ok`), and both leave the screen without writing anything if `q`/`Esc` is
  pressed before `a` — the diff/plan pane already *is* the preview, so quitting early is the
  same "plan only, nothing written" outcome `cmd_tag`/`cmd_rename` give without `--yes`.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (unchanged pass count —
no `lh-core`/`lh-cli` logic changed) and `cargo clippy --all-targets` both clean; `cargo fmt`
clean. Run for real under a real pty (`tmux`, this time surviving between tool calls): against
a copy of `cdda-aligned.flac`/`hires-24bit.flac`/`mono-48k.wav` inside a folder named
`gd1977-05-08.sbd.someone.12345.sbeok.flac16`, `lh-tui tag` seeded `DATE` from the folder name
live on open, typing into `ARTIST` and the two titles updated the diff pane on every
keystroke, leaving the titles block with `Esc` correctly returned `q`/`a` to their unfocused
meanings, and `a` wrote both FLACs (`OK`) while the WAV stayed `N/A` — `metaflac
--export-tags-to=-` afterwards showed exactly the typed `ARTIST`/`DATE`/`TITLE`/`TRACKNUMBER`
values, and `lh ffp` on both FLACs matched `lh-core/tests/fixtures/reference.ffp`'s digests
for the same source files exactly, byte for byte — the audio-unchanged postcondition held
against the real reference checksums, not just the in-process check. `lh-tui rename` against
a folder of oddly-named files showed the live `from -> to` table update on every keystroke of
`BAND`/`DATE`, and `a` renamed all three; the resulting file names on disk matched the plan
exactly. A stray `a`/`q` typed while a field was still focused was confirmed to land as a
character in that field rather than applying or quitting, on both screens.

---

## 8. TUI7 — Check screen — done

Same per-file batch shape as verify/checksum (§2), but the file list doesn't come from
`collect` scanning a folder for audio — it comes from `ChecksumFile::read`'s own
`entries`, the same source `cmd_check` (`lh-cli/src/lib.rs:664`) reads. That's the one real
divergence from §2's template, and it is why a row can land in a state neither verify nor
checksum has: `Missing`, an entry naming a file that isn't on disk at all.

* **Kind resolution built on the same `lh_core` primitive, not copied.** The `.ffp`/`.md5`/
  `.st5` extension match is `ChecksumKind::from_path`, wrapped by a local
  `checksum_kind_for(&Path) -> Result<ChecksumKind>` here and by `lh-cli`'s own
  same-named private function — each binary owns its error wording rather than one reaching
  into the other's internals (`docs/architecture-cleanup.md` A2).
* **`CheckOutcome`** (`Ok`, `Mismatch { expected, actual }`, `Missing`, `Failed(String)`) is
  the queue's `T`, computed inside the job closure exactly the way `cmd_check`'s loop body
  does: `target.exists()` first (→ `Missing` without ever calling `compute`), then
  `compute(kind, target)` compared against the entry's stored digest (→ `Ok`/`Mismatch`), a
  compute error stringified into `Failed` — the same three-way-plus-missing split `cmd_check`
  prints as `OK`/`MISMATCH`/`MISSING`/`FAILED`. `CheckStatus` mirrors it with `Pending`/
  `Running` added, same relationship verify's `Status` has to `Verification` (§0).
* **Header names the checksum file, not a directory** — `check FFP  test.ffp`, matching
  `cmd_check` which only ever gets one file as its argument, not a `Paths` glob; the target
  directory used to resolve each entry is that file's own parent (`file.parent()`, `.` if
  none), identical to `cmd_check`'s `dir`.
* **An empty checksum file skips the screen entirely** — `ChecksumFile::read` succeeding with
  zero entries prints `no entries in <path>` and returns success without calling
  `ratatui::init()`, the same empty-input early-return every other screen has for zero files
  (§2), just phrased for entries instead of files.
* **Return value**: `missing_count == 0 && mismatch_count == 0 && failed_count == 0` — the
  same "clean" `cmd_check`'s own `ok` flag tracks, so `$?` matches between `lh check` and
  `lh-tui check`.
* **Function/type names carry a `check*`/`Check*` prefix already used by the unrelated
  `torrent check` screen** (`CheckStage`, `draw_check_table`, `draw_check_gauge` there are
  per-`TorrentReport`, not per-entry) — the draw functions here are named `draw_checklist*`/
  `checklist_status_*` to avoid colliding with those, since both screens live in the same
  `lh-tui/src/main.rs` and Rust has one flat function namespace per module.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (unchanged pass count —
no `lh-core` logic changed) and `cargo clippy --all-targets` both clean; `cargo fmt` clean.
Run for real inside `tmux`: `lh-tui check reference.ffp` against `lh-core/tests/fixtures`
showed all four entries reach `OK` live, gauge `4/4 ok:4 missing:0 mismatch:0 failed:0` in
green, `q` exited `0` — matching `lh check reference.ffp` run headless first for comparison.
A second fixture built by hand (a `.ffp` with one correct entry, one entry given a wrong
digest, one entry naming a file never copied in, and one entry pointing at a text file saved
with a `.flac` extension) reproduced all four outcomes side by side: `OK` for the untouched
file, `MISMATCH` for the wrong digest (detail: `expected ... actual ...`, the real computed
digest), `MISSING` for the absent file (detail: `no such file`), and `FAILED` for the
unparseable one (detail: the same `FLAC metadata read failed` message `lh check` prints to
stderr) — gauge `4/4 ok:1 missing:1 mismatch:1 failed:1` in red, `q` exited `1`, both
matching the headless `lh check` run against the identical fixture byte for byte.

## 9. TUI8 — Torrent info screen — done

Resolves open question 1 (§11) for `torrent info` specifically: it does earn a screen,
because someone asked. `Info`, `Tools`, `Torrent trackers` remain headless (below).

* **No `Queue` at all** — the first screen with none. `Metainfo::read` is a single
  in-process parse, not a job worth submitting anywhere (`cmd_torrent_info`,
  `lh-cli/src/lib.rs:926`, never touches a `Queue` either), so there is nothing to stream.
  The file is read once, before `ratatui::init()`; a read error prints and exits (code 2)
  the same as every other screen's setup failure, without ever opening the alternate screen.
* **Static content, still redrawn every 80ms tick** — not because anything changes, but
  because that's what lets a terminal resize reflow the layout for free, the same as every
  live screen already gets from redrawing on its own poll loop. `q`/`Esc`/`Ctrl-C` all quit
  immediately; there is no job to cancel, so unlike every batch screen there is no
  `cancel_token` at all.
* **No gauge** — there is no ratio of anything to show for a single static read, so the
  layout is header / detail panel / (optional) files table / footer, not the header / table
  / gauge / footer shape §2 describes. The detail panel's height is computed from its own
  line count (`lines.len() as u16 + 2` for the borders) rather than a fixed `Constraint`,
  since the number of optional fields (`private`, `source`, `created by`, `created`,
  `comment`, multi-tier `trackers`) varies per torrent.
* **`torrent_info_lines` mirrors `cmd_torrent_info` line-by-line**, the same discipline
  `draw_check_table`/`check_rows` use for `torrent check` (§4) — one function decides what a
  field means, read by both the plain-text and the ratatui rendering, so they cannot drift
  on content, only on presentation. `format_date` (`lh-cli/src/lib.rs:1475`) is now `pub` so
  `lh-tui` calls the same civil-from-days formatter rather than a second copy.
* **`--no-files` (`no_files` on `TorrentCommand::Info`) removes the files table entirely**
  rather than showing it empty — the detail panel's `Constraint::Min(3)` then takes the rest
  of the screen instead of splitting it with a table, matching `cmd_torrent_info`'s own
  `list_files` flag skipping the file loop outright.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (161 tests, unchanged)
and `cargo clippy --all-targets` all clean; `cargo fmt --check` clean. Run for real inside
`tmux` against `lh-core/tests/fixtures/torrents/mktorrent-oracle.torrent`: the detail panel
showed infohash, piece count/length, total size, file count, `created by`/`created` (this
fixture has neither `private` nor `source` nor a `comment` nor trackers, so those lines were
correctly absent), and the files table listed all 9 real files with sizes matching `lh
torrent info` run headless on the same file; `--no-files` produced the same detail panel with
the files table gone and the panel filling the freed space; `q` exited 0; pointing the screen
at a nonexistent path printed the same "reading ...: No such file or directory" `lh` itself
would and exited 2 without ever touching the terminal.

## 10. TUI9 — Interactive tracker picker for torrent create — done

`torrent create`'s only input was `--tracker`, repeated on the command line before the
screen ever opened (§4) — every other option a CLI flag too, which defeats the point of a
TUI screen for the one thing (choosing a tracker) a person plausibly wants to decide once
they can see the known list, not before. `CreateStage` gains a `ChoosingTrackers` stage
ahead of `Preparing`, seeded from `--tracker` if any were given but always shown and always
confirmed — `run_torrent_create` no longer calls `resolve()` itself; it only loads
`TrackerList`/`Passkeys` before `ratatui::init()`, same as before, and hands the raw
`TorrentCreateArgs` to the screen.

* **Two widgets, one `CreateFocus`.** A `List` of `TrackerList::iter()`'s entries (id, name,
  `Health::label()`) with an app-owned `cursor: usize` and a `ListState` built fresh each
  frame purely for the selection highlight — the same pattern `draw_titles` (§7) uses — plus
  one `Field` (§7's single-line text widget, reused verbatim) for a comma-separated custom
  id/URL. `CreateFocus::None` is "nothing is being edited," the same convention
  `TagFocus::None` uses: `q`/`Esc` there quit immediately (nothing has started, so there is
  nothing to cancel), `Tab`/`BackTab` enter a widget, and `a`/`Enter` confirm.
* **`picked: Vec<String>`, not a set of booleans.** Selection order is tier order, so toggling
  a list row with `Space` appends or removes that tracker's id from one ordered vector — the
  same shape `--tracker` already takes, just built interactively instead of parsed from
  `argv`. The custom field's `Enter` splits on `,` and pushes each trimmed piece; there is no
  list row for an id that does not exist or a mistyped URL, so `Backspace` on an *already
  empty* field pops the most recent pick instead — a comma-tag-input's usual "backspace
  removes the last chip" convention, and the only way to undo a bad custom entry short of
  restarting.
* **`resolve()` runs on confirm, not on submit.** `start_create` (called from the `a`/`Enter`
  handler) does what `run_torrent_create` used to do before the terminal ever opened: resolve
  the picks, build `CreateOpts`, start the queue. A refusal (unknown id, `PersonalUrl`/
  `Broken` health, conflicting `info.source`) becomes `pick_error`, shown inline in place of
  the picked-list summary, and the picker stays up to fix it — no preflight exit, no restart.
* **The loop can now exit without ever reaching `Done`.** Quitting from `ChoosingTrackers`
  breaks immediately, since no job was ever submitted; the old
  `unreachable!("loop only exits once stage is Done")` is gone, replaced by mapping any
  non-`Done` exit to `Error::Cancelled` — accurate, since "quit before starting" and "cancel
  mid-hash" are both just "no torrent got written."
* **`private`/`source` stay CLI-only flags**, deliberately — the ask was tracker selection,
  and a tracker's own `private`/`source` already applies automatically the moment it is
  picked, the same as `--tracker` always implied.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (229 tests, unchanged),
`cargo clippy --all-targets` and `cargo fmt --check` all clean. Run for real inside `tmux`
against a 3-file copy of `lh-core/tests/fixtures/torrents/payload/verified` with an isolated
`LH_CONFIG_DIR`: launching with no `--tracker` opened on the picker showing all 11 bundled
trackers with their health labels; `Tab` moved focus into the list, `Space` toggled `genesis`
to `[x]`, `Esc` returned focus to `None`, and `a` resolved it, started hashing and wrote a
`.torrent` whose panel showed the one resolved tier. Launching again with `--tracker etree`
pre-selected `etree` (`[x]` on load); confirming it surfaced `etree`'s real `PersonalUrl`
refusal inline without leaving the picker or crashing; toggling `etree` off and `crosstown`
plus `genesis` on then confirming wrote a torrent with both tiers shown, in pick order. Typing
`udp://example.org:1337/announce, badid` into the custom field and pressing `Enter` split it
into two picks; confirming surfaced `badid`'s "no tracker by that name" error (with the full
known-ids list) inline; `Backspace` on the then-empty custom field popped `badid` off,
confirmed by the status line reverting to "no trackers picked yet." `q` quit cleanly from the
picker itself (immediately, nothing running yet) and from the progress screen after a run
finished.

**Follow-ups.**

* **The header clock no longer runs while picking trackers.** `elapsed` reported
  `start.elapsed()` from the moment the screen opened even during `ChoosingTrackers`, so
  the time spent deciding which trackers to use counted toward the shown duration —
  misleading, since nothing is being done with the payload yet. The header now shows a flat
  `0.0s` for that stage, and `start` itself resets to `Instant::now()` at the point `a`/`Enter`
  actually confirms the picks and calls `start_create`, so the clock the rest of the screen
  shows counts only the work, same as every other screen's header.
* **A file-preview panel sits beside the tracker list**, since the picker alone left the
  right half of the screen empty. `lh_core::torrent::create` gains `preview()`, returning a
  `Preview { files: Vec<PreviewFile>, excluded: Vec<(PathBuf, Skipped)> }` built from the same
  private `collect`/`walk` the real create path uses — so the preview can never show a file
  list that `create` would then include or exclude differently, the way a second,
  independent walk could drift. `run_torrent_create_screen` calls it once, up front (the
  tracker pick does not change what is on disk, so there is nothing to re-walk for as the
  user picks); the panel shows each file's path and size, and the block title rolls up count,
  total size, and how many were excluded as noise.

**Real evidence.** `cargo build --workspace`, `cargo test --workspace` (229 tests, unchanged),
`cargo clippy --all-targets` and `cargo fmt --check` all clean. Run for real inside `tmux`
against the 3-file `payload/verified` fixture: the picker opened with the known-trackers list
on the left and a `files: 3 (500 B)` panel on the right listing `d1t01.flac` (100 B),
`d1t02.flac` (250 B) and `d1t03.flac` (150 B); `q` quit cleanly from the picker with the panel
on screen.

## 10a. Sample screen — done

`lh-tui sample <folder|file>`, and `p` in the workspace. The design lives in
`docs/sample.md` §4, next to `lh sample` itself. What is particular to it as a screen:

* **An editor that also runs a job.** Like tag/rename, the fields take typed input, so `q`
  only quits from the track list. Like convert, the encode runs on a `Queue` (one job,
  `with_workers(1)`) so the draw loop keeps going. It shows a gauge while the FLAC is cut
  and the spinner while the encoder runs (`(0, 0)` progress, as with `to_flac`), and `q`
  cancels it.
* **The estimate is redrawn from the fields every frame**, so a clip that will not fit
  shows red before it is asked for. `lh_core::sample::encode` still does the refusing.
  The screen does not duplicate the rule.
* **Checked by driving it in tmux** (2026-09-25): opening from the workspace and from a
  file, 320 kbps over the limit (refused, reason shown), editing the length and encoding,
  the output-exists → `F` replace flow, and `q` during the cut (nothing left behind, not
  even a `.part`).

---

## 11. Screens not yet planned

Named so the gap is visible, not to commit to an order:

* **`info`, `tools`, `torrent trackers`** — all cheap, in-process, no queue really needed for
  a single pass (none of them even touch a `Queue` in `lh-cli` today). Plausibly fine as
  `run_headless` forever (§1) rather than earning a screen, the same reasoning that used to
  cover `torrent info` too before §9 above — nobody has asked for these three yet. (`torrent
  create`'s own tracker input got interactive in §10 above without `torrent trackers` itself
  earning a screen — the picker lists what it needs inline rather than depending on that
  command ever getting one.)

---

## 12. What has and has not been checked

* **Verify, checksum, convert, SBE, check, torrent info and torrent create's tracker picker
  screens**: all run for real against the fixture corpus (§3's, §5's, §6's, §8's, §9's and
  §10's "Real evidence") — table/detail panel, quit key and exit code all confirmed, closing
  the gap this section used to flag ("compiled but never actually run").
* **Verify screen against a real show, not just the fixture corpus.** Run against a genuine
  17-track FLAC show (~600 MB, already carrying its own `.ffp`/`.md5`) sitting in
  `~/Downloads`: all 17 decoded and reported `OK` live in the table, the gauge tracked
  progress correctly across a run that took under a minute on 12 cores, `q` exited cleanly
  (code 0), and `lh check` against that show's own committed `.ffp` and `.md5` independently
  reported all 17 (and, for the `.md5`, all 20 including the artwork and info text) `OK` too
  — three independent checks (the screen's own decode, and both sidecar files) agreeing on
  real trader material, not synthetic fixtures.
* **Tag and rename screens**: run for real (§7's "Real evidence") against copies of real
  fixtures — live editing, focus-gated `q`/`a`/`e`, the diff/plan pane updating on every
  keystroke, a successful write/rename, and the audio-unchanged postcondition checked against
  the reference FFPs, not just the in-process assertion.
* **Headless passthrough**: `run_headless` is a two-line wrapper around an already-tested
  `lh_cli::run`, so the only real risk is argument parsing drift between `lh` and `lh-tui` —
  and there is none, since both parse the same `Cli` (§0).
* **Not yet checked anywhere**: Ctrl-C specifically (only `q` has been pressed by hand so
  far — the code path is identical, per §0/§2, but untried); any screen on a narrower
  terminal than the 100–120 columns used above, where the fixed-width status column and
  percentage-width columns have not been checked for wrapping or truncation; tag's diff
  table and rename's plan table specifically, both of which get tighter with more files or
  longer values than the three-file fixtures §7 tested with.

---

## 13. Open questions

1. **Do `info`, `tools`, `torrent trackers` ever get screens, or stay headless forever?**
   §11 leans "stay headless" — a ratatui table over static, non-streaming output is not an
   obvious improvement over `lh`'s own text — but nobody has asked either way. `torrent info`
   itself is resolved (§9): it got a screen because someone did ask, which is some evidence
   against "nobody ever asks for these," but not proof any particular one of the remaining
   three will follow.
2. **Does a screen need a `--no-tui` escape hatch** for scripting (piping `lh-tui verify`'s
   output, redirecting to a file where an alternate-screen ratatui app would misbehave)? The
   original `lh verify` already covers this case by existing as a separate binary. No longer
   entirely untested: running `lh-tui convert` with stdin redirected from `/dev/null` and no
   pty at all (§5's testing) hit exactly this — `ratatui::init()` panics (`failed to
   initialize terminal: ... No such device or address`) instead of degrading, the same as
   every other screen would. Confirms the gap is real; still nothing decided about closing
   it.
3. **Should `Theme` gain a light/dark or no-color variant**, or does "no inverted
   backgrounds, only fg color + bold/underline" (§0) already cover every terminal this
   project cares about? No report of it looking wrong anywhere yet.
4. **Should `--private`/`--source` join the tracker picker (§10) as on-screen toggles?**
   Deliberately left CLI-only for now — the ask was tracker selection, and a chosen tracker's
   own `private`/`source` already takes effect without either flag. Revisit if someone wants
   to set a private torrent's flag or override a tracker's `source` without a command-line
   flag the same way trackers no longer need one.
