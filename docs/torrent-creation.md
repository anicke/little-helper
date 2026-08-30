# Torrent creation

Make a `.torrent` for a show, with the trackers traders actually use, without a BitTorrent
client.

This is [torrent-verification.md](torrent-verification.md) pointed the other way.
Verification answers *is this fileset what the torrent says it is*; creation answers *make a
torrent that says what this fileset is*. It is the same walk over the same concatenated
stream, hashing to **produce** piece hashes instead of to compare them, and it should be the
same code.

It sits inside the existing principles without straining any of them. The payload is only
ever read; the single thing written is a new `.torrent` (Principle 1). It is pure Rust with
no external tool (Principle 3). And the infohash is a provenance record in exactly the sense
Principle 2 means — it is the name the rest of the world will know this show by.

---

## 0. What the original did

*Established 2026-08-30 from the real thing: Trader's Little Helper 2.8.4.185
(`tralih284185.exe`, Inno Setup 5.5.7), unpacked with `innoextract`, plus the copy installed
under Wine.*

### Its tracker list, verbatim

TLH ships `tracker.lst` and installs it to `%APPDATA%\Trader's Little Helper\`. The installed
copy is byte-identical to the one inside the installer. The format is one entry per line,
`Display Name|announce URL`, CRLF-terminated. No comments, no dates, no version.

| Name | Announce URL | What it does (2026-08-30) |
|---|---|---|
| Crosstown Torrents | `http://crosstowntorrents.org:5555/announce` | did not answer |
| DIME | `http://bt.dimeadozen.org/announce.php` | announces, personal URL |
| etree.org | `http://tracker.etree.org:6969/announce` | announces, personal URL |
| Genesis-Movement Torrent | `http://torrent.genesis-movement.org/announce.php` | **announces** |
| JamToThis | `http://www.jamtothis.com:2710/announce` | did not answer |
| Lossless Legs | `http://www.shnflac.net/announce.php` | broken |
| Mind-Warp PaVilion | `http://www.mindwarppavilion.org/ezt/announce.php` | broken |
| The Traders' Den | `http://www.thetradersden.org/forums/tracker/announce.php` | **announces** |
| YEESHKUL! | `http://www.yeeshkul.com:2710/announce` | did not answer |
| Zappateers | `http://www.zappateers.com/bb/` | broken |
| ZOMB Torrents | `http://t1.the-zomb.com/announce.php` | broken, no A record |

> **The third column was a DNS check when this was first written**, and every host but one
> resolved. C3 replaced it with a GET of each announce URL, which is what the first draft
> called "the ceiling, not the floor", and the answer is much worse: **two** of eleven are
> usable as shipped. The evidence for each entry — the tracker's own failure reason where it
> spoke — is in `lh-core/src/torrent/trackers.rs` and is printed by `lh torrent trackers`.
> The lesson is the one the section already drew, only sharper: a cheap check that says
> "fine" is how a list rots without anyone noticing.

Three things this tells us, and all three are design input:

* **The list is still worth shipping — as a starting point with its rot exposed.** Two
  entries announce today, two more announce to a personal URL the site issues, three did
  not answer from where we looked, and four cannot work at all. That is not a list to hand
  someone silently; it is a list to hand someone *with the dates on it*.
* **The list rots, and shipping it without a date hides that.** `tracker.lst` is dated
  2018-08-03 inside a build released 2020-10-15 — already two years stale when it shipped,
  and eight years stale now. `t1.the-zomb.com` is gone. Mind-Warp's `announce.php` now
  redirects to the site's front page. And the Zappateers entry is
  `http://www.zappateers.com/bb/`, which is not an announce URL at all but a static page
  reading "Zappateers is currently off-line for a major overhaul", dated 2020-08-18 — six
  years of an entry nothing ever re-checked.
* **It is meant to be edited.** TLH's changelog moved the list out of the program directory
  into appdata "so edited tracker lists can be saved without requiring administrative
  rights", and the installer merges into an existing list rather than overwriting it. Ours
  must do the same, and should read TLH's own `Name|URL` format so people can bring theirs.

### What its torrent maker learned the hard way

Three fixed bugs from the changelog, which are our test cases before we write a line:

