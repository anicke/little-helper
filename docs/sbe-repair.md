# SBE repair

Fix sector boundary errors, not just report them.

[`lh-core/src/analysis/sbe.rs`](../lh-core/src/analysis/sbe.rs) already answers *is this file
misaligned*, per file, in-process (PLAN.md Principle 3 — read-only analysis needs no reference
tool). This document is about the other half: making a misaligned track aligned, which is a
different kind of operation from anything v0.1 ships. Every other v0.1 command — `verify`,
`ffp`/`md5`/`st5`, `sbe`, `convert` — takes a *batch* of files where order never matters and
each one is judged alone. Repair cannot work that way, and that shapes everything below.

---

## 0. What the original did

Trader's Little Helper does not compute the fix itself. It is a front end for `shntool`'s
`fix` mode, run over the ordered file list in its main window. `shntool fix` shifts audio
**samples across the split point between adjacent files** so each boundary lands on a CD
sector (2352 bytes / 588 frames — the same constant `FRAMES_PER_SECTOR` already names),
without changing the concatenation of the whole set: nothing is added or dropped, only
reassigned to the file on the correct side of the boundary. Its boundary policy is a flag —
shift backward (`-b`, the default), forward (`-f`), or to the nearest sector (`-u`) — and it
has a `--pad`-equivalent for the one case that policy cannot fix: the very last file in the
set, which shntool leaves alone unless told to pad it with silence, because there is no
"next file" to borrow from or lend to.

TLH's own UI reflects this: it works from the *file list window*, and its default "skip the
first N files that would not be changed" wording only makes sense if the whole set is
in view before the tool decides anything. Fixing one file was never the operation; fixing a
disc's worth of splits was.

Two things this tells us, and both are design input:

* **The unit of work is an ordered set of files, not a file.** `sbe()` (per-file) stays
  exactly as it is — it is the detector repair runs before and after — but repair itself
  needs siblings and their order the way `torrent::layout` already groups a release's files.
  There is no such grouping concept anywhere near `analysis` or `convert` today.
* **A boundary fix touches two files at once, or the whole chain touches all of them.**
  Shifting `remainder_frames` samples from the end of track *N* into the front of track
  *N+1* changes both files' lengths — track *N+1* now carries someone else's leftover frames,
  which may itself walk its own boundary into misalignment, which is why TLH runs this over
  the *entire* set in one pass rather than one boundary at a time.

---

## 1. What "fix" means here, precisely

> **Move samples, don't invent or destroy them.** For every boundary between file *N* and
> file *N+1*, decide how many frames (if any) cross it, and in which direction, so file *N*'s
> length becomes a multiple of 588. Do this in file order, left to right, because fixing
> boundary *N* changes how many frames file *N+1* starts with, which changes what boundary
> *N+1* needs.

The last file in the set is the one case this cannot fix by borrowing, because there is no
file *N+1* to hand its remainder to. Two outcomes are legal for it, and both must be a choice
the caller makes, not a silent default:

* **Leave it and report it.** If the last file isn't sector-aligned once every other boundary
  in the set is fixed, that's not a splitting mistake — either the set is not a complete
  contiguous recording, or the source disc genuinely ends off-sector. Reporting it as
  `Sbe::Misaligned`, unchanged, is honest.
* **Pad it with silence**, TLH's opt-in behaviour. This is the one operation in this document
  that actually **adds** samples rather than reassigning them — the audio changes, audibly at
  the last fraction of a sector, in exchange for a disc that burns clean. It must never be the
  default.

**Invariant that has to hold no matter what:** decode every file in the set before the fix,
concatenate the PCM, and MD5 it. Decode every file after the fix (excluding a padded last
file, whose stream legitimately grew), concatenate, and MD5 it again. **The two must match.**
That one check is the whole correctness argument for the non-padding files — see §5.

---

## 2. Where this sits in the existing architecture

It sits inside every principle without straining one:

* **Principle 1 (never destroy data).** Fixing two files at once means two temp-then-rename
  commits have to succeed or fail *together* — `TempOutput` (`lh-core/src/output.rs`) commits
  one file at a time today, and nothing currently needs it to do otherwise. This is new
  infrastructure, not a new use of existing infrastructure.
* **Principle 2 (provenance).** The output is still FLAC that traders will inspect, so the
  final write still has to go through the reference `flac` binary the way `convert::to_flac`
  already does — a fix that round-tripped through a homemade encoder would carry no vendor
  string worth trusting.
