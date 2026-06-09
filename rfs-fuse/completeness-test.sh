#!/usr/bin/env bash
# Exercises statfs (df), symlinks, and hard links on a live rfs-fuse mount.
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

echo "== df =="; df -h "$MNT" | sed -n 2p
echo "== symlink =="; echo "real content" > "$MNT/real.txt"
ln -s real.txt "$MNT/link.txt"
ls -l "$MNT/link.txt"
echo "readlink: $(readlink "$MNT/link.txt")"
echo "via link: $(cat "$MNT/link.txt")"
echo "== hard link =="; ln "$MNT/real.txt" "$MNT/hard.txt"
echo "real.txt links=$(stat -c %h "$MNT/real.txt")"
echo "hard.txt content: $(cat "$MNT/hard.txt")"
echo "appending via hard link..."; echo "more" >> "$MNT/hard.txt"; echo "real.txt now: $(cat "$MNT/real.txt")"
rm "$MNT/real.txt"; echo "after rm real.txt, hard.txt links=$(stat -c %h "$MNT/hard.txt") content=$(cat "$MNT/hard.txt")"
echo "== df after writes =="; df -h "$MNT" | sed -n 2p

fusermount3 -u "$MNT"
echo OK
