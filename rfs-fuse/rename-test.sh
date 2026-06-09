#!/usr/bin/env bash
# Exercises rename (mv) on a live rfs-fuse mount.
set -euo pipefail
BIN=/tmp/rfs-fuse/debug/rfs-fuse
IMG=/tmp/rfs.img
MNT=/tmp/rfsmnt

fusermount3 -u "$MNT" 2>/dev/null || true
pkill -f "rfs-fuse $IMG" 2>/dev/null || true
sleep 0.5
mkdir -p "$MNT"
rm -f "$IMG"
nohup "$BIN" "$IMG" "$MNT" >/tmp/rfs-fuse.log 2>&1 &
for _ in $(seq 1 50); do grep -q "$MNT" /proc/mounts && break; sleep 0.1; done
cd "$MNT"

mkdir -p src dst
echo "hello rename" > src/a.txt
echo "== mv within dir =="; mv src/a.txt src/b.txt; cat src/b.txt
echo "== mv across dirs =="; mv src/b.txt dst/c.txt; cat dst/c.txt; ls src
echo "== mv a directory tree =="; mkdir -p tree/deep; echo deepfile > tree/deep/x; mv tree dst/tree2; cat dst/tree2/deep/x
echo "== overwrite via mv (replace) =="; echo first > f1; echo second > f2; mv -f f2 f1; cat f1; ls f2 2>&1 || echo "f2 gone (good)"
echo "== final tree =="; ls -R "$MNT"

cd /
fusermount3 -u "$MNT"
echo "OK"
