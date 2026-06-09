# RFS benchmark harness

A three-layer baseline that separates the **engine** from the **FUSE adapter** and
compares against **ZFS** and **ext4** on the same disk. Latest numbers and the
full methodology/caveats are in [`RESULTS.md`](RESULTS.md).

## Layers

| Script | Layer | Privilege | What it measures |
|---|---|---|---|
| `engine.sh` → `rfs-bench` | rfs-core, in-RAM | user | algorithmic ceiling (no FUSE/syscalls) |
| `run-rfs-fuse.sh` | RFS via FUSE | user (fusermount3) | usable-today |
| `run-ext4.sh` | ext4 on loopback | root | kernel-fs floor |
| `run-zfs.sh` | OpenZFS dataset | root | reference target |

`fs-bench.sh` is the shared `fio` + metadata workload (seq R/W, random 4K R/W,
fsync-per-write, and create/stat/unlink via `metadata.c`). `provision.sh` builds
the binaries; `rfs-bench` lives in the `rfs-fuse` crate behind the `bench` feature
(`cargo build --release --features bench --bin rfs-bench`).

## Knobs (env)

`SIZE` (fio working set, 2G), `RT`/`FSRT` (random/fsync seconds), `NFILES`
(metadata count), `RFS_MB`/`EXT4_GB`/`ZFS_GB` (volume sizes), `BENCH_ARGS`
(engine sizes). `BASE` (work dir, default `/var/tmp/rfsbench`) and `SRC` (repo).

## Fairness notes

- All targets sit on one disk; ZFS uses a single file-vdev to match RFS's
  file-backed image (not a production ZFS layout).
- ZFS data caching is disabled on the bench dataset only (`primarycache=metadata`)
  so the host's global ARC and real pools are untouched; kernel targets use
  `O_DIRECT`. FUSE ignores `O_DIRECT`, so RFS seq-read is cache-warm.
- The `bench` feature and `rfs-bench` binary are off by default and never affect
  the shipped `rfs-fuse` binary or the `.deb`/`.rpm` packages.
