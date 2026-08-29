# Torrent verification

Load a `.torrent` file and check the local files against it — without a BitTorrent client.

Two workflows this serves, both common in trading:

* **"Do I already have this show, complete and intact?"** Point `lh` at a `.torrent` and a
  folder; get a per-file answer.
* **"My download finished but the client is gone."** Re-verify a folder years later against
  the `.torrent` that came with it, the same way a `.ffp` is re-checked.

It fits the existing principles without straining any of them: read-only (Principle 1),
pure Rust with no external tool (Principle 3), and the infohash is a provenance record in
exactly the sense Principle 2 means.

---

## 0. T0 result — bendy is adopted

*Spike run 2026-08-29. T1 landed the same day.*

**Both T0 questions are answered, and the strictness worry was overstated.**

`bendy`'s `DictDecoder::into_raw()` returns the exact byte slice of the `info` dictionary,
which is precisely the infohash requirement from §1. Verified against an independent
bencode implementation written for the spike: **six real torrents, six identical infohashes.**

| Torrent | Generator | Shape | bendy | Infohash matches reference |
|---|---|---|---|---|
| debian-13.6.0-amd64-netinst | `mktorrent 1.1` | single-file | accepted | yes |
| ubuntu-24.04.3-desktop-amd64 | (distro tooling) | single-file | accepted | yes |
| `mo1999-01-22` (LMA) | `ia_make_torrent` | 77 files | accepted | yes |
| `mo1999-01-28` (LMA) | `ia_make_torrent` | 50 files | accepted | yes |
| `mo2000-04-20` (LMA) | `ia_make_torrent` | 40 files | accepted | yes |
| `modereko2003-04-19.flac16` (LMA) | `ia_make_torrent` | 115 files | accepted | yes |

The four Live Music Archive torrents are the actual target corpus — real multi-file shows
from archive.org's etree collection.

**On strictness.** bendy does reject unsorted keys, confirmed both in its source
(`StructureError::UnsortedKeys`) and empirically against hand-built non-canonical input. But
the concern that old tools emit unsorted dictionaries did not survive contact with real
files: BEP 3 *requires* keys in sorted order, and every torrent tested is canonical. bendy
also correctly rejects duplicate keys and integers with leading zeros.

Its errors are specific enough to satisfy Principle 5 — `Keys were not sorted`,
`Malformed number of unexpected character: Expected 'e', got '5' at offset 18` — so a
refused torrent can be explained rather than merely denied.

**What this does not prove.** Six torrents from three generators, all BitTorrent v1, and
**none using BEP 47 padding files** — so the pad-file path in §3 remains untested against
real data. No sample from a 2005-era desktop client (µTorrent, Azureus) either. The residual
risk is low but not zero; if a legitimate torrent is ever refused, revisit then rather than
pre-building a lenient path nobody needs.

### T1 notes

Path components are validated at **parse time**, not at join time as §5 originally implied,
so an unsafe `TorrentFile` can never be constructed at all. Root resolution and the
containment check still belong to T2; Windows reserved device names are checked there, where
the join actually happens.

Two things the real torrents taught us:

* **Archive.org's boilerplate comment mentions a &ldquo;pad file directory&rdquo;** even when the torrent
  has none. Padding detection now accepts BEP 47's authoritative `attr` flag plus two older
  naming conventions (`.pad/…` and `_____padding_file…`), because treating padding as a
  missing file would report a good download as broken.
* **Multi-line comments** are normal — archive.org writes five lines — so `lh torrent info`
  indents continuation lines rather than breaking its own column alignment.

A pure v2 torrent is detected before the missing-key checks run, so it reports
&ldquo;this is a BitTorrent v2 torrent&rdquo; rather than the useless &ldquo;info dictionary has no pieces&rdquo;.

### T2 notes

Root resolution works from either side of the show folder, as §5 specified. Two decisions
the implementation forced:

* **Single-file torrents report no extras.** The containing directory is the user's own —
  listing everything else in their downloads folder would be noise, not information. Extras
  are only meaningful for multi-file torrents, where the folder belongs to the show.
* **`join_checked` is the second line of defence, not the first.** Components are already
  validated at parse time, so its real job is the platform rule that only matters once a
  path exists: Windows reserved device names, matched on the stem so `NUL.txt` is caught
  too. The containment check after joining should be unreachable; if it ever fires, parse-time
  validation has a hole.

`lh torrent check` without `--quick` fails with the specific message that piece verification
is milestone T3, rather than silently doing something less than asked.

### T3 notes