* *"When creating a torrent file and a file in the torrent 'ends' exactly on a piece boundary
  the hash of the next file in the torrent will be wrong."*
* *"in some cases the torrent file may be invalid if files with a size of zero bytes are part
  of the torrent"*
* *"the time stored is not UTC time but local time"*

And the shape the feature grew into over fourteen years, which we should not have to
rediscover: a private flag, UTF-8 strings, trackerless **and** multi-tracker (announce-list)
torrents with a tier editor, excluding `.db` and `.torrent` files from the file list, and a
default output location of the *parent* of the source folder — with a confirmation prompt if
you try to write the `.torrent` inside the folder it describes.

### C1 notes

*Landed 2026-08-30.*

The free oracle in §8 turned out to be better than expected. Every torrent in the fixture
corpus uses **only** the `info` keys we model — `length`/`files`, `name`, `piece length`,
`pieces`, plus `attr` on padding — so re-encoding is not limited to the debian vector. Six
torrents from three generators now go parse → `Draft` → encode → SHA-1 and come back with
the infohash they arrived with, and the debian one matches the absolute value two
independent implementations agreed on during T0.

Two things the implementation forced:

* **bendy cannot splice pre-encoded bytes into a dictionary it is building.** The `info`
  dictionary is bendy's throughout, but the outer dictionary is assembled in `RawDict`,
  because §1's rule requires `info` to go in as the exact bytes that were hashed and there
  is no `emit_raw`. `RawDict` still *sorts* its keys rather than trusting the author to
  write them in order, which is the property that mattered about `emit_and_sort_dict`.
* **`announce` is not simply the first entry of `announce-list`.** BEP 12 says it should
  also appear there and real torrents comply, so the parser only promotes it to a tier of
  its own when it is genuinely absent — otherwise a well-formed torrent grows a duplicate
  tracker every time it is read.

`Draft.creation_date` is documented as epoch seconds UTC at the field, because the type
cannot stop us repeating the bug TLH shipped.

### C2 notes

*Landed 2026-08-30.*

**The oracle earned its keep on the first run.** §2's ordering rule was wrong as written —
component-wise instead of joined-path — and nothing but `mktorrent` would have caught it,
because our torrents were perfectly self-consistent and verified against their own payload.
The corrected rule is in §2, and the fixture that discriminates the two (`d1.txt` beside a
`d1/` directory) is in the test on purpose; without it the test passes either way, which was
checked by deliberately reverting the sort.

Three things the implementation forced:

* **The corpus did not contain the non-ASCII names three places claimed it did.** PLAN.md
  §6 listed them, §9 cited them as the mitigation for the Windows/Unicode risk, and §8 here
  said "already in the corpus". None existed. There is one now, and the creation tests cover
  the normalization case in §2 that only macOS exhibits.
* **`mktorrent 1.1` cannot express our smallest piece length.** It accepts exponents 15–28,
  so 32 KiB to 256 MiB, while our floor is 16 KiB. The equality test therefore covers 32 KiB
  and up; the 16 KiB case — which `auto_piece_length` picks for anything under about 32 MB —
  is pinned only by our own round trip. That is a real gap in the oracle, not a rounding
  error, and it is the reason to keep the self-check in §5 step 8.
* **The span walk is now shared** (`torrent/stream.rs`). Verification's copy was entangled
  with its own per-file status machinery; what actually generalised was `Span`,
  `build_spans`, `spans_overlapping`, `SpanReader` and `feed_zeros`. Creation needs none of
  the status logic — every file is present by construction — so extracting the walk was the
  right size of sharing, and `build_spans` now takes lengths rather than a `Metainfo`.
* **`TempOutput` moved to `lh-core/src/output.rs`.** Conversion and creation both need
  Principle 1's stage-then-rename, and it was private to `convert`.

### C3 notes

*Landed 2026-08-30.*

**Checking the list properly is what C3 turned out to be about.** The plan said ship TLH's
eleven "corrected and dated"; the work of correcting them is what produced everything else
in this section. A GET of each announce URL — the check the first draft named and did not
do — costs one afternoon and changes the shipped list from "ten of eleven resolve" to "two
of eleven work". §0's table now records what each one actually does.

Four things the implementation forced:

