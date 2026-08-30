# GUI shell

`lh-gui` works but has no shape. G1–G4 (`docs/gui.md`) each bolted one more panel onto the
bottom of a single `column!`, and the window now renders **ten stacked regions at once** —
path bar, file table, operation picker, torrent-create form, torrent-check form, torrent
results table, job queue, log, tools list (`lh-gui/src/main.rs:654-691`). Every control for
every operation is on screen whether or not it applies to what the user is doing.

This document plans the replacement: a **left rail of application areas**, one area visible
at a time, over a shared working set and a persistent job/log dock.

It is a re-layout, not new domain logic. `lh-core` gains nothing (Principle 4), the single
long-lived `Queue<JobOutcome>` and its `Subscription` bridge (`docs/gui.md` §1, §2) are
untouched, and S1 moves existing panels without changing what any of them do.

---

## 0. What the original actually does

*Established 2026-08-30 by parsing Trader's Little Helper's own form definitions out of
`tralih.exe` 2.8.4.185 — not from memory of the program and not from its screenshots.*

TLH is Delphi/VCL, so every form it has is serialised into the executable as a binary DFM
resource that records the whole component tree: each `TMenuItem`, each `TTabSheet`, and the
`TAction` captions the menus actually display. `scripts/dump-tlh-forms.py` (added with this
doc) reads them. `tralih.exe` is UPX-packed, so unpack a copy first — the same
`apt-get download` + `dpkg-deb -x` route the `.st5` work already used for `shntool`:

```
upx-ucl -d -o /tmp/tralih.exe "$WINE/Trader's Little Helper/tralih.exe"
python3 scripts/dump-tlh-forms.py /tmp/tralih.exe             # 15 forms
python3 scripts/dump-tlh-forms.py /tmp/tralih.exe frmTraLiH   # the main window
```

### The main window is a hidden card stack driven only by the menu

`frmTraLiH` is 634×407 client pixels, `poScreenCenter`, and contains exactly one thing: a
`TPageControl` named `pcTasks` with **sixteen `TTabSheet`s, every single one
`TabVisible: False`**, opening on `ActivePage: tsNone` — an empty page. The tabs exist as a
container mechanism and are never drawn. The menu bar is the *entire* navigation.

| Menu | `TAction` captions, verbatim | Tab sheet |
|---|---|---|
| **&Format** | `&Encode wav files` | `tsEncodeWav` |
| | `&Re-encode flac files` | `tsReEncodeFlac` |
| | `&Decode audio files` | `tsDecodeAudio` |
| | `&Convert encoding format` | `tsConvertFormat` |
| | `E&xit` | — |
| **&Checksum** | `&Create checksum file` | `tsCreateChecksum` |
| | `&Verify checksum files` | `tsVerifyChecksum` |
| **To&rrent** | `C&reate torrent file` | `tsCreateTorrent` |
| | `C&heck torrent` | `tsCheckTorrent` |
| **&Analysis** | `Show audio file &details` | `tsAudioDetails` |
| | `Test &encoded audio files` | `tsTestEncodedAudio` |
| | `Check audio files for &SBEs` | `tsCheckForSBEs` |
| | `Test wav files for &MPEG` | `tsTestWavForMPEG` |
| **&Tools** | `&Fix SBEs` | `tsFixSBEs` |
| | `Strip audio file &header` | `tsStripHeader` |
| | `&Create skt files` | `tsCreateSkt` |
| | `&Screenshot ...` | — |
| **&Options** | `&Preferences ...` · `&Restore integration into Windows Explorer context menus` | — |
| **&Help** | `User &manual` (F1) · `&Homepage` · `&Check for update ...` · `&About ...` | — |

Every task card is built the same way: a ` … files to X ` group box holding a list with
`Add ... / Remove / Clear`, the action button and a `Cancel`; one or more option group
boxes; and its own ` Process log ` `TRichEdit`. Sixteen separate file lists, sixteen
separate logs.

### The author rendered that same taxonomy three times

This is the finding that decides the design, and it is why a rail is not a departure from
the original:

