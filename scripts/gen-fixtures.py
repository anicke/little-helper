#!/usr/bin/env python3
"""Generate the test fixture corpus.

WAV files are written here; FLAC files are produced by the *reference* encoder, which is
what makes them a usable oracle. Requires `flac` and `metaflac` on PATH.

Run from the repo root:  python3 scripts/gen-fixtures.py
"""
import math
import pathlib
import struct
import subprocess
import sys

OUT = pathlib.Path(__file__).resolve().parent.parent / "lh-core" / "tests" / "fixtures"
SECTOR = 588


def pcm(frames, rate, channels, bits):
    """A quiet, deterministic tone. Low amplitude so FLAC compresses it well."""
    data = bytearray()
    peak = (1 << (bits - 1)) - 1
    amp = peak // 8
    for i in range(frames):
        for c in range(channels):
            v = int(amp * math.sin(2 * math.pi * (440 + 110 * c) * i / rate))
            data += v.to_bytes(bits // 8, "little", signed=True)
    return bytes(data)


def wav(path, frames, rate=44100, channels=2, bits=16, extra_chunk=False):
    audio = pcm(frames, rate, channels, bits)
    block = channels * bits // 8
    fmt = struct.pack("<HHIIHH", 1, channels, rate, rate * block, block, bits)
    chunks = b"fmt " + struct.pack("<I", len(fmt)) + fmt
    if extra_chunk:
        # A LIST/INFO chunk of the kind tagging tools leave behind; we must skip it.
        info = b"INFO" + b"ICMT" + struct.pack("<I", 12) + b"taper notes\x00"
        chunks += b"LIST" + struct.pack("<I", len(info)) + info
    chunks += b"data" + struct.pack("<I", len(audio)) + audio
    body = b"WAVE" + chunks
    path.write_bytes(b"RIFF" + struct.pack("<I", len(body)) + body)
    return path


def flac(src, dst):
    subprocess.run(
        ["flac", "--silent", "--force", "--no-padding", "-o", str(dst), str(src)],
        check=True,
    )
    return dst


def md5_of(path):
    out = subprocess.run(
        ["metaflac", "--show-md5sum", str(path)], check=True, capture_output=True, text=True
    )
    return out.stdout.strip()


def main():
    OUT.mkdir(parents=True, exist_ok=True)

    aligned = wav(OUT / "cdda-aligned.wav", SECTOR * 10)
    sbe = wav(OUT / "cdda-sbe.wav", SECTOR * 10 + 137)
    wav(OUT / "cdda-listchunk.wav", SECTOR * 10, extra_chunk=True)
    wav(OUT / "hires-24bit.wav", 4410, bits=24)
    wav(OUT / "mono-48k.wav", 4800, rate=48000, channels=1)

    # Non-ASCII name. Live recordings are traded with the taper's own spelling of the
    # venue and the band, so this is the normal case rather than an exotic one — and it
    # is the fixture PLAN.md section 9 names as the mitigation for path/Unicode risk.
    # Written NFC; see docs/torrent-creation.md on why we never normalize it ourselves.
    wav(OUT / "non-ascii-t\u00e4pe.wav", SECTOR * 5)

    flac(aligned, OUT / "cdda-aligned.flac")
    flac(sbe, OUT / "cdda-sbe.flac")
    flac(OUT / "hires-24bit.wav", OUT / "hires-24bit.flac")
    flac(OUT / "non-ascii-t\u00e4pe.wav", OUT / "non-ascii-t\u00e4pe.flac")

    # Decodes cleanly, but the STREAMINFO MD5 is a lie. STREAMINFO's 16-byte MD5 sits at
    # offset 26: "fLaC" (4) + metadata block header (4) + 34-byte STREAMINFO, MD5 last.
    good = (OUT / "cdda-aligned.flac").read_bytes()
    tampered = bytearray(good)
    tampered[26] ^= 0xFF
    (OUT / "wrong-md5.flac").write_bytes(bytes(tampered))

    # Cut the audio short: this must fail to decode, which is a different report line
    # from "decoded fine but the hash disagrees".
    (OUT / "truncated.flac").write_bytes(good[: len(good) - 2048])

    # Golden checksums, straight from the reference tool.
    lines = []
    for name in (
        "cdda-aligned.flac",
        "cdda-sbe.flac",
        "hires-24bit.flac",
        "non-ascii-t\u00e4pe.flac",
    ):
        lines.append(f"{name}:{md5_of(OUT / name)}")
    (OUT / "reference.ffp").write_text("\n".join(lines) + "\n")

    for p in sorted(OUT.iterdir()):
        print(f"  {p.name:24} {p.stat().st_size:>8} bytes")


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as e:
        sys.exit(f"reference tool failed: {e}")