* **`confirmed: &'static str` could not survive contact with the evidence.** A date says
  *when* we looked; it cannot say *what we saw*, and four of the five outcomes we found need
  to say that. So the field became a `Health` enum plus `checked` plus `evidence` — the
  tracker's own failure reason, verbatim, where it spoke one. `lh torrent trackers` prints
  all three, and `Health::usable()` is the single gate every refusal goes through:
  `PersonalUrl` and `Broken` are refused by id, `Announces`, `Unreachable` and `Unchecked`
  are used.
* **"Unreachable" had to be its own state, and it must not block.** Three hosts resolve but
  answered nothing on their tracker port. That is our network's evidence, not theirs. It was
  worth ruling out the obvious confound — `portquiz.net:6969` answers from here, and so does
  `tracker.etree.org:6969`, so high ports are not filtered on our side — but a firewall at
  *their* end is indistinguishable from death, and deciding a live tracker is dead on that
  basis would be exactly the overreach this list exists to avoid. It warns, in those words,
  and proceeds.
* **No bundled entry is marked private and none carries a `{passkey}`, deliberately.** Both
  mechanisms are built, and a user's own entry exercises both; neither is *asserted* for a
  site we ship, because we have no evidence for either. `private` and `source` live inside
  the info dictionary, so a guess there silently changes the infohash of every torrent made
  for that site — the worst possible place to be approximately right. TLH is no help: its
  changelog shows `Private torrent` as a manual checkbox added in 2011 and no passkey
  support at any point.
* **etree and DIME plainly need a personal announce URL, and we still do not know its
  shape.** They are the strongest evidence in the sweep — etree answers `Missing Key.`, DIME
  answers `not authorized; download a new copy of the .torrent file from the tracker` — and
  the temptation was to ship `…/{passkey}/announce` and let the mechanism do the rest.
  Probing for the shape got us rate-limited, which is the right outcome: a guessed template
  produces a confidently wrong URL, which is the failure mode of the entry we are replacing.
  They refuse by id, quoting the tracker, and point at `--tracker <URL>`.

Two smaller decisions worth recording:

* **`lh-core/src/config.rs` is a directory locator, not the config module PLAN.md §3
  describes.** There is no serde and no `toml` in it, because the only thing that needs
  configuring is the tracker list and it already has a format — TLH's. `LH_CONFIG_DIR`
  overrides the whole search, matching the `LH_FLAC` idiom the tool registry uses, and it is
  how the CLI tests get a config directory of their own without touching process
  environment from a parallel test.
* **A bundled entry a user list replaces stays visible, marked `(replaced by your own
  list)`.** Dropping it makes a mistyped `id=` look like a tracker that went missing.

Passkeys live in `passkeys.lst` beside the tracker list, as `id|passkey` — the same shape as
the tracker list, so there is one format in that directory rather than two.

### What we already have

* `Metainfo`, path validation at parse time, and the infohash-from-raw-bytes rule (T1).
* The span machinery in `torrent/verify.rs` — `build_spans`, `spans_overlapping`,
  `SpanReader` — which already walks files-concatenated-into-one-stream and already handles
  pad files and boundary spans. Creation needs that walk, not a second one.
* An external test vector: `debian-13.6.0-amd64-netinst.iso.torrent` with a known infohash.
* `bendy`. Only its decoder has been used so far. Its encoder is in the default build, and
  `emit_dict` **rejects unsorted keys** while `emit_unsorted_dict` sorts for us — which is
  exactly the canonical form BEP 3 requires, enforced by the library rather than by our care.

### Two gaps in the read side this will expose

* `Metainfo.announce` flattens BEP 12 tiers into one `Vec<String>`. Creation needs tiers, so
  the model has to grow them, and `lh torrent info` should show them as tiers too.
* The parser ignores `private` and `source`. Creation must write both, and `info` should
  report both — a trader looking at a torrent is entitled to know it is private.

---

## 1. One byte sequence, one hash

The verification doc's central rule, reversed. There: compute the infohash from the `info`
dictionary's original bytes, never by re-encoding what we parsed. Here:

> **Compute the infohash from the bytes we are about to write, never from a second encoding
> of the same data.**

So: encode the `info` dictionary once into a `Vec<u8>`, SHA-1 *that buffer*, and splice the
buffer verbatim into the outer dictionary. Never encode `info` twice — not once for the hash
and once for the file. Two encodings that differ by a byte produce a torrent whose advertised
identity is not its real identity, and nothing downstream would ever notice.

