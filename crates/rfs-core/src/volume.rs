//! A mounted volume: the first object that drives the *whole* stack through one
//! crash-atomic transaction.
//!
//! It owns the device, allocator, buffer pool, the in-memory superblock, and the
//! tree root, and ties them together:
//!
//! * `insert` mutates the tree copy-on-write (new blocks, old root intact).
//! * `commit` publishes the new root by stamping it into the superblock and
//!   writing the superblock ring — the single durability barrier. Deferred frees
//!   are drained only *after* the new root is durable.
//! * `open` recovers the newest consistent superblock, reconstructs the tree
//!   from its root pointer, and rebuilds the allocator by **mark-and-sweep**:
//!   it walks every reachable block and marks it live, so a remounted volume is
//!   immediately safe to write to (and leaked copy-on-write garbage is reclaimed
//!   as a side effect). The walk also checksum-verifies the whole tree at mount.
//!
//! This is a minimal precursor to the M4 transaction layer (one open txg at a
//! time, no batching/ZIL yet) and the first end-to-end power-cut-safe path.
//!
//! Recovery currently does a full tree scan on `open`; a persisted space map to
//! avoid that is a later optimization. Birth-time/dead-list snapshot reclamation
//! is also still pending.

use alloc::vec::Vec;

use crate::allocator::{Allocator, SegKind};
use crate::buffer::BufferPool;
use crate::checksum::DigestMode;
use crate::device::BlockDevice;
use crate::digest::Hasher;
use crate::error::{CorruptKind, StorageError};
use crate::snapshot::{self, SnapEntry};
use crate::superblock::{self, Superblock};
use crate::tree::{BlockPtr, Key, MAX_CKSUM, Tree, Value};
use crate::txg::Txg;
use crate::zil::{self, ZIL_CAPACITY, ZilRecord};

/// A mounted RFS volume parameterized by key/value record types and the
/// allocator/device implementations.
///
/// Writes accumulate in an open [`Txg`] (in-memory, coalesced) and become
/// durable only at [`commit`](Volume::commit) / [`snapshot`](Volume::snapshot),
/// which serialize the dirty nodes and publish a new root through the superblock
/// ring.
pub struct Volume<K, V, A, D> {
    dev: D,
    alloc: A,
    pool: BufferPool,
    hasher: Hasher,
    sb: Superblock,
    txg: Txg<K, V>,
    snaps: Vec<SnapEntry>,
    /// Next ZIL sequence number to write (records pending since last commit).
    zil_seq: u64,
    /// Out-of-tree data blocks superseded this txg, freed (birth-gated) at commit.
    data_free: Vec<BlockPtr>,
}

impl<K: Key, V: Value, A: Allocator, D: BlockDevice> Volume<K, V, A, D> {
    /// Formats a fresh volume on `dev` and returns it mounted and empty.
    ///
    /// # Errors
    /// Device errors, or [`StorageError::Unsupported`] for a digest mode without
    /// an implementation.
    pub async fn format(dev: D, alloc: A, digest: DigestMode) -> Result<Self, StorageError> {
        let sb = superblock::format(&dev, digest).await?;
        let hasher = Hasher::new(digest)?;
        let pool = BufferPool::for_block_size(dev.block_size());
        Ok(Self {
            dev,
            alloc,
            pool,
            hasher,
            sb,
            txg: Txg::begin(None),
            snaps: Vec::new(),
            zil_seq: 0,
            data_free: Vec::new(),
        })
    }

