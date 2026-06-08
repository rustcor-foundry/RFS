# RFS

A copy-on-write, log-structured, self-healing filesystem engine in Rust.

RFS is built for the **[Feox](../Feox) exokernel on RISC-V** first, but its core
is hardware-agnostic `no_std` and compiles bare-metal today
(`riscv64gc-unknown-none-elf`) as well as for a desktop FUSE testbed. It borrows
the survival traits of ZFS, Btrfs, and F2FS while keeping the layers strictly
decoupled so a bug in one cannot silently corrupt another.

> **Status: early, but real.** The bottom of the stack works end to end and is
> tested: a `Volume` does whole-stack crash-atomic transactions
> (`insert → commit → reopen → get` survives; a torn commit rolls back), remounts
> are writable via mark-and-sweep recovery, and **snapshots** pin state that
> survives overwrites and remount. Below it: atomic superblock commit, a segment
> allocator, the CoW Merkle B-tree with verify-on-read, and the hardware/transport
> seams. Snapshots delete and reclaim space; writes batch into a coalescing
> transaction (each touched node written once at commit); and an intent log (ZIL)
> makes `fsync`-style writes survive a crash. A randomized, model-checked
> crash-recovery simulation hammers the whole stack with power-cuts and torn
> commits. The VFS/POSIX layer and kernel/FUSE adapters are next. *(2026-06-08:
> 46 passing tests, clippy-pedantic clean, bare-metal RISC-V build green.)*

## Why it's built this way

The downfall of ambitious filesystems is rarely the concept — it's
*architectural entanglement*, where the disk allocator and the logical tree are
so coupled that a low-level bug corrupts high-level data. RFS copies the one
decision that made ZFS legendary: a hard wall between layers, expressed in Rust
as traits with static dispatch.

```
┌──────────────────────────────────────────────────────────┐
│  VFS / POSIX (inodes, dirs, symlinks)            [M7]      │
├──────────────────────────────────────────────────────────┤
│  Volume: root publish via superblock commit      [M3] ✅   │
│  Transactional layer: txg batching + ZIL         [M4] ✅   │
├──────────────────────────────────────────────────────────┤
│  CoW Merkle B-tree (BlockPtr, verify-on-read)    [M3] ✅   │
├──────────────────────────────────────────────────────────┤
│  Segment allocator (F2FS-style, hot/cold)        [M2] ✅   │
├──────────────────────────────────────────────────────────┤
│  Superblock ring + atomic commit                 [M1] ✅   │
├──────────────────────────────────────────────────────────┤
│  BlockDevice seam (async, !Send, capability-aware)         │
│    └─ in-memory testkit · NVMe · NVMe-oF/RDMA · FUSE       │
└──────────────────────────────────────────────────────────┘
```

Upper layers never know whether they're writing to NVMe, an SD card, a remote
RDMA target, or a RAM array in a unit test — they only see `BlockDevice`.

## Core principles

- **Copy-on-write, never overwrite.** Atomicity comes from publishing a new tree
  root via a superblock ring (ZFS uberblock model), not a journal.
- **Self-healing.** Every node's checksum lives in its *parent* (the root's in
  the superblock), verified on read — silent corruption is detected, not trusted.
- **Single-writer, MVCC reads.** One writer builds a transaction; readers run
  lock-free against the last committed root. No deep tree locking (the trap that
  mired Bcachefs).
- **Distribution and RAID live *outside* the FS** — below the `BlockDevice` seam
  (replicated block layer) or above it (CSI), never tangled into tree logic (the
  trap that mired Btrfs RAID 5/6).
- **Hardware behind traits.** ZNS, TRIM/FDP, RDMA registration, and RVV vector
  acceleration are optional capabilities/implementations; the core stays portable
  with scalar fallbacks.

## Quick start

```sh
cargo test  -p rfs-core                                  # 29 tests
cargo clippy -p rfs-core --all-features                  # pedantic, clean
cargo build -p rfs-core --target riscv64gc-unknown-none-elf   # bare-metal proof
```

Requires a recent stable Rust (edition 2024; developed on 1.94). Add the target
with `rustup target add riscv64gc-unknown-none-elf`.

## Repository layout

```
crates/rfs-core/        #![no_std] engine (the only crate so far)
  src/device.rs           async BlockDevice seam + Zoned/Deallocate/Placement caps
  src/buffer.rs           AlignedBuf (RDMA-registerable) + BufferPool
  src/superblock.rs       superblock ring: atomic commit / open / format
  src/allocator.rs        SegmentAllocator: hot/cold segments, deferred free
  src/digest.rs           Digest trait + Hasher (runtime SimdLevel, RVV plug point)
  src/checksum.rs         fletcher64 + DigestMode
  src/tree/               BlockPtr + B+-tree node codec + verify-on-read
  src/error.rs            StorageError (incl. CapabilityRevoked / DeviceRemoved)
  src/testkit.rs          in-memory crash-injecting device + block_on  [feature: testkit]
docs/FORMAT.md          on-disk format (frozen where marked)
PLAN.md                 milestones, resolved design decisions, stances
```

## How it's tested

The same crate that targets bare metal runs in user space against an in-memory
`BlockDevice` that can **inject torn writes and power cuts**. The headline tests:

- *Power cut at every byte offset of a commit* recovers the new or prior state —
  never a torn in-between.
- *Flip a byte under a tree node* → `read_node` returns `BadChecksum`, proving the
  parent-checksum self-healing path.

A randomized, **model-checked crash-recovery simulation**
(`testkit::fuzz_crash_recovery`, driven by `src/sim.rs`) runs random sequences of
insert/delete/fsync/commit with power-cuts and torn commits, asserting recovery
always equals the last commit plus the `fsync`'d ops — never a torn or corrupt
state. The same driver backs a `cargo fuzz` target (`fuzz/`, run on Linux):

```sh
cargo +nightly fuzz run crash_recovery     # coverage-guided, on Linux/macOS
```

This is the lower half of the eventual pipeline: in-memory → FUSE on desktop →
`cargo fuzz` → bare-metal kernel, so corruption is caught in user space long
before it reaches a Picoprobe.

## Documentation

- **[PLAN.md](PLAN.md)** — roadmap, resolved design decisions, and the
  clustering / hardware / transport stances.
- **[docs/FORMAT.md](docs/FORMAT.md)** — on-disk format and reserved fields.

## License

MIT OR Apache-2.0.
