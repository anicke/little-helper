# Little Helper — Application Plan

A cross-platform, open-source successor to *Traders' Little Helper* (Windows freeware),
for the live-music trading community: verify, checksum and convert lossless audio.

- **Repo:** `little-helper`
- **Binary:** `lh`
- **Platforms:** Linux, macOS, Windows
- **License:** MIT OR Apache-2.0 (bundled reference tools remain GPL — see Licensing)

---

## 0. Status

*Updated 2026-08-30.*

| Milestone | State |
|---|---|
| M0 Scaffold | **done** — workspace, CI matrix, toolchain pin, fixture generator |
| M1 `lh-core` | **in progress** — probe, checksums, SBE, verify, scan, tool registry, conversion, torrent read/write/tracker list, job queue (J1, J2) done; see [docs/job-queue.md](docs/job-queue.md) |
| M2 `lh-cli` | **in progress** — `info`, `verify`, `sbe`, `ffp`, `md5`, `st5`, `check`, `convert`, `tools`, `torrent info/create/check/trackers` all work |
| M3 `lh-gui` | **in progress** — see [docs/gui.md](docs/gui.md); G0 spike and G1 (scaffold, file table, Tools panel) done, G2 (job queue) not started |
| M4 Packaging | not started |

130 tests passing, clippy clean. Our FFP output matches `metaflac --show-md5sum` byte for byte
on the fixture corpus, and our FLAC → WAV output matches `flac -d` byte for byte — including
the `WAVE_FORMAT_EXTENSIBLE` header at 24 bits.

Every v0.1 operation in §2 is now implemented, and multi-file CLI batches (`verify`, `sbe`,
`ffp`/`md5`/`st5`, `convert`) and `torrent create` run through the job queue for parallelism,
progress and `Ctrl-C` cancellation. `convert`'s FLAC → WAV direction also reports frame-level
progress, and its WAV → FLAC direction can be killed mid-`flac`, not just between files
(J2; see [docs/job-queue.md](docs/job-queue.md) §8 for why `flac`'s own progress display
turned out not to be usable at all). What remains for v0.1 is the GUI and packaging.
[docs/gui.md](docs/gui.md) plans M3: a spike (G0) confirmed the Iced 0.14 `Subscription`
bridge over `job::Queue`'s event channel works as `docs/job-queue.md` hoped, and that
window-level drag-and-drop is available.

**Known limitations to close before v0.1:**

1. ~~**ST5 has not been checked against real shntool.**~~ **Closed 2026-08-30**, and the
   assumption was wrong. Trader's Little Helper bundles `shntool.exe` 3.0.4 in
   `CmdlineApps/`, which runs under Wine, so the oracle was on the machine all along.
   Golden `reference.st5` is now committed (`scripts/gen-st5-oracle.py`).
   * The **digests** were right: six fixtures, WAV and FLAC, match shntool exactly.
   * The **layout** was not. A `.st5` is `shntool hash -m` output *verbatim*, which puts a
     `  [shntool]  ` tag between the digest and the name — TLH runs
     `shntool.exe hash -m -- "%s"` and its `.st5` reader splits each line on that exact
     literal, carrying no other separator. Our md5sum-style lines would have been rejected
     by TLH wholesale, and our reader took `[shntool]  name` as the filename, so we could
     not read a genuine `.st5` either. Both fixed; the reader still accepts untagged lines.
2. **SBE is a header-derived check.** It reads the declared frame count, so a truncated file
   whose header still claims a full length reports as aligned. `lh verify` catches that case;
   `lh sbe` alone does not. Decide whether `sbe` should cross-check the declared length
   against the actual data.
3. **The 24-bit WAV fixture uses legacy format tag 1**, not `WAVE_FORMAT_EXTENSIBLE`; real
   24-bit WAVs use the latter, and `flac` warns about ours. Extend the generator. Related
   find from the ST5 work: shntool 3.0.4 *refuses* `WAVE_FORMAT_EXTENSIBLE` outright
   ("unsupported format 0xfffe"), so it cannot ST5 a 24-bit FLAC at all — where we can, and
   our answer for `hires-24bit.flac` equals shntool's for `hires-24bit.wav`, which is what
   ST5 is supposed to mean. `reference.st5` therefore has no 24-bit entry to check us with.
