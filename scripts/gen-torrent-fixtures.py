#!/usr/bin/env python3
"""Generate synthetic .torrent fixtures, including deliberately broken ones.

The real external vector (a Debian ISO torrent) is committed alongside these and is what
keeps the suite honest — these fixtures are produced by our own encoder, so they cannot
be the only source of truth. See docs/torrent-verification.md section 10.

Run from the repo root:  python3 scripts/gen-torrent-fixtures.py
"""
import hashlib
import pathlib

OUT = pathlib.Path(__file__).resolve().parent.parent / "lh-core" / "tests" / "fixtures" / "torrents"


def enc(o):
    if isinstance(o, bool):
        raise TypeError("bencode has no boolean")
    if isinstance(o, int):
        return b"i%de" % o
    if isinstance(o, bytes):
        return b"%d:%s" % (len(o), o)
    if isinstance(o, str):
        return enc(o.encode())
    if isinstance(o, list):
        return b"l" + b"".join(enc(x) for x in o) + b"e"
    if isinstance(o, dict):
        # insertion order is preserved on purpose, so we can emit non-canonical files too
        return b"d" + b"".join(enc(k) + enc(v) for k, v in o.items()) + b"e"
    raise TypeError(type(o))


def pieces_for(total, piece_length):
    """Dummy digests, one per piece. T1 only parses; piece content is T3's problem."""
    count = -(-total // piece_length)
    return b"".join(hashlib.sha1(b"piece%d" % i).digest() for i in range(count))


def write(name, top):
    blob = enc(top)
    (OUT / name).write_bytes(blob)
    info = enc(top[b"info"]) if b"info" in top else b""
    ih = hashlib.sha1(info).hexdigest() if info else "-"
    print(f"  {name:32} {len(blob):5} bytes  infohash={ih}")


def multi_file_info(files, piece_length=128, name="show"):
    total = sum(f[b"length"] for f in files)
    return {
        b"files": files,
        b"name": name,
        b"piece length": piece_length,
        b"pieces": pieces_for(total, piece_length),
    }


def f(length, *path, attr=None):
    d = {b"length": length}
    if attr is not None:
        d[b"attr"] = attr
    d[b"path"] = list(path)
    # canonical order: attr, length, path
    return {k: d[k] for k in sorted(d)}


def main():
    OUT.mkdir(parents=True, exist_ok=True)

    # 500 bytes over 128-byte pieces: piece 0 straddles d1t01 and d1t02.
    write("multi-file.torrent", {
        b"announce": "http://tracker.example/announce",
        b"info": multi_file_info([f(100, "d1t01.flac"), f(250, "d1t02.flac"), f(150, "d1t03.flac")]),
    })

    write("nested-dirs.torrent", {
        b"announce": "http://tracker.example/announce",
        b"info": multi_file_info([f(200, "disc1", "d1t01.flac"), f(300, "disc2", "d2t01.flac")]),
    })

    # BEP 47 padding: the pad file is zero bytes that never exist on disk.
    write("padded.torrent", {
        b"info": multi_file_info([
            f(100, "d1t01.flac"),
            f(28, ".pad", "28", attr="p"),
            f(250, "d1t02.flac"),
        ]),
    })

    # --- files that must be refused ---

    write("unsorted-keys.torrent", {
        b"info": {  # name before files: not sorted, so not valid bencode
            b"name": "show",
            b"files": [f(100, "a.flac")],
            b"piece length": 128,
            b"pieces": pieces_for(100, 128),
        },
    })

    write("traversal.torrent", {
        b"info": multi_file_info([f(100, "..", "..", "etc", "passwd")]),
    })

    write("separator-in-path.torrent", {
        b"info": multi_file_info([f(100, "../../etc/passwd")]),
    })

    bad = multi_file_info([f(100, "a.flac")])
    bad[b"pieces"] = bad[b"pieces"][:-3]  # not a multiple of 20
    write("short-pieces.torrent", {b"info": bad})

    bad = multi_file_info([f(500, "a.flac")])
    bad[b"pieces"] = pieces_for(500, 128)[:20]  # one piece where four are needed
    write("piece-count-mismatch.torrent", {b"info": bad})

    bad = multi_file_info([f(100, "a.flac")])
    bad[b"piece length"] = 0
    write("zero-piece-length.torrent", {b"info": bad})

    both = multi_file_info([f(100, "a.flac")])
    both[b"length"] = 100
    write("length-and-files.torrent", {b"info": {k: both[k] for k in sorted(both)}})

    # A pure v2 torrent: no `pieces`, no `files`, just a file tree.
    write("v2-only.torrent", {
        b"info": {
            b"file tree": {b"a.flac": {b"": {b"length": 100, b"pieces root": b"\x00" * 32}}},
            b"meta version": 2,
            b"name": "show",
            b"piece length": 128,
        },
    })

    # --- torrents with real piece hashes, plus the payload they describe ---
    #
    # Everything above carries dummy digests because T1/T2 only parse. Verification needs
    # hashes actually computed over bytes, so these ship with the files themselves.
    payload = OUT / "payload"

    def content(tag, n):
        """Deterministic, non-repeating bytes, so corruption is actually detectable."""
        out = bytearray()
        i = 0
        while len(out) < n:
            out += hashlib.sha256(b"%s-%d" % (tag.encode(), i)).digest()
            i += 1
        return bytes(out[:n])

    def real_torrent(name, root, entries, piece_length):
        """entries: [(relative path parts, length or None for a pad file)]"""
        stream = b""
        files = []
        for parts, length, pad in entries:
            if pad:
                stream += b"\x00" * length
                files.append(f(length, *parts, attr="p"))
            else:
                data = content("/".join(parts), length)
                stream += data
                target = payload / root / pathlib.Path(*parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                files.append(f(length, *parts))
        pieces = b"".join(
            hashlib.sha1(stream[i:i + piece_length]).digest()
            for i in range(0, len(stream), piece_length)
        )
        info = {
            b"files": files,
            b"name": root,
            b"piece length": piece_length,
            b"pieces": pieces,
        }
        write(name, {b"info": info})

    # 500 bytes over 128-byte pieces. Piece 0 spans d1t01/d1t02 and piece 2 spans
    # d1t02/d1t03, which is what makes boundary attribution testable.
    real_torrent("verify-multi.torrent", "verified", [
        (["d1t01.flac"], 100, False),
        (["d1t02.flac"], 250, False),
        (["d1t03.flac"], 150, False),
    ], 128)

    # Padding aligns d1t02 to a piece boundary: 100 + 28 = 128.
    real_torrent("verify-padded.torrent", "padded", [
        (["d1t01.flac"], 100, False),
        ([".pad", "28"], 28, True),
        (["d1t02.flac"], 256, False),
    ], 128)

    print(f"\n  wrote to {OUT}")


if __name__ == "__main__":
    main()