* **Principle 3 (in-process where deterministic).** The sample-shifting arithmetic itself —
  "take these frames off the end of this decoded buffer, prepend them to that one" — is
  exactly as deterministic as the FLAC decode already done in-process for `convert::to_wav`.
  There is no need to shell out to `shntool fix` to get this right; §3 below is explicit about
  why re-implementing the arithmetic beats driving the binary.
* **Principle 4 (headless-complete).** `lh sbe fix` has to be a real CLI command before any
  TUI/GUI screen exists for it, same as every other v0.1 operation.
* **Principle 5 (fail loudly).** A set that isn't sector-aligned even after every internal
  boundary is fixed is not a bug to swallow — say which file, by how much, and that `--pad`
  was not given.

### Why re-implement the shift instead of shelling out to `shntool fix`

`shntool` is already a discovered tool (`ToolId::Shntool` in `lh-core/src/tools/mod.rs`),
currently used only for ST5 checksums, and it would be the obvious reference binary to drive
for repair the same way `flac` is driven for encoding. Two things point the other way:

* **Its role in Principle 2 doesn't apply here.** The FLAC vendor string is a signature other
  traders' tools inspect and *must* say `reference libFLAC`; nothing inspects "which tool
  moved these samples across a WAV boundary" the way it inspects an encoder's identity. The
  arithmetic has no audience the way the encode does.
* **Its CLI semantics are the file-list-shaped UI TLH wraps, not a clean library call.**
  "Skip the first N files that would not be changed" and per-invocation output-naming
  conventions are things to *parse around* rather than reasons to prefer it. Decoding with
  `claxon` (already how `format::flac::decode_to_wav` works), shifting sample slices in Rust,
  and re-encoding with the reference `flac` binary is fewer moving parts and fully covered by
  the existing corpus-test machinery.

`shntool` stays in the registry for ST5 either way; this doesn't touch that.

### Tags, which repair cannot be allowed to drop

`convert` round-trips FLAC → WAV → FLAC and currently loses tags, because tag editing is its
own deferred item (PLAN.md §2). Repair inherits that same round trip for two files at a time,
but here it isn't a deferred nicety — a fix that silently strips `TITLE`/`ARTIST`/
`TRACKNUMBER` from every track in a show is a regression, not a missing feature, and would
make the tool worse than not touching the file. So repair pulls in, as a hard dependency
rather than an optional extra:

1. Read each file's Vorbis comments before decode (`metaflac`, already a discovered tool).
2. Write them back onto the re-encoded FLAC after `flac` produces it, before the temp file is
   committed.

This is the one place SBE repair reaches into territory PLAN.md files under "tag editing" —
it has to, narrowly, because the alternative is destructive.

---

## 3. Data model

New module, `lh-core/src/analysis/sbe_fix.rs`, beside the detector it consumes.

```rust
/// One boundary between adjacent files, and what crossed it.
pub struct BoundaryFix {
    /// Index into the ordered set; the boundary sits between `index` and `index + 1`.
    pub index: usize,
    /// Frames moved. Positive: taken from the end of `index`, prepended to `index + 1`.
    /// Negative: taken from the start of `index + 1`, appended to `index`. Zero: already
    /// aligned, listed so a caller can show "no change" rather than a gap.
    pub shifted_frames: i64,
}

/// What happened to the last file in the set, which no boundary can fix by borrowing.
pub enum TailPolicy {
    /// Leave it. If still misaligned, that is reported, not hidden.
    Report,
    /// Add silence to reach the next sector boundary.
    Pad,
}

/// The whole set's outcome, computed before anything is written.
pub struct FixPlan {
    pub boundaries: Vec<BoundaryFix>,
    /// `Some` only when the tail needed padding and `TailPolicy::Pad` was given.
    pub tail_padding_frames: Option<u64>,
    /// True when every file in the set reports `Sbe::Aligned` after applying this plan
    /// (the padded tail excepted, which is aligned by construction).
    pub fully_fixed: bool,
}
```

`FixPlan` is computed from nothing but each file's `StreamInfo.total_frames` — the same
header-only read `sbe()` already does — so a plan can be shown to a user (CLI dry run, TUI
preview) with **no decode**, exactly like `Sbe` itself. Only executing the plan touches audio.