    /// Mounts an existing volume: recovers the newest superblock and rebuilds the
    /// tree handle from its root pointer.
    ///
    /// See the module note: the returned volume is read-consistent; writing
    /// before allocator recovery exists is not yet supported.
    ///
    /// # Errors
    /// Device errors, no valid superblock, or an unsupported digest mode.
    pub async fn open(dev: D, mut alloc: A) -> Result<Self, StorageError> {
        let sb = superblock::open(&dev).await?;
        let hasher = Hasher::new(sb.digest)?;
        let mut pool = BufferPool::for_block_size(dev.block_size());
        let root = root_ptr(&sb);

        // Mark-and-sweep: rebuild allocator free space from everything still
        // referenced — the live tree, the snapshot directory block, and every
        // snapshot's tree — so the volume is writable and snapshot-pinned blocks
        // are never handed out or reclaimed.
        let mut live: Vec<u64> = Vec::new();
        if let Some(r) = root {
            Tree::<K, V>::at(r)
                .collect_blocks(&dev, &mut pool, hasher, &mut live)
                .await?;
        }

        let mut snaps = Vec::new();
        if sb.snaplist_root != 0 {
            live.push(sb.snaplist_root);
            snaps = snapshot::read(sb.snaplist_root, &dev, &mut pool).await?;
            for snap in &snaps {
                if !snap.root.is_null() {
                    Tree::<K, V>::at(snap.root)
                        .collect_blocks(&dev, &mut pool, hasher, &mut live)
                        .await?;
                }
            }
        }

        for block in live {
            alloc.mark_live(block)?;
        }
        alloc.finish_rebuild();

        // Replay the intent log on top of the committed root: re-apply any
        // fsync'd ops recorded since the last commit (the ZIL ring is reserved,
        // so it is untouched by mark-and-sweep above).
        let mut txg = Txg::begin(root);
        let mut zil_seq = 0u64;
        while zil_seq < ZIL_CAPACITY {
            match zil::read::<K, V, D>(zil_seq, sb.txg, &dev, &mut pool).await? {
                None => break,
                Some(records) => {
                    for record in records {
                        match record {
                            ZilRecord::Insert(k, v) => {
                                txg.insert(k, v, &dev, &mut pool, hasher).await?;
                            }
                            ZilRecord::Delete(k) => {
                                txg.delete(&k, &dev, &mut pool, hasher).await?;
                            }
                        }
                    }
                    zil_seq += 1;
                }
            }
        }

        Ok(Self {
            dev,
            alloc,
            pool,
            hasher,
            sb,
            txg,
            snaps,
            zil_seq,
            data_free: Vec::new(),
        })
    }

    /// The committed (durable) root pointer, or `None` for an empty tree.
    #[must_use]
    fn committed_root(&self) -> Option<BlockPtr> {
        root_ptr(&self.sb)
    }

    /// The most recently committed transaction group.
    #[must_use]
    pub fn committed_txg(&self) -> u64 {
        self.sb.txg
    }

    /// Borrows the backing device (e.g. to inspect or, in tests, inject faults).
    pub fn device(&self) -> &D {
        &self.dev
    }

    /// Inserts or updates `key => val` in the open transaction (not yet durable;
    /// call [`commit`](Self::commit)).
    ///
    /// # Errors
    /// Allocation, device, or verification errors.
    pub async fn insert(&mut self, key: K, val: V) -> Result<(), StorageError> {
        self.txg.insert(key, val, &self.dev, &mut self.pool, self.hasher).await
    }

