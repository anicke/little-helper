#!/usr/bin/env python3
"""Generate `reference.st5` from real shntool, the way `gen-fixtures.py` does for metaflac.

`.st5` is defined by a tool, not by a spec: it is what `shntool hash -m` prints. That makes
shntool the only authority on both halves of the format — the digest (MD5 of the audio data
with the WAV header excluded) and the line layout, which carries a `  [shntool]  ` tag
between the two fields. Trader's Little Helper runs `shntool.exe hash -m -- "%s"` and its
`.st5` reader splits on that exact literal, so the tag is not cosmetic: a file without it is
one TLH cannot read.

shntool is not packaged for every platform we build on, and the Windows build bundled with
TLH runs fine under Wine, so the answer is committed rather than merely invoked — the same
move `reference.ffp` and `mktorrent-oracle.torrent` already make.

Requires `shntool` on PATH, or Wine plus the copy TLH bundles. Run from the repo root:

    python3 scripts/gen-st5-oracle.py
"""
import pathlib
import shutil
import subprocess
import sys

FIXTURES = pathlib.Path(__file__).resolve().parent.parent / "lh-core" / "tests" / "fixtures"
OUT = FIXTURES / "reference.st5"

# 24-bit is deliberately absent: shntool 3.0.4 refuses the WAVE_FORMAT_EXTENSIBLE stream
# `flac -d` produces from `hires-24bit.flac` ("unsupported format 0xfffe"), so it has no
# opinion to record. See PLAN.md's known limitations.
INPUTS = [
    "cdda-aligned.wav",
    "cdda-aligned.flac",
    "cdda-listchunk.wav",
    "cdda-sbe.wav",
    "cdda-sbe.flac",
    "mono-48k.wav",
]

TLH_CMDLINE = pathlib.Path.home() / ".wine/drive_c/Program Files (x86)/Trader's Little Helper/CmdlineApps"


def shntool_argv():
    """Native shntool if there is one, else TLH's bundled Windows build under Wine."""
    native = shutil.which("shntool")
    if native:
        return [native], None
    bundled = TLH_CMDLINE / "shntool.exe"
    if shutil.which("wine") and bundled.exists():
        # shntool shells out to flac.exe for FLAC input, and finds it in the same directory.
        return ["wine", str(bundled)], TLH_CMDLINE
    sys.exit("need shntool on PATH, or wine plus TLH's bundled CmdlineApps/shntool.exe")


def main():
    argv, helper_dir = shntool_argv()
    work = FIXTURES / ".st5-oracle-tmp"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    try:
        for name in INPUTS:
            shutil.copy(FIXTURES / name, work / name)
        if helper_dir:
            shutil.copy(helper_dir / "flac.exe", work / "flac.exe")

        proc = subprocess.run(
            argv + ["hash", "-m", "--"] + INPUTS,
            cwd=work,
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            sys.exit(f"shntool failed: {proc.stderr.strip()}")

        # shntool orders its output itself. Keep its order and its line layout; normalise
        # only the line ending, which is CRLF from the Windows build and LF from a native
        # one, and is the one part of the file that is not shntool's opinion about content.
        lines = [l.strip("\r") for l in proc.stdout.splitlines() if l.strip()]
        for line in lines:
            print(line)
        OUT.write_text("\n".join(lines) + "\n", newline="\n")
        print(f"\nwrote {OUT} ({len(lines)} entries)")
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
