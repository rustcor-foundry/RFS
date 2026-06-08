# RFS — build plan

A copy-on-write, log-structured filesystem engine. First target: the **Feox**
exokernel on **RISC-V**. RFS sits *above* the block-device seam, so the kernel's
ISA barely touches the engine — the core compiles bare-metal `no_std` today
(verified for `riscv64gc-unknown-none-elf`).

## Progress snapshot — 2026-06-08

**42 tests passing · clippy-pedantic clean · bare-metal RISC-V build green.**

Done: M1 (superblock ring + atomic commit), M2 (segment allocator), the
hardware/transport seams (zero-copy buffers, vectored I/O, Zoned/Deallocate/
Placement caps, Digest+SIMD dispatch), M3 inc.1 (BlockPtr + B+-tree node codec +
verify-on-read), M3 inc.2 (CoW `Tree::insert`/`get` walk, splits, height growth,
MVCC), and M3 inc.3 (`Volume`: root publish through the superblock commit —
**first whole-stack crash-atomic transaction**: `insert → commit → reopen → get`
survives, and a torn commit rolls back to the prior committed tree).

Also done: mount-time **mark-and-sweep** allocator recovery (writable remount,
garbage reclaim, full-tree checksum verify), and **snapshots** — `Volume::snapshot`
captures a root into a self-checksummed snapshot directory; `get_in_snapshot`
reads pinned state; mark-and-sweep walks every snapshot tree so pinned blocks
survive remount.

Also done: **proactive free on CoW overwrite** (a replaced block is freed unless
its `birth_txg` shows a snapshot pins it) and **`Volume::delete_snapshot`** with
reclamation (removes the entry, commits, then an online mark-and-sweep frees the
blocks the snapshot alone pinned).

In progress: M3 — per-snapshot dead-lists to make delete/free incremental (vs.
the current full sweep); persisted space map to avoid the full-tree scan on
mount. Then M4 (txg + ZIL).

Next after that: M4 (txg + ZIL), M5 (feox adapter), M6 (FUSE + fuzz), M7 (VFS).

## Locked decisions

- **Strict layering** (ZFS DMU/SPA split): VFS → transactional object/B-tree →
  segment allocator → block device. Upper layers never know what's underneath.
- **Async I/O seam, end-to-end.** Feox storage is completion-driven and
  core-local (`!Send`); `BlockDevice` uses native `async fn` in traits over
  *static* dispatch (generics), giving concrete `!Send` futures with no
  `async-trait` allocation and no `dyn`.
- **Consistency via CoW + superblock ring, not a journal.** A commit publishes a
  new tree root by writing the superblock to slot `txg % RING` and flushing.
  Consecutive `txg`s use distinct slots, so a torn write can't damage the prior
  good slot. `open()` picks the highest valid slot. (ZFS uberblock model.)
- **Conventions mirror Feox:** edition 2024, `no_std` + `alloc`, pedantic clippy
  clean, `undocumented_unsafe_blocks = deny`.

## Resolved format decisions (see `docs/FORMAT.md`)

1. **txg batching** — dirty-node cache with write coalescing; commit on
   dirty-bytes watermark / time / explicit sync. Single-writer builds one txg;
   readers run MVCC against the last committed root. Start 2-phase, leave room
   for ZFS-style 3-stage pipelining. Pointers kept compact to limit per-txg
   rewrite cost.
2. **Snapshots** — ZFS birth-times + dead-lists now; **`BACKREF_TREE` feature
   flag reserved** to add Btrfs-style extent backrefs/reflinks later without
   breaking volumes. `birth_txg` lives in every block pointer.
3. **Integrity** — parent-stores-child checksum (Merkle, self-healing) via a
   `Digest` trait; **per-volume mode: `Fast64` (8 B) default, `Blake3` (32 B)
   opt-in**. Width fixes pointer size → fanout → write amp. Superblock anchors
   the root checksum max-width.
4. **fsync** — **intent log (ZIL) built with the first txg layer (M4)**, not
   deferred. Consistency still comes from the superblock ring; the ZIL is purely
   for sync latency.

## Milestones

- [x] **M1 — Atomic commit primitive.** `BlockDevice` seam, `Superblock` ring,
  Fletcher-64, in-memory crash-injecting device + poll-once `block_on`.
  Property test cuts power at every byte offset of a commit and proves recovery
  is always the new state or the prior state — never torn. *(done)*
- [x] **M2 — Segment allocator** (F2FS-style): fixed-size segments, per-`SegKind`
  active segment (hot meta / cold data isolation), append-only allocation,
  per-block liveness bitmap. `Allocator` trait with **deferred free drained at
  `commit`** (CoW-within-txg rule) and whole-segment reclamation. In-memory for
  now; persistence (space-map / mark-and-sweep) and live-block *relocation*
  (cleaning) are later. *(done)*