    /// Looks up `key` in the current (committed + open) tree.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn get(&mut self, key: &K) -> Result<Option<V>, StorageError> {
        self.txg.get(key, &self.dev, &mut self.pool, self.hasher).await
    }

    /// Collects `[start, end)` from the **committed** tree into `out`, ascending.
    /// (Callers that need their own uncommitted writes reflected should commit
    /// first; the higher FS layer commits per operation.)
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn range(
        &mut self,
        start: K,
        end: K,
        out: &mut Vec<(K, V)>,
    ) -> Result<(), StorageError> {
        if let Some(root) = self.committed_root() {
            Tree::<K, V>::at(root)
                .range(start, end, &self.dev, &mut self.pool, self.hasher, out)
                .await?;
        }
        Ok(())
    }

    /// The device block size — the unit of file-data extents.
    #[must_use]
    pub fn block_size(&self) -> usize {
        self.dev.block_size()
    }

    /// Allocates a fresh data block, writes `data` (exactly one block), checksums
    /// it, and returns a pointer stamped with the current transaction's `txg`.
    /// Becomes durable when the caller next commits; reclaimed on crash otherwise.
    ///
    /// # Errors
    /// Allocation or device errors.
    pub async fn alloc_data_block(&mut self, data: &[u8]) -> Result<BlockPtr, StorageError> {
        let addr = self.alloc.alloc(SegKind::Data)?;
        self.dev.write_block(addr, data).await?;
        let mut checksum = [0u8; MAX_CKSUM];
        self.hasher.hash(data, &mut checksum);
        Ok(BlockPtr {
            addr,
            birth_txg: self.sb.txg + 1,
            checksum,
        })
    }

    /// Reads a data block into `out` and verifies its checksum (self-healing).
    ///
    /// # Errors
    /// Device errors or [`CorruptKind::BadChecksum`].
    pub async fn read_data_block(
        &mut self,
        ptr: BlockPtr,
        out: &mut [u8],
    ) -> Result<(), StorageError> {
        self.dev.read_block(ptr.addr, out).await?;
        let mut got = [0u8; MAX_CKSUM];
        self.hasher.hash(out, &mut got);
        let len = self.hasher.output_len();
        if got[..len] != ptr.checksum[..len] {
            return Err(StorageError::Corrupt(CorruptKind::BadChecksum));
        }
        Ok(())
    }

    /// Stages a data block for freeing at the next commit (birth-gated, so a
    /// snapshot keeps what it pins). The block stays readable until then.
    pub fn free_data_block(&mut self, ptr: BlockPtr) {
        self.data_free.push(ptr);
    }

    /// Removes `key` from the open transaction (not durable until
    /// [`commit`](Self::commit)). Returns whether it was present.
    ///
    /// # Errors
    /// Allocation, device, or verification errors.
    pub async fn delete(&mut self, key: &K) -> Result<bool, StorageError> {
        self.txg.delete(key, &self.dev, &mut self.pool, self.hasher).await
    }

    /// Inserts `key => val` and makes it durable immediately via the intent log
    /// (an `fsync`-style write), without forcing a full commit. Survives a crash;
    /// is folded into the next commit.
    ///
    /// # Errors
    /// Allocation, device, or verification errors.
    pub async fn sync_insert(&mut self, key: K, val: V) -> Result<(), StorageError> {
        if self.zil_seq >= ZIL_CAPACITY {
            self.commit().await?; // ring full: drain it into a real txg (resets seq)
        }
        self.txg.insert(key, val.clone(), &self.dev, &mut self.pool, self.hasher).await?;
        zil::append(
            &[ZilRecord::Insert(key, val)],
            self.sb.txg,
            self.zil_seq,
            &self.dev,
            &mut self.pool,
        )
        .await?;
        self.zil_seq += 1;
        Ok(())
    }

    /// Removes `key` durably via the intent log. Returns whether it was present.
    ///
    /// # Errors
    /// Allocation, device, or verification errors.
    pub async fn sync_delete(&mut self, key: &K) -> Result<bool, StorageError> {
        if self.zil_seq >= ZIL_CAPACITY {
            self.commit().await?;
        }
        let removed = self.txg.delete(key, &self.dev, &mut self.pool, self.hasher).await?;
        zil::append(
            &[ZilRecord::<K, V>::Delete(*key)],
            self.sb.txg,
            self.zil_seq,
            &self.dev,
            &mut self.pool,
        )
        .await?;
        self.zil_seq += 1;
        Ok(removed)
    }

    /// Publishes the open transaction: stamps the tree root into a new superblock
    /// and writes it to the ring (the durability barrier). On success advances
    /// the txg and drains the allocator's deferred frees.
    ///
    /// If the superblock write tears (power cut), nothing is mutated and the
    /// previously committed state remains the one [`open`](Self::open) recovers.
    ///
    /// # Errors
    /// Device errors from the superblock write.
    pub async fn commit(&mut self) -> Result<(), StorageError> {
        let next = self.sb.txg + 1;
        let youngest = self.youngest_snap_txg();
        let cr = self.committed_root();
        // Serialize the dirty shadow (each touched node written once).
        let working = core::mem::replace(&mut self.txg, Txg::begin(cr));
        let (new_root, freed) = working
            .serialize(next, &mut self.alloc, &self.dev, &mut self.pool, self.hasher)
            .await?;

        let mut sb = self.sb;
        sb.txg = next;
        stamp_root(&mut sb, new_root);

        // The durability barrier. Only after the new root is durable do we free
        // superseded blocks (birth-gated so snapshots keep what they pin).
        superblock::commit(&self.dev, &sb).await?;
        self.sb = sb;
        self.txg = Txg::begin(new_root);
        self.zil_seq = 0; // ZIL records are now folded into the committed tree
        self.free_superseded(&freed, youngest)?;
        let data_free = core::mem::take(&mut self.data_free);
        self.free_superseded(&data_free, youngest)?;
        self.alloc.commit();
        Ok(())
    }

    /// Frees blocks superseded this txg whose `birth_txg` shows no snapshot pins
    /// them (`> youngest`). Staged frees are reclaimed by the caller's
    /// `alloc.commit()`.
    fn free_superseded(&mut self, freed: &[BlockPtr], youngest: u64) -> Result<(), StorageError> {
        for ptr in freed {
            if ptr.birth_txg > youngest {
                self.alloc.free(ptr.addr)?;
            }
        }
        Ok(())
    }

    /// Captures the current tree as a snapshot and commits. The snapshot pins the
    /// committed state; later overwrites copy-on-write around it. Returns the new
    /// snapshot id.
    ///
    /// (Space held only by snapshots is reclaimed when the snapshot is deleted —
    /// a later increment; for now snapshots accumulate.)
    ///
    /// # Errors
    /// Allocation, device, or capacity errors.
    pub async fn snapshot(&mut self) -> Result<u64, StorageError> {
        let next = self.sb.txg + 1;
        let youngest = self.youngest_snap_txg();
        let cr = self.committed_root();
        let working = core::mem::replace(&mut self.txg, Txg::begin(cr));
        let (new_root, freed) = working
            .serialize(next, &mut self.alloc, &self.dev, &mut self.pool, self.hasher)
            .await?;

        let id = self.snaps.iter().map(|s| s.id).max().map_or(0, |m| m + 1);
        let mut snaps = self.snaps.clone();
        snaps.push(SnapEntry {
            id,
            txg: next,
            root: new_root.unwrap_or(BlockPtr::NULL),
        });
        let snaplist_root = snapshot::write(&snaps, &mut self.alloc, &self.dev, &mut self.pool).await?;

        let mut sb = self.sb;
        sb.txg = next;
        sb.snaplist_root = snaplist_root;
        stamp_root(&mut sb, new_root);

        superblock::commit(&self.dev, &sb).await?;
        self.sb = sb;
        self.snaps = snaps;
        self.txg = Txg::begin(new_root);
        self.zil_seq = 0;
        self.free_superseded(&freed, youngest)?;
        let data_free = core::mem::take(&mut self.data_free);
        self.free_superseded(&data_free, youngest)?;
        self.alloc.commit();
        Ok(id)
    }

    /// Youngest (most recent) snapshot's txg, or 0 if there are none. Blocks born
    /// at or before this are pinned by a snapshot and not freed on overwrite.
    #[must_use]
    fn youngest_snap_txg(&self) -> u64 {
        self.snaps.iter().map(|s| s.txg).max().unwrap_or(0)
    }

    /// Deletes snapshot `id` and reclaims the blocks it alone pinned.
    ///
    /// Removes the entry, commits, then runs an in-place mark-and-sweep so blocks
    /// no longer reachable from the live tree or any remaining snapshot become
    /// free. (Reclamation is currently a full sweep; per-snapshot dead-lists to
    /// make it incremental are a later optimization.)
    ///
    /// # Errors
    /// [`StorageError::NotFound`] for an unknown id, or device errors.
    pub async fn delete_snapshot(&mut self, id: u64) -> Result<(), StorageError> {
        let pos = self
            .snaps
            .iter()
            .position(|s| s.id == id)
            .ok_or(StorageError::NotFound)?;

        // Flush any pending writes so the committed root is current, then drop
        // the entry. `gc` below recomputes allocator state, so superseded blocks
        // need no separate freeing here.
        let next = self.sb.txg + 1;
        let cr = self.committed_root();
        let working = core::mem::replace(&mut self.txg, Txg::begin(cr));
        let (new_root, _freed) = working
            .serialize(next, &mut self.alloc, &self.dev, &mut self.pool, self.hasher)
            .await?;

        let mut snaps = self.snaps.clone();
        snaps.remove(pos);

        let mut sb = self.sb;
        sb.txg = next;
        sb.snaplist_root = if snaps.is_empty() {
            0
        } else {
            snapshot::write(&snaps, &mut self.alloc, &self.dev, &mut self.pool).await?
        };
        stamp_root(&mut sb, new_root);

        superblock::commit(&self.dev, &sb).await?;
        self.sb = sb;
        self.snaps = snaps;
        self.txg = Txg::begin(new_root);
        self.zil_seq = 0;
        self.data_free.clear(); // gc() below recomputes allocator state

        self.gc().await
    }

    /// In-place mark-and-sweep: rebuild allocator free space from everything
    /// currently reachable (live tree + snapshot directory + snapshot trees),
    /// freeing all unreachable blocks. Same computation `open` does, run online.
    async fn gc(&mut self) -> Result<(), StorageError> {
        let mut live = Vec::new();
        if let Some(r) = self.committed_root() {
            Tree::<K, V>::at(r)
                .collect_blocks(&self.dev, &mut self.pool, self.hasher, &mut live)
                .await?;
        }
        if self.sb.snaplist_root != 0 {
            live.push(self.sb.snaplist_root);
            for snap in &self.snaps {
                if !snap.root.is_null() {
                    Tree::<K, V>::at(snap.root)
                        .collect_blocks(&self.dev, &mut self.pool, self.hasher, &mut live)
                        .await?;
                }
            }
        }
        self.alloc.reset();
        for block in live {
            self.alloc.mark_live(block)?;
        }
        self.alloc.finish_rebuild();
        Ok(())
    }

    /// Enumerates every block referenced by the current (live) tree.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn live_blocks(&mut self) -> Result<Vec<u64>, StorageError> {
        let mut out = Vec::new();
        if let Some(r) = self.committed_root() {
            Tree::<K, V>::at(r)
                .collect_blocks(&self.dev, &mut self.pool, self.hasher, &mut out)
                .await?;
        }
        Ok(out)
    }

    /// The recorded snapshots, oldest first.
    #[must_use]
    pub fn snapshots(&self) -> &[SnapEntry] {
        &self.snaps
    }

    /// Borrows the allocator (e.g. to inspect free-space accounting in tests).
    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    /// Looks up `key` as of snapshot `id`.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] for an unknown id, or device/verification errors.
    pub async fn get_in_snapshot(&mut self, id: u64, key: &K) -> Result<Option<V>, StorageError> {
        let root = self
            .snaps
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.root)
            .ok_or(StorageError::NotFound)?;
        if root.is_null() {
            return Ok(None);
        }
        Tree::<K, V>::at(root).get(key, &self.dev, &mut self.pool, self.hasher).await
    }

    /// Enumerates every block referenced by snapshot `id` (for replication,
    /// verification, or tests).
    ///
    /// # Errors
    /// [`StorageError::NotFound`] for an unknown id, or device/verification errors.
    pub async fn snapshot_blocks(&mut self, id: u64) -> Result<Vec<u64>, StorageError> {
        let root = self
            .snaps
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.root)
            .ok_or(StorageError::NotFound)?;
        let mut out = Vec::new();
        if !root.is_null() {
            Tree::<K, V>::at(root)
                .collect_blocks(&self.dev, &mut self.pool, self.hasher, &mut out)
                .await?;
        }
        Ok(out)
    }
}