Piece streaming, pad-file zeros and attribution all landed as designed. The boundary rule
works: damage inside one file convicts only that file, damage on a shared piece convicts
neither neighbour.

One status had to be added that §6 did not anticipate. When a file is missing, the piece it
shares with its neighbour cannot be hashed at all — so the *innocent* neighbour has pieces
that were never checked. Calling it `Complete` would overstate what we know, and `Suspect`
would imply it might be corrupt. It reports **`Partial { verified, unverifiable }`** instead:
"every piece I could check passed, and N could not be checked because a neighbouring file is
bad." `Partial` counts toward the overall verdict being incomplete, and toward the
files-needing-attention total.

The size pre-check earns its place here: a wrong-sized file is never read, so its pieces are
marked unverifiable rather than being hashed into garbage that would convict its neighbours.

**Not yet done: an end-to-end run against a real third-party torrent and its payload.** The
verification fixtures carry real SHA-1 piece hashes, but over payload our own script
generated. No independent torrent creator (`mktorrent`, `transmission-create`) is installed
here, and archive.org's own torrents are unsuitable as a vector because they warn that the
files behind them change over time. Worth closing on a machine that has `mktorrent`.

---

## 1. What a `.torrent` actually is

A bencoded dictionary. The parts that matter:

```
{
  announce:      "http://tracker.example/announce",
  announce-list: [[...]],
  creation date: 1180000000,
  created by:    "mktorrent 1.1",
  info: {
    name:         "Grateful Dead 1977-05-08",
    piece length: 262144,
    pieces:       <20 * N raw bytes: one SHA-1 per piece>,
    files: [ { length: 41231234, path: ["d1t01.flac"] }, ... ]   # multi-file
    length: 41231234                                            # single-file, instead of `files`
  }
}
```

The **infohash** is the SHA-1 of the bencoded `info` dictionary. It is the torrent's
identity — what a magnet link carries and what a tracker keys on.

> **Compute the infohash from the original byte range of the `info` value, never by
> re-encoding what we parsed.** A torrent whose `info` dict is not in canonical form still
> has a valid infohash over its own bytes; re-encoding it would silently produce a different
> hash and we would tell the user their file is a different torrent than it is.

SHA-1 is used here because BitTorrent v1 specifies it, not because it was chosen. Say so in
a comment so nobody later "upgrades" it.

---

## 2. The hard part: pieces straddle files

This is the thing that makes torrent verification different from every other check in
Little Helper, and the source of every subtlety below.

All files are concatenated, in listed order, into one logical byte stream. That stream is cut
into fixed-size pieces of `piece length`, and `pieces` holds one SHA-1 per piece. **Piece
boundaries have nothing to do with file boundaries.**

```
files:   [-------- d1t01.flac --------][------ d1t02.flac ------][-- d1t03 --]
pieces:  [--p0--][--p1--][--p2--][--p3--][--p4--][--p5--][--p6--][--p7--][-p8-]
                                     ^ p3 covers the end of t01 and the start of t02
```

Consequences to design for, not discover later:

* **You cannot hash file by file.** Verification walks the concatenated stream.
* **Per-file status is derived, not measured.** A file is complete iff every piece
  overlapping it verifies.
* **A failing boundary piece is genuinely ambiguous.** If p3 fails and every other piece of
  both t01 and t02 passes, the corruption is in one of them and the data does not say which.
  Report both as *suspect* and say why. Do not pick one — a confident wrong answer sends
  someone re-downloading the wrong file.
* **A missing file poisons its neighbours' boundary pieces.** Mark those pieces
  *unverifiable* rather than *failed*, and do not read them at all.

---

## 3. Padding files (BEP 47)

Torrents made by modern libtorrent-based tools may include padding files that align real
files to piece boundaries. They carry `attr` containing `"p"`, and conventionally a path
like `[".pad", "31337"]`.

**Pad files do not exist on disk.** They are zero bytes contributed to the stream. If we
treat them as missing files we will report a perfectly good download as broken, and if we
skip them without contributing their bytes every subsequent piece hash is wrong. Handle
them in the stream reader, and never show them in the per-file report.

---

## 4. Locating the files

The torrent gives a root `name` and paths relative to it. Users point at either the folder
*containing* the show or the show folder *itself*, and both should work:

1. If `<given>/<name>` exists and is a directory, the root is `<given>/<name>`.
2. Otherwise if `<given>`'s own name equals `name`, the root is `<given>`.
3. Otherwise fall back to `<given>` and report unmatched files honestly.

Renamed or relocated files are out of scope for the first version. A later
`--relocate` could match by size and then by hash, but guessing is worse than reporting.