1. **The menu bar**, above.
2. **`frmTypeOfAction`** — a dialog titled ` Action to perform`, shown when files arrive
   from the Explorer context menu. It is the identical list as five labelled group boxes of
   radio buttons: ` Format ` (4), ` Checksum ` (2), ` Torrent ` (2), ` Analysis ` (4),
   ` Tools ` (2). A grouped, flat, always-visible list of every area — a sidebar in a
   modal's clothing.
3. **`frmPreferences`** — a `TTreeView` on the left driving a `TPageControl` on the right,
   and its 25 pages are *the menu taxonomy again*: `tsGeneral`, `tsAudioCompression`,
   `tsDirectories`, `tsShellIntegration`, `tsOther`, then `tsFormat` → `tsEncodeWavFiles` /
   `tsReEncodeFlacFiles` / `tsDecodeAudioFiles` / `tsConvertEncodingFormat`, `tsChecksum` →
   `tsCreateChecksumFile` / `tsVerifyChecksumFiles`, `tsTorrent` → …, `tsAnalysis` → …,
   `tsTools` → … . **This is literally a grouped left rail over a content pane, written by
   TLH's own author, using TLH's own taxonomy.**

So the proposal here is not "replace TLH's menus with something else." It is: take the
navigation TLH already expressed as a tree-over-pane in its own Preferences window and use
it for the main window too.

### Two parity facts picked up on the way

* **TLH knows five checksum formats, not three.** `frmTypeChecksumFile` — the fallback
  dialog when an extension is ambiguous — offers `wholefile md5 checksum file (md5)`,
  `flac fingerprint file (ffp)`, `shntool md5 fingerprint file (st5)`,
  `composite md5 fingerprint file (cfp)` and `simple file verification file (sfv)`.
  `PLAN.md` §2 names only the first three. `.cfp` and `.sfv` are unplanned and unscoped;
  recorded here so the omission is deliberate rather than unnoticed.