4. **`.md5` is written md5sum-style** (`hash  name`) — *reading* is now confirmed, writing
   is not. TLH's `.md5` reader carries exactly two separator constants, `' '` and `'*'`, so
   it accepts md5sum's text (`hash  name`) and binary (`hash *name`) forms and nothing else;
   we already write one of those and read both. Which of the two TLH itself *writes* is
   still unestablished: it is GUI-only, has no create-MD5 code path with a separator
   constant to read off, and screen capture is blocked on this machine, so the GUI could not
   be driven to produce a sample. The remaining risk is cosmetic — either form parses
   everywhere — but it is not evidence, and it should be settled from a real TLH-written
   `.md5` when one is to hand.
5. **Encoding drops foreign RIFF chunks.** `flac` only preserves a WAV's `LIST`/`INFO`
   chunks under `--keep-foreign-metadata`, which we do not pass, so `wav → flac → wav` is
   byte-identical only for a canonical WAV. Taper metadata in a source WAV does not survive
   the trip. Decide whether to pass the flag before anyone converts an archive with it.

---

## 1. Design principles

These are the constraints every later decision is checked against.

1. **Never destroy data.** v0.1 modifies nothing in place. Outputs are new files written
   to a temp path and atomically renamed. Originals are never touched, never deleted.
   Users are handing us archives they cannot re-acquire.
2. **Provenance is a feature, not plumbing.** Every operation records which tool ran, its
   version, and the exact argv. That record is exportable. Encoding uses the *reference*
   `flac` binary so the FLAC vendor string reads `reference libFLAC x.y.z` — the string
   seeding standards and other traders actually inspect.
3. **Fast path in-process, trusted path via reference binaries.** Read-only analysis
   (probing, checksums, SBE, verification) is pure Rust: nobody cares what *read* a file,
   only that the answer is right. Anything that *produces* a file people will trade goes
   through the reference tool.
4. **The core is UI-agnostic and headless-complete.** Anything the GUI can do, the CLI can
   do. The CLI is both a real feature (scriptable batch work, which the original never had)
   and our correctness harness.
5. **Fail loudly and specifically.** `SHN requires shntool, which was not found` — never a
   generic error, never a silent skip.
6. **Cross-platform parity.** No platform-only features.

---

## 2. Scope

### v0.1

| Operation | Implementation |
|---|---|
| Scan folder (recursive) into a working set | in-process |
| File info (rate, bits, channels, duration, **encoder vendor string**) | in-process |
| Verify FLAC (decode + compare STREAMINFO MD5) | in-process, `flac -t` on demand |
| Create / check **FFP** | in-process (STREAMINFO MD5, header read only) |
| Create / check **MD5** (file bytes) | in-process |
| Create / check **ST5** (shntool audio-data MD5) | in-process |
| SBE test (sector boundary errors) | in-process |
| FLAC → WAV | in-process (lossless decode is deterministic and bit-identical) |
| WAV → FLAC | **reference `flac` binary** (provenance) |

### Explicitly deferred to v0.2+

Tag editing (via reference `metaflac`), SBE repair, SHN via `shntool`, APE / TTA / WavPack,
MP3 export, cue split & join, 24-bit→16-bit resample, batch rename.

Torrent verification — checking a local fileset against a `.torrent` — is planned separately
in [docs/torrent-verification.md](docs/torrent-verification.md). It is read-only and pure
Rust, so it could land earlier than the rest of this list, and T1–T3 already have.

Torrent *creation* — making a `.torrent` for a show, with the trackers traders use — is
planned in [docs/torrent-creation.md](docs/torrent-creation.md). It reuses the verification
walk and is the natural place to refuse to seed a show that does not verify.

SHN is legacy — nothing has been created in it in twenty years, and circulating material has
been reseeded as FLAC. Since v0.1 already shells out to reference tools, adding `shntool`
later is a registry entry, not an architectural change.

### Checksum semantics (the distinction users rely on)

- **FFP** — MD5 of the *unencoded* audio, stored in FLAC's STREAMINFO. Survives re-encoding
  and retagging. Read straight from the header; no decode needed.
- **MD5** — MD5 of the *file bytes*. Breaks on any re-encode or tag edit.
- **ST5** — shntool's MD5 of *audio data only*, excluding the WAV header. Survives format
  changes.

### SBE

A CD sector is 588 frames (44100 / 75). A file whose `total_samples % 588 != 0` has a
sector boundary error. Only meaningful for 44.1 kHz / 16-bit / stereo; report as
*not applicable* otherwise rather than as a pass.

---

## 3. Architecture