Canonical form is not optional: keys sorted by raw byte value, integers with no leading zeros
and no `-0`, no floats. Use `emit_unsorted_dict` and let bendy sort. Writing the keys in
sorted order by hand works right up until someone adds `source` between `pieces` and
`private` and the infohash silently changes for every torrent we make.

SHA-1 here for the same reason as on the read side: BitTorrent v1 specifies it. Same comment
in the code, so nobody "upgrades" it.

---

## 2. From a folder to a byte stream

### What goes in

Everything in the show folder. A show is the FLACs *plus* its `.ffp`, `.md5`, `info.txt` and
artwork — the extras that verification reports are not noise, they are the show.

Excluded by default, and **listed in the output** so the user sees what we dropped:

* `.torrent` files — a torrent of a torrent.
* OS droppings: `.DS_Store`, `Thumbs.db`, `desktop.ini`, `._*` AppleDouble files.
* Our own staging files (`.*.lh-*.part`), which should never exist at rest anyway.

TLH made `.db` exclusion a preference. Make it a default with `--include-all` to override:
nobody wants `Thumbs.db` in a seed, and the few who do can say so.

**Symlinks are refused, not followed.** Following one pulls data from outside the folder into
a torrent the user believes describes the folder. Name the link and stop.

**Names must be valid UTF-8**, because BEP 3 says paths are UTF-8. On Linux a filename is
bytes and need not be. Refuse and name the file rather than transcoding lossily — a mangled
name produces a torrent that will not verify against the folder it was made from, which is
the worst possible failure here.

**We never normalize a name.** The torrent says what the filesystem says, byte for byte.
This is the one place where the same show genuinely produces different torrents on different
machines: HFS+ normalized names to NFD and APFS compares them normalization-insensitively,
so a name written as NFC `é` may come back decomposed, and `café.flac` and `cafe\u{301}.flac`
are two files on Linux and one on macOS. Normalizing to "fix" that would be worse — the
torrent would stop describing the folder it was made from, which is the only property that
has to hold. `mktorrent` takes the bytes as they come too. The tests assert the consistency
(what is in the torrent equals what is on disk) rather than a fixed infohash, so they pass
on both, and the difference is documented rather than pretended away.

**Zero-byte files** are legal, appear in `files` with `length: 0`, and contribute nothing to
the stream. The walk has to cross a zero-length span without stumbling. TLH shipped a bug
here; we get a fixture instead.

**Empty directories cannot be expressed in v1 at all.** Say so and name them; do not silently
drop them.

### Order

The order of `files` *is* the byte stream. It must be deterministic, and it should agree with
what every other tool produces for the same folder, or our torrent will not deduplicate or
cross-seed against theirs. The rule: **byte-wise over the joined path**, `/` and all.

> **Corrected in C2.** This section first said "lexicographic over path *components*", which
> is the obvious reading and is wrong. The two rules differ whenever a directory name is a
> prefix of a sibling file's name: component-wise puts `d1/t01.flac` before `d1.txt`, because
> it compares `d1` against `d1.txt`; joined-path puts `d1.txt` first, because `.` (0x2E)
> sorts before `/` (0x2F). `mktorrent` does the latter. We follow it.

Writing the rule down is not what makes it right, which is exactly the point: the rule got
written down wrong and the mktorrent equality test in §8 is what caught it.

### Piece length

Auto by default: the smallest power of two between 16 KiB and 16 MiB that keeps the piece
count at or under 2000. A 400 MB FLAC16 show lands on 256 KiB; a 1.1 GB FLAC24 show on 1 MiB.

Both bounds earn their place. Every piece costs 20 bytes in `pieces` — the `.torrent`'s own
size is decided here — and one hash check. Every *failed* piece costs re-downloading that
whole piece. Small pieces make a fat torrent file; large pieces make retries expensive.

`--piece-length` overrides, and **refuses anything that is not a power of two in range**.
BEP 3 does not strictly require it, but clients in the wild reject it, and a torrent nobody
can load is worse than an argument error.

### The walk