* **TLH had a real tracker-list editor.** `frmEditTrackerList` is a `TListView` with
  `Add ... / Remove / Edit ... / Save`, and `frmGetAnnounceList` fetches announce lists.
  That is direct evidence on `docs/gui.md` §5 open question 5 ("the tracker picker is a
  comma-separated text field"): the original did not make users type tracker ids either.
  It does not change v0.1's answer — nobody has asked yet — but it removes the "maybe the
  original got by without one" defence.

### What this did **not** establish

The DFM records how the window is *composed*, and nothing about how it *behaves*. TLH's own
GUI still cannot be driven on this machine — it starts under Wine, but screen capture is
blocked under Wayland (`import`/`scrot`/`xwd`/`gnome-screenshot` all absent, checked again
this session; `docs/gui.md`'s G1–G4 notes hit the same wall for our own window). So all of the following are unknown and are **not** relied on below:

* Whether the menu disables items while a task is running, or lets you switch away mid-run.
* What `tsNone` actually shows on an empty start — the DFM says the page has no child
  controls at all, which suggests a blank grey panel, but "suggests" is not "was seen."
* Whether a file list survives switching away from its card and back.
* Anything about focus, tab order, or keyboard behaviour beyond `F1` on `actManual`, which
  is the one `ShortCut` property set anywhere in the form.

---

## 1. What `lh-gui` does today

`view()` (`lh-gui/src/main.rs:654`) builds one `column!` of ten children, unconditionally:

```
path_bar, error, table, operations, torrent_create, torrent_check,
torrent_results, jobs_panel, log, tools_panel
```

The concrete consequences, none of them cosmetic:

* **Every control is always live.** The torrent-create tracker field, the `.torrent` Browse
  button and the Overwrite checkbox are on screen while the user is reading verify results.
  `App::overwrite` is already shared between convert and torrent-create precisely because
  both were visible at once and a second identical checkbox looked absurd — a state
  decision forced by the layout rather than by the domain.
* **The file table has no fixed share of the window.** It is
  `.height(Length::FillPortion(3))` inside a column that also has to fit two full forms and
  three result panes, so at 12 files it is already competing with panels the user is not
  using.
* **There is no room for anything else.** `lh check` (verify against an existing `.ffp` /
  `.md5` / `.st5`) and writing a checksum file both exist in `lh-core` and in `lh-cli` and
  are absent from the GUI (§6) — there is nowhere to put them.
* **`Operation` has to be one flat `pick_list`.** Seven entries mixing three unrelated
  kinds of work (`Verify`, three checksum digests, `SBE`, two convert directions), because
  a single dropdown is the only navigation the layout has.

---

## 2. Why a rail, and not a menu bar

The obvious "match the original" move is a menu bar. It is not available:

* **Iced 0.14 has no menu widget.** The only `menu` in the whole 0.14 tree is
  `iced_widget::overlay::menu` — "Build and show dropdown menus", the popup that `pick_list`
  and `combo_box` open. There is no `iced_widget::menu`, no menu bar, no application menu.
  `iced_winit`'s single occurrence of the word is `show_window_menu`, the OS *window
  decoration* menu (`iced_winit-0.14.0/src/lib.rs:1621`), which is the title-bar
  right-click menu and not an app menu.
* **A real menu bar means a third-party dependency and per-platform work** — `muda` or
  similar, plus the macOS distinction between an in-window menu and the system menu bar.
  That is squarely against Principle 6 (cross-platform parity, no platform-only features)
  and adds a native dependency to M4 packaging for a navigation control we can draw
  ourselves out of `button` and `container`.
* **§0 says the rail *is* the original's own idiom.** `frmPreferences` is a grouped rail
  over a content pane. `frmTypeOfAction` is the same grouped list again. Adopting it costs
  no parity.

One thing a rail keeps that a menu bar loses: **the current area stays visible**. TLH's
menu tells you nothing about which of sixteen hidden cards you are looking at once it
closes; a rail row is a persistent "you are here."

---

## 3. The areas

TLH's menu, mapped onto what this repo actually has. "gap" means `lh-core` and `lh-cli`
can already do it and only `lh-gui` cannot (§6).

| TLH menu item | Area here | v0.1 |
|---|---|---|
| Show audio file &details | **Files** (promoted out of Analysis — see below) | have |
| &Encode wav files | **Format → Convert**, direction `WAV → FLAC` | have |
| &Decode audio files | **Format → Convert**, direction `FLAC → WAV` | have |
| &Re-encode flac files | — | v0.2 (`PLAN.md` §2) |
| &Convert encoding format | — | v0.2; needs a second codec to mean anything |
| &Create checksum file | **Checksum → Create** | **gap** |
| &Verify checksum files | **Checksum → Check** | **gap** |
| C&reate torrent file | **Torrent → Create** | have (G4) |
| C&heck torrent | **Torrent → Check** | have (G4) |
| Test &encoded audio files | **Analysis → Verify** | have (G2) |
| Check audio files for &SBEs | **Analysis → SBE** | have (G2) |
| Test wav files for &MPEG | — | not scoped anywhere; see §10 Q6 |
| &Fix SBEs | — | v0.2 (`PLAN.md` §2 "SBE repair") |
| Strip audio file &header | — | not scoped anywhere; see §10 Q6 |
| &Create skt files | — | v0.2, arrives with SHN |
| &Preferences ... | — | no settings UI yet; see §10 Q1 |
| — | **Binaries** (`tools::Registry`) | have (G1) |
| &About ... | **About** | new, trivial; M4 needs the GPL notice somewhere |

Two deliberate divergences, both recorded rather than silently taken:

* **"Files" is promoted out of Analysis to an ungrouped row at the top.** In TLH,
  `Show audio file details` is one of sixteen cards with its own file list, because *every*
  card has its own file list. Here the working set is shared across every area (§4), so the
  file list is not one task among many — it is the thing the other areas act on. It gets
  the top row, above the first group, and it is the area the window opens on, replacing
  TLH's blank `tsNone`.
* **`Tools` does not mean what it means in TLH.** TLH's `&Tools` menu is *repair*:
  Fix SBEs, Strip header, Create skt. `PLAN.md` §4 and `lh-gui` already use "Tools panel"
  for the discovered-binary registry, which is a completely different thing. The rail names
  that row **Binaries**, leaving `Tools` free for TLH's meaning when the v0.2 repair
  operations arrive. `Registry`, `ToolId` and `lh tools` keep their names — this is a label
  in the GUI, not a rename in `lh-core`.

The rail, in TLH's own menu order:

```
  Files
FORMAT
  Convert
CHECKSUM
  Create
  Check
TORRENT
  Create
  Check
ANALYSIS
  Verify
  SBE
──────────
  Binaries
  About
```

Group headers are labels, not buttons: TLH's own `&Format` opens a menu but performs
nothing, and a header that looks clickable and is not is worse than one that plainly is not.

---

## 4. Layout

```
┌──────────────┬──────────────────────────────────────────────────────┐
│              │  ~/shows/gd77-05-08         [ Browse… ]  [ Scan ]     │  ← global, shared
│  Files       ├──────────────────────────────────────────────────────┤
│              │                                                      │
│ FORMAT       │   Convert                                            │
│  Convert   ◄ │   ────────────────────────────────────────────       │
│              │   Direction: [ FLAC → WAV ▾ ]   ☐ Overwrite existing │  ← area controls
│ CHECKSUM     │                                                      │
│  Create      │   ┌──────────────────────────────────────────────┐   │
│  Check       │   │ ☑ d1t01.flac  FLAC  4:12  44.1/16/2  ok      │   │  ← shared table,
│              │   │ ☑ d1t02.flac  FLAC  6:03  44.1/16/2  ok      │   │    working-set
│ TORRENT      │   │ ☐ d1t03.flac  FLAC  8:47  44.1/16/2  —       │   │    areas only
│  Create      │   └──────────────────────────────────────────────┘   │
│  Check       │                                    2 of 3   [ Run ]  │
│              │                                                      │
│ ANALYSIS     │                                                      │
│  Verify      │                                                      │
│  SBE         │                                                      │
│ ──────────── │                                                      │
│  Binaries    │                                                      │
│  About       │                                                      │
├──────────────┴──────────────────────────────────────────────────────┤
│ Jobs  7 of 12 done                       [ Jobs | Log ]  [ Cancel ] │  ← global dock
│  ▸ d1t01.flac   running (204/512)                                   │
│  ▸ d1t02.flac   OK, checked against source                          │
└─────────────────────────────────────────────────────────────────────┘
```

**Three persistent regions, one switchable one.**

* **The rail** (left, fixed width). Group labels and area rows; the selected row is styled,
  not merely remembered.
* **The path bar** (top of the content pane, global). It edits the *shared* working set, so
  it belongs to the window and not to any one area. Window-wide drag-and-drop
  (`docs/gui.md` §G0) keeps working exactly as it does now, including the existing
  route-a-dropped-`.torrent`-to-the-check-area behaviour, which now also switches the rail
  to that area.
* **The area pane** (centre). One area's controls, plus the file table for working-set
  areas.
* **The dock** (bottom, global, always visible). The job queue and the log. This is the
  whole point of `docs/gui.md` §1's single long-lived queue: a convert started in Convert
  must stay visible after switching to Torrent, and a per-area log — TLH's sixteen separate
  ` Process log ` boxes — would hide exactly the job the user most wants to watch.

  The dock header always shows the aggregate `N of M done` and `Cancel`; a `Jobs | Log`
  toggle switches only the *body*. Aggregate progress is the thing that must never be one
  click away, and the two bodies want the same vertical space.

**Working-set areas vs. document areas.** Areas take their input from one of two places,
and the table is shown only for the first kind:

| Kind | Areas | Input |
|---|---|---|
| working set | Files, Convert, Checksum → Create, Verify, SBE | the ticked rows |
| working set, **whole folder** | Torrent → Create | `App::working_root`, *not* the ticked rows |
| document | Checksum → Check, Torrent → Check | a `.ffp`/`.md5`/`.st5`, a `.torrent` |
| static | Binaries, About | — |

Torrent → Create is the one that must ignore the selection, and it is worth saying why
rather than letting it look like an oversight: a torrent describes a folder as it exists on
disk, and `torrent::create_with_progress` walks that folder. Filtering by ticked rows would
produce a `.torrent` whose file list did not match the directory it names, which is a
broken torrent, not a subset. The create area therefore says which folder it will use and
shows the ticked count nowhere.

---

## 5. What changes in `App`

Additive; nothing existing is redesigned.

```rust
enum Area {
    Files,
    Convert,
    ChecksumCreate, ChecksumCheck,
    TorrentCreate,  TorrentCheck,
    Verify, Sbe,
    Binaries, About,
}
```

* **`App::area: Area`**, defaulting to `Area::Files`. `view()` becomes rail + path bar +
  `match app.area { … }` + dock, and every existing `*_panel` function is reused unchanged
  as the body of its area. `torrent_create_panel`, `torrent_check_panel`,
  `torrent_check_results_panel`, `log_panel`, `job_queue_panel` and `tools_panel` all keep
  their signatures.
* **`App::selected: HashSet<PathBuf>`**, filled with every path on each `scan` (scanning
  selects all — the common case is "do this to the show"). Path-keyed rather than an index
  or a parallel `Vec<bool>` because `App::latest_job_by_path` already keys the table's other
  per-row state by path, and one convention beats two. Selection is GUI state and stays in
  `lh-gui`: `lh_core::scan::WorkingSet` gains no `selected` field (Principle 4).
* **`Operation` stops being a user-facing `pick_list` and becomes a function of the area.**
  It survives as the internal description of what to submit —
  `run_operation(&mut self, operation: Operation)` instead of reading `self.operation` —
  built from `area` plus area-local state (`App::convert_target`, and a new
  `App::checksum_kind` for Checksum → Create). `Message::OperationSelected` is replaced by
  `Message::AreaSelected(Area)` plus the two narrower pickers.

  This touches the five existing tests that set `app.operation` before calling
  `run_operation` (`lh-gui/src/main.rs:1088, 1127, 1157, 1206, 1232`) — each becomes a
  one-line change from field assignment to argument. Mechanical, and worth stating plainly
  rather than discovering during S1: the *bodies* of those tests, which are the real
  evidence that verify/convert work through the real queue, are unaffected.
* **`App::overwrite` splits in two.** It is shared today only because convert and
  torrent-create were on screen together (§1). Once they are separate areas the sharing is
  a bug waiting to happen — ticking Overwrite for a convert must not silently arm
  "overwrite an existing `.torrent`" in an area the user has not visited.
* **The window gains a minimum size.** `main()` sets no `window::Settings` today
  (`lh-gui/src/main.rs:998`). Rail + table + dock has a floor below which it stops being
  usable; TLH's own window is 634×407 and is not resizable below its design size.

**What does not change:** `Queue<JobOutcome>`, `subscription()`, the `QueueEvents` hash
identity, `JobUpdate` and the render-at-the-boundary rule that keeps `lh_core::Error` out
of `Message` (`docs/gui.md` G2 notes). No `lh-core` change of any kind — unlike G2, which
needed `CancelToken::reset()`, and G3/G4, which lifted `convert::destination` and
`torrent::default_output`.

---

## 6. The two gaps this exposes

Giving Checksum its own two rail rows makes it obvious that neither is wired, though both
exist below the GUI. Neither is new domain logic; both are call sites.

* **Checksum → Create.** The GUI's `Operation::Checksum(kind)` computes a digest per file
  and renders it as a status line (`run_operation`, `lh-gui/src/main.rs:453`). It never
  builds a `ChecksumFile` and never calls `ChecksumFile::write`
  (`lh-core/src/checksum/file.rs:67`). TLH's card is called *Create checksum file* and
  writes one; `lh ffp -o out.ffp` writes one (`cmd_checksum`,
  `lh-cli/src/main.rs:384`). The GUI is the only front end that cannot. The area is the
  existing per-file digest jobs plus one `ChecksumFile::write` when they all finish, with
  a kind picker (FFP/MD5/ST5) and an output path.
* **Checksum → Check.** `lh check file.ffp` exists (`cmd_check`, `lh-cli/src/main.rs:420`):
  infer the kind from the extension, `ChecksumFile::read`, compare each entry against the
  file beside it. The GUI has no equivalent at all. As an area it is a document area —
  Browse or drop a `.ffp`/`.md5`/`.st5` — that submits one job per entry and renders a
  per-file results table. That table is not new work either: G4 already built exactly this
  shape for torrent check (`TorrentFileRow`, `report_rows()` at the `JobUpdate` boundary),
  and this is the second caller that pattern was waiting for.

  It also inherits `frmTypeChecksumFile`'s problem — a checksum file whose extension does
  not name its kind. `cmd_check` bails with an error there. TLH asks. §10 Q4.

---

## 7. `iced::widget::table`

Iced 0.14 ships a `table` widget (`iced_widget-0.14.2/src/table.rs`, re-exported as
`iced_widget::table`), new this release and not available when G1 hand-rolled the file
table as a `Column` of `row!`s with `Length::FillPortion` weights.

`table(columns, rows)` requires `T: Clone` for the row type. `lh_core::model::AudioFile` is
`#[derive(Debug, Clone)]` (`lh-core/src/model.rs:92`), so rows can be the domain type
directly, with one `table::column(header, view_fn)` per column. That is the natural home
for the selection checkbox column (§5) and for Files' wider column set — `lh info`'s
encoder vendor string has nowhere to go in the current six columns.

Left to S4 rather than folded into S1 deliberately: S1 must be a pure move, so that if
anything regresses it is the layout and not the table. Neither the widget's real column
sizing behaviour nor its scrolling has been checked against a 30-row working set — only the
signature has been read.

---

## 8. Out of scope

* **Any new `lh-core` capability.** The two §6 areas call existing functions. Everything in
  §3's "v0.2" column stays v0.2; a rail with disabled rows advertising features that do not
  exist is worse than a rail without them.
* **A Preferences area.** `lh-core::config` exists but nothing writes settings; §10 Q1.
* **Icons.** Iced bundles no icon font, and the rail is legible as text. Not a design
  preference — an unevaluated dependency.
* **Persisting the working set, the selection, or the current area across restarts.**
* **`.cfp` and `.sfv`** (§0), `Test wav files for MPEG`, `Strip audio file header`.
* **M4 packaging**, including whether About is where the GPL sidecar notice actually
  belongs.

---

## 9. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**S1**~~ | ~~The shell~~ | **Done** — `Area`, the rail, the global path bar, the area pane, the dock with its `Jobs \| Log` toggle. Every existing panel moved into its area unchanged; `run_operation` takes its operation as an argument; `overwrite` split into `convert_overwrite`/`torrent_overwrite`; the window gained a 900×600 minimum size. **No behaviour change** — the existing 14 tests kept passing, five of them with a one-line edit each (§5). See §9 notes. |
| **S2** | Selection | The checkbox column, select-all in the header, `App::selected`, `run_operation` over the ticked rows, and Torrent → Create explicitly *not* filtered by it (§4). A test that an unticked file gets no job, and one that torrent create ignores ticking entirely. |
| **S3** | The checksum areas | Checksum → Create (kind picker, output path, `ChecksumFile::write` after the digests land) and Checksum → Check (`ChecksumFile::read`, one job per entry, per-file results table reusing G4's `JobUpdate` boundary). Tests through the real queue against the fixture corpus and the committed `reference.ffp`/`reference.st5` goldens. |
| **S4** | The table widget | Replace the hand-rolled file table with `iced::widget::table`; give Files the wide column set including the encoder vendor string. |

S1 before everything: it is the only one that touches every existing panel, and doing it
first means S2–S4 each land in one area instead of in a ten-panel column.

### S1 notes

What the plan above got right without needing a correction: `Area::RAIL` as one data table
(group header, area, label) drives both the rail's rendering and its selected-row styling,
and every G1–G4 panel function moved into `area_pane`'s `match` with its own signature
completely unchanged, exactly as §5 predicted.

What it did not anticipate:

* **`ChecksumKind` (`lh_core::checksum`) has no `Display`, and the orphan rule blocks adding
  one from `lh-gui`** (`std::fmt::Display` is foreign, `ChecksumKind` is foreign). The old
  `Operation` `pick_list` sidestepped this because `Operation` itself was local. The
  Checksum → Create kind picker is therefore three plain buttons (`kind_button`, styled
  selected/unselected like a rail row) rather than a `pick_list`, with a free
  `checksum_kind_label` function standing in for `Display`. `ConvertTarget` (already local)
  keeps its `pick_list`, now with a real `Display` impl instead of the old shared
  `Operation::Display`.
* **The dock's two bodies each used to carry their own header line** (`job_queue_panel`'s
  `Jobs: N of M done`, `log_panel`'s `Log` label). Both moved up into `dock`'s
  always-visible header — `job_queue_panel` and `log_panel` now render only their list,
  which is the change that makes the aggregate line survive switching to the Log tab, the
  behaviour §4 asked for.
* **Torrent → Create needed a checkbox it never had.** The old single-column layout let it
  read `App::overwrite` off the checkbox `operation_panel` drew for Convert, visible in the
  same screen at the same time — exactly the "a second identical checkbox looked absurd"
  state decision §1 flagged as forced by the layout rather than the domain. Splitting the
  field (`torrent_overwrite`) exposed that Torrent → Create had never had its own control;
  `torrent_create_panel` now does.
* **`iced::widget::button::secondary`/`::text` are usable directly as `.style()` closures**
  by value (`fn(&Theme, Status) -> Style`, the exact signature `.style()` wants), so the
  selected/unselected switch on the rail, the checksum-kind buttons and the dock tabs is
  one `if selected { button::secondary(theme, status) } else { button::text(theme, status) }`
  each, no custom `Style` construction needed.
* Real evidence, same bar as G1–G4's own notes: the compiled binary was run for real under
  this machine's X11 display (`timeout 6 ./target/debug/little-helper`, `DISPLAY=:0`) and
  stayed up the full 6 seconds without panicking. No screenshot tool exists in this sandbox
  for a native window (`import`/`scrot`/`xwd`/`gnome-screenshot` all absent, same wall every
  G-milestone hit) — nobody has clicked a rail row, toggled the dock tab, or watched an area
  switch. The 14 tests are evidence the *logic* moved correctly; the rail's layout itself is
  unverified.

---

## 10. Open questions

1. **Does Preferences get a rail row in v0.1?** `lh-core::config` exists and
   `Registry`'s override paths are environment variables (`LH_FLAC` etc.) with no UI.
   TLH's `frmPreferences` has 25 pages; ours would have roughly two settings. Probably
   v0.2, but the rail should be designed knowing where it would go.
2. **Keyboard navigation.** TLH's menu had Alt accelerators on every item (`&Format`,
   `C&heck torrent`) and exactly one `ShortCut`, F1 on the manual. A rail has neither.
   `iced::event::listen_with` could bind Ctrl+1…9 to areas — but nine bindings for ten
   areas is already wrong, and nobody has asked. Unresolved.
3. **What does the dock show when nothing has run?** TLH opens on a blank page and its
   logs start empty. An empty jobs list and an empty log are two-thirds of the window's
   height on first launch. Collapse the dock until the first job, or leave it empty?
   Leaving it empty is honest about the layout; collapsing it makes the first-run window
   look like a different program from the second-run one.
4. **A checksum file whose extension does not name its kind** (§6). `lh check` bails.
   TLH asks, via `frmTypeChecksumFile`. A picker in the Check area that defaults to the
   extension and is editable would cover both, but it is a fifth control in an area that
   otherwise has two.
5. **Does switching areas mid-run need any guard at all?** The single shared queue means it
   is safe — jobs are unaffected by what is on screen. TLH's behaviour here is unknown
   (§0), and "safe" is not the same as "not confusing": Cancel in the dock cancels the
   *queue*, not the area, so switching from Convert to Verify and pressing Cancel stops the
   convert. That is correct and may still surprise. Consider labelling it `Cancel all`.
6. **Are `Test wav files for MPEG` and `Strip audio file header` in parity scope?** Both
   are TLH menu items and neither appears anywhere in `PLAN.md` — not in §2's v0.1 scope,
   not in its explicit v0.2 deferral list. They are the only two TLH operations with no
   recorded decision either way.
7. **Does the rail need a scroll?** Ten rows plus four group labels fits any window that
   meets the new minimum height. Fifteen v0.2 rows might not.