- [~] **M3 — CoW Merkle B-tree** (`BTree<K, V, A: Allocator>`):
  - [x] `BlockPtr {addr, birth_txg, checksum}` codec (fast64/blake3 widths).
  - [x] `Record`/`Key` traits; B+-tree leaf/internal node codec + capacities.
  - [x] `write_node`/`read_node` — CoW write (leaf→Data, internal→Meta) +
    parent-checksum verify-on-read (corruption → `BadChecksum`).
  - [x] `Tree::insert`/`get` — iterative CoW walk: leaf upsert, node splits,
    height growth, MVCC (old root intact). `Txn` write context.
  - [x] `Volume` — root publish via superblock commit; `insert → commit →
    reopen → get` round-trip; torn-commit rolls back to prior tree.
  - [x] Mount-time mark-and-sweep allocator recovery — writable remount,
    garbage reclaim, full-tree checksum verify on mount.
  - [x] Snapshots: `Volume::snapshot`/`get_in_snapshot`, self-checksummed
    snapshot directory, mark-and-sweep walks snapshot trees (pinned blocks
    survive remount).
  - [x] Proactive free on CoW overwrite (birth-time gated) + `delete_snapshot`
    with online mark-and-sweep reclamation.
  - [x] `Tree::delete` / `Volume::delete` — CoW key removal (no merge yet;
    emptied leaves retained, lookups stay correct).
  - [ ] Structural compaction on delete (merge underfull nodes, shrink height).
  - [ ] Per-snapshot dead-lists (incremental delete/free vs. full sweep).
  - [ ] Persisted space map (avoid full-tree scan on mount).
- [ ] **M4 — txg transaction layer + ZIL**: dirty-node cache → coalesce → write
  nodes → publish root via M1. Intent log for fsync (replay on mount). Power-cut
  fuzzing through the whole stack, including the ZIL tail.
- [ ] **M5 — `rfs-feox` adapter**: bridge `feox-nvme` async queues to
  `BlockDevice`, capability-revocation aware.
- [ ] **M6 — FUSE testbed** (desktop) + `cargo fuzz` campaign.
- [ ] **M7 — VFS / POSIX inode layer.**
- [ ] **M8 — CSI driver** on the std/FUSE build: provision/attach/mount/expand,
  VolumeSnapshot → CoW snapshots, topology-aware (`WaitForFirstConsumer`).
- [ ] **M9 — Replicated `BlockDevice`**: network RAID-1 below the engine
  (sync + async/txg-keyed), MMP fencing, incremental resync via `birth_txg`.

## Hardware / transport stance

Acceleration and fabrics stay out of the core, behind the buffer/device seam:

- **Zero-copy ready:** `AlignedBuf` carries an optional RDMA `RegionKey`; vectored
  `read_extent`/`write_extent` are the scatter-gather path. NVMe-oF/RDMA are just
  `BlockDevice` impls below the seam (and the M9 transport).
- **Flash/SSD aware:** optional `ZonedDevice` (ZNS — segment == zone), `Deallocate`
  (TRIM on reclaim), `PlacementWrite` (FDP/streams, `SegKind` → hint).
- **RVV/K1:** `Hasher` dispatches on a runtime `SimdLevel`; RVV BLAKE3/xxh3/Fletcher
  plug in with a scalar fallback. Bandwidth-bound — measure on silicon. Prefer
  ChaCha20-Poly1305 + keyed-BLAKE3 on K1 unless vector-AES is present.
- **Transforms reserved:** extent record carries logical/physical length +
  transform algo + key id (compression/encryption/dedup) — see `docs/FORMAT.md`.

## Clustering stance

RFS stays **single-writer, never distributed**. Distribution goes *below* (a
replicated `BlockDevice`) or *above* (CSI + orchestration), never in the core —
the same rule as keeping RAID out of the FS. Target: **replicated-local + CSI**.
Format already reserves `volume_uuid`, MMP fence (`owner_hostid`/`mmp_seq`), and
the `REPLICATED` flag; `birth_txg` doubles as the incremental-replication key.

## Layout

```
crates/rfs-core/   #![no_std] engine — device seam, superblock, (later) allocator + tree
  src/device.rs        async BlockDevice trait (the seam)
  src/superblock.rs    ring + atomic commit/open/format
  src/allocator.rs     SegmentAllocator: hot/cold segments, deferred free
  src/buffer.rs        AlignedBuf (RDMA-registerable) + BufferPool
  src/device.rs        BlockDevice + read/write_extent + Zoned/Deallocate/Placement
  src/digest.rs        Digest trait + Hasher with runtime SimdLevel (RVV plug point)
  src/checksum.rs      fletcher64 + DigestMode
  src/tree/ptr.rs      BlockPtr {addr, birth_txg, checksum}
  src/tree/node.rs     B+-tree node codec + write_node/read_node (verify-on-read)
  src/tree/btree.rs    Tree::insert/get — CoW walk, splits, MVCC; Txn context
  src/volume.rs        Volume: format/open/insert/get/commit/snapshot (whole-stack txn)
  src/snapshot.rs      self-checksummed snapshot directory (SnapEntry)
  src/error.rs         StorageError incl. CapabilityRevoked / DeviceRemoved
  src/testkit.rs       MemDevice (crash injection) + block_on  [feature: testkit]
```

## Commands

```
cargo test -p rfs-core
cargo clippy -p rfs-core --all-features
cargo build -p rfs-core --target riscv64gc-unknown-none-elf   # bare-metal proof
```
