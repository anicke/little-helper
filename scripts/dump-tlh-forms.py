#!/usr/bin/env python3
"""Dump Trader's Little Helper's own form definitions, so parity claims are checkable.

`PLAN.md` and every `docs/*.md` here ground parity questions in the original binary rather
than in memory of it (see `docs/torrent-creation.md`, which took its edge cases from TLH's
changelog). `docs/gui-shell.md` needed a different kind of answer — not "what does TLH
compute" but "how is TLH's window put together" — and that is answerable from the same
binary, because TLH is a Delphi/VCL program: every form it has is serialised into the
executable as a binary DFM resource, and a DFM records the whole component tree, including
each `TMenuItem`, each `TTabSheet`, and the `TAction` captions the menus display.

Two obstacles, both handled here:

1. `tralih.exe` is UPX-packed, so the DFM blocks are compressed. Unpack a copy first:

       upx-ucl -d -o /tmp/tralih.exe "$WINE/Trader's Little Helper/tralih.exe"

   `upx-ucl` is not a hard dependency of this repo; `apt-get download upx-ucl` plus
   `dpkg-deb -x` is enough, no root needed.

2. There is no PE resource walk here on purpose. Every DFM starts with the four bytes
   `TPF0`, and scanning for that literal finds all sixteen of TLH 2.8.4.185's forms
   directly, which is less code than a resource-directory parser and does not care how the
   linker laid the section out.

The DFM binary grammar implemented below is Delphi's `TReader`/`TWriter` pair:
an object is `[flags] ClassName ObjectName {property}* 0x00 {nested object}* 0x00`, where
names are Pascal short strings and each property value is tagged with a `TValueType` byte.

Usage, from the repo root:

    python3 scripts/dump-tlh-forms.py /tmp/tralih.exe            # list every form
    python3 scripts/dump-tlh-forms.py /tmp/tralih.exe frmTraLiH  # dump the main window
"""

import re
import struct
import sys


class Reader:
    def __init__(self, data, pos=0):
        self.d = data
        self.p = pos

    def u8(self):
        v = self.d[self.p]
        self.p += 1
        return v

    def take(self, n):
        v = self.d[self.p : self.p + n]
        self.p += n
        return v

    def short_string(self):
        return self.take(self.u8()).decode("latin-1")

    def u32(self):
        return struct.unpack("<I", self.take(4))[0]


# TValueType, in Delphi's own declaration order. Only the tags TLH actually emits are
# decoded into useful Python; the rest are consumed correctly and returned opaquely, which
# is all this script needs — it prints names, classes and captions, not pixel geometry.
def read_value(r):
    t = r.u8()
    if t == 0:  # vaNull
        return None
    if t == 1:  # vaList
        out = []
        while r.d[r.p] != 0:
            out.append(read_value(r))
        r.p += 1
        return out
    if t == 2:
        return struct.unpack("<b", r.take(1))[0]
    if t == 3:
        return struct.unpack("<h", r.take(2))[0]
    if t == 4:
        return struct.unpack("<i", r.take(4))[0]
    if t == 5:  # vaExtended, an 80-bit float
        return ("extended", r.take(10))
    if t == 6:  # vaString
        return r.short_string()
    if t == 7:  # vaIdent — an enum member, or the name of another component
        return ("ident", r.short_string())
    if t == 8:
        return False
    if t == 9:
        return True
    if t == 10:  # vaBinary
        return ("binary", r.take(r.u32()))
    if t == 11:  # vaSet
        out = []
        while True:
            n = r.u8()
            if n == 0:
                return set(out)
            out.append(r.take(n).decode("latin-1"))
    if t == 12:  # vaLString
        return r.take(r.u32()).decode("latin-1")
    if t == 13:
        return "nil"
    if t == 14:  # vaCollection
        out = []
        while r.d[r.p] != 0:
            if r.d[r.p] in (2, 3, 4):  # an optional item index
                read_value(r)
            assert r.u8() == 1, "collection item must open with vaList"
            item = {}
            while True:
                n = r.u8()
                if n == 0:
                    break
                # Bound before the value is read: in `d[k] = v` Python evaluates `v`
                # first, which would consume the value bytes before the name's.
                key = r.take(n).decode("latin-1")
                item[key] = read_value(r)
            out.append(item)
        r.p += 1
        return out
    if t == 15:
        return struct.unpack("<f", r.take(4))[0]
    if t in (16, 17):  # vaCurrency, vaDate
        return ("fixed64", r.take(8))
    if t == 18:  # vaWString
        return r.take(r.u32() * 2).decode("utf-16-le")
    if t == 19:
        return struct.unpack("<q", r.take(8))[0]
    if t == 20:  # vaUTF8String
        return r.take(r.u32()).decode("utf-8", "replace")
    if t == 21:
        return struct.unpack("<d", r.take(8))[0]
    raise ValueError(f"unknown TValueType {t} at offset {r.p}")


def read_object(r, depth, out):
    # ReadPrefix: a leading byte with the top nibble set carries filer flags, and
    # ffChildPos (0x02) means an integer position follows. Otherwise there is no prefix.
    if r.d[r.p] & 0xF0 == 0xF0:
        flags = r.u8() & 0x0F
        if flags & 0x02:
            read_value(r)
    class_name = r.short_string()
    name = r.short_string()
    props = {}
    while True:
        n = r.u8()
        if n == 0:
            break
        key = r.take(n).decode("latin-1")
        props[key] = read_value(r)
    out.append((depth, class_name, name, props))
    while r.d[r.p] != 0:
        read_object(r, depth + 1, out)
    r.p += 1


def forms(data):
    """Every DFM in the image, as (offset, class name, object name)."""
    found = []
    for m in re.finditer(b"TPF0", data):
        p = m.end()
        try:
            r = Reader(data, p)
            class_name = r.short_string()
            name = r.short_string()
        except IndexError:
            continue
        if class_name.startswith("T") and name:
            found.append((m.start(), class_name, name))
    return found


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    data = open(sys.argv[1], "rb").read()
    found = forms(data)
    if not found:
        sys.exit(
            "no DFM resources found — is this still UPX-packed? Run `upx-ucl -d` first."
        )
    if len(sys.argv) == 2:
        for offset, class_name, name in found:
            print(f"0x{offset:x}  {name}: {class_name}")
        return
    wanted = sys.argv[2]
    for offset, class_name, name in found:
        if name != wanted:
            continue
        r = Reader(data, offset + 4)
        tree = []
        try:
            read_object(r, 0, tree)
        except (IndexError, ValueError) as e:
            print(f"! stopped early: {e}", file=sys.stderr)
        for depth, cls, obj, props in tree:
            caption = props.get("Caption")
            action = props.get("Action")
            note = ""
            if isinstance(caption, str):
                note = f" = {caption!r}"
            elif isinstance(action, tuple):
                note = f" -> {action[1]}"
            print("  " * depth + f"{obj}: {cls}{note}")
        return
    sys.exit(f"no form named {wanted}")


if __name__ == "__main__":
    main()
