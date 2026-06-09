#!/usr/bin/env bash
# User: mount RFS via the FUSE adapter (fusermount3, no root) on an image on the
# same disk, and benchmark it (buffered I/O — FUSE doesn't honor O_DIRECT, so
# sequential-read numbers are page-cache-warm and labeled as such in the report).
# Emits "rfs-fuse key value" lines.
set -uo pipefail
BASE="${BASE:-/var/tmp/rfsbench}"; SRC="${SRC:-$BASE/src}"
IMG="$BASE/rfs.img"; MNT="$BASE/mnt-rfs"; RFS_BIN="$BASE/target/release/rfs-fuse"
cleanup() {
  fusermount3 -u "$MNT" 2>/dev/null || true
  pkill -f "$RFS_BIN $IMG" 2>/dev/null || true
  rm -f "$IMG"
}
trap cleanup EXIT
fusermount3 -u "$MNT" 2>/dev/null || true
rm -f "$IMG"; mkdir -p "$MNT"
RFS_IMAGE_MB="${RFS_MB:-12288}" nohup "$RFS_BIN" "$IMG" "$MNT" >"$BASE/rfs.log" 2>&1 &
for _ in $(seq 1 100); do grep -q "$MNT" /proc/mounts && break; sleep 0.1; done
if ! grep -q "$MNT" /proc/mounts; then echo "MOUNT FAILED:" >&2; cat "$BASE/rfs.log" >&2; exit 1; fi
export META_BIN="$BASE/metadata"
bash "$SRC/bench/fs-bench.sh" "$MNT" rfs-fuse buffered
fusermount3 -u "$MNT"
