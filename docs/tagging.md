# Renaming and tagging

Finish a show: give its files the names and the tags the community publishes standards for.

`PLAN.md` §2 defers both — "Tag editing (via reference `metaflac`)" and "batch rename" sit in
the v0.2+ list, unplanned. They are the two things a trader does between *the show verifies*
and *the show is seedable*, so a set that passes `lh verify` still has to be finished by hand
in something else. This document plans both together, because they are one workflow over one
piece of domain knowledge: an etree file name and an etree tag set carry the same facts (band,
date, track number, title), and a tool that knows one for free knows most of the other.

---

## 0. The standards, and where they come from

Not from memory — these are quoted from the wiki the community maintains, the same way
`docs/torrent-creation.md` took its edge cases from TLH's own changelog rather than from
recollection of how torrents work.

### Naming — [NamingStandards](http://wiki.etree.org/index.php?page=NamingStandards), [ExtendedNamingStandards](http://wiki.etree.org/index.php?page=ExtendedNamingStandards)

* The base pattern is **`bbyyyy-mm-dd`**: a lowercase band abbreviation, then a four-digit
  year, two-digit month and two-digit day.
* **Tracks** carry disc and track on top of it: `ph2000-04-20d1t01.shn`. A short year is also
  in circulation (`gd77-05-08d1t01.flac`), and a title suffix is allowed —
  `gd1973-02-09d1t01bertha.shn` is the wiki's own example.
* **"Place a 0 before any single-digit track number (1 to 9). Without the 0, 10 would come
  before 2 during file sorting"** — which matters because that sort order is what gets burned
  to a disc.
* **"Use only letters, numbers, hyphens (-), and when necessary, periods (.) or underscores
  (_). Do not include spaces, ampersands (&), slashes or any other special characters."**
* The **show folder** has its own, longer format:
  `bbyyyy-mm-dd.mics.taper.sourceid.sbestatus.filetype`, e.g.
  `gd1969-01-25.sbd.kaplan.7923.sbeok.shnf` or `jgb1987-01-25.nak.corley.19810.sbeok.shnf`.
  `filetype` is `shnf`, `flac16` or `flac24` — **"Don't use .flac or .flacf when naming a
  directory."** The Extended page is explicit that this applies "only to the top-level
  directory holding a particular show" and is "not meant to be used for naming the individual
  Flac or Shorten files."

### Tagging — [FlacMetadata](http://wiki.etree.org/index.php?page=FlacMetadata)

Vorbis comments, which the page notes is "the only officially supported tagging mechanism in
FLAC", UTF-8 throughout:

| Field | What the wiki says goes in it |
|---|---|
| `TITLE` | the track title |
| `ARTIST` | the artist's name |
| `ALBUM` | "the Venue - City, State" |
| `DATE` | the show date, "follow the ISO standard, YYYY-MM-DD" |
| `TRACKNUMBER` | the track number |
| `GENRE` | the music style (its example is "Polka", plainly a joke) |
| `COMMENT` | "the source and other lineage information (taping rig, conversion...)" |
| `LOCATION` | venue, city, state and country — and this "should not go in the ALBUM field" |

On top of those eight we also write `TRACKTOTAL`, the number of tracks in the show. The etree
page does not name it, but Xiph's own Vorbis comment field recommendations do, and it is what
players such as VLC and foobar2000 read to show "track 1 of 16". Writing it as its own field
keeps `TRACKNUMBER` a plain number, as the etree page expects, rather than the ID3-style `1/16`.

The last two rows contradict each other about the venue. §7 Q1 records how we resolve it and
why that is a judgement call rather than a reading of the standard.

One more line from that page is load-bearing for §1: users are encouraged to correct metadata
freely, "since it won't change the audio, nor will it change the FLAC Fingerprint (FFP)."

---

## 1. The Principle 1 problem, and the contract that replaces it

`PLAN.md` §1 Principle 1 reads *"Never destroy data. v0.1 modifies nothing in place. Outputs
are new files written to a temp path and atomically renamed. Originals are never touched,
never deleted."* Renaming and tagging break that sentence literally, and this is the first
work in the repo that does.

