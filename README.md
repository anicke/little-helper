# Lossless Little Helper

Verify, checksum and convert lossless audio for live-music trading: FLAC and WAV
verification, FFP/MD5/ST5 checksums, sector boundary error (SBE) detection and repair,
etree tagging and renaming, and torrent creation and checking.

A cross-platform, open-source successor to *Traders' Little Helper*. It is an independent
project, not affiliated with that program's author.

It ships as two programs:

* `lh` — the command line. Run `lh --help` for the full list of commands.
* `lh-tui` — the same operations in an interactive terminal UI.

## Features

Works on FLAC and WAV.

**Inspect and verify**
* `info` — what each file is: format, sample rate, bit depth, channels, length, the encoder
  that made it, and whether it has a sector boundary error.
* `verify` — decode every file, and check each FLAC against the audio MD5 it carries.
* `check` — check a folder against an existing `.ffp`, `.md5` or `.st5` file.

**Checksums**
* `ffp`, `md5`, `st5` — write or print FFP (FLAC audio MD5), MD5 (file bytes) and ST5
  (audio data only, shntool-compatible) checksum files.

**Sector boundary errors**
* `sbe` — report tracks whose lengths are not whole CD sectors.
* `sbe fix` — repair a whole show at once, shifting samples across each track boundary
  (backward, forward or nearest, like shntool), optionally padding the last track with
  silence. Writes to a new folder, or with `--in-place` sets the originals aside in
  `_original/`. On WAV the audio is never decoded or re-encoded.

**Convert**
* `convert` — FLAC to WAV, or WAV to FLAC with the official `flac` encoder, so the vendor
  string says `reference libFLAC`. Every output is checked against its source before it is
  kept; `--move-sources` then sets checked WAVs aside.

**Prepare a show for trading**
* `rename` — rename one show's files to the etree standard (`gd1977-05-08t01.flac`).
* `tag` — write etree Vorbis-comment tags for a show, including track numbers and totals.
  It prints the full change first and writes only with `--yes`; the audio is never
  touched, and that is checked.
* `setlist` — write the show's info `.txt`: header, setlist and track times, from the tags.
* `sample` — cut a short MP3 sample from a track that fits under a size limit (default
  1 MB).

**Torrents**
* `torrent create` — make a `.torrent` for a show, with a built-in list of trading
  trackers (`torrent trackers`) that you can extend.
* `torrent check` — check local files against a `.torrent`.
* `torrent info` — show a `.torrent`'s infohash, trackers, pieces and file list.

**Traceability**
* `tools` — show the external binaries found, with versions and SHA-256 hashes. Every file
  written through one of them can print its full provenance record (`--provenance`).

Your audio is never overwritten or deleted without being asked: new files go beside the
sources or into a folder you choose, each is checked before it is kept, and any file that
gets replaced is moved into `_original/` rather than removed.

## Installing

Download the archive for your platform from the
[latest release](https://github.com/anicke/little-helper/releases/latest), verify it (below),
and unpack it anywhere.

### `flac` is required for encoding

Checking and verifying files works on its own. Writing FLAC (`convert --to flac`, and
`sbe fix` on FLAC files) uses the official `flac` encoder, which you install yourself:

| Platform | Install |
|---|---|
| Debian / Ubuntu | `sudo apt install flac` |
| macOS | `brew install flac` |
| Windows | Download from [xiph.org](https://xiph.org/flac/download.html), and put `flac.exe` on your `PATH` |

MP3 sample clips (`sample`) need `lame`, or `ffmpeg` as a fallback, installed the same way.
Any tool can be pointed at directly with `LH_FLAC`, `LH_LAME` or `LH_FFMPEG`.

`lh tools` shows which binaries were found, with their versions and SHA-256 hashes.

## Verifying a download

Each release archive is built by GitHub Actions from the tagged source and carries a signed
build-provenance attestation. With the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify little-helper-<version>-<target>.tar.gz --repo anicke/little-helper
```

Or compare the archive's SHA-256 against `SHA256SUMS` from the same release:

```sh
sha256sum -c SHA256SUMS --ignore-missing          # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing      # macOS
Get-FileHash .\little-helper-<version>-<target>.zip  # Windows (PowerShell), then compare
```

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your
option.