```rust
pub struct FixOpts {
    pub direction: BoundaryDirection, // Backward | Forward | Nearest — shntool's -b/-f/-u
    pub tail: TailPolicy,
}

/// One committed repair.
pub struct Fixed {
    pub path: PathBuf,
    pub shifted_in: i64,   // frames gained from the previous file's tail (can be negative)
    pub shifted_out: i64,  // frames given to the next file's head (can be negative)
    pub audio_md5: [u8; 16],
    pub provenance: Provenance,
}
```

---

## 4. Algorithm

1. **Order the set.** Same rule any per-release grouping needs: filename order (track
   number), refuse to guess when it's ambiguous. Whether this reuses or duplicates
   `torrent::layout`'s ordering is an open question (§7).
2. **Probe every file's `StreamInfo`** (header-only, already-existing `format::probe`).
   Refuse the set if any file is not CD audio (`is_cdda()`), the same case `sbe()` reports as
   `NotApplicable` — repair has nothing to align for a file with no sector concept.
3. **Compute the plan** (`FixPlan`), left to right: at each boundary, apply `direction` to
   decide how many frames of file *N*'s remainder move, and which way. This is pure integer
   arithmetic over frame counts, no audio read yet.
4. **Show the plan.** CLI and TUI both get a preview before committing — which boundaries
   move, by how many frames, and whether the tail needs `--pad` to finish the job. A plan
   that reports `fully_fixed: false` without `TailPolicy::Pad` set is not an error, it's the
   expected shape when the caller didn't ask for padding.
5. **Execute**, per boundary, left to right:
   a. Decode both neighbouring files to raw interleaved samples in memory (`claxon`, the same
      path `decode_to_wav` uses — full decode, not header-only, from here on).
   b. Move the boundary's frames between the two in-memory sample buffers.
   c. Read each file's Vorbis comments before this step ever touches it, if not already read.
   d. Re-encode each finished buffer to FLAC via the reference `flac` binary
      (`convert::to_flac_cancellable`'s WAV → FLAC path, fed an in-memory WAV rather than one
      read from disk — a small generalisation of that function's `src` argument, or a sibling
      that takes samples directly).
   e. Write the original Vorbis comments back with `metaflac`.
   f. Stage both outputs with `TempOutput`, but do not commit yet.