/// Reconstructs the root pointer from a superblock (`None` if empty).
fn root_ptr(sb: &Superblock) -> Option<BlockPtr> {
    if sb.root_addr == 0 {
        None
    } else {
        Some(BlockPtr {
            addr: sb.root_addr,
            birth_txg: sb.root_birth_txg,
            checksum: sb.root_checksum,
        })
    }
}

/// Stamps a tree root (or the null root, when empty) into a superblock.
fn stamp_root(sb: &mut Superblock, root: Option<BlockPtr>) {
    if let Some(ptr) = root {
        sb.root_addr = ptr.addr;
        sb.root_birth_txg = ptr.birth_txg;
        sb.root_checksum = ptr.checksum;
    } else {
        sb.root_addr = 0;
        sb.root_birth_txg = 0;
        sb.root_checksum = [0u8; 32];
    }
}

#[cfg(test)]
mod tests {
    use super::Volume;
    use crate::allocator::{SegmentAllocator, SegmentGeom};
    use crate::checksum::DigestMode;
    use crate::testkit::{MemDevice, block_on};

    const BS: usize = 256;
    const BLOCKS: u64 = 16_384;

    fn fresh_alloc() -> SegmentAllocator {
        SegmentAllocator::new(SegmentGeom::new(BLOCKS, 64, 1).unwrap())
    }

