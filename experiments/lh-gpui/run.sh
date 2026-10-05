#!/bin/sh
# Build and run the gpui-kit trial without the -dev system packages: fontconfig is
# dlopen'd, and the linker is pointed at symlinks to the runtime xcb/xkbcommon libs.
# Pass a show folder to open on it, or a .torrent to open Torrent check with it loaded.
set -e
cd "$(dirname "$0")"
: "${CARGO_TARGET_DIR:=${TMPDIR:-/tmp}/lh-gpui-target}"
libs="$CARGO_TARGET_DIR/linklibs"
mkdir -p "$libs"
for l in xcb xkbcommon xkbcommon-x11; do
    [ -e "$libs/lib$l.so" ] || ln -sf "$(ls /usr/lib/x86_64-linux-gnu/lib$l.so.* | head -1)" "$libs/lib$l.so"
done
export CARGO_TARGET_DIR RUST_FONTCONFIG_DLOPEN=on RUSTFLAGS="-L $libs"
exec cargo run -- "$@"
