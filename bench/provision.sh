#!/usr/bin/env bash
# User: build the benchmark binaries from the extracted source tree.
# Expects the repo extracted at $BASE/src. Outputs binaries under $BASE.
set -euo pipefail
BASE="${BASE:-/var/tmp/rfsbench}"; SRC="$BASE/src"
export CARGO_TARGET_DIR="$BASE/target"
export PATH="$HOME/.cargo/bin:$PATH"

cd "$SRC"
echo "== cargo build (rfs-fuse + rfs-bench, release) =="
cargo build --release --features bench --bin rfs-fuse --bin rfs-bench \
  --manifest-path rfs-fuse/Cargo.toml 2>&1 | tail -3
echo "== gcc metadata =="
gcc -O2 -o "$BASE/metadata" "$SRC/bench/metadata.c"
ls -l "$BASE/target/release/rfs-fuse" "$BASE/target/release/rfs-bench" "$BASE/metadata"
echo "provisioned"