### Path safety — this is untrusted input

`.torrent` files come from strangers on the internet, and their path components are
attacker-controlled. This is the zip-slip class of bug. Before joining anything:

* Reject any component that is `..`, `.`, or empty.
* Reject absolute paths, drive-letter and UNC forms, and any component containing a path
  separator or a NUL byte.
* Reject Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`…`LPT9`).
* Validate *before* normalization, and confirm the joined path is still under the root
  afterwards.

Malformed structure gets the same treatment: `pieces` must be a multiple of 20 bytes and its
count must equal `ceil(total_length / piece_length)`; `piece length` must be non-zero and
sane. Never preallocate a `Vec` from an untrusted count.

---

## 5. Data model

Lives in `lh-core/src/torrent/`, alongside `format/` and `checksum/` rather than inside them —
it is a fourth kind of check, not a checksum format.

```rust
pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u32,
    pub pieces: Vec<[u8; 20]>,
    pub files: Vec<TorrentFile>,   // pad files included; flagged
    pub total_length: u64,
    pub announce: Vec<String>,
    pub created_by: Option<String>,
    pub creation_date: Option<i64>,
    pub comment: Option<String>,
}

pub struct TorrentFile {
    pub path: Vec<String>,   // validated components, root excluded
    pub length: u64,
    pub is_pad: bool,
}

pub enum FileStatus {
    Complete,
    Corrupt { bad_pieces: Vec<u32> },
    /// Only a shared boundary piece failed; the neighbour is equally likely to be at fault.
    Suspect { piece: u32, shared_with: Vec<usize> },
    Missing,
    WrongSize { expected: u64, actual: u64 },
    Unreadable { reason: String },
}

pub struct TorrentReport {
    pub metainfo_summary: /* name, infohash, counts */,
    pub root: PathBuf,
    pub files: Vec<(usize, FileStatus)>,
    pub pieces_total: u32,
    pub pieces_ok: u32,
    pub pieces_unverifiable: u32,
    /// Files present locally that the torrent does not list — info.txt, artwork, .ffp.
    /// Not an error; traders want to know.
    pub extra_local: Vec<PathBuf>,
}
```

---

## 6. Algorithm

1. **Parse** the metainfo; compute the infohash from the raw `info` byte range.
2. **Resolve** the root and validate every path.
3. **Size pre-check.** Stat every file. Any wrong size will fail hashing, so report it now
   and mark its pieces unverifiable. This alone answers "did the download finish?" instantly
   and is what `--quick` stops at.
4. **Stream and hash.** Walk the concatenated stream once with a piece-sized buffer, reading
   across file boundaries and substituting zeros for pad files. Emit a per-piece pass/fail.
5. **Attribute.** Map each failed piece back to the files it overlaps, applying the
   boundary-ambiguity rule from §2.
6. **Scan for extras.** Walk the root and list anything not in the torrent.

### Performance

One sequential pass, buffered reads, streaming SHA-1. A 700 MB show is a second or two of
hashing and is disk-bound long before it is CPU-bound.

**Do not parallelize this until a real folder is measured being too slow.** Pieces are
independent and `rayon` would be easy, but it means concurrent reads at scattered offsets,
which is worse on spinning disks — where the archives actually live. Sequential first.

Use ordinary buffered reads, not `mmap`: a file truncated by something else mid-verification
turns an mmap read into a SIGBUS that we cannot catch, and this tool runs over folders the
user may be touching.

---

## 7. CLI surface

```
lh torrent info  <file.torrent>
lh torrent check <file.torrent> [--path DIR] [--quick] [--json]
```

`info` alone is useful — name, infohash, tracker, creation date, piece length, file list —
and is the natural first milestone.

Exit codes follow the existing contract: `0` everything verified, `1` something is missing
or corrupt, `2` the command failed.

```
$ lh torrent check gd1977-05-08.torrent --path ~/shows
Grateful Dead 1977-05-08
  infohash 5a8e...c31f   16 files   1.1 GiB   256 KiB pieces
  created by mktorrent 1.1 on 2007-05-08

OK          d1t01.flac
OK          d1t02.flac
CORRUPT     d1t03.flac  (pieces 41-44)
SUSPECT     d1t04.flac  (piece 45 is shared with d1t03.flac)
MISSING     d1t05.flac
EXTRA       info.txt, gd1977-05-08.ffp  (not in the torrent)