The alternative was available and was rejected: both could write a full copy of the set into
an `-o` directory, exactly the contract `sbe fix` has. That is the right shape for repair,
which re-encodes every file and genuinely produces new audio. It is the wrong shape here —
duplicating a 600 MB show on disk to set three tags is not a safety feature, it is a
disk-space tax, and nobody would use the tool that way.

So Principle 1 is **amended**, in `PLAN.md`, rather than quietly contradicted:

> From v0.2, two operations modify files in place — `rename` and `tag` — because neither has
> a meaningful copy-to-output form. Both are bound by a stricter contract instead:
>
> 1. **Nothing is written until the complete before→after diff has been shown and confirmed.**
>    `--yes` on the CLI, `a` in the TUI. There is no unconfirmed path.
> 2. **Audio is never touched, and that is checked rather than asserted.** A tag write
>    replaces only the `VORBIS_COMMENT` block; a rename moves a directory entry. Every written
>    file is re-probed afterwards and its STREAMINFO MD5 compared against the digest read
>    before the write. A file whose audio MD5 moved is a bug: it is reported as one and aborts
>    the rest of the run. This is the wiki's "it won't change the FFP" turned into an enforced
>    postcondition.
> 3. **A set is renamed all at once or not at all** — the same guarantee `output::commit_all`
>    already gives a repair (`lh-core/src/output.rs:71`).

Point 2 is what makes point 1 safe to offer at all. The worst outcome of a confirmed-but-wrong
run is a set with wrong names or wrong titles, which is annoying and fixable; the outcome
Principle 1 exists to prevent — audio a trader cannot re-acquire, altered — is checked for on
every single file.

---

## 2. Decisions taken up front

Recorded here rather than left implicit, the way `docs/tui.md` §1 records what the TUI is for:

* **Writes land in place**, per §1. Not into an `-o` copy.
* **The full etree tag set**, all eight fields of §0 plus `TRACKTOTAL`, not just the three that started this
  (`TITLE`, `ARTIST`, `TRACKNUMBER`). The other five are show-level — one value for the whole
  set — so they cost one small form, not per-track work.
* **Titles are entered as one block**, one line per track in file order, pasted or edited.
  Not typed per row, and not scraped out of the show's info `.txt`: info files are free-form
  prose and a scraper that is right 70% of the time produces work rather than saving it. §7 Q5
  keeps the option open.
* **Rename touches files only.** The show folder's own name is *parsed*, to seed the band and
  date so they are not retyped, and then left alone. §7 Q2.
* **Preview is the default; `--yes` writes.** There is deliberately no `--dry-run` flag — see
  §5.

---

## 3. The name as a type (N1)

`lh-core/src/etree/mod.rs`. Pure, no I/O, no new dependency.

* `ShowDate { year, month, day }`, with `parse`, `render_long` (`1977-05-08`) and
  `render_short` (`77-05-08`). No date crate: the workspace has none, and this needs three
  fields and a range check, not a calendar.
* `TrackName { band, date, short_year, disc: Option<u32>, track: u32, suffix: Option<String>,
  ext }`.
  * `TrackName::parse(&str) -> Option<Self>` accepts both year forms, with and without `dN`,
    with and without a title suffix. A name that is not an etree name returns `None` — never a
    half-parse that silently loses a segment.
  * `TrackName::render()` always zero-pads the track to two digits (§0's sort-order rule lives
    here and nowhere else).
* `ShowName::parse(&str) -> Option<Self>` for the folder: band, date, the free middle segments
  (mics, taper, source id, sbe status) and the format tag. **Parse only.** There is no
  `render`, so there is no code path that could rename a folder — §2's decision enforced by
  the absence of a function rather than by a flag defaulting to off.
* `sanitize(&str)` — §0's charset rule, applied to any user-supplied suffix. One
  implementation, one place.

---

## 4. Tags and renames as operations (N2, N3)

### `lh-core/src/tag/mod.rs`

`Tags`, the eight fields of §0 plus `TRACKTOTAL`. `read(path)` and `apply(path, &Tags)`, over the `metaflac`
crate `format::flac::probe` already uses (`lh-core/src/format/flac.rs:11`). `apply` writes only
the fields the edit names and **preserves every other comment in the block, and the vendor
string** — that string is Principle 2's provenance marker and not ours to rewrite.

