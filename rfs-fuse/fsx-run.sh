#!/usr/bin/env bash
# Run the fsx-style exerciser against a fresh rfs-fuse mount, then verify the
# file survives a remount byte-for-byte. Builds the FUSE binary itself into a
# persistent target dir (WSL's /tmp does not survive VM restarts).
set -euo pipefail
HERE="$(dirname "$0")"
export CARGO_TARGET_DIR="$HOME/rfs-wsl-target"
BIN="$CARGO_TARGET_DIR/debug/rfs-fuse"
IMG=/tmp/rfs.img
MNT=/tmp/rfsmnt
OPS="${1:-50000}"

(cd "$HERE" && cargo build) 2>&1 | grep -aE "error|Finished" || true
gcc -O2 -o "$HOME/fsxlite" "$HERE/fsxlite.c"

fusermount3 -u "$MNT" 2>/dev/null || true
pkill -9 -f "rfs-fuse $IMG" 2>/dev/null || true
sleep 0.5

mount_rfs() {
  nohup "$BIN" "$IMG" "$MNT" >/tmp/rfs-fuse.log 2>&1 &
  for _ in $(seq 1 80); do grep -q "$MNT" /proc/mounts && return 0; sleep 0.1; done
  echo "mount failed"; cat /tmp/rfs-fuse.log; exit 1
}

mkdir -p "$MNT"
rm -f "$IMG"
mount_rfs
for seed in 1 2 3; do
  "$HOME/fsxlite" "$MNT/fsxfile" "$OPS" "$seed"
done
before=$(sha256sum "$MNT/fsxfile" | cut -d" " -f1)

fusermount3 -u "$MNT"
sleep 1
mount_rfs
after=$(sha256sum "$MNT/fsxfile" | cut -d" " -f1)
if [ "$before" = "$after" ]; then echo "PERSIST OK ($after)"; else echo "PERSIST MISMATCH $before != $after"; exit 1; fi
fusermount3 -u "$MNT"
echo "done"
