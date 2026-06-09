#!/usr/bin/env bash
# Torture + persistence check for the rfs-fuse mount (run on Linux).
set -euo pipefail
BIN=/tmp/rfs-fuse/debug/rfs-fuse
IMG=/tmp/rfs.img
MNT=/tmp/rfsmnt

mount_rfs() {
  nohup "$BIN" "$IMG" "$MNT" >/tmp/rfs-fuse.log 2>&1 &
  for _ in $(seq 1 50); do grep -q "$MNT" /proc/mounts && return 0; sleep 0.1; done
  echo "mount failed"; cat /tmp/rfs-fuse.log; exit 1
}

mkdir -p "$MNT"
# Clear any stale mount/process from a previous run.
fusermount3 -u "$MNT" 2>/dev/null || true
pkill -f "rfs-fuse $IMG" 2>/dev/null || true
sleep 0.5
rm -f "$IMG"
mount_rfs
cd "$MNT"

for d in a b c; do mkdir -p "$d/x" "$d/y"; done
i=0
for dir in . a a/x a/y b b/x b/y c c/x c/y; do
  for _ in $(seq 1 12); do
    sz=$(( (RANDOM % 131072) + 1 ))
    head -c "$sz" /dev/urandom > "$dir/f$i"
    i=$((i + 1))
  done
done
echo "created $i files across $(find . -type d | wc -l) dirs"

find . -type f | sort | xargs sha256sum > /tmp/manifest.txt
echo "== verify LIVE =="
sha256sum -c --quiet /tmp/manifest.txt && echo "LIVE OK ($(wc -l < /tmp/manifest.txt) files)"

cd /
fusermount3 -u "$MNT"
sleep 1
mount_rfs
cd "$MNT"
echo "== verify AFTER REMOUNT =="
sha256sum -c --quiet /tmp/manifest.txt && echo "REMOUNT OK"
echo "files after remount: $(find . -type f | wc -l)"

cd /
fusermount3 -u "$MNT"
echo "done"