This module is mostly a promotion, not new code: `analysis/sbe_fix.rs:440-467` already carries
private `read_vorbis_comments` / `write_vorbis_comments`, written for repair's tag-preservation
step and covered by `lh-core/tests/sbe_fix.rs:231`. They move here and `sbe_fix` calls them
from here, so the new module starts with the existing tests behind it instead of growing a
second implementation of the same thing.

`assert_audio_unchanged(path, before)` — re-probe, compare STREAMINFO MD5 — is §1 contract
point 2, and both executors call it.

Non-FLAC members of a set have no Vorbis comments at all, and they are not tracks either: a
WAV left beside its converted FLAC would otherwise sort between the FLACs and shift every
track number after it. So tagging runs over the FLAC files alone — `TRACKNUMBER`, the titles
list and the `--titles` line count all count FLACs only — and what was left out is still
said, never a silent skip (Principle 5): `lh tag` prints an `N/A` line per non-FLAC file, the
TUI's diff pane title counts them. A folder with no FLAC at all is refused.

### `lh-core/src/rename/mod.rs`

Plan and execute, mirroring `analysis::sbe_fix`'s split — the repo's established shape for a
whole-set operation.

* `NameSpec { band, date, short_year, disc, keep_suffix }`, and
  `plan_rename(files, spec) -> RenamePlan`. Track numbers come from position in `scan`'s
  order, which is `sort_by_file_name` (`lh-core/src/scan.rs:21`) — the order every other
  whole-set operation here already trusts. Each row is `Unchanged`, `Changed`, or `Collision`
  (two files targeting one name).
* `execute_rename(plan)` refuses a plan holding any `Collision`, and is **two-phase**: every
  file renamed to a unique temp name in its own directory first, then to its final name. A
  plan can legitimately be a permutation (`t01`↔`t02`), which a one-phase rename would clobber
  mid-cycle. The temp phase also handles the case-only rename (`T01` → `t01`) a
  case-insensitive macOS or Windows filesystem refuses as a no-op — `PLAN.md` §9 already
  tracks Windows path handling as a risk, and this needs its own test there.
* Failure rolls every completed rename back, the same all-or-nothing guarantee
  `output::commit_all` gives.

---

## 5. The commands (N4)

```
lh tag <DIR> [--artist S] [--album S] [--date YYYY-MM-DD] [--genre S]
             [--comment S] [--location S] [--titles FILE] [--yes]
lh rename <DIR> [--band bb] [--date YYYY-MM-DD] [--short-year] [--disc N] [--yes]
```

Both take a directory rather than `Paths`, like `sbe fix` does (`SbeFixArgs.dir`): a show is
the unit, and a track number only means something relative to its siblings.

* `--titles FILE` reads one title per line in file order; `-` reads stdin, which is the
  headless equivalent of the TUI's paste block. A line count that does not match the file
  count is an error naming both numbers — never a best-effort partial apply.
* `TRACKNUMBER` is always derived from position, and `TRACKTOTAL` from the number of FLAC
  files, never typed. There is no case where typing either by hand is right.
* Band and date default from `ShowName::parse` of the directory's own name when it is an etree
  name. When it is not, they are required, and their absence is a specific error naming what
  it wanted (Principle 5).

**Both commands print the full diff and exit without writing unless `--yes` is given**, and
there is deliberately **no `--dry-run` flag**: the preview *is* the command, so a second
spelling of the default would be redundant.

This inverts the workspace's one existing precedent, and the inversion is the point.
`lh sbe fix` executes unless you pass `--dry-run` (`lh-cli/src/lib.rs:244`, `:460`) — safe
there only because `-o/--output` is mandatory to execute and repair never writes over an
original, so there is nothing a preview needs to guard. `tag` and `rename` write in place and
have no `-o` to serve as that guard, so the guard moves to the confirmation. `sbe fix`'s own
flag and default are unchanged by any of this.

---

## 6. The screens (N5)

