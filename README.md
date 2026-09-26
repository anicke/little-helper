# Lossless Little Helper

Verify, checksum and convert lossless audio for live-music trading: FLAC and WAV
verification, FFP/MD5/ST5 checksums, sector boundary error (SBE) detection and repair,
etree tagging and renaming, and torrent creation and checking.

A cross-platform, open-source successor to *Traders' Little Helper*. It is an independent
project, not affiliated with that program's author.

It ships as two programs:

* `lh` — the command line. Run `lh --help` for the full list of commands.
* `lh-tui` — the same operations in an interactive terminal UI.

## Installing

Download the archive for your platform from the
[latest release](https://github.com/anicke/little-helper/releases/latest), verify it (below),
and unpack it anywhere.

### `flac` is required for encoding

Checking and verifying files works on its own. Writing FLAC (`convert`, `sbe fix`) uses the
official `flac` encoder, which you install yourself:

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