6. **Verify the invariant** (§1, §5): decode the *whole staged set* in order, MD5 the
   concatenated PCM, compare against the MD5 of the whole original set decoded the same way
   (excluding a padded tail's added silence from the "before" side of the comparison).
7. **Commit all staged outputs, or none.** This is the one place existing infrastructure falls
   short: `TempOutput::commit` is one file at a time. A `MultiTempOutput` (or a `Vec<TempOutput>`
   plus a helper that renames all-or-nothing, rolling back any already-renamed file if a later
   one fails) is new, small, and belongs in `lh-core/src/output.rs` beside what's there.
8. **Report**, per file: frames gained/lost at each edge, new audio MD5, whether it now
   reports `Sbe::Aligned`.

Step 6 is not optional the way a similar self-check is optional elsewhere. `torrent::create`
re-parses its own output as a cheap sanity check (docs/torrent-creation.md §5 step 8); here
the check is the entire evidence that repair didn't corrupt audio, so it runs unconditionally
and a mismatch aborts the whole batch before anything is renamed into place.

---

## 5. Testing — the invariant is the oracle

There's no third-party binary to check output equality against the way `mktorrent` checks
torrent creation (docs/torrent-creation.md §8) — no independent tool computes "what would a
correctly repaired version of this exact fixture look like" for comparison. The invariant
from §1 has to carry the whole correctness argument:

* **Round-trip identity.** Concatenated audio MD5 before and after must match, for any
  fixture where the tail isn't padded. This is the one test that would catch a shift in the
  wrong direction, an off-by-one in frame count, or a boundary applied to the wrong pair of
  files — all of which would still produce *aligned* output, just wrong audio, so `sbe()`
  reporting `Aligned` afterward is necessary but nowhere near sufficient.
* **Alignment achieved.** Every file except a deliberately-unpadded tail reports
  `Sbe::Aligned` via the existing detector after the fix — reusing `sbe()` as the assertion,
  not reimplementing a second check.
* **Tags survive.** A fixture with real Vorbis comments on both sides of a boundary, asserted
  byte-identical after repair.
* **Single boundary, exact case from real life.** Two tracks, `remainder_frames: N` on the
  first, verify the fix moves exactly `N` frames and not, say, `588 - N`.
* **Chained boundaries.** Three or more tracks where fixing boundary 1 changes what boundary 2
  needs — the case that makes this a left-to-right pass rather than independent per-boundary
  fixes.
* **Tail cases.** Last file misaligned with `TailPolicy::Report` → reported, unchanged,
  nothing written for it. Last file misaligned with `TailPolicy::Pad` → padded, and the
  invariant check accounts for the added silence rather than failing on it.
* **Atomic failure.** Kill the process (or inject a failure) between staging and committing
  a multi-file batch, assert nothing under the real names changed — the same property
  `TempOutput`'s `Drop` already gives a single file, extended to the batch.
* **Non-CD-audio member of a set** → whole set refused, matching `NotApplicable`'s reasoning
  in the single-file detector.

Fixtures are synthetic, generated the way the existing corpus is (a known-good continuous PCM
stream, split at a deliberately wrong sample offset into two or three FLACs) rather than
sourced from a real misaligned rip — the point of the test is the arithmetic, not a specific
disc.

---

## 6. CLI surface

```
lh sbe fix <DIR> [--direction backward|forward|nearest] [--pad-tail] [--dry-run]
                 [-o DIR] [--overwrite]
```

Operates on one directory as one ordered set (by filename), the same way `lh torrent create`
takes a folder rather than a file list. `--dry-run` prints the plan (§4 step 4) and writes
nothing — cheap, because computing a plan needs no decode. `-o` is required to execute
without `--dry-run` (Principle 1: never write over the originals). `--pad-tail` is honoured
by both planning and execution as of R3: it actually adds the silence rather than only
reporting the gap.

Execution reports per file as it writes it — there is no in-place-feeling multi-line summary
the way the plan preview has one, because each file is a separate encode-and-commit rather
than a single pass over a table:

```
$ lh sbe fix ~/shows/gd1977-05-08/d1 --dry-run
d1
  01 → 02   shift 3 frames backward   (t01 was +3 past a sector)
  02 → 03   already aligned
  03        last file, +141 frames short of a sector — not fixed without --pad-tail

$ lh sbe fix ~/shows/gd1977-05-08/d1 -o ~/shows/gd1977-05-08/d1-fixed
FIXED     01.flac -> /home/.../d1-fixed/01.flac   audio md5 81e1447c...
FIXED     02.flac -> /home/.../d1-fixed/02.flac   audio md5 ab8c522d...
FIXED     03.flac -> /home/.../d1-fixed/03.flac   audio md5 4a33dfcd...
03.flac   still misaligned — rerun with --pad-tail to close it with silence

$ lh sbe fix ~/shows/gd1977-05-08/d1 -o ~/shows/gd1977-05-08/d1-fixed --pad-tail
FIXED     01.flac -> /home/.../d1-fixed/01.flac   audio md5 81e1447c...
FIXED     02.flac -> /home/.../d1-fixed/02.flac   audio md5 ab8c522d...
FIXED     03.flac -> /home/.../d1-fixed/03.flac   audio md5 283c0f1a...
```

Exit codes follow the existing contract (docs/torrent-creation.md §6): `0` fully fixed
(or nothing needed fixing), `1` the set has something the tool won't override (an unpadded
misaligned tail, a non-CD-audio member), `2` the command failed — including a missing `-o`
or a non-FLAC member of the set.

---

## 7. TUI / GUI

**TUI: done.** `run_sbe_fix` (`lh-tui/src/main.rs`) still follows `docs/tui.md` §2's overall
shape (input → table + gauge), but not its `Queue<T>`-per-file mechanics: those screens' jobs
are independent files, submitted and reported one row per file with no relationship between
rows, and a fix's boundaries are the opposite — boundary *N*'s outcome depends on boundary
*N-1*'s, and `execute_fix` computes/writes the whole set as one atomic call, not a pool of
independent per-file jobs. Rather than growing `Queue<T>` a chained-submission mode nothing
else needs, the fix screen routes around it: `plan_fix` runs before the terminal even opens
(pure arithmetic, no decode) and renders as a static per-file table for `--dry-run`;
executing submits `execute_fix` itself as the single job on a `Queue::with_workers(1)`, the
same "one job on a queue of one" shape `torrent create`/`check` already use for a single
sequential operation over a whole set. Every row updates together when the one `Finished`
event lands, because the underlying operation is atomic — there is no meaningful per-file
"running" state to show in between.

**GUI: still open.** The GUI's own "Tools" menu (below) is unclaimed territory.