4312 of 4320 pieces verified, 8 unverifiable
```

## 8. GUI

Later, once the job queue exists: drop a `.torrent` on the window, get the file table with a
status column and a piece-progress bar. The engine work above is what makes that a thin
layer, and none of it needs the GUI to exist first.

---

## 9. Dependencies

| Concern | Choice |
|---|---|
| Bencode | `bendy` 0.6 — written for BitTorrent, gives raw-slice access for the infohash |
| SHA-1 | `sha1` 0.11 |

Bump the existing `md-5` from 0.10 to **0.11** at the same time, so `sha1` and `md-5` share
one `digest` trait generation instead of pulling two into the tree.

Rejected: `lava_torrent` and `bip_metainfo` parse `.torrent` files wholesale, but this is a
data-integrity feature where we need exact control over the infohash byte range, pad-file
semantics and per-piece attribution — and `bip_metainfo` has not moved in years. `memmap2`
is rejected for the SIGBUS reason in §6.

**Settled by the T0 spike (§0):** `bendy` decodes every real torrent tested, including four
multi-file Live Music Archive shows, and `into_raw()` yields infohashes identical to an
independent implementation. No lenient fallback is being built; a hand-rolled parser for
hostile input would be a security surface we do not want to own, and nothing so far needs it.

---

## 10. Testing, and the oracle problem

`mktorrent`, `transmission-create` and `bencodepy` are all absent from the dev machine, so
fixtures have to be generated by a small bencoder in `scripts/`. **That is circular** — our
encoder would be validating our decoder — so it cannot be the only source of truth.

Break the circle with a **fixed external vector**: commit `debian-13.6.0-amd64-netinst.iso.torrent`
and assert our infohash equals `481b6e3617be4c88f96cb25e47c9d8272130071e`, the value two
independent implementations agreed on during T0. That tests bencode parsing and the infohash end to end against the
outside world, and needs none of the payload data.

Then, with generated fixtures:

* A multi-file torrent with a piece deliberately straddling a boundary; corrupt the second
  file and assert the first is reported `Suspect`, not `Corrupt`.
* A torrent with BEP 47 pad files, verifying clean with no pad files on disk.
* A truncated file (right name, wrong size) caught by the size pre-check without hashing.
* A missing file, asserting its neighbours' boundary pieces are `unverifiable` and not
  `failed`.
* Extra local files listed and not treated as errors.
* **Path traversal**: a torrent with `..` in a path is rejected, and nothing outside the root
  is ever opened.
* Single-file torrents, which take the `length` branch instead of `files`.

---

## 11. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**T0**~~ | ~~bendy spike~~ | **Done** — bendy adopted, infohash mechanism verified against six real torrents. See §0. |
| ~~**T1**~~ | ~~Parse + `lh torrent info`~~ | **Done** — `Metainfo`, infohash from raw bytes, path validation at parse time, `lh torrent info`. 14 tests. |
| ~~**T2**~~ | ~~Layout + `--quick`~~ | **Done** — root resolution, join safety, size pre-check, missing and extra files, `lh torrent check --quick`. 11 tests. |
| ~~**T3**~~ | ~~`lh torrent check`~~ | **Done** — piece streaming, pad-file zeros, per-file attribution, the boundary rule. 6 tests. |
| **T1** | Parse + `lh torrent info` | Metainfo model, infohash from raw bytes, external test vector. |
| **T2** | Layout + `--quick` | Root resolution, path validation, size pre-check, missing and extra files. |
| **T3** | `lh torrent check` | Piece streaming, pad files, per-file attribution, the boundary rule. |
| **T4** | GUI panel | Drop a torrent, file table with status, piece progress. Needs the job queue. |
| **T5** | BitTorrent v2 | See below. |

## 12. BitTorrent v2 (BEP 52)

Deliberately last. Effectively everything circulating in trading circles is v1, and v2 is a
different shape: SHA-256 merkle trees, a `file tree` instead of `files`, per-file `pieces
root` values, and hybrid torrents carrying both.

The payoff when it comes is real, though: **v2 gives per-file merkle roots, so file
verification is exact and the boundary ambiguity in §2 disappears entirely.** Keep
`FileStatus::Suspect` a v1-only outcome so that stays true when v2 lands.

## 13. Open questions

1. Should `check` fall back to matching by size and hash when a file has been renamed, or
   stay strict and only report? Strict is the safer default; relocation is a separate verb.
2. Does `--quick` (sizes only) deserve to be the default, with full hashing behind a flag?
   Sizes answer "did it finish" instantly; hashing answers "is it intact" in seconds.
3. Should a verified torrent be able to *emit* an `.ffp`/`.md5` for the same fileset, so a
   torrent-sourced show enters the normal checksum workflow in one step?