```
little-helper/            cargo workspace
├── lh-core/              domain logic — zero UI, zero CLI
│   ├── format/           FLAC (claxon, metaflac), WAV reader/writer
│   ├── checksum/         ffp, md5, st5 — compute, parse, write
│   ├── analysis/         sbe, verify, info
│   ├── convert/          flac→wav in-process, wav→flac via reference flac
│   ├── tools/            registry: discovery, version capture, argv, process runner
│   ├── torrent/          metainfo, verify, create, encode, tracker list
│   ├── job/              queue, worker pool, progress events, cancellation (docs/job-queue.md)
│   ├── report/           structured results + provenance/audit trail
│   └── config.rs         where config files live (LH_CONFIG_DIR, then the platform's)
├── lh-cli/               headless batch (clap)
└── lh-gui/               iced 0.14
```

### The tool registry

Promoted from implementation detail to headline feature, because it is the traceability story.

*Implemented in `lh-core/src/tools/`, surfaced as `lh tools`.*

- Discovers `flac`, later `metaflac` / `shntool`, in this order: user-configured path →
  bundled sidecar directory → `PATH` (via `which`). A configured path is the *whole* search:
  if the binary the user named is absent we fail rather than quietly running a different
  one, which would make the provenance record a lie. Until the config module exists, the
  user-configured path is `LH_FLAC` / `LH_METAFLAC` / `LH_SHNTOOL`.
- Captures each tool's `--version` at startup and displays it.
- Shows the SHA-256 of every bundled binary, and lets the user point at their own build.
- Logs tool + version + exact argv for every operation performed.

### Codec abstraction

Keeps SHN/APE/TTA addable later without rearchitecting. **Not built yet:** `convert`
dispatches on `AudioFormat` the way `format::probe` already does, because one implementor
is not an abstraction. The trait earns its place when SHN arrives and there are two.

```rust
pub trait Codec {
    fn id(&self) -> CodecId;
    fn probe(&self, path: &Path) -> Result<Option<StreamInfo>>;
    fn decode_to_wav(&self, src: &Path, dst: &Path, p: &Progress) -> Result<Provenance>;
    fn encode_from_wav(&self, src: &Path, dst: &Path,
                       opts: &EncodeOpts, p: &Progress) -> Result<Provenance>;
}
```

Every operation returns a `Provenance` describing what ran.

### Concurrency

A bounded worker pool plus `crossbeam-channel` progress events. Cancellation via a shared
token. No async runtime — the workload is CPU- and process-bound, not IO-concurrent.

`lh-core` emits a UI-agnostic `Event` stream. The GUI adapts it into an Iced `Subscription`;
the CLI renders a progress bar from the same events.

---

## 4. GUI

**Framework: Iced 0.14** (just released, so the API-churn risk that dogged Iced through the
0.14 development cycle is at its low point). Its subscription / `Task` model is a natural fit
for a job-queue application: a worker emits progress, the update function folds it into state.

**The working set is bounded by design.** A working set is a show — ten to thirty tracks —
or a handful of shows. It is not an archive. Iced has no built-in virtualized list, and it does
not need one at this size: a `scrollable` over a `Column` is fine for hundreds of rows.

Collection-wide work still exists, but it is not a table. A batch audit across many folders is a
*job* that emits a **report** — pass/fail per file, aggregate totals, the provenance log — not
tens of thousands of rows to scroll. Run it from `lh-cli` or from the job queue; read the result
as a report. That is the better product anyway: nobody audits a collection by eye.

If the set exceeds a soft cap, the table says so and offers the batch-audit path instead of
rendering. `lh-core` has no UI in it either way, so the framework stays swappable if Iced
disappoints for unrelated reasons.

### Layout

- **Path bar / Add folder**, with the whole window as a drag-and-drop target.
- **File table** — name, format, duration, rate/bits/channels, SBE, checksum status, result.
- **Operation panel** — pick operation, set options, run.
- **Job queue** — per-file and aggregate progress, cancel.
- **Log / audit pane** — the provenance trail, exportable.
- **Tools panel** — discovered binaries, versions, hashes, override paths.

---

## 5. Dependencies

