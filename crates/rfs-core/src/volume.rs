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

use crate::allocator::Allocator;
use crate::buffer::BufferPool;
use crate::checksum::DigestMode;
use crate::device::BlockDevice;
use crate::digest::Hasher;
use crate::error::StorageError;
use crate::superblock::{self, Superblock};
use crate::tree::{BlockPtr, Key, Record, Tree, Txn};

/// A mounted RFS volume parameterized by key/value record types and the
/// allocator/device implementations.
pub struct Volume<K, V, A, D> {
    dev: D,
    alloc: A,
    pool: BufferPool,
    hasher: Hasher,
    sb: Superblock,
    tree: Tree<K, V>,
}

impl<K: Key, V: Record, A: Allocator, D: BlockDevice> Volume<K, V, A, D> {
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
            tree: Tree::empty(),
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
        let tree = if sb.root_addr == 0 {
            Tree::empty()
        } else {
            Tree::at(BlockPtr {
                addr: sb.root_addr,
                birth_txg: sb.root_birth_txg,
                checksum: sb.root_checksum,
            })
        };

        // Mark-and-sweep: rebuild allocator free space from the live tree so the
        // volume is writable, never handing out a block the tree occupies.
        let mut live: Vec<u64> = Vec::new();
        tree.collect_blocks(&dev, &mut pool, hasher, &mut live).await?;
        for block in live {
            alloc.mark_live(block)?;
        }
        alloc.finish_rebuild();

        Ok(Self {
            dev,
            alloc,
            pool,
            hasher,
            sb,
            tree,
        })
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
        // Tree is Copy (just a root pointer), so operate on a local copy and
        // store it back — sidesteps borrowing several fields of `self` at once.
        let mut tree = self.tree;
        let mut txn = Txn {
            txg: self.sb.txg + 1,
            alloc: &mut self.alloc,
            dev: &self.dev,
            pool: &mut self.pool,
            hasher: self.hasher,
        };
        tree.insert(key, val, &mut txn).await?;
        self.tree = tree;
        Ok(())
    }

    /// Looks up `key` in the current (committed + open) tree.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn get(&mut self, key: &K) -> Result<Option<V>, StorageError> {
        self.tree.get(key, &self.dev, &mut self.pool, self.hasher).await
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
        let mut sb = self.sb;
        sb.txg += 1;
        if let Some(ptr) = self.tree.root {
            sb.root_addr = ptr.addr;
            sb.root_birth_txg = ptr.birth_txg;
            sb.root_checksum = ptr.checksum;
        } else {
            sb.root_addr = 0;
            sb.root_birth_txg = 0;
            sb.root_checksum = [0u8; 32];
        }

        // The durability barrier. Only on success do we adopt the new superblock
        // and let the allocator reuse blocks the old root no longer needs.
        superblock::commit(&self.dev, &sb).await?;
        self.sb = sb;
        self.alloc.commit();
        Ok(())
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