One sequential pass over the concatenated stream with a piece-sized buffer, SHA-1 per piece —
the read-side walk with the comparison replaced by a `push`. Pad files contribute zeros
without being read, exactly as in verification.

Do not parallelize, for the reason already written down in the verification doc: scattered
concurrent reads are worse on the spinning disks where these archives actually live. Buffered
reads, not `mmap`, for the same SIGBUS reason.

---

## 3. Trackers

### The bundled list

Ship TLH's eleven, checked and **dated** — including the ones the check condemned:

```rust
pub struct Tracker {
    /// What `--tracker` accepts: "etree", "dime".
    pub id: String,
    pub name: String,
    /// May contain `{passkey}`.
    pub announce: String,
    /// Sets `private: 1` in the info dictionary.
    pub private: bool,
    /// Some private sites require an `info.source` to make the infohash theirs.
    pub source: Option<String>,
    /// What we found when we looked, and therefore whether we will write this URL.
    pub health: Health,
    /// ISO date we last checked. `None` for an entry we have never checked.
    pub checked: Option<String>,
    /// What we saw — the tracker's own words wherever it spoke.
    pub evidence: Option<String>,
    pub origin: Origin,
}
```

A date says *when* we looked. It cannot say *what we saw*, which is the part that decides
whether an entry can be used at all, so the status is an enum:

| `Health` | What it means | What `create` does |
|---|---|---|
| `Announces` | It answered as a tracker. | uses it |
| `PersonalUrl` | It answered, but only a URL the site issues will authorize. | **refuses the id** |
| `Broken` | This URL cannot work — not a tracker, or no DNS. | **refuses the id** |
| `Unreachable` | Nothing answered from where we checked. Not proof it is gone. | uses it, with a warning |
| `Unchecked` | A user's own entry. Used exactly as given. | uses it |

Every refusal quotes the evidence and the date, and every refusal names the same escape
hatch: `--tracker <URL>`, which is taken verbatim. We may be the ones who are wrong, and a
list that cannot be overridden is a list that is wrong forever.

`checked` is the whole point. `lh torrent trackers` prints it, so a user can see an entry
was last checked in 2018 and go and look rather than trusting us. A list with no date is how
TLH ended up recommending a dead tracker and a forum root for years without anyone noticing.

### The list is a default, not a fact

* A user list in the config directory extends and overrides the bundled one, and reads TLH's
  `Name|URL` format so people can bring the list they already curate. Anything past the URL
  is ours — `private`, `source=TAG`, `id=SLUG`, pipe-separated — and so is `#` for a
  comment; TLH's own files contain none of them, so they read unchanged.
* An entry a user list replaces stays in the listing, marked as replaced. Dropping it makes
  a mistyped `id=` look like a tracker that went missing.
* `--tracker` takes an id from the list **or** a URL. A URL is used verbatim.
* We never silently substitute or "correct" a URL. An unknown id is an error that prints the
  ids we do have (Principle 5).

### Passkeys

Private trackers key the announce URL to the individual user. An entry carries `{passkey}`
where one is needed, filled from `passkeys.lst` in the config directory — `id|passkey`, the
same shape as the tracker list, because one format in that directory beats two. **A torrent
with an unresolved `{passkey}` is never written.** An announce URL that cannot work is worse
than no tracker at all, because the user finds out only when nobody ever connects.

No *bundled* entry carries one. Two of them demonstrably need a personal URL and we do not
know its shape (see the C3 notes); shipping a guessed template would reproduce, with more
confidence, the exact failure this list is being rebuilt to fix.

### Tiers

`announce` (BEP 3) carries the first tracker; `announce-list` (BEP 12) carries the tiers.
One `--tracker` sets both, consistently. Repeated `--tracker` makes **one tier per tracker**,
in the order given: clients pick at random within a tier and fall through between tiers, so
putting unrelated sites in one tier means a coin flip decides which one hears about the seed.

Trackerless is allowed — no `announce` at all — which is what a DHT-only or a private-archive
torrent wants. TLH supports it and so should we.

### Private torrents

`private: 1` lives **inside** the `info` dictionary (BEP 27), so it changes the infohash. It
is not a setting that can be flipped afterwards; flipping it makes a different torrent. It
has to be decided before hashing, and the CLI help should say so in those words.

