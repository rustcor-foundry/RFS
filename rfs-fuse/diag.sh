#!/usr/bin/env bash
set -uo pipefail
HERE="$(dirname "$0")"
export CARGO_TARGET_DIR="$HOME/rfs-wsl-target"   # persists across WSL restarts
BIN="$CARGO_TARGET_DIR/debug/rfs-fuse"
IMG=/tmp/rfs.img
MNT=/tmp/rfsmnt

(cd "$HERE" && cargo build) >/dev/null 2>&1 || { (cd "$HERE" && cargo build); exit 1; }
gcc -O2 -o "$HOME/fsxlite" "$HERE/fsxlite.c"

fusermount3 -u "$MNT" 2>/dev/null || true
pkill -9 -f "rfs-fuse $IMG" 2>/dev/null || true
sleep 0.5
mkdir -p "$MNT"
rm -f "$IMG"
nohup "$BIN" "$IMG" "$MNT" >/tmp/l.log 2>&1 &
for _ in $(seq 1 80); do grep -q "$MNT" /proc/mounts && break; sleep 0.1; done
grep -q "$MNT" /proc/mounts && echo "MOUNTED" || { echo "NOMOUNT"; cat /tmp/l.log; exit 1; }
"$HOME/fsxlite" "$MNT/f" "${1:-31}" "${2:-1}" v
echo "fsxlite rc=$?"
fusermount3 -u "$MNT"
echo "unmounted"
