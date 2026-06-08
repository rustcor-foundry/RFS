//! Copy-on-write B+-tree operations.
//!
//! Lookup descends separator keys to a leaf. Insert is the classic `CoW` copy-up:
//! the target leaf is rewritten to *new* space ([`write_node`] never overwrites),
//! and the new [`BlockPtr`] bubbles up each ancestor — every one of which is
//! itself rewritten — producing a brand-new root pointer. Nodes split on the way
//! up when they would exceed a block; if the root splits, the tree grows a level.
//!
//! Because nothing is overwritten, the previous root remains a complete, valid
//! snapshot (MVCC): a reader holding the old pointer sees the old tree until it
//! chooses to advance. The new root is not yet *durable* — publishing it through
//! the superblock commit is the M4 transaction layer's job.
//!
//! The walk is iterative (descend collecting a path, rebuild bottom-up), which
//! sidesteps recursive `async` and keeps allocation to the path depth.

use alloc::vec::Vec;
use core::marker::PhantomData;

use super::node::{Internal, Leaf, Node, max_internal_keys, max_leaf_entries, read_node, write_node};
use super::ptr::BlockPtr;
use super::{Key, Record};
use crate::allocator::Allocator;
use crate::buffer::BufferPool;
use crate::device::BlockDevice;
use crate::digest::Hasher;
use crate::error::StorageError;

/// The mutable context a write threads through the tree: the open transaction's
/// `txg`, the allocator, the device, a buffer pool, and the hasher. The M4
/// transaction layer will own one of these per open txg.
pub struct Txn<'a, A, D> {
    /// Transaction group stamped onto every node written.
    pub txg: u64,
    /// Block allocator (single-writer owned).
    pub alloc: &'a mut A,
    /// Backing device.
    pub dev: &'a D,
    /// Recyclable aligned buffers.
    pub pool: &'a mut BufferPool,
    /// Selected digest.
    pub hasher: Hasher,
    /// Birth-time watermark for reclamation: a replaced block is freed only if
    /// its `birth_txg` is strictly greater than this (i.e. it was born after the
    /// most recent snapshot, so no snapshot pins it). `0` ⇒ free everything.
    pub keep_through_txg: u64,
}

/// A copy-on-write B+-tree rooted at an optional [`BlockPtr`].
///
/// `None` is the empty tree. The root pointer is the only state; everything else
/// lives on the device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tree<K, V> {
    /// Pointer to the root node, or `None` when empty.
    pub root: Option<BlockPtr>,
    marker: PhantomData<(K, V)>,
}

