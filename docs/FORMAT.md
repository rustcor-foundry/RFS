# RFS on-disk format (v1)

Status: **superblock frozen**; block pointer, node, ZIL, and snapshot directory
specified but not yet implemented. All multi-byte integers are little-endian.

Design decisions this format encodes:

- **Consistency:** CoW + superblock ring (no consistency journal). ZFS uberblock
  model. See `superblock.rs`.
- **Write amplification:** bounded by txg batching; pointers kept compact.
- **Snapshots:** ZFS birth-times + dead-lists, with a reserved feature flag to
  add a Btrfs-style backref tree later (escape hatch).
- **Integrity:** parent-stores-child-checksum Merkle chain; per-volume digest
  mode (`Fast64` 8 B default / `Blake3` 32 B opt-in).
- **fsync:** intent log (ZIL) — built with the first txg layer, not deferred.

## Superblock (one block, ring of `RING = 4` slots at LBA 0..4)

Slot for txg N = `N % RING`. Commit writes one slot + flush; open scans all,
discards bad magic/checksum, keeps highest `txg`. **Implemented.**

| Offset | Size | Field | Notes |
|-------:|-----:|-------|-------|
| 0   | 8  | magic           | `b"RFS\0SBLK"` LE |
| 8   | 4  | version         | = 1 |
| 12  | 4  | block_size      | bytes/block at format time |
| 16  | 8  | txg             | monotonic; higher = newer |
| 24  | 8  | flags           | feature bits (bit0 reserved: `BACKREF_TREE`) |
| 32  | 1  | digest          | 0 = Fast64, 1 = Blake3 (+7 pad) |
| 40  | 8  | root_addr       | live tree root block; 0 = none |
| 48  | 8  | root_birth_txg  | txg the root was born |
| 56  | 32 | root_checksum   | Merkle anchor; max-width (Fast64 uses first 8) |
| 88  | 8  | zil_head        | intent-log head block; 0 = empty |
| 96  | 8  | snaplist_root   | snapshot-directory root; 0 = none |
| 104 | 16 | volume_uuid     | stable cluster identity; all-zero = unassigned |
| 120 | 8  | owner_hostid    | MMP fence: current owner host; 0 = unowned |
| 128 | 8  | mmp_seq         | MMP heartbeat counter; 0 = MMP disabled |
| 136 | …  | reserved        | zeroed to block_end-8 |
| end-8 | 8 | checksum       | Fletcher-64 over bytes[0 .. end-8] |

`flags` bits: bit0 `BACKREF_TREE` (snapshot escape hatch), bit1 `REPLICATED`
(block-level replication; MMP fence fields active).

## Segment geometry (M2, implemented in-memory)

The device is divided into fixed-size **segments** (target 2 MiB = 512 × 4 KiB
blocks). A reserved prefix of segments holds the superblock ring (and, later,
persisted allocator metadata) and is never allocated. Allocation appends within
a per-`SegKind` active segment — `Meta` (hot) and `Data` (cold) never share a
segment. Frees are staged and applied at `commit`; a closed segment whose live
count reaches zero is reclaimed whole. **Not yet persisted** — the on-disk
space-map / mark-and-sweep recovery and live-block relocation (cleaning) are
later milestones, so no on-disk layout is frozen for the allocator yet.

## Block pointer (planned, M3) — fixed width per volume

The convergence point of all three decisions. Width is set by the volume digest.

| Field | Size | Purpose |
|-------|-----:|---------|
| addr      | 8 | child block address |
| birth_txg | 8 | snapshot birth-time reclamation (decision 2) |
| checksum  | 8 (Fast64) / 32 (Blake3) | self-healing Merkle link (decision 3) |

**Total: 24 B (Fast64) / 48 B (Blake3).** Fanout in a 4 KiB node ≈ 170 / 85 —
the reason the fast mode is default (decision 1: smaller pointers → flatter tree
→ less rewritten per txg).

## Extent record (planned, M3) — transform-ready

A leaf's value for file data is an *extent*: a run of blocks plus the metadata
needed to decode it. Reserved up front so compression / encryption / dedup need
no later format break:

| Field | Size | Purpose |
|-------|-----:|---------|
| ptr           | 24/48 | `BlockPtr` to the (possibly transformed) bytes |
| logical_len   | 4 | size after decode (what the file sees) |
| physical_len  | 4 | size on media (after compression/padding) |
| transform_algo| 1 | 0 none, 1 lz4, 2 zstd, …; high nibble reserved for encryption |
| key_id        | 4 | encryption key handle (capability id); 0 = plaintext |
| flags         | 1 | e.g. dedup-shared |

`physical_len < logical_len` ⇒ compressed; `key_id != 0` ⇒ encrypted (authenticated).
Dedup uses the `Blake3` digest mode as the content address.

## Hardware acceleration & transports (seams in place, impls later)

None of this lives in the FS core; it sits at the buffer/device seam.

- **Zero-copy buffers** (`buffer.rs`): `AlignedBuf` is page-aligned and carries an
  optional `RegionKey` (`lkey`/`rkey`/remote addr) so an RDMA / NVMe-oF transport
  can register the engine's own buffers and do one-sided transfers. `BufferPool`
  recycles them.
- **Vectored I/O**: `BlockDevice::read_extent` / `write_extent` are the
  scatter-gather seam (default fans out per block; a real NVMe SGL / RDMA backend
  overrides with one transfer).
