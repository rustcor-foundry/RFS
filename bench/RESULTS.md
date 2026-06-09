# RFS baseline benchmarks — RFS vs ZFS (and ext4 floor)

First baseline pass, run 2026-06-09 on **lx-ws01** (Debian 13, kernel 6.12,
80 cores, 503 GiB RAM, all targets on the same disk `/dev/sdo2`). The goal is a
rough "where are we" picture, not a tuned competitive result.

## Three layers measured

1. **RFS engine** — `rfs-core` driven in-process over a RAM-backed block device
   (`rfs-bench`). No FUSE, no syscalls, no real disk. This is the *algorithmic
   ceiling* and the number most relevant to the eventual in-kernel / Feox target.
2. **RFS via FUSE** — the `rfs-fuse` adapter (userspace, single-threaded
   poll-once executor) on an image file, `fio` workload. The "usable today" figure.
3. **Reference** — OpenZFS 2.3.2 and ext4, same `fio` workload, same disk.

## Results

| Workload | RFS engine (RAM ceiling) | RFS via FUSE (today) | ZFS 2.3.2 | ext4 |
|---|--:|--:|--:|--:|
| Seq write (MB/s)        | 604     | **102**   | 361     | 1897    |
| Seq read (MB/s)         | 201     | 166 ⁺     | 3425    | 3715    |
| Rand 4K write (IOPS)    | 41,879  | **12,710**| 2,361 § | 113,257 |
| Rand 4K read (IOPS)     | 31,761  | 16,871    | 4,328 § | 283,237 |
| fsync 4K commit (ops/s) | 39,250 ⁑| **4,157** | 1,123   | 2,596   |
| fsync latency (µs, mean)| 25 ⁑    | ~240      | ~890    | ~385    |
| create empty (ops/s)    | 364,259 | **2,839** | 56,433  | 61,786  |
| lookup+stat (ops/s)     | 32,836  | 17,433    | 660,792 | 879,459 |
| unlink (ops/s)          | 350,847 | 33,543    | 59,014  | 154,712 |

⁺ FUSE doesn't honor `O_DIRECT`; RFS seq-read is page-cache-warm (others are not).
§ ZFS ran at its default `recordsize=128k`; 4K-random becomes 128K read-modify-write — a
recordsize artifact, not a fundamental ZFS limit (a 4K-recordsize dataset would be far faster).
⁑ Engine fsync is in-RAM (no media flush) — it measures commit *CPU* cost, not durability.

## Reading the numbers

**RFS's commit path looks strong.** On the durable-fsync test (every op forced to
disk — the most apples-to-apples row), RFS-via-FUSE does 4,157 ops/s, *ahead of*
ext4 (2,596) and ZFS (1,123) on this NVMe. RFS's superblock-ring + single
`fdatasync` per txg is cheaper per durable op than ext4's journal or ZFS's ZIL
here. Caveat: each commit only had to make 1 block durable; a heavier transaction
commits more of the CoW tree.

**4K-native design pays off vs default ZFS.** RFS-via-FUSE random 4K write
(12,710) beats default-recordsize ZFS (2,361) because RFS writes 4K blocks
natively with no read-modify-write. ext4 still wins big (113k) — partly real,
partly because the kernel targets ran at iodepth=16 (libaio) while FUSE is
effectively serial (iodepth=1).

**The FUSE adapter is the dominant tax today**, exactly as expected:
- create: 364k/s in-engine → **2.8k/s** through FUSE (~130× — FUSE round-trips
  plus a synchronous commit per op dominate). This is the single worst gap.
- seq write: 604 → 102 MB/s; random write: 42k → 13k IOPS.
- The engine ceiling shows the algorithm is sound; most of the gap to ZFS/ext4 is
  the userspace adapter, not the core.

**Genuine engine weak spots** (slow even in RAM, so these are core, not FUSE):
- **Seq read 201 MB/s < seq write 604** — the read path allocates a `Vec` per
  block and verifies the parent-stored checksum on every read; both are
  optimizable (buffer reuse, batch verify).
- **lookup+stat 33k/s** — the dirent path does two B-tree gets plus a bucket
  scan; a hot-path the directory index can improve.

## Honest caveats

- **Not a kernel-vs-kernel comparison.** RFS runs in userspace via FUSE with a
  naive executor; ZFS/ext4 are in-kernel. The engine column is the fairer proxy
  for RFS's eventual target.
- **Methodology asymmetry:** ZFS/ext4 used `O_DIRECT` + `iodepth=16` (libaio);
  RFS-FUSE used buffered `psync` (iodepth=1) because FUSE ignores `O_DIRECT`.
  RFS seq-read is cache-warm.
- **ZFS was deliberately constrained for fairness**, not tuned for speed: a single
  file-vdev on the same disk (matching RFS's file-backed image), `recordsize=128k`
  (default), `compression=off`, and `primarycache=metadata` so its ARC and the
  host's real pools were untouched. A production ZFS (raw disks, ARC, tuned
  recordsize) would post very different numbers.
- Single run, no repetition/variance bars. Baseline only.

## What this says about priorities

1. The engine is competitive at the algorithm level; the path to real-world speed
   runs through a **better adapter** (multi-threaded executor, batched FUSE
   replies, eventually the in-kernel/Feox path) rather than core rewrites.
2. **Read path** and **directory/metadata path** are the two core hot spots worth
   profiling next.
3. RFS's **commit/durability** and **small-random-write** behavior are already
   differentiators worth protecting as the adapter improves.

## Reproducing

See `bench/README.md`. In short, on a Linux host with `fio`, `gcc`, a Rust
toolchain, root (for ZFS/ext4), and ZFS installed:

```
BASE=/var/tmp/rfsbench SRC=$BASE/src bash bench/provision.sh   # build
bash bench/engine.sh                                           # layer 1
bash bench/run-rfs-fuse.sh                                     # layer 2 (user)
sudo bash bench/run-ext4.sh                                    # layer 3
sudo bash bench/run-zfs.sh                                     # layer 3
```
