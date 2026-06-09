#!/usr/bin/env bash
# Root: build an ext4 filesystem on a loopback file (same disk as the others)
# and benchmark it. Emits "ext4 key value" lines.
set -euo pipefail
BASE="${BASE:-/var/tmp/rfsbench}"; SRC="${SRC:-$BASE/src}"
IMG="$BASE/ext4.img"; MNT="$BASE/mnt-ext4"; SIZE_GB="${EXT4_GB:-12}"
LOOP=""
cleanup() {
  umount "$MNT" 2>/dev/null || true
  [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
  rm -f "$IMG"
}
trap cleanup EXIT
rm -f "$IMG"; truncate -s "${SIZE_GB}G" "$IMG"
LOOP=$(losetup --find --show "$IMG")
mkfs.ext4 -q -F "$LOOP"
mkdir -p "$MNT"; mount "$LOOP" "$MNT"
export META_BIN="$BASE/metadata"
bash "$SRC/bench/fs-bench.sh" "$MNT" ext4 direct
