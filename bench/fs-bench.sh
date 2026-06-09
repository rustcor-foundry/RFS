#!/usr/bin/env bash
# Run a fixed fio + metadata workload against a mounted directory and emit
# normalized "label key value" lines on stdout.
#
#   fs-bench.sh <dir> <label> <direct|buffered>
#
# Tunables (env): SIZE (4G), RT (30s random), FSRT (15s fsync), NFILES (20000),
#                 META_BIN (path to compiled metadata tool).
set -uo pipefail

DIR="$1"; LABEL="$2"; MODE="${3:-direct}"
SIZE="${SIZE:-4G}"; RT="${RT:-30}"; FSRT="${FSRT:-15}"; NFILES="${NFILES:-20000}"
META_BIN="${META_BIN:-./metadata}"

if [ "$MODE" = direct ]; then
  ENG="--ioengine=libaio --direct=1 --iodepth=16"
  WSYNC=""
else
  ENG="--ioengine=psync --direct=0 --iodepth=1"
  WSYNC="--end_fsync=1"
fi

emit() { echo "$LABEL $1 $2"; }

# Run one fio job (args after $1 section name); echo MB/s, IOPS, lat_us for the
# given direction ($1 = read|write) parsed from JSON.
fio_metric() {
  local dir="$1"; shift
  local js; js=$(fio --output-format=json --name=j "$@" 2>/dev/null)
  echo "$js" | jq -r --arg d "$dir" '
    .jobs[0][$d] as $s |
    "\($s.bw*1024/1000000) \($s.iops) \(($s.clat_ns.mean // 0)/1000)"'
}

mkdir -p "$DIR/meta"
rm -f "$DIR"/fio.* 2>/dev/null

# 1) sequential write
read -r mb iops lat < <(fio_metric write --rw=write --bs=1M --size="$SIZE" \
    --numjobs=1 $ENG $WSYNC --filename="$DIR/fio.seq")
emit seqwrite_MBs "$mb"

# 2) sequential read (same file)
read -r mb iops lat < <(fio_metric read --rw=read --bs=1M --size="$SIZE" \
    --numjobs=1 $ENG --filename="$DIR/fio.seq")
emit seqread_MBs "$mb"
rm -f "$DIR/fio.seq"

# 3) random 4K write (time-based)
read -r mb iops lat < <(fio_metric write --rw=randwrite --bs=4k --size="$SIZE" \
    --numjobs=1 $ENG $WSYNC --runtime="$RT" --time_based --filename="$DIR/fio.rand")
emit randwrite_iops "$iops"; emit randwrite_lat_us "$lat"

# 4) random 4K read (same region)
read -r mb iops lat < <(fio_metric read --rw=randread --bs=4k --size="$SIZE" \
    --numjobs=1 $ENG --runtime="$RT" --time_based --filename="$DIR/fio.rand")
emit randread_iops "$iops"; emit randread_lat_us "$lat"
rm -f "$DIR/fio.rand"

# 5) fsync-after-every-write (durable 4K commit latency), same for all targets
read -r mb iops lat < <(fio_metric write --rw=randwrite --bs=4k --size="$SIZE" \
    --numjobs=1 --ioengine=psync --direct=0 --iodepth=1 --fsync=1 \
    --runtime="$FSRT" --time_based --filename="$DIR/fio.fsync")
emit fsync_iops "$iops"; emit fsync_lat_us "$lat"
rm -f "$DIR/fio.fsync"

# 6) metadata (create / stat / unlink)
if [ -x "$META_BIN" ]; then
  while read -r k v; do
    case "$k" in meta_*) emit "$k" "$v";; esac
  done < <("$META_BIN" "$DIR/meta" "$NFILES")
fi
rm -rf "$DIR/meta"