The GUI's own "Tools" menu — the name TLH itself uses for exactly this kind of repair, per
PLAN.md §4's note distinguishing it from the discovered-binary "Binaries" panel — is where
this belongs once it exists.

---

## 8. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**R1**~~ | ~~Plan, no execution~~ | **Done** — `analysis::sbe_fix::plan_fix` (§3, §4 steps 1–4): left-to-right arithmetic over `StreamInfo.total_frames`, no decode, refuses a non-CD-audio member. `lh sbe fix <DIR> --dry-run [--direction] [--pad-tail]`; bare `lh sbe <paths>` is unchanged. 10 tests (aligned/misaligned/chained boundaries, all three directions, tail report vs. pad). |
| ~~**R2**~~ | ~~Two-file execution~~ | **Done** — `analysis::sbe_fix::execute_single_boundary` (§4 steps 5–7), the one-boundary entry point now built on top of `execute_fix` (R3): decode both neighbours in full (`format::flac::decode_to_samples`), shift frames across the split, re-encode each through the reference `flac` binary (`convert::encode_flac_staged`, the staged-not-committed half of `to_flac_cancellable`), restore original Vorbis comments via `metaflac`, verify the round-trip PCM MD5 invariant (§1, §5, `format::flac::concatenated_pcm_md5`) unconditionally, then commit both outputs atomically (`output::commit_all`, all-or-nothing with rollback). `-o` is mandatory to execute (open question 4 resolved this way: never an implicit default of overwriting the originals). |
| ~~**R3**~~ | ~~Chained sets~~ | **Done** — `analysis::sbe_fix::execute_fix` generalizes R2 to any number of files: every boundary in `FixPlan.boundaries` applied left to right against a `Vec<Vec<i32>>` of decoded buffers (chaining falls out for free, since each boundary reads whatever the previous one already wrote into the shared buffer), `TailPolicy::Pad`'s silence appended to the last buffer and excluded from the invariant's "after" side, then every file encoded, tag-restored, checked and committed exactly as R2 did per pair. `lh sbe fix <DIR> -o <OUT> [--direction] [--pad-tail] [--overwrite]` (no `--dry-run`) now executes a directory of any size, per §6. 3 more tests in `lh-core/tests/sbe_fix.rs` (a real three-file chain verified end to end, tail padding executed and excluded from the invariant, atomic rollback across three staged outputs), 3 unit tests for `execute_fix`'s own shape checks, and 2 more CLI tests (`--pad-tail` fully aligning a set, a chained three-file directory). |
| ~~**R4**~~ | ~~TUI~~ | **Done** — `run_sbe_fix` (`lh-tui/src/main.rs`): §9 open question 3 resolved by routing around `Queue<T>` rather than growing it a chained-submission mode, since a fix's unit of work is the whole ordered set, not an independent file. `plan_fix` runs before the terminal opens (pure arithmetic, no decode) and is shown as a static per-file table for `--dry-run`; executing submits `execute_fix` as the one job on a `Queue::with_workers(1)`, the same "one job on a queue of one" shape `torrent create`/`check` use for a single sequential operation — quitting breaks the screen immediately rather than waiting for `Done`, since `execute_fix` has no cancellation checkpoint to honor, matching `run_torrent_check_screen`'s own acceptance of that gap. GUI screen remains open. |

---

## 9. Open questions

1. **How is an ordered set named?** By directory, by filename/track-number sort, or does it
   need the same grouping concept a multi-disc torrent needs (PLAN.md's torrent-creation open
   question 6)? Worth settling once, not once per feature that needs "the tracks of this
   release, in order."
2. **`--direction`: expose all three of shntool's policies, or pick one and drop the flag?**
   TLH exposes backward/forward/nearest because different trading circles have different
   conventions for where a "correct" split falls. Nothing about this codebase's own principles
   picks one over the others — this is a community-convention question, not an engineering one.
3. ~~**Does `Queue<T>` grow a sequential mode, or does the fix screen route around it?**~~
   **Resolved in R4** (§7): it routes around `Queue<T>` — `execute_fix` runs as the one job
   on a `Queue::with_workers(1)`, the shape `torrent create`/`check` already use.
4. **In-place by default, or always a new directory?** `convert` defaults beside the source
   with a new extension, never overwriting without `--overwrite`, and `TempOutput::stage`
   actively refuses `src == dst`. A "fix" conceptually replaces the same track in place, which
   is exactly the shape Principle 1 is most wary of — this may want a mandatory `-o` (never an
   implicit default of overwriting the originals) even where `convert` allows one.