`lh-tui tag <dir>` and `lh-tui rename <dir>`, reachable for free since `lh-tui` parses the
identical `Cli` (`docs/tui.md` §0).

**These are the first screens in this repo that edit rather than watch**, and `docs/tui.md`
§2's per-screen pattern does not cover input at all — every screen so far submits jobs to a
`Queue<T>` and folds events into a table. That doc gains an "editor screen" section recording
two things this work forces:

* **`q` cannot mean quit while a field has focus** — it is a letter someone is typing. The key
  contract becomes: `Esc` leaves the focused field, and from no field leaves the screen;
  `Ctrl-C` always aborts, everywhere, unchanged; `q` quits only when nothing is focused. This
  is the one real break from the `q`/`Esc`/`Ctrl-C`-are-identical rule §2 fixes for every
  other screen, and it is forced rather than chosen.
* **Bracketed paste has to be enabled explicitly.** `ratatui::init()` does not turn it on, so
  a pasted setlist arrives as a stream of individual `Char` events racing the 80 ms poll loop.
  Enable `crossterm::event::EnableBracketedPaste` for these two screens, handle
  `CtEvent::Paste(String)` by splitting on newlines, and restore it alongside
  `ratatui::restore()`. This is what makes "paste a setlist" one operation.

No new dependency for text entry: a `Field { value, cursor }` handling
`Char`/`Backspace`/`Left`/`Right`/`Home`/`End` covers eight single-line fields, and a
`Vec<String>` with a selected index covers the titles block.

**Tag screen** opens on an **overview** of what the files carry now: the show-level fields
once in a pane at the top (or "differs between files" when they are not uniform), and per
file its `#` (`1/16`), `TITLE` and name. Most folders need nothing, or only titles, so
editing is a choice made from there: `e` opens the editor, and pasting titles straight into
the overview opens it with the paste applied. `Esc` from the editor (with no field focused)
goes back to the overview, keeping the edits.

The **editor** is three panes over the usual footer:

1. **Show fields**: `ARTIST`, `ALBUM`, `DATE`, `GENRE`, `COMMENT`, `LOCATION`, one value for
   the whole set, `Tab`/`Shift-Tab` between them. Seeded from `ShowName::parse` of the folder
   and from the existing tags of the first file that has any.
2. **Titles**: N lines, one per file, in order, pre-filled from existing `TITLE` tags. `e`
   enters the block, paste replaces it wholesale, `Esc` leaves it.
3. **Diff**: per file, every field that would change, `old -> new`, unchanged fields dim.
   `TRACKNUMBER` and `TRACKTOTAL` appear here — visible, derived, not editable — as a
   leading `#` column reading `1/16`, accented when either would change.

**Rename screen**: the `NameSpec` fields above a live `from -> to` table, recomputed by
`plan_rename` on every keystroke. That is pure arithmetic over names already in memory, the
same reason the SBE fix plan is computed before the terminal opens
(`lh-tui/src/main.rs:1135`). `Collision` rows render in `theme.error`, and `a` refuses to
apply while any exist.

**Applying** in both screens is the shape every existing screen already has: one `job::Queue`
job per file, status going `Pending → Running → OK/FAILED` with the gauge beneath — so the
write phase reuses `docs/tui.md` §2 unchanged and only the edit phase is new. The post-write
audio-MD5 re-check (§1, point 2) runs inside each job, so a file that somehow changed shows
`FAILED` on its own row rather than in a summary nobody reads.

The tag screen's apply stage is the overview again, with a status column added: each job
re-reads the file's tags after the audio check, and the row shows that read, so the table is
the file's state rather than a restatement of the plan. When the writes finish the screen
stays on that overview, and what the files now carry becomes the baseline for another `e`.

Every colour comes from `Theme` (`lh-tui/src/main.rs:89`), never inlined — `docs/tui.md` §0.

---