Choosing a tracker whose entry is `private: true` sets it. `--private` sets it. Passing
several private trackers from different sites earns a **warning, not a block** — cross-posting
a private torrent breaks most sites' rules and gets accounts banned, but it is the user's
account and they may know something we do not.

`source` is per-tracker for the same reason: only the site knows what string it wants.

---

## 4. Data model

Lives in `lh-core/src/torrent/create.rs`, beside `verify.rs`, sharing the span walk.

```rust
pub struct CreateOpts<'a> {
    pub trackers: Vec<Vec<String>>,   // tiers, already resolved to real URLs
    pub piece_length: Option<u32>,    // None = auto
    pub private: bool,
    pub source: Option<String>,
    pub comment: Option<String>,
    pub include_all: bool,
    pub created_by: &'a str,          // "Little Helper 0.1.0"
}

/// What was made, and what was left out of it.
pub struct Created {
    pub path: PathBuf,
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u32,
    pub pieces: usize,
    pub files: Vec<TorrentFile>,
    pub total_length: u64,
    /// Skipped, with the reason. Never dropped silently.
    pub excluded: Vec<(PathBuf, &'static str)>,
}
```

---

## 5. Algorithm

1. **Collect.** Walk the folder, apply exclusions, refuse symlinks and non-UTF-8 names,
   record what was excluded and why.
2. **Order.** Sort by path components, byte-wise.
3. **Choose the piece length**, or validate the one given.
4. **Pre-flight** (§7) unless waived.
5. **Walk and hash.** One sequential pass, SHA-1 per piece, reusing the span machinery.
6. **Encode `info` once** into a buffer; SHA-1 it for the infohash.
7. **Encode the outer dictionary**, splicing that buffer in verbatim. `creation date` is
   epoch seconds **UTC** — TLH's bug, not ours.
8. **Self-check.** Re-parse the bytes with our own `Metainfo::from_bytes` and assert the
   infohash, the file list and the piece hashes are what we intended. Cheap, needs no second
   pass over the payload, and catches an encoder that scrambled `pieces`.
9. **Write** to a temp path beside the destination and rename into place, refusing an existing
   output unless asked — the same rules `convert` follows.

---

## 6. CLI surface

```
lh torrent create <DIR|FILE> [-o FILE] [--tracker ID|URL]... [--piece-length N]
                             [--private] [--source TAG] [--comment TEXT]
                             [--include-all] [--no-check] [--write-ffp]
lh torrent trackers
```

Default output is `<parent of source>/<name>.torrent` — TLH's rule, and it is the right one.
Writing the `.torrent` *inside* the folder it describes adds a file to that folder, so
re-creating it later yields a different infohash. Allowed with an explicit `-o`, with a
warning saying exactly that.

```
$ lh torrent create ~/shows/gd1977-05-08 --tracker tradersden
gd1977-05-08
  16 files   1.1 GiB   2148 pieces of 512 KiB
  excluded   Thumbs.db (not part of the recording)
  verified   16 of 16 files decode and match their FFP

  infohash   5a8e...c31f
  tracker    The Traders' Den  http://www.thetradersden.org/forums/tracker/announce.php  (announces — checked 2026-08-30)
  wrote      /home/nicke/shows/gd1977-05-08.torrent
```

An entry we checked and found unusable never becomes an announce URL:

```
$ lh torrent create ~/shows/gd1977-05-08 --tracker zomb
lh: tracker zomb: http://t1.the-zomb.com/announce.php cannot work. Checked 2026-08-30: no
    DNS A record. If you know a URL that does, pass it with --tracker <URL>.
```

Exit codes follow the contract: `0` written, `1` the fileset has something wrong with it
(§7), `2` the command failed.

---

## 7. Pre-flight — the reason this is not a shell script around mktorrent

> **Postponed 2026-08-30.** This section describes a feature the original never had, and
> that is the problem with it. TLH's changelog introduces torrent creation in 1.1.0.89
> (2006-09-15) as "select a tracker from a default tracker list, add a comment, set the
> piece size and exclude .db files", and in fourteen years of releases nothing was ever
> added to check a show before making a torrent of it — the `Verify` and `Create torrent`
> pages stayed separate for the life of the program. So the claim below that this is "the
> feature" is an argument, not an observation, and no user has yet asked for it. C4 waits
> until someone does, or until the GUI makes the joined-up flow obvious. Nothing else in
> this document depends on it: `create` writes torrents today, and adding a check before
> the write is additive whenever it happens.