    type Vol = Volume<u64, u64, SegmentAllocator, MemDevice>;

    #[test]
    fn insert_commit_reopen_roundtrip() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();

        // Enough inserts to build a multi-level tree.
        for k in 0..300u64 {
            block_on(vol.insert(k, k.wrapping_mul(7))).unwrap();
        }
        assert_eq!(vol.committed_txg(), 1, "uncommitted work does not advance txg");
        block_on(vol.commit()).unwrap();
        assert_eq!(vol.committed_txg(), 2);

        // Simulate unmount + remount from the persisted media.
        let media = vol.device().snapshot();
        let mut reopened: Vol = block_on(Volume::open(media, fresh_alloc())).unwrap();
        assert_eq!(reopened.committed_txg(), 2);
        for k in 0..300u64 {
            assert_eq!(
                block_on(reopened.get(&k)).unwrap(),
                Some(k.wrapping_mul(7)),
                "key {k} must survive commit + reopen"
            );
        }
        assert_eq!(block_on(reopened.get(&9999)).unwrap(), None);
    }

    #[test]
    fn remount_is_writable_without_corrupting_prior_data() {
        // Batch 1: commit keys 0..100 (txg 2).
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        for k in 0..100u64 {
            block_on(vol.insert(k, k + 1)).unwrap();
        }
        block_on(vol.commit()).unwrap();

        // Remount with a *fresh* allocator (mark-and-sweep rebuilds it), then
        // write batch 2 (keys 100..200) and commit. New allocations must not
        // overwrite batch-1 blocks.
        let media = vol.device().snapshot();
        let mut vol2: Vol = block_on(Volume::open(media, fresh_alloc())).unwrap();
        for k in 100..200u64 {
            block_on(vol2.insert(k, k + 1)).unwrap();
        }
        block_on(vol2.commit()).unwrap();
        assert_eq!(vol2.committed_txg(), 3);

        // Remount once more: both batches must be intact.
        let media2 = vol2.device().snapshot();
        let mut vol3: Vol = block_on(Volume::open(media2, fresh_alloc())).unwrap();
        for k in 0..200u64 {
            assert_eq!(
                block_on(vol3.get(&k)).unwrap(),
                Some(k + 1),
                "key {k} must survive write-after-remount"
            );
        }
    }

    #[test]
    fn snapshot_pins_state_against_later_overwrites() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        for k in 0..50u64 {
            block_on(vol.insert(k, k)).unwrap();
        }
        let s0 = block_on(vol.snapshot()).unwrap(); // captures k => k

        // Overwrite every key in the live tree.
        for k in 0..50u64 {
            block_on(vol.insert(k, k + 1000)).unwrap();
        }
        block_on(vol.commit()).unwrap();

        for k in 0..50u64 {
            assert_eq!(block_on(vol.get(&k)).unwrap(), Some(k + 1000), "live updated");
            assert_eq!(
                block_on(vol.get_in_snapshot(s0, &k)).unwrap(),
                Some(k),
                "snapshot pins the old value"
            );
        }
        assert!(matches!(block_on(vol.get_in_snapshot(404, &0)), Err(_)));
    }

    #[test]
    fn snapshot_blocks_survive_remount_marked_live() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        for k in 0..50u64 {
            block_on(vol.insert(k, k)).unwrap();
        }
        let s0 = block_on(vol.snapshot()).unwrap();
        for k in 0..50u64 {
            block_on(vol.insert(k, k + 1000)).unwrap();
        }
        block_on(vol.commit()).unwrap();

        // Remount with a fresh allocator: mark-and-sweep must walk the snapshot.
        let media = vol.device().snapshot();
        let mut vol2: Vol = block_on(Volume::open(media, fresh_alloc())).unwrap();

        // Every block the snapshot references must be marked allocated — proving
        // recovery walked the snapshot tree, not just the live tree.
        let blocks = block_on(vol2.snapshot_blocks(s0)).unwrap();
        assert!(!blocks.is_empty());
        for b in blocks {
            assert!(
                vol2.allocator().is_allocated(b),
                "snapshot block {b} must survive mark-and-sweep"
            );
        }

        // And it still reads its pinned values, while live keeps the new ones.
        for k in 0..50u64 {
            assert_eq!(block_on(vol2.get_in_snapshot(s0, &k)).unwrap(), Some(k));
            assert_eq!(block_on(vol2.get(&k)).unwrap(), Some(k + 1000));
        }
        assert_eq!(vol2.snapshots().len(), 1);
    }

    #[test]
    fn delete_snapshot_reclaims_only_its_unique_blocks() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        for k in 0..50u64 {
            block_on(vol.insert(k, k)).unwrap();
        }
        let s0 = block_on(vol.snapshot()).unwrap();
        for k in 0..50u64 {
            block_on(vol.insert(k, k + 1000)).unwrap();
        }
        block_on(vol.commit()).unwrap();

        // Blocks held by the snapshot but no longer by the live tree.
        let snap_blocks = block_on(vol.snapshot_blocks(s0)).unwrap();
        let live_blocks = block_on(vol.live_blocks()).unwrap();
        let unique: alloc::vec::Vec<u64> = snap_blocks
            .iter()
            .copied()
            .filter(|b| !live_blocks.contains(b))
            .collect();
        assert!(!unique.is_empty(), "overwrites should orphan snapshot-only blocks");
        // Pre-delete: snapshot-only blocks are still pinned (allocated).
        for b in &unique {
            assert!(vol.allocator().is_allocated(*b));
        }

        block_on(vol.delete_snapshot(s0)).unwrap();
        assert!(vol.snapshots().is_empty());
        assert!(matches!(block_on(vol.get_in_snapshot(s0, &0)), Err(_)));

        // Snapshot-only blocks reclaimed; live blocks retained.
        for b in &unique {
            assert!(!vol.allocator().is_allocated(*b), "block {b} should be reclaimed");
        }
        for b in &live_blocks {
            assert!(vol.allocator().is_allocated(*b), "live block {b} must remain");
        }
        for k in 0..50u64 {
            assert_eq!(block_on(vol.get(&k)).unwrap(), Some(k + 1000));
        }
    }

    #[test]
    fn delete_is_durable_and_snapshots_pin_deleted_keys() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        for k in 0..40u64 {
            block_on(vol.insert(k, k)).unwrap();
        }
        let s0 = block_on(vol.snapshot()).unwrap(); // pins 0..40

        // Delete the first 20 from the live tree, commit.
        for k in 0..20u64 {
            assert!(block_on(vol.delete(&k)).unwrap());
        }
        assert!(!block_on(vol.delete(&999)).unwrap());
        block_on(vol.commit()).unwrap();

        // Remount: deletions persisted, but the snapshot still has them.
        let media = vol.device().snapshot();
        let mut vol2: Vol = block_on(Volume::open(media, fresh_alloc())).unwrap();
        for k in 0..40u64 {
            let live = block_on(vol2.get(&k)).unwrap();
            if k < 20 {
                assert_eq!(live, None, "deleted {k} stays gone after remount");
            } else {
                assert_eq!(live, Some(k));
            }
            assert_eq!(
                block_on(vol2.get_in_snapshot(s0, &k)).unwrap(),
                Some(k),
                "snapshot pins deleted {k}"
            );
        }
    }

    #[test]
    fn txg_coalesces_repeated_writes() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        let before = vol.device().write_count();

        // 500 updates to the SAME key in one transaction.
        for i in 0..500u64 {
            block_on(vol.insert(7, i)).unwrap();
        }
        // In-memory only: no device writes yet.
        assert_eq!(vol.device().write_count(), before, "ops buffer in RAM");

        block_on(vol.commit()).unwrap();
        // Commit writes the single dirty leaf once + one superblock block — a
        // small constant, NOT ~500. (Without coalescing this would be hundreds.)
        let writes = vol.device().write_count() - before;
        assert!(writes <= 4, "expected coalesced commit, got {writes} writes");
        assert_eq!(block_on(vol.get(&7)).unwrap(), Some(499));
    }

    #[test]
    fn zil_recovers_synced_writes_but_not_unsynced() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();

        // A non-sync insert (only in the open txg) and a sync insert (also in the
        // ZIL), with NO commit — then simulate a crash by remounting the media.
        block_on(vol.insert(1, 10)).unwrap();
        block_on(vol.sync_insert(2, 20)).unwrap();

        let crashed = vol.device().snapshot();
        let mut recovered: Vol = block_on(Volume::open(crashed, fresh_alloc())).unwrap();
        assert_eq!(
            block_on(recovered.get(&2)).unwrap(),
            Some(20),
            "fsync'd write must survive the crash"
        );
        assert_eq!(
            block_on(recovered.get(&1)).unwrap(),
            None,
            "un-synced write may be lost"
        );

        // The replayed op is durable: committing then remounting keeps it.
        block_on(recovered.commit()).unwrap();
        let media2 = recovered.device().snapshot();
        let mut again: Vol = block_on(Volume::open(media2, fresh_alloc())).unwrap();
        assert_eq!(block_on(again.get(&2)).unwrap(), Some(20));
    }

    #[test]
    fn zil_ring_full_forces_commit_and_keeps_data() {
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        // More sync writes than the ring holds: forces commits along the way.
        for k in 0..100u64 {
            block_on(vol.sync_insert(k, k + 1)).unwrap();
        }
        let media = vol.device().snapshot();
        let mut recovered: Vol = block_on(Volume::open(media, fresh_alloc())).unwrap();
        for k in 0..100u64 {
            assert_eq!(block_on(recovered.get(&k)).unwrap(), Some(k + 1), "synced {k}");
        }
    }

    #[test]
    fn crash_during_commit_keeps_prior_committed_tree() {
        // One continuous allocator (no remount-before-write, per the module's
        // limitation). Commit txg 2 with key 1 durably...
        let dev = MemDevice::new(BS, BLOCKS);
        let mut vol: Vol =
            block_on(Volume::format(dev, fresh_alloc(), DigestMode::Fast64)).unwrap();
        block_on(vol.insert(1, 111)).unwrap();
        block_on(vol.commit()).unwrap();
        assert_eq!(vol.committed_txg(), 2);

        // ...then stage key 2 (CoW: new blocks, txg-2 tree untouched) and tear
        // the superblock write of the txg-3 commit (power cut at publish time).
        block_on(vol.insert(2, 222)).unwrap();
        vol.device().set_write_budget(Some(10));
        assert!(block_on(vol.commit()).is_err(), "torn superblock write must error");
        assert_eq!(vol.committed_txg(), 2, "failed commit does not advance txg");

        // Recover from the crashed media (read-only mount): prior committed state.
        let crashed = vol.device().snapshot();
        let mut recovered: Vol = block_on(Volume::open(crashed, fresh_alloc())).unwrap();
        assert_eq!(recovered.committed_txg(), 2, "rolled back to last good txg");
        assert_eq!(block_on(recovered.get(&1)).unwrap(), Some(111), "committed key survives");
        assert_eq!(
            block_on(recovered.get(&2)).unwrap(),
            None,
            "uncommitted key must not appear after crash"
        );
    }
}