| Concern | Crate |
|---|---|
| GUI | `iced` 0.14 |
| FLAC decode / STREAMINFO | `claxon`, `metaflac` |
| Hashing | `md-5` |
| File dialogs | `rfd` |
| Directory walk | `walkdir` |
| Tool discovery | `which` |
| CLI | `clap` |
| Config | `directories` (`serde` + `toml` when there are settings; the tracker list uses TLH's own format) |
| Errors | `thiserror` (core), `anyhow` (binaries) |
| Logging | `tracing`, `tracing-subscriber` (custom layer feeds the GUI log pane) |
| Concurrency | `crossbeam-channel`, `rayon` |
| Tests | `assert_cmd`, `insta` |

Deliberately **not** used: `flacenc` (pure-Rust encoder — produces a non-reference vendor
string, which is the exact provenance problem we are avoiding) and `flac-bound` (libFLAC FFI
— bit-identical output, but no externally verifiable record that reference `flac` ran).

---

## 6. Testing

Correctness is the product. A trader who loses an archive to us does not come back.

- **Fixture corpus** — tiny (~0.5 s) files committed to the repo: 44.1/16/2 with and without
  SBE; 48 kHz; 24-bit; mono; a truncated FLAC; a FLAC with a deliberately wrong STREAMINFO
  MD5; a WAV with extra `LIST`/`INFO` chunks; non-ASCII filenames; an untagged file.
- **Golden checksum files** generated by the *real* `flac`, `metaflac` and `shntool` and
  committed. Those tools are the oracle — matching shntool's exact ST5 output format and its
  quirks is a requirement, not an aspiration.
- **Round-trip property** — `wav → flac → wav` is byte-identical.
- **CLI snapshots** — `assert_cmd` + `insta` over every subcommand.
- **CI** on Linux, macOS and Windows.

---

## 7. Packaging & licensing

- GitHub Actions release matrix for the three platforms.
- **Bundle known-good `flac` (later `metaflac`, `shntool`) sidecars per platform**, with
  published SHA-256 hashes, overridable by the user.
- macOS needs notarization (Apple Developer Program, $99/yr) or users hit Gatekeeper; a
  Homebrew cask covers part of the audience.
- **Licensing:** our code is MIT OR Apache-2.0. The `flac`/`metaflac` *tools* are GPL-2
  (libFLAC itself is BSD). Shipping them alongside `lh` is mere aggregation and is fine, but
  we must offer their source — mirror the tarballs in our releases.

---

## 8. Milestones

| # | Milestone | Contents | Rough size |
|---|---|---|---|
| **M0** | Scaffold | Cargo workspace, CI matrix on all three platforms from day one, fixture-generation script. | 0.5 day |
| **M1** | `lh-core` | Format probing, checksums, SBE, verify, job queue, tool registry, fixture corpus, tests. No UI. | Largest single chunk |
| **M2** | `lh-cli` | `scan`, `info`, `verify`, `ffp`, `md5`, `st5`, `sbe`, `convert`. Snapshot tests. | Small on top of M1 |
| **M3** | `lh-gui` | Table, drag & drop, operation panel, job queue, log pane, Tools panel. | Medium |
| **M4** | Packaging | CI matrix, sidecar bundling, installers, notarization, README and docs. | Medium; front-load the CI |
| — | **v0.1** | | |
| **M5+** | Parity work | Tagging, SBE repair, SHN, APE/TTA/WavPack, MP3, cue split/join, resample. | Ongoing |

Build M1 and M2 before M3. The CLI proves the engine is right while the GUI is still an
opinion.

---

## 9. Risks

| Risk | Mitigation |
|---|---|
| Iced table performance | Working set is bounded by design (§4); collection-wide audits emit a report, not rows |
| shntool ST5 format quirks | Golden files generated by real shntool; match it exactly |
| macOS notarization cost and friction | Budget it, or ship via Homebrew and document the Gatekeeper step |
| GPL sidecar obligations | Mirror upstream source tarballs in every release |
| Scope creep toward full TLH parity | The v0.2+ list is written down and explicitly out of v0.1 |
| Windows path and Unicode handling | Non-ASCII filenames are in the fixture corpus; CI covers Windows. Torrent creation additionally asserts that a torrent names files exactly as the filesystem spells them, NFC or NFD, which is the case that differs on macOS |
| Users trusting a new tool with irreplaceable archives | Principle 1: nothing in place, nothing deleted, in v0.1 |

---

## 10. Open questions

1. ~~Soft cap on working-set size?~~ **Deferred.** No cap in v0.1. The working set is a show or
   a few shows; if table performance ever actually bites, measure it then and add a cap.
2. Binary name `lh` collides on `PATH` with an unrelated `ls` alternative published on
   crates.io as `lh` 1.0.0. Accept the collision, or ship as `lhelp`? (The crate names
   `little-helper`, `lh-core`, `lh-cli`, `lh-gui` are all free.)
3. Bundle sidecars in the installer, or download on first run? Bundling is better for
   offline use and trust; it costs installer size and per-platform build work.
4. Does verification default to the fast in-process path, or to `flac -t`, for users who
   want the reference tool in every audit line?