- **Device capabilities** (optional traits): `ZonedDevice` (ZNS: segment == zone,
  reclaim == zone reset, no device GC), `Deallocate` (TRIM on segment reclaim),
  `PlacementWrite` + `PlacementHint` (NVMe FDP / streams; `SegKind` → hint).
- **Digest acceleration** (`digest.rs`): `Hasher` binds the volume `DigestMode` to
  a runtime `SimdLevel`. RVV (e.g. K1 / SpacemiT X60, RVV 1.0 / 256-bit VLEN) is
  the intended fast path for BLAKE3 / xxh3 / Fletcher; a scalar fallback always
  exists. Speedups are bandwidth-sensitive and must be measured on silicon.
- **NVMe-oF / RDMA**: realized as a `BlockDevice` impl below the seam and as the
  M9 replication transport — never in the core. Prefer ChaCha20-Poly1305 over
  AES-GCM on K1 unless vector-AES (Zvk*) is present.

## Tree node (M3 — codec implemented)

B+-tree node, one per block. Header `{ magic=b"RFND", level(1), flags(1),
key_count(2), generation/txg(8) }` = 16 B, then:

- **leaf** (level 0): `key_count` × `(key, value)`, sorted ascending.
- **internal** (level > 0): `key_count` separator keys, then `key_count + 1`
  `BlockPtr`s.

Checksummed over the *entire block* (zeroed tail) → that digest lives in the
parent's `BlockPtr` (root's in `root_checksum`). `read_node` recomputes and
compares before decoding — silent corruption returns `BadChecksum` (self-heal
trigger). Header counts are bounds-checked against block capacity before any
payload is read, so a corrupt header cannot cause an OOB read.

Capacities (4 KiB block, u64/u64, Fast64): **255** leaf entries, **126** internal
keys. Tree operations (CoW insert/split/lookup) are the next M3 increment.

## Intent log / ZIL (planned, M4)

Self-checksummed append log for synchronous (`fsync`) writes. Records logical
operations, acks immediately, folds into the next regular txg.

- Record header `{ checksum, seq, txg, op_type, len }` + payload.
- Chain anchored at superblock `zil_head`; the run ends at the first record whose
  checksum fails (torn tail) — the same torn-write tolerance as the superblock
  ring.
- Mount order: recover superblock → replay ZIL records newer than the committed
  root → fold into the open txg.
- Reclaimed once the folding txg commits (head pointer advances).
- Gets its own crash-injection fuzzing, like the superblock ring already has.

## Snapshot directory (implemented) + space reclamation (partial)

The directory is a single self-checksummed block at `snaplist_root` (magic
`b"RSNP"`, u32 count, then 64-byte entries `{id, txg, root.addr, root.birth,
root.checksum[32]}`, Fletcher-64 trailer). Written copy-on-write; the old block
becomes unreferenced and is reclaimed at the next mount. Mount-time mark-and-sweep
walks the directory block **and every snapshot's tree**, so snapshot-pinned blocks
are never reclaimed or reused. Snapshot *deletion* and precise dead-list freeing
are the remaining piece.

- Snapshot = preserved root tagged with its `txg`, recorded under `snaplist_root`.
- Free path: `block.birth_txg > youngest_snapshot.txg` → free now; else append to
  that snapshot's dead-list (deferred).
- Snapshot delete: merge dead-list against the next-older snapshot; free what is
  now unreferenced.
- Even with no snapshots, CoW requires deferred frees within a txg: a block freed
  in txg N is reusable only after the root referencing its replacement commits.
  The allocator stays dumb (`free` = mark free); the transaction layer owns the
  pending-free set and birth/dead-list logic, calling `free` only for truly-dead
  blocks.

## Clustering / k8s (planned, M8–M9)

RFS itself stays **single-writer, never distributed**. Distribution lives *below*
RFS (a replicated `BlockDevice`) or *above* it (CSI + orchestration) — never in
the core. Target model: **replicated-local + CSI** (Longhorn/Mayastor-style):
local-read performance with HA.

What the existing format already gives the cluster layer for free:

- **Incremental replication / DR:** ship only blocks with `birth_txg >
  last_synced_txg` (ZFS send/receive). The snapshot birth-time field *is* the
  replication primitive.
- **CSI snapshots/clones:** map directly onto the CoW snapshot directory; each
  committed `txg` is a consistent replication/snapshot epoch.
- **Over-the-wire integrity + Merkle-diff resync:** parent-stored checksums.
- **No network write-hole:** full-segment (log-structured) writes ship whole,
  torn-write-safe segments.
- **CSI runs on the std/FUSE build** on Linux nodes; the bare-metal Feox build is
  a separate deployment universe (kubelet can't run on the exokernel).

Reserved now (model-independent): `volume_uuid`, `owner_hostid` + `mmp_seq`
(multihost fence / split-brain prevention, ZFS MMP-style), and the `REPLICATED`
flag. None implemented yet.

## Feature-flag policy (escape hatch)

Unknown bits in `flags` that the build does not understand make a volume
read-only (or refused), so older builds never corrupt newer formats. `BACKREF_TREE`
(bit0) is the reserved slot for adding Btrfs-style extent backrefs/reflinks
without breaking existing volumes.