`mktorrent` hashes bytes. We already know what the bytes *are*.

Before writing, by default:

* every FLAC decodes and matches its STREAMINFO MD5 (`lh verify`);
* if the folder carries an `.ffp`, `.md5` or `.st5`, it matches;
* SBEs are reported — not an error, but a taper wants to know before a hundred people have
  the file.

A show that fails gets a torrent only when asked (`--no-check`). The default is to stop,
because the single thing this community most wants prevented is a broken show being seeded.
That is the feature, and it is why creation belongs in Little Helper.

`--write-ffp` closes the loop from the other side (open question 3 of the verification doc):
emit the `.ffp` for the fileset so a torrent and its fingerprint file come out of one read.
Note the ordering trap — **writing the `.ffp` changes the folder, so it must happen before
the file list is built**, or the torrent will not describe what is on disk.

---

## 8. Testing — and this time there is a real oracle

The verification doc had to work around having no independent bencoder on the machine.
Creation does not have that problem.

**`mktorrent` is packaged for Ubuntu** (`mktorrent 1.1`, universe), so CI can install it
exactly the way it now installs `flac`. That buys the strongest test available:

> Create a torrent for a fixture folder with our code and with `mktorrent`, and assert the
> **infohashes are equal**.

One assertion pins the bencoding, the key order, the file order, the piece length and every
piece hash at once. If it holds, our torrent is the torrent everyone else's tool would have
made — which is precisely what cross-seeding and deduplication depend on. It also closes the
loose end the verification doc left open ("not yet done: an end-to-end run against a real
third-party torrent and its payload"), because a mktorrent-made torrent over our fixture
payload, verified by `lh torrent check`, *is* that run.

Free and worth doing on day one of C1, before any of that: **re-encode the `info` dictionary
of the committed `debian-13.6.0-amd64-netinst.iso.torrent`** from what our parser read and
assert the infohash is still `481b6e3617be4c88f96cb25e47c9d8272130071e`. It needs no payload
and no external tool, and it tests our encoder against a real canonical torrent from outside.

> **Done in C1, and it generalised.** Every fixture uses only the `info` keys we model, so
> six torrents from three generators re-encode to their own infohashes, not just the debian
> one.

### Commit the oracle's answer, not only the tool

`mktorrent` is packaged for Linux and not for Windows, and running it on macOS only re-checks
arithmetic Linux already checked — the encoder is pure computation, and mktorrent's output is
deterministic for the same input bytes. What actually varies across platforms is the half
*before* the encoder: the directory walk, name encoding, case sensitivity, ordering. Skipping
the comparison wherever the tool is missing puts the thinnest coverage on the platform with
the highest risk.

So the oracle is committed rather than merely invoked: `scripts/gen-mktorrent-oracle.py`
writes a payload and the torrent mktorrent makes of it, both go into the fixture corpus, and
every platform asserts our infohash equals that stored value. The live mktorrent comparison
stays on Linux, where it catches the tool changing under us; the committed one is what covers
macOS and Windows. It is the same move `reference.ffp` already makes for `metaflac`.

The payload is deliberately all-ASCII with no two names differing only by case, because a
fixed infohash has to be a platform-independent assertion. Non-ASCII names are tested
separately, for the property that survives normalization (§2). `.gitattributes` marks the
whole fixture tree `-text` so Git cannot rewrite a line ending inside a file whose bytes are
being hashed.

Then the fixtures — three of which are the bugs TLH shipped:

* a file ending **exactly** on a piece boundary;
* a **zero-byte** file, once in the middle and once at the end;
* `creation date` is UTC, not local;
* a single-file torrent (the `length` branch);
* nested directories;
* non-ASCII names, and an NFC/NFD pair, asserted against what the filesystem actually stored
  rather than against a fixed hash;
* a name that is not valid UTF-8 → refused, not mangled (Unix only);
* a symlink → refused, and nothing outside the folder is read;
* `--piece-length` not a power of two → refused;
* the same folder twice → the **same infohash**, because `creation date` lives outside `info`;
* create → `lh torrent check` against the source folder → complete.

Windows has no `mktorrent` package, so the equality test skips there and says so on stderr,
exactly as the conversion tests do.

---

## 9. GUI

The same panel as verification, with the direction reversed: point at a folder, pick trackers
from the list, see the pre-flight result before the Create button is live. Needs the job queue
for piece progress. None of §1–§8 needs the GUI to exist first.

---

## 10. Milestones

| # | Milestone | Contents |
|---|---|---|
| ~~**C1**~~ | ~~Bencode out~~ | **Done** — canonical encoder, `info` encoded once and hashed from that buffer, the debian vector re-encoded to its published infohash. Tiered `announce`, `private` and `source` in `Metainfo` and `lh torrent info`. 9 tests. See §0. |
| ~~**C2**~~ | ~~Folder → torrent~~ | **Done** — collection, exclusions, ordering, piece-length choice, the shared span walk, self-check, atomic write. `lh torrent create` with `--tracker URL`. mktorrent equality live on Linux and committed for every platform. 16 tests. See §0. |
| ~~**C3**~~ | ~~The tracker list~~ | **Done** — TLH's eleven, re-checked by announce GET rather than DNS, each with what we saw and when. `Health` gates what may be written; user list in TLH's format from the config directory, `--tracker` by id or URL, passkeys, one tier per tracker, `--private`, `--source`, `lh torrent trackers`. 20 tests. See §0. |
| **C4** | Pre-flight | **Postponed 2026-08-30** — a feature TLH never had and nobody has asked for. Verify/FFP/SBE checks before writing, `--no-check`, `--write-ffp`. See §7. |
| ~~**C5**~~ | ~~GUI panel~~ | **Done** — folder (from the working set already scanned) → trackers (comma-separated ids/URLs, resolved the same way `lh torrent create` does) → create, with real piece progress through `lh-gui`'s job queue. Pre-flight is still C4, still postponed. See `docs/gui.md` §G4 notes. |
| — | later | v2 and hybrid creation, alongside T5. Creating hybrid torrents is worth more than parsing them — a hybrid seeds to both swarms — but it is still last. |

---

## 11. Open questions

1. ~~**Refuse or warn when the show fails pre-flight?**~~ **Moot while C4 is postponed.**
   Refusing is the safer default and the whole point of §7, but a taper re-seeding a
   known-imperfect historical show will hit it, and `--no-check` may not be enough of an
   escape hatch for that case. Worth answering before any of §7 is built, not now.
2. ~~**Should choosing a private tracker set `private: 1` automatically?**~~ **Settled: yes.**
   A tracker entry marked `private` sets the flag. Getting it wrong wastes an upload, and the
   user's intent when they name a private site is not ambiguous. Because it is an invisible
   change to the infohash driven by a table we ship, `create` must *say* it set the flag, and
   `lh torrent trackers` must show which entries carry it.
3. **How is the bundled list maintained?** A date on each entry is honest but static, and
   C3 showed how fast the answer moves: DNS said ten of eleven were fine, an announce GET
   said two. Re-checking is now a concrete, scriptable job — GET each announce URL, keep the
   bencoded failure reason — so the question narrows to *who runs it and when*: at release
   time, by community PR, or fetched at runtime. If we fetch, what happens offline is still
   the case the bundling was for. The one thing C3 settles is that whatever we do, the date
   and the evidence ship next to the URL, so a stale list is visibly stale.
4. **Is infohash equality with `mktorrent` a requirement or a test?** It is a test, in CI
   since C2, and it has already paid for itself by catching the file-ordering rule (§2). Open
   part: mktorrent cannot make 16 KiB pieces, so it cannot check our smallest piece length at
   all — and if a future mktorrent changed its ordering, would we follow it or hold our rule
   and drop the assertion? Following it silently changes the infohash of every torrent we
   make, which is not a thing to do quietly.
5. **Padding files (BEP 47): never, or opt-in?** They help modern clients dedupe; the trading
   community's tooling is old and some of it may not cope. Default off is obvious; the
   question is whether opt-in is worth building at all.
6. **Should `create` know about multi-disc shows?** A show is often `d1/` and `d2/`, and the
   torrent name is the parent. Nothing special is needed — but the *default* torrent name is
   the folder name, and folder names in the wild are not always the name a tracker wants.
