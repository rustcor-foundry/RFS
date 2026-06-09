#!/usr/bin/env bash
# Root: create a dedicated single-file-vdev ZFS pool on the same disk as the
# other targets (NOT the production pools) and benchmark a dataset on it.
# ARC data-caching is disabled for THIS dataset only (primarycache=metadata) so
# the global ARC and the host's real pools are left untouched. Emits "zfs ...".
set -euo pipefail
BASE="${BASE:-/var/tmp/rfsbench}"; SRC="${SRC:-$BASE/src}"
VDEV="$BASE/zfs.img"; POOL="${ZPOOL:-rfsbench}"; SIZE_GB="${ZFS_GB:-16}"
cleanup() { zpool destroy "$POOL" 2>/dev/null || true; rm -f "$VDEV"; }
trap cleanup EXIT
zpool destroy "$POOL" 2>/dev/null || true
rm -f "$VDEV"; truncate -s "${SIZE_GB}G" "$VDEV"
zpool create -f -o ashift=12 "$POOL" "$VDEV"
zfs create -o primarycache=metadata -o compression=off -o recordsize=128k "$POOL/bench"
MNT=$(zfs get -H -o value mountpoint "$POOL/bench")
echo "# zfs $(zfs version | head -1) recordsize=128k primarycache=metadata compression=off" >&2
export META_BIN="$BASE/metadata"
bash "$SRC/bench/fs-bench.sh" "$MNT" zfs direct
