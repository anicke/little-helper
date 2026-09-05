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

* **`lh-cli`'s command logic lives in `lib.rs`, `main.rs` is a thin wrapper.** `Cli`,
  `Command`, `Paths`, `ChecksumArgs`, `ConvertArgs`, `TorrentCommand` and every `cmd_*`
  function are `pub` in `lh-cli/src/lib.rs`; `lh-cli/src/main.rs` is just `Cli::parse()` +
  `run(cli)` + exit code. `lh-tui` depends on `lh-cli` as a library and parses the identical
  `Cli`, so every subcommand `lh` accepts, `lh-tui` accepts too, with identical flags —
  there is exactly one command grammar in the workspace, not two kept in sync by hand.
* **Only `Command::Verify` has a screen.** `main()` matches on `cli.command`: `Verify` goes
  to `run_verify`, everything else to `run_headless`, which calls `lh_cli::run(cli)` and
  prints exactly what `lh` would — no ratatui, no alternate screen. This is deliberate and
  stated in the module doc comment, not an oversight to route around silently.
* **The verify screen's shape**, which every future screen should default to unless a
  command's own result type says otherwise:
  * `lh_cli::collect(&paths)` for the file list — same skip-reporting as every `lh` command.
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
  window, but a terminal screen can at least be captured as text; §5 records what has and
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

Everything else about scope matches `lh-gui`'s own framing: `lh-tui` adds no operation
`lh-core` does not already expose (Principle 4), and a command with no screen yet keeps
working exactly as `lh` does — `run_headless` is not a placeholder to delete, it is the
permanent fallback for whichever commands never earn a screen of their own (`tools`,
`torrent info` and `info` are plausibly fine as plain text forever; §4 Q1).

---

## 2. The per-screen pattern

Distilled from §0 so the next screen does not have to reverse-engineer verify's:

```rust
fn run_x(args: XArgs) -> ExitCode {
    let (files, mut clean) = lh_cli::collect(&args.paths)?;   // same skip reporting as `lh`
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

A screen whose operation reports true sub-file progress (`convert`'s frame counts,
`torrent create`'s piece counts — both already flow through `job::Event::Progress` for
`lh-cli`'s own `run_batch`, §8 below) should render it; verify has no such screen because
`analysis::verify` never calls `Progress::report` (module doc, `lh-tui/src/main.rs:11`), not
because the pattern above cannot show one. §3 (Checksum) is the same shape as verify — no
sub-file progress exists for `checksum::compute` either.

---

## 3. TUI2 — Checksum screen (ffp / md5 / st5)

*Planned this session, next to build.*

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

---

## 4. Screens not yet planned

Named so the gap is visible, not to commit to an order:

* **Convert** — the one screen that actually has sub-file progress to show
  (`to_wav_with_progress`'s frame counts, `to_flac_cancellable`'s killable child — J2). The
  richest screen to build, and the reason §2 calls out progress rendering as the real
  per-screen variable.
* **Torrent create / check** — `create_with_progress`'s piece counts, and `check`'s
  per-file `TorrentReport` (already rendered as a table once, for `lh-gui` G4's
  `TorrentFileRow`/`report_rows` — likely reusable as data even though the widget is
  ratatui, not Iced).
* **SBE, Check, Info, Tools, Torrent info/trackers** — all cheap, in-process, no queue really
  needed for a single pass (`sbe`, `check` don't decode audio; `info`/`tools`/`torrent
  info`/`trackers` don't even touch a `Queue` in `lh-cli` today). Plausibly fine as
  `run_headless` forever (§1) rather than earning a screen — nobody has asked, and a
  ratatui table over already-known, non-streaming data is not obviously better than the
  plain text `lh` already prints for these.

---

## 5. What has and has not been checked

* **Verify screen**: compiled and run for real — the commits' own testing is unrecorded
  beyond "it compiles," which this doc flags as a gap rather than papering over it, matching
  every other doc's standard here. Before TUI2 lands, run it against the fixture corpus
  (`lh-core/tests/fixtures`) with a real terminal and confirm the table, gauge and quit key
  all behave, the same "compiles vs. was run" distinction `docs/job-queue.md` and `gui.md`
  §G0 insist on elsewhere.
* **Headless passthrough**: `run_headless` is a two-line wrapper around an already-tested
  `lh_cli::run`, so the only real risk is argument parsing drift between `lh` and `lh-tui` —
  and there is none, since both parse the same `Cli` (§0).

---

## 6. Open questions

1. **Do `info`, `tools`, `torrent info`/`trackers` ever get screens, or stay headless
   forever?** §4 leans "stay headless" — a ratatui table over static, non-streaming output
   is not an obvious improvement over `lh`'s own text — but nobody has asked either way.
2. **Does a screen need a `--no-tui` escape hatch** for scripting (piping `lh-tui verify`'s
   output, redirecting to a file where an alternate-screen ratatui app would misbehave)? The
   original `lh verify` already covers this case by existing as a separate binary; whether
   `lh-tui verify` should also degrade gracefully when stdout is not a tty is untested.
3. **Should `Theme` gain a light/dark or no-color variant**, or does "no inverted
   backgrounds, only fg color + bold/underline" (§0) already cover every terminal this
   project cares about? No report of it looking wrong anywhere yet.