impl<K: Key, V: Record> Tree<K, V> {
    /// An empty tree.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            root: None,
            marker: PhantomData,
        }
    }

    /// Opens an existing tree at `root`.
    #[must_use]
    pub const fn at(root: BlockPtr) -> Self {
        Self {
            root: Some(root),
            marker: PhantomData,
        }
    }

    /// Looks up `key`, returning its value if present.
    ///
    /// # Errors
    /// Device or verification errors (a corrupt node yields `BadChecksum`).
    pub async fn get<D: BlockDevice>(
        &self,
        key: &K,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
    ) -> Result<Option<V>, StorageError> {
        let Some(mut current) = self.root else {
            return Ok(None);
        };
        loop {
            match read_node::<K, V, D>(&current, dev, pool, hasher).await? {
                Node::Leaf(leaf) => {
                    return Ok(leaf
                        .entries
                        .binary_search_by(|(k, _)| k.cmp(key))
                        .ok()
                        .map(|i| leaf.entries[i].1));
                }
                Node::Internal(node) => {
                    let idx = node.keys.partition_point(|sep| *sep <= *key);
                    current = node.children[idx];
                }
            }
        }
    }

    /// Appends every block reachable from the root to `out` (the mark phase of
    /// mount-time mark-and-sweep). Reads — and therefore checksum-verifies —
    /// every node, so a corrupt tree is detected at mount.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn collect_blocks<D: BlockDevice>(
        &self,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
        out: &mut Vec<u64>,
    ) -> Result<(), StorageError> {
        let Some(root) = self.root else {
            return Ok(());
        };
        let mut stack = alloc::vec![root];
        while let Some(ptr) = stack.pop() {
            out.push(ptr.addr);
            if let Node::Internal(node) = read_node::<K, V, D>(&ptr, dev, pool, hasher).await? {
                for child in &node.children {
                    stack.push(*child);
                }
            }
        }
        Ok(())
    }

    /// Inserts or updates `key => val`, copying every node on the root path and
    /// leaving the previous root intact (MVCC). Updates [`self.root`](Self::root)
    /// to the new root pointer.
    ///
    /// # Errors
    /// Allocation, device, encoding, or verification errors.
    pub async fn insert<A: Allocator, D: BlockDevice>(
        &mut self,
        key: K,
        val: V,
        txn: &mut Txn<'_, A, D>,
    ) -> Result<(), StorageError> {
        let txg = txn.txg;
        let dev = txn.dev;
        let hasher = txn.hasher;
        let block_size = dev.block_size();

        // Empty tree: a single fresh leaf becomes the root.
        let Some(root_ptr) = self.root else {
            let leaf = Node::Leaf(Leaf {
                generation: txg,
                entries: alloc::vec![(key, val)],
            });
            self.root = Some(put(&leaf, txn).await?);
            return Ok(());
        };

        // 1. Descend to the target leaf, recording the path and each visited
        //    node's *old* pointer (so its block can be freed once replaced).
        let keep = txn.keep_through_txg;
        let mut path: Vec<(Internal<K>, usize, BlockPtr)> = Vec::new();
        let mut current = root_ptr;
        let (mut leaf, leaf_old) = loop {
            let here = current;
            match read_node::<K, V, D>(&here, dev, &mut *txn.pool, hasher).await? {
                Node::Leaf(l) => break (l, here),
                Node::Internal(node) => {
                    let idx = node.keys.partition_point(|sep| *sep <= key);
                    current = node.children[idx];
                    path.push((node, idx, here));
                }
            }
        };

        // 2. Upsert into the leaf.
        match leaf.entries.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(i) => leaf.entries[i].1 = val,
            Err(i) => leaf.entries.insert(i, (key, val)),
        }
        leaf.generation = txg;

        // 3. Write the leaf, splitting if it overflows.
        let max_leaf = max_leaf_entries::<K, V>(block_size);
        let (mut child_ptr, mut split) = if leaf.entries.len() > max_leaf {
            let mid = leaf.entries.len() / 2;
            let right_entries = leaf.entries.split_off(mid);
            let separator = right_entries[0].0;
            let right = Node::Leaf(Leaf {
                generation: txg,
                entries: right_entries,
            });
            let left = Node::Leaf(leaf);
            let lptr = put(&left, txn).await?;
            let rptr = put(&right, txn).await?;
            (lptr, Some((separator, rptr)))
        } else {
            let ptr = put(&Node::Leaf(leaf), txn).await?;
            (ptr, None)
        };
        // The old leaf block is now superseded; free it unless a snapshot pins it.
        if leaf_old.birth_txg > keep {
            txn.alloc.free(leaf_old.addr)?;
        }

        // 4. Rebuild ancestors bottom-up, propagating splits.
        let max_int = max_internal_keys::<K>(block_size, hasher.output_len());
        let mut child_level: u8 = 0;
        while let Some((mut parent, idx, parent_old)) = path.pop() {
            let plevel = parent.level;
            parent.children[idx] = child_ptr;
            if let Some((separator, rptr)) = split.take() {
                parent.keys.insert(idx, separator);
                parent.children.insert(idx + 1, rptr);
            }
            parent.generation = txg;

            if parent.keys.len() > max_int {
                let mid = parent.keys.len() / 2;
                let promote = parent.keys[mid];
                let right_keys = parent.keys.split_off(mid + 1);
                parent.keys.pop(); // remove the promoted separator
                let right_children = parent.children.split_off(mid + 1);
                let right: Node<K, V> = Node::Internal(Internal {
                    level: plevel,
                    generation: txg,
                    keys: right_keys,
                    children: right_children,
                });
                let lptr = put(&Node::<K, V>::Internal(parent), txn).await?;
                let rptr = put(&right, txn).await?;
                child_ptr = lptr;
                split = Some((promote, rptr));
            } else {
                child_ptr = put(&Node::<K, V>::Internal(parent), txn).await?;
            }
            if parent_old.birth_txg > keep {
                txn.alloc.free(parent_old.addr)?;
            }
            child_level = plevel;
        }

        // 5. If the root split, grow a new level above the two halves.
        self.root = Some(if let Some((separator, rptr)) = split {
            let new_root: Node<K, V> = Node::Internal(Internal {
                level: child_level + 1,
                generation: txg,
                keys: alloc::vec![separator],
                children: alloc::vec![child_ptr, rptr],
            });
            put(&new_root, txn).await?
        } else {
            child_ptr
        });
        Ok(())
    }
}

