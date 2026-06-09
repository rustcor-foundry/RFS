# RFS — build plan

A copy-on-write, log-structured filesystem engine. First target: the **Feox**
exokernel on **RISC-V**. RFS sits *above* the block-device seam, so the kernel's
ISA barely touches the engine — the core compiles bare-metal `no_std` today
(verified for `riscv64gc-unknown-none-elf`).

## Progress snapshot — 2026-06-08

**59 tests passing (incl. crash-recovery simulation) · clippy-pedantic clean ·
bare-metal RISC-V build green. Mounts via FUSE on Linux (`mkdir`/`write`/`cat`/
`ls`/`rm`/`mv`); survives a torture pass (120 varied-size files, checksums match
across remount). FS ops batch into the txg (durable on `sync`/`fsync`/unmount).**

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

M4 done: **txg batching with coalescing** (in-memory `Txg` dirty-node shadow;
each touched node written once at commit — 500 updates to one key → ≤4 writes)
and the **ZIL** — `sync_insert`/`sync_delete` make writes durable immediately via
a reserved intent-log ring, replayed on mount. fsync'd writes survive a crash;
un-synced ones may not; a full ring forces a commit.

M7 (functionally complete): a POSIX-ish `Filesystem` over `FsKey (object_id,
kind, k2)` — inodes, directories (`mkdir`/`create`/`lookup`/`resolve`/`readdir`/
`getattr`/`unlink`/`rmdir`, collision-safe dirent buckets), and **file data via
extents** (`read`/`write`/`truncate`, block-granular CoW, checksummed data
blocks, birth-gated freeing, sparse holes). mark-and-sweep follows extent
pointers (`Value::referenced_blocks`) so file data survives reclamation/remount.
Next: per-op batching (vs commit-per-op) and the FUSE mount on Linux.

M6 (started): a **byte-driven, model-checked crash-recovery driver**
(`testkit::fuzz_crash_recovery`) shared by a deterministic in-crate simulation
(48 seeds × random insert/delete/fsync/commit/power-cut/torn-commit steps, each
verified to recover exactly the last commit + fsync'd ops) and a `cargo fuzz`
target under `fuzz/` (nightly/Linux). Reads validate every checksum, so the
self-healing path is exercised too.

Next in M6: run the libFuzzer campaign on a Linux host; the FUSE mount belongs
*after* the VFS layer (it exposes POSIX semantics), so M7 comes first.

Deferred optimizations: per-snapshot dead-lists; persisted space map.

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
- [x] **M4 — txg transaction layer + ZIL**:
  - [x] In-memory dirty-node shadow (`txg.rs`) with coalescing: ops buffer in
    RAM, `serialize` writes each touched node once, birth-gated free at commit.
  - [x] ZIL (`zil.rs`): reserved-ring intent log; `sync_insert`/`sync_delete`
    durable immediately, replayed on mount; full ring forces a commit.
  - [ ] Power-cut fuzzing through the whole stack, including the ZIL tail (M6).
- [~] **M5 — `rfs-feox` adapter**: bridge `feox-nvme` async queues to
  `BlockDevice`, capability-revocation aware.
  - [x] Design + gap analysis (`docs/FEOX-INTEGRATION.md`): the seam already fits
    (async/`!Send`/capability/registered-buffer); the async future maps 1:1.
  - [ ] **Blocked on `feox-nvme`** growing a data path: command payload on
    `submit` (opcode/lba/buffer), SQE+doorbell, CQ processing, DMA buffers,
    namespace identify. Today it is inflight-tracking only. Adapter lands after.
- [x] **M6 — crash-recovery fuzzing + FUSE testbed**:
  - [x] Model-checked crash-recovery driver (`testkit::fuzz_crash_recovery`),
    deterministic simulation test, and a `cargo fuzz` target (`fuzz/`).
  - [x] libFuzzer campaign runs on Linux (WSL): 2340 deep execs, no failures.
  - [x] FUSE mount (`rfs-fuse/`, std + `fuser`): mounts a file-backed image as a
    real directory; verified mkdir/write/cat/ls/rm and persistence across
    remount (200 KB file SHA-256 matches).
- [~] **M7 — VFS / POSIX inode layer** (inodes, directories, paths over the KV tree):
  - [x] Phase 1: variable-length leaf values (`Value` trait; Btrfs "fixed key,
    variable item data"). Leaves pack length-prefixed values and split by bytes;
    keys + internal nodes stay fixed. Tree/txg/ZIL all converted.
  - [x] Phase 2a: VFS namespace — `FsKey (object_id, kind, k2)`, inode/dirent
    records, tree range scan, `Filesystem` (mkdir/create/lookup/resolve/readdir/
    getattr/unlink/rmdir), collision-safe dirent buckets, remount-durable.
  - [x] Phase 2b: file data via extents — `read`/`write`/`truncate`,
    block-granular CoW, checksummed data blocks, birth-gated free, sparse holes;
    `Value::referenced_blocks` so mark-and-sweep keeps data blocks live.
  - [x] FUSE mount on Linux (`rfs-fuse/`) — RFS works as a real mounted directory.
  - [x] `rename`/`mv` (atomic, replaces dest; dir subtree moves) + fix: `unlink`
    now frees a file's data extents/blocks at nlink 0 (was leaking them).
  - [x] Batched commits: FS ops accumulate in the open txg (range-over-txg so
    reads see uncommitted state) and flush at a block threshold or on
    `sync`/`fsync`/`flush`/unmount — many ops now coalesce into one commit
    (100 mkdirs → <20 device writes vs ~100 before).
  - [ ] Phase 2c: timestamps (host clock), statfs/df, symlinks/hardlinks.
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
  src/volume.rs        Volume: format/open/insert/get/delete/range/commit/snapshot/sync
  src/fs.rs            Filesystem: inodes, directories, paths (FsKey/FsValue)
  src/txg.rs           Txg: in-memory dirty-node shadow + coalesced serialize
  src/zil.rs           intent log: reserved ring, sync writes + mount replay
  src/snapshot.rs      self-checksummed snapshot directory (SnapEntry)
  src/error.rs         StorageError incl. CapabilityRevoked / DeviceRemoved
  src/testkit.rs       MemDevice (crash injection) + block_on + fuzz driver  [feature: testkit]
  src/sim.rs           deterministic crash-recovery simulation  [cfg(test)]
fuzz/                  cargo-fuzz target (nightly/Linux; excluded from workspace)
rfs-fuse/              FUSE adapter — mounts a Filesystem on Linux (excluded)
```

## Commands

```
cargo test -p rfs-core
cargo clippy -p rfs-core --all-features
cargo build -p rfs-core --target riscv64gc-unknown-none-elf   # bare-metal proof
```
