#!/usr/bin/env bash
# Exercises timestamps on a live rfs-fuse mount.
set -uo pipefail
HERE="$(dirname "$0")"
export CARGO_TARGET_DIR="$HOME/rfs-wsl-target"
BIN="$CARGO_TARGET_DIR/debug/rfs-fuse"
IMG=/tmp/rfs.img
MNT=/tmp/rfsmnt

(cd "$HERE" && cargo build) 2>&1 | grep -aE "error|Finished" || true
fusermount3 -u "$MNT" 2>/dev/null || true
pkill -9 -f "rfs-fuse $IMG" 2>/dev/null || true
sleep 0.5
mkdir -p "$MNT"
rm -f "$IMG"
nohup "$BIN" "$IMG" "$MNT" >/tmp/l.log 2>&1 &
for _ in $(seq 1 80); do grep -q "$MNT" /proc/mounts && break; sleep 0.1; done
grep -q "$MNT" /proc/mounts || { echo NOMOUNT; cat /tmp/l.log; exit 1; }

echo "current date: $(date)"
echo hi > "$MNT/f.txt"
echo "== ls -l (should show today, not 1969) =="; ls -l --time-style=long-iso "$MNT/f.txt"
echo "== stat mtime =="; stat -c "mtime=%y" "$MNT/f.txt"
echo "== touch -d to a fixed time =="; touch -d "2021-03-04 05:06:07" "$MNT/f.txt"
stat -c "after touch: mtime=%y" "$MNT/f.txt"
echo "== write updates mtime =="; sleep 1; echo more >> "$MNT/f.txt"; stat -c "after write: mtime=%y" "$MNT/f.txt"

fusermount3 -u "$MNT"
echo OK
