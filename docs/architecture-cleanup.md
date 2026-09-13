# Architecture cleanup

Tighten the boundaries between the four crates before the front ends grow any further.

This is a plan for behaviour-preserving refactors only. Every step leaves `lh`'s output, exit
codes and on-disk results exactly as they are — `lh-cli/tests/cli.rs` and the `lh-core`
integration tests are the contract — and every step ends green on the same three commands CI
runs:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features
cargo test --workspace --all-features
```

---

## 0. Where things stand

*Established 2026-09-13 by reading the public API, imports and repeated code of every crate.*

| Crate | Size | Role |
|---|---|---|
| `lh-core` | ~7k lines, 15 modules | the engine |
| `lh-cli` | `lib.rs` 1,500 lines | clap grammar plus every `cmd_*`; `main.rs` is 19 lines |
| `lh-tui` | `main.rs` 5,208 lines, one file | depends on `lh-cli` *and* `lh-core` |
| `lh-gui` | `main.rs` 2,257 + `job.rs` 352 | Iced app |

**`lh-core` is in good shape.** Its internal dependency graph is acyclic and mostly what the
module names promise:

```
format   → model           checksum → format          tag    → checksum, model
convert  → format, output, tools                      rename → etree, output
torrent  → config, output  (self-contained otherwise; `stream` already private)
analysis → convert, format, output, scan, tag, tools   ← the outlier (§A4)
```

**The front ends are where the debt is.** Each one re-implements a little logic `lh-core`
should own, and each one copies code within itself. Concretely:

1. **Logic missing from core, so written per front end.**
   * Checksum-file kind from extension: `lh_cli::checksum_kind_for` (`lh-cli/src/lib.rs:667`),
     an inline copy in `lh-gui/src/main.rs:415`, and `lh-tui` importing the CLI's.
   * Checking a `.ffp`/`.md5`/`.st5` against disk — read the list, recompute each entry,
     classify ok / mismatch / missing / failed — is a loop in `lh-cli/src/lib.rs:684`,
     `lh-gui/src/main.rs:725` and `lh-tui/src/main.rs:966`.
   * Expanding files and folders into `AudioFile`s with skip reasons is `lh_cli::collect`
     (`lh-cli/src/lib.rs:375`), which the TUI imports four times. It also prints, which is
     why it cannot move wholesale.
   * `convert::destination` returns `Option`, so all three turn `None` into an error
     themselves (`lh-gui/src/main.rs:898` is the GUI's wrapper).
   * `lh-gui/src/main.rs:81` defines `checksum_kind_label`, identical to the existing
     `ChecksumKind::label`, with a comment claiming the latter does not exist.
2. **Copied display helpers.** `pieces_phrase` three times, identically (`lh-cli`, `lh-tui`,
   `lh-gui/src/job.rs`); `format_bytes` twice, identically; `format_duration` twice, and the
   two disagree (`m:ss.mmm` in the CLI, `m:ss` in the GUI). `lh_cli::format_date` is imported
   by the TUI.
3. **Four progress/cancel callback shapes** for what is one idea:

   | Function | Callback | Cancellable |
   |---|---|---|
   | `convert::to_wav_with_progress` | `FnMut(u32, u32) -> bool` | yes |
   | `convert::to_flac_cancellable` | `FnMut() -> bool` | yes |
   | `torrent::create_with_progress` | `FnMut(u32, u32) -> bool` | yes |
   | `torrent::check_with_progress` | `FnMut(u32, u32)` | **no** |

   Each also has a thin no-callback twin (`to_wav`, `to_flac`, `create`, `check`). Both the
   GUI (`lh-gui/src/main.rs:527`) and the TUI (`lh-tui/src/main.rs:3233`) carry comments
   explaining why Cancel cannot stop a torrent check.
4. **`analysis` contradicts `lh-core/src/lib.rs`.** The crate doc says read-only analysis is
   pure Rust and anything that produces a file goes through `tools`. `analysis::sbe_fix`
   re-encodes through `flac`, rewrites tags and commits outputs.
5. **A third-party type in the public API.** `tag::read_comment_block` /
   `restore_comment_block` take and return `metaflac::block::VorbisComment`; their only
   caller is `sbe_fix`.
6. **`lh-tui` reaches into `lh-cli` for more than the grammar.** Sharing `Cli` is a
   deliberate decision (`docs/tui.md` §0) and stays. Importing `collect`,
   `checksum_kind_for` and `format_date` is a symptom of 1 and 2, not a design.
7. **Monolithic front-end files, with copy-paste screens.** `lh-tui/src/main.rs` has 9
   `draw_*_gauge`, 9 `draw_*_table`, 5 `*_status_cell`, 5 `*_status_detail`, 5 `*Stats`
   structs and 14 `ratatui::init`/`restore` sites; the gauges compared differ only in their
   label's counters.
8. **Manifest drift.** `lh-core`'s `[dev-dependencies]` repeat `hex`, `which` and `sha1`,
   already normal dependencies. `lh-tui` reaches `lh-cli` by path rather than through
   `[workspace.dependencies]`, and has no `description` or `publish = false`.

### What this plan deliberately does not narrow

Much of `lh-core`'s surface that no front end calls is still imported by the integration
tests, which pin it as a contract: `torrent::{encode, Draft, Content, info_bytes,
join_checked, resolve_root}`, `torrent::trackers::parse_list`, `tools::{Agent, Provenance}`,
`analysis::execute_single_boundary`. Those stay `pub`. Moving those tests into unit tests just
to hide the items is churn with no user-visible payoff.

---

## A1. Give `lh-core` the pieces the front ends are missing

No behaviour change; call sites shrink.

* **`ChecksumKind::from_path(&Path) -> Option<ChecksumKind>`** in `checksum/mod.rs`. Replace
  `lh_cli::checksum_kind_for`'s body (keeping its `anyhow` error message in the CLI, where
  `lh check`'s wording is pinned) and the GUI's inline match.
* **Per-entry checksum check** in `checksum/`:
  `check_entry(kind, base_dir, &Entry) -> EntryOutcome`, with
  `EntryOutcome { Ok, Mismatch { expected, actual }, Missing, Failed(Error) }`. Per entry, not
  per file, because the TUI and GUI submit one queue job per entry and must keep doing so.
  Replace the three loops.
* **`scan::collect(paths, recursive) -> WorkingSet`**: the scan-and-probe half of
  `lh_cli::collect`, returning files plus `(path, reason)` skips, printing nothing.
  `lh_cli::collect` keeps its `eprintln!`s and becomes a loop over the result.
* **`convert::destination` returns `Result<PathBuf>`**, with the "has no file name to work
  from" error the GUI wrapper uses today. Delete that wrapper.
* **`lh_core::display`**: plain-string formatters with no terminal or widget types —
  `pieces_phrase`, `bytes`, `date` (from `lh_cli::format_date`), and the two duration
  formats under names that say which they are (`duration_precise` → `m:ss.mmm`,
  `duration_short` → `m:ss`). Each keeps its current output exactly; move the GUI's existing
  `format_duration_matches_mm_ss` / `format_bytes_matches_kib_mib` tests along with them.
  Delete every copy.
* Delete `checksum_kind_label` in the GUI in favour of `ChecksumKind::label`.

**Done when** no front end defines any of the functions named above.

## A2. Shrink `lh-cli` to its grammar and its commands

Depends on A1.

* `lh-cli`'s public API becomes `Cli`, `Command`, the arg structs and `run`. `collect`,
  `checksum_kind_for` and `format_date` go private or disappear.
* `lh-tui` imports nothing from `lh_cli` beyond that list.
* Manifests (§0.8): add `lh-cli` to `[workspace.dependencies]` and use it from `lh-tui`; give
  `lh-tui` a `description` and `publish = false`; drop `lh-core`'s redundant dev-dependencies.
* Update `docs/tui.md` §0, which describes `lh-cli`'s `pub` surface.

**Done when** `lh-tui/src/main.rs`'s only `lh_cli::` paths are the grammar and `run`.

## A3. One progress/cancel callback

Independent of A1/A2. The only step that changes an `lh-core` signature a front end calls.

* One shape for every long operation: `&mut dyn FnMut(u32, u32) -> bool` — (done, total),
  return whether to keep going, `Err(Error::Cancelled)` when told to stop. It is already the
  shape of two of the four, and the one `job::Progress` is naturally adapted to.
* `to_flac_cancellable` → takes that callback and calls it with `(0, 0)` while `flac` runs:
  "no count available". Every existing display already copes: the TUI gauges render
  `total == 0` as an empty bar, and the GUI's job status only shows `done/total` when
  `total > 0` (`lh-gui/src/main.rs:171`).
  Document that convention once, next to the callback.
* `check_with_progress` gains cancellation, polled per piece, returning `Error::Cancelled`
  with no partial report. Delete the two "Cancel cannot stop a check" comments and wire
  Cancel through in the GUI and TUI.
* One function per operation: `to_wav`, `to_flac`, `torrent::create`, `torrent::check` take
  the callback; the `_with_progress` / `_cancellable` names go. Tests and one-shot callers
  pass `&mut |_, _| true`.
* Update `docs/job-queue.md` where it names the old functions.

**Out of scope:** `analysis::execute_fix` has no progress at all (`lh-tui/src/main.rs:1784`).
Adding it is a feature, not a cleanup; note it in `docs/sbe-repair.md` instead.

**Done when** every long-running `lh-core` operation has exactly one entry point and it
accepts the callback above.

## A4. Fix `lh-core`'s own boundaries

Independent of A1–A3.

* **Move `analysis::sbe_fix` to a top-level `repair` module** (`lh-core/src/repair/mod.rs`).
  `analysis` keeps `sbe` and `verify` and becomes the pure, read-only module `lib.rs` says it
  is. Update `lib.rs`'s module doc, the `analysis` re-exports, `lh-core/tests/sbe_fix.rs`,
  and the CLI/TUI imports. No compatibility re-export: this is a workspace, not a published
  API.
* **`tag::read_comment_block` / `restore_comment_block` → `pub(crate)`.** No `metaflac` type
  remains in `tag`'s public API.
* **`output` → `pub(crate) mod`**, and **`format::wav::WavLayout` → `pub(crate)`**: neither
  has a user outside `lh-core/src`.
* **Update `PLAN.md` §3's module tree**, which is already stale: it lists a `report/` that
  does not exist and omits `etree`, `tag`, `rename`, `output`, `scan` and `model`. Add
  `repair/` and `lh-tui/`.

**Done when** `analysis` depends on nothing but `error`, `format` and `model`, and no public
`lh-core` signature mentions `metaflac`.

## A5. Split the front ends into files

Pure moves, no logic changes, one commit per crate so each diff reviews as a rename. Easier
after A1–A3, since fewer duplicated functions have to be moved only to be deleted.

* `lh-cli/src/lib.rs` → `lib.rs` (grammar, `run`, `run_batch`) plus `commands/{info,
  verify, sbe, checksum, convert, tag, rename, torrent, tools}.rs`.
* `lh-gui/src/main.rs` → `main.rs` (app, `update`, `view`, `subscription`) plus one module per
  area (`areas/{convert, checksum, torrent, about}.rs`) and `widgets.rs` for the shared
  table, dock and panels. `job.rs` stays.
* `lh-tui/src/main.rs` → `main.rs` (arg parsing, dispatch, `run_headless`), `theme.rs`, and
  `screens/{verify, checksum, check, sbe, sbe_fix, convert, torrent_create, torrent_check,
  torrent_info, tag, rename}.rs`. The editor `Field` type moves to `fields.rs`.

**Done when** no front-end source file is over roughly 800 lines, and `git diff -M` shows the
moves as moves.

## A6. Share the TUI's batch-screen scaffolding

Depends on A5.

* A **terminal guard** that does `ratatui::init`, bracketed paste on, and `ratatui::restore`
  on drop, replacing the 14 hand-paired sites. It also makes restore-on-panic automatic.
* A **generic batch screen** for the five watch-only screens that share the verify shape
  (`docs/tui.md` §0): verify, checksum, check, sbe, convert. One header, table, gauge, footer
  and event loop, parameterised by a row-status trait:

  ```rust
  trait RowStatus {
      fn cell(&self, spin: char, theme: &Theme) -> (String, Style);
      fn detail(&self) -> String;
      fn tally(&self, counts: &mut Counts);   // drives the gauge label and colour
  }
  ```

  Each screen keeps only its status enum, its `impl RowStatus`, and how it submits jobs.
* **Leave the editor screens alone** (tag, rename, torrent create, sbe fix): they have
  focus, stages and pickers the batch shape does not, and forcing them into it would add
  indirection rather than remove code. Adopt the guard and shared gauge there only.
* Check each screen's on-screen output against the pre-refactor build by eye; there are no
  TUI render tests to lean on.

**Done when** the five batch screens have no `draw_*` functions of their own.

---

## Order

```
A1 ──► A2
A3                (any time)
A4                (any time)
A1–A3 ──► A5 ──► A6
```

A1 is the smallest and removes the most duplication per line changed; start there. A6 is
the largest win in lines and the largest diff, and gets cheaper with every step before it.

## Not in this plan

* Wrapping `claxon`/`metaflac`/`bendy` errors out of `Error`'s public variants. It matters
  for a published library, which `lh-core` is not yet.
* A `job::Queue::run_all` to replace `lh-cli`'s `run_batch` and the TUI's per-screen event
  loops. Revisit once A6 shows whether the TUI still wants one.
* The GUI tag/rename screen, SBE-fix progress, or anything else that adds behaviour.
