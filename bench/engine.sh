#!/usr/bin/env bash
# Layer 1 — rfs-core engine micro-benchmark (in-memory device, no FUSE/syscalls).
# Builds the rfs-bench binary (release) and runs it. Output is the engine ceiling.
#
# Env:
#   SRC          repo root (default: script's repo)
#   TARGET_DIR   cargo target dir (default: $HOME/rfs-bench-target)
#   BENCH_ARGS   args to rfs-bench (default: full run)
set -euo pipefail

SRC="${SRC:-$(cd "$(dirname "$0")/.." && pwd)}"
TARGET_DIR="${TARGET_DIR:-$HOME/rfs-bench-target}"
BENCH_ARGS="${BENCH_ARGS:---mb 512 --rand 100000 --files 50000 --fsync 10000}"

export CARGO_TARGET_DIR="$TARGET_DIR"
echo "== building rfs-bench (release) =="
cargo build --release --features bench --bin rfs-bench \
  --manifest-path "$SRC/rfs-fuse/Cargo.toml" >/dev/null 2>&1
BIN="$TARGET_DIR/release/rfs-bench"

echo "== running: rfs-bench $BENCH_ARGS =="
"$BIN" $BENCH_ARGS