## 7. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**N1**~~ | ~~The name as a type~~ | **Done** — `lh-core/src/etree/mod.rs` (§3): `ShowDate`, `TrackName` parse/render, `ShowName::parse`, `sanitize`. Pure, no I/O. Tests round-trip every example name on both wiki pages, plus the leading-zero rule and a non-etree name that must return `None`. |
| ~~**N2**~~ | ~~Tags~~ | **Done** — `lh-core/src/tag/mod.rs` (§4): `Tags`, `read`, `apply`, `assert_audio_unchanged`; `sbe_fix`'s private comment helpers promoted here and called from there. Oracle test against the real `metaflac` binary — `ToolId::Metaflac` is in the registry (`lh-core/src/tools/mod.rs:40`) and has never had a caller; this is its first. Plus the FFP invariant: `checksum::compute(Ffp)` equal before and after an `apply`. |
| ~~**N3**~~ | ~~Renames~~ | **Done** — `lh-core/src/rename/mod.rs` (§4): `NameSpec`, `plan_rename`, `execute_rename`, two-phase with rollback. Tests for a permutation, a collision refused, a case-only rename, and a deliberately provoked rollback. |
| ~~**N4**~~ | ~~CLI~~ | **Done** — `lh tag` and `lh rename` (§5), preview-by-default with `--yes`; the Principle 1 amendment written into `PLAN.md` §1 and both operations moved out of its §2 deferred list. `lh-cli/tests/cli.rs` covers the unconfirmed default writing nothing, a collision, and a title-count mismatch. |
| ~~**N5**~~ | ~~TUI~~ | **Done** — both screens (§6), and `docs/tui.md` §2 extended with the editor-screen pattern (`docs/tui.md` §7, "TUI6"). Driven for real in a pty against copies of real fixtures, with focus-gating, a real tag write and a real rename, and post-write FFPs confirmed against `lh-core/tests/fixtures/reference.ffp`. |

GUI: open, as it is for `sbe fix` (`docs/sbe-repair.md` §7). TLH's own **Tools** menu is where
this belongs there once that exists.

### How each milestone is checked

`cargo test --workspace`, `cargo clippy --all-targets` and `cargo fmt --check` clean, as
everywhere else here, plus:

1. **`metaflac` as the oracle**, the same way it is the oracle for FFP in
   `lh-core/tests/corpus.rs`: tag a fixture with us, read it back with
   `metaflac --export-tags-to=-`, compare field for field. Ours must not be the only thing
   that can read what we wrote.
2. **The FFP invariant, measured**: `lh ffp` over a folder before and after a full
   `lh tag --yes` run — the two files byte-identical. Same after `lh rename --yes`, where only
   the names in it may differ.
3. **`lh verify` after every write** — the end-to-end form of §1 point 2.
4. **A real show, not just fixtures.** `docs/tui.md` §8 records a genuine 17-track FLAC show
   used to check the verify screen; run both screens against a **copy** of it, paste a
   17-line setlist, apply, then confirm with `lh check` against that show's own committed
   `.ffp` that nothing about the audio moved.
5. **Rollback, provoked deliberately**: a plan whose third rename is made to fail must leave
   all three original names in place.

---

## 8. Open questions

1. **The etree wiki contradicts itself about the venue.** FlacMetadata says `ALBUM` holds "the
   Venue - City, State" and also says venue/city/state "should not go in the ALBUM field"
   because `LOCATION` exists. Proposed resolution: default `ALBUM` to the show date, offer
   venue/city/state in `LOCATION`, let the user override either. That is a convention call,
   not a reading of the standard, and it should be made deliberately.
2. **The folder is left alone** (§2), so a set can end up with etree filenames inside a
   non-etree folder. `ShowName` already parses one; rendering it is a small addition if folder
   renaming is later wanted.
3. **Multi-disc sets.** `--disc` is one value for the whole run, right for a set laid out one
   folder per disc and wrong for a flat folder holding two. Detecting the split means
   guessing; deferred until someone hits it.
4. **`GENRE` has no sensible default** for live trading material. Left empty unless typed.
5. **Info-file import**, ruled out in §2, is the obvious next convenience if typing setlists
   turns out to be the slow part in practice. It would want to be a proposal the user edits,
   never an automatic apply.
6. **Does `tag` belong in the same screen as `rename`?** They share every fact (band, date,
   track number, title) and a user doing one usually wants the other. Two screens is the
   smaller first step; one combined "finish this show" screen is a plausible second.
