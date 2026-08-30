#!/usr/bin/env python3
"""Generate the committed `mktorrent` oracle: a payload plus the torrent mktorrent makes of it.

Every other torrent fixture is produced by our own bencoder, which cannot validate our own
decoder. This one is produced by a tool nobody here wrote, and the test that uses it asserts
our infohash equals mktorrent's for the same folder — which pins the bencoding, the key
order, the *file* order, the piece length and every piece hash in a single comparison.

Committing mktorrent's answer, rather than only running mktorrent in CI, is what lets that
comparison run on macOS and Windows too. Neither has a convenient mktorrent package, and
those are the platforms where our directory walk is most likely to differ.

Requires `mktorrent` on PATH. Run from the repo root:

    python3 scripts/gen-mktorrent-oracle.py
"""
import hashlib
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "lh-core" / "tests" / "fixtures" / "torrents"
PAYLOAD = ROOT / "payload" / "oracle"
NAME = "gd1977-05-08"
TORRENT = ROOT / "mktorrent-oracle.torrent"

# mktorrent 1.1 accepts exponents 15..28 only, so 32 KiB is the smallest it can express.
# Our own floor is 16 KiB, which this oracle therefore cannot check — see the C2 notes in
# docs/torrent-creation.md.
PIECE_EXPONENT = 15
PIECE_LENGTH = 1 << PIECE_EXPONENT

# Chosen to exercise, in one torrent, everything the ordering and boundary rules turn on.
# Names are ASCII, contain no Windows-illegal characters, and no two differ only by case,
# so the payload checks out identically on every platform. Non-ASCII names are deliberately
# *not* here: what the filesystem does to them differs per platform, which would make this
# fixed infohash a platform-dependent assertion. They are tested separately.
FILES = [
    # Ends exactly on a piece boundary — the first bug the original TLH shipped.
    ("d1/t01.flac", PIECE_LENGTH),
    ("d1/t02.flac", 40_000),
    # `d1.txt` against the `d1/` directory is the pair that tells a joined-path sort apart
    # from a component-wise one: '.' (0x2E) sorts before '/' (0x2F).
    ("d1.txt", 100),
    ("a b", 100),
    ("a.txt", 100),
    ("Zed", 100),
    # Zero-byte files, mid-stream and last — the second bug TLH shipped.
    ("empty-middle.txt", 0),
    ("info.txt", 700),
    ("zzz-empty-last.txt", 0),
]


def content(rel, length):
    """Deterministic and non-repeating, so a file in the wrong stream position changes the
    hashes instead of hiding in identical bytes. Cycles all 256 values, so git sees binary."""
    seed = sum(rel.encode())
    return bytes((i * 31 + seed) % 256 for i in range(length))


def main():
    if not shutil.which("mktorrent"):
        sys.exit("mktorrent is not on PATH; it is the whole point of this fixture")

    show = PAYLOAD / NAME
    if show.exists():
        shutil.rmtree(show)
    for rel, length in FILES:
        path = show / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content(rel, length))

    TORRENT.unlink(missing_ok=True)
    subprocess.run(
        ["mktorrent", "-l", str(PIECE_EXPONENT), "-o", str(TORRENT), str(show)],
        check=True,
        capture_output=True,
    )

    blob = TORRENT.read_bytes()
    start = blob.index(b"4:info") + len(b"4:info")
    info_hash = hashlib.sha1(blob[start:-1]).hexdigest()

    total = sum(length for _, length in FILES)
    print(f"  payload   {show}")
    print(f"  {len(FILES)} files, {total} bytes, {-(-total // PIECE_LENGTH)} pieces of {PIECE_LENGTH}")
    print(f"  torrent   {TORRENT}")
    print(f"  infohash  {info_hash}")
    banner = subprocess.run(["mktorrent"], capture_output=True, text=True)
    made_by = next(
        (l for l in (banner.stderr + banner.stdout).splitlines() if l.startswith("mktorrent")),
        "mktorrent",
    )
    print(f"  made by   {made_by}")


if __name__ == "__main__":
    main()