/// Writes a node through the transaction: copy-on-write (new block, never
/// overwrite), stamped with the txn's `txg`.
async fn put<K: Key, V: Record, A: Allocator, D: BlockDevice>(
    node: &Node<K, V>,
    txn: &mut Txn<'_, A, D>,
) -> Result<BlockPtr, StorageError> {
    write_node(
        node,
        txn.txg,
        &mut *txn.alloc,
        txn.dev,
        &mut *txn.pool,
        txn.hasher,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{Node, Tree, Txn};
    use crate::allocator::{SegmentAllocator, SegmentGeom};
    use crate::buffer::BufferPool;
    use crate::checksum::DigestMode;
    use crate::digest::Hasher;
    use crate::testkit::{MemDevice, block_on};
    use crate::tree::node::read_node;

    // Small blocks so splits happen quickly: leaf cap 15, internal cap 6.
    const BS: usize = 256;

    struct Env {
        dev: MemDevice,
        alloc: SegmentAllocator,
        pool: BufferPool,
        hasher: Hasher,
    }

    fn env() -> Env {
        Env {
            dev: MemDevice::new(BS, 8192),
            alloc: SegmentAllocator::new(SegmentGeom::new(8192, 64, 1).unwrap()),
            pool: BufferPool::for_block_size(BS),
            hasher: Hasher::new(DigestMode::Fast64).unwrap(),
        }
    }

    fn txn<'a>(e: &'a mut Env, txg: u64) -> Txn<'a, SegmentAllocator, MemDevice> {
        Txn {
            txg,
            alloc: &mut e.alloc,
            dev: &e.dev,
            pool: &mut e.pool,
            hasher: e.hasher,
            keep_through_txg: 0,
        }
    }

    #[test]
    fn insert_then_get() {
        let mut e = env();
        let mut tree = Tree::<u64, u64>::empty();
        block_on(tree.insert(42, 4242, &mut txn(&mut e, 1))).unwrap();
        let got = block_on(tree.get(&42, &e.dev, &mut e.pool, e.hasher)).unwrap();
        assert_eq!(got, Some(4242));
        let missing = block_on(tree.get(&7, &e.dev, &mut e.pool, e.hasher)).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn upsert_overwrites() {
        let mut e = env();
        let mut tree = Tree::<u64, u64>::empty();
        block_on(tree.insert(1, 100, &mut txn(&mut e, 1))).unwrap();
        block_on(tree.insert(1, 999, &mut txn(&mut e, 2))).unwrap();
        assert_eq!(
            block_on(tree.get(&1, &e.dev, &mut e.pool, e.hasher)).unwrap(),
            Some(999)
        );
    }

    #[test]
    fn many_inserts_force_splits_and_grow_height() {
        let mut e = env();
        let mut tree = Tree::<u64, u64>::empty();
        // Interleave order so it isn't purely sequential appends.
        let keys: alloc::vec::Vec<u64> = (0..200u64).map(|i| (i * 73) % 200).collect();
        for (txg, &k) in keys.iter().enumerate() {
            block_on(tree.insert(k, k * 10, &mut txn(&mut e, txg as u64 + 1))).unwrap();
        }
        // Every key retrievable.
        for k in 0..200u64 {
            assert_eq!(
                block_on(tree.get(&k, &e.dev, &mut e.pool, e.hasher)).unwrap(),
                Some(k * 10),
                "missing key {k}"
            );
        }
        // Absent keys.
        assert_eq!(
            block_on(tree.get(&500, &e.dev, &mut e.pool, e.hasher)).unwrap(),
            None
        );
        // The tree grew past a single leaf.
        let root = block_on(read_node::<u64, u64, _>(
            &tree.root.unwrap(),
            &e.dev,
            &mut e.pool,
            e.hasher,
        ))
        .unwrap();
        assert!(
            matches!(root, Node::Internal(_)),
            "root should be internal after 200 inserts"
        );
    }

    #[test]
    fn cow_preserves_old_root_as_snapshot() {
        let mut e = env();
        let mut tree = Tree::<u64, u64>::empty();
        block_on(tree.insert(1, 11, &mut txn(&mut e, 1))).unwrap();
        block_on(tree.insert(2, 22, &mut txn(&mut e, 2))).unwrap();
        let old_root = tree.root.unwrap();

        // A third insert publishes a new root; the old one must be untouched.
        block_on(tree.insert(3, 33, &mut txn(&mut e, 3))).unwrap();
        assert_ne!(tree.root.unwrap(), old_root, "insert must produce a new root");

        let snapshot = Tree::<u64, u64>::at(old_root);
        assert_eq!(
            block_on(snapshot.get(&3, &e.dev, &mut e.pool, e.hasher)).unwrap(),
            None,
            "old snapshot must not see the later insert"
        );
        assert_eq!(
            block_on(snapshot.get(&1, &e.dev, &mut e.pool, e.hasher)).unwrap(),
            Some(11)
        );
    }
}
