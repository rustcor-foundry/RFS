//! Transaction groups: in-memory write-back with coalescing.
//!
//! Without batching, each `insert` rewrites the whole root→leaf path to disk
//! immediately, so the root is written *once per operation* and all but the last
//! version is dead — heavy write amplification. A [`Txg`] instead builds an
//! in-memory shadow of only the modified nodes: operations fault nodes in from
//! disk on demand, mutate them in RAM, and split in RAM. Nothing is written
//! until [`serialize`](Txg::serialize), which walks the dirty shadow
//! bottom-up, assigning addresses and writing each dirty node **once**.
//!
//! Result: a node touched many times in one txg is written once; upper nodes are
//! written once per txg, not once per op. Superseded on-disk blocks are collected
//! in `freed` for the volume to release (birth-time gated) after the commit is
//! durable.
//!
//! Reads see the open transaction (in-memory changes over the on-disk base).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::allocator::Allocator;
use crate::buffer::BufferPool;
use crate::device::BlockDevice;
use crate::digest::Hasher;
use crate::error::StorageError;
use crate::tree::{
    BlockPtr, Internal, Key, Leaf, Node, Value, leaf_fits, leaf_split_index, max_internal_keys,
    read_node, write_node,
};

use core::future::Future;
use core::pin::Pin;

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, StorageError>> + 'a>>;

/// A child link: either still on disk, or a dirtied in-memory node.
enum Slot<K, V> {
    Disk(BlockPtr),
    Mem(Box<DNode<K, V>>),
}

/// A dirtied (in-memory) node.
enum DNode<K, V> {
    Leaf(Vec<(K, V)>),
    Internal {
        level: u8,
        keys: Vec<K>,
        kids: Vec<Slot<K, V>>,
    },
}

/// Read cursor that walks the in-memory shadow and falls through to disk.
enum Cursor<'a, K, V> {
    Slot(&'a Slot<K, V>),
    Disk(BlockPtr),
}

/// An open transaction group rooted at an optional node.
pub struct Txg<K, V> {
    root: Option<Slot<K, V>>,
    /// On-disk blocks superseded this txg (to free, birth-gated, after commit).
    freed: Vec<BlockPtr>,
}

impl<K: Key, V: Value> Txg<K, V> {
    /// Opens a transaction over an existing on-disk root (or none).
    #[must_use]
    pub fn begin(root: Option<BlockPtr>) -> Self {
        Self {
            root: root.map(Slot::Disk),
            freed: Vec::new(),
        }
    }

    /// Looks up `key` in the open transaction.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn get<D: BlockDevice>(
        &self,
        key: &K,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
    ) -> Result<Option<V>, StorageError> {
        let mut cur = match &self.root {
            None => return Ok(None),
            Some(slot) => Cursor::Slot(slot),
        };
        loop {
            match cur {
                Cursor::Slot(Slot::Mem(node)) => match &**node {
                    DNode::Leaf(entries) => return Ok(search(entries, key)),
                    DNode::Internal { keys, kids, .. } => {
                        cur = Cursor::Slot(&kids[child_index(keys, key)]);
                    }
                },
                Cursor::Slot(Slot::Disk(ptr)) => cur = Cursor::Disk(*ptr),
                Cursor::Disk(ptr) => match read_node::<K, V, D>(&ptr, dev, pool, hasher).await? {
                    Node::Leaf(leaf) => return Ok(search(&leaf.entries, key)),
                    Node::Internal(node) => {
                        cur = Cursor::Disk(node.children[child_index(&node.keys, key)]);
                    }
                },
            }
        }
    }

    /// Collects `[start, end)` from the open transaction (uncommitted changes
    /// over the on-disk base), ascending, into `out`.
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn range<D: BlockDevice>(
        &self,
        start: K,
        end: K,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
        out: &mut Vec<(K, V)>,
    ) -> Result<(), StorageError> {
        if let Some(slot) = &self.root {
            range_slot::<K, V, D>(slot, &start, &end, dev, pool, hasher, out).await?;
        }
        Ok(())
    }

    /// Inserts or updates `key => val` in the open transaction.
    ///
    /// # Errors
    /// Device or verification errors (from faulting nodes in).
    pub async fn insert<D: BlockDevice>(
        &mut self,
        key: K,
        val: V,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
    ) -> Result<(), StorageError> {
        let block_size = dev.block_size();
        let max_int = max_internal_keys::<K>(block_size, hasher.output_len());

        let mut root = self.root.take();
        match root {
            None => {
                root = Some(Slot::Mem(Box::new(DNode::Leaf(alloc::vec![(key, val)]))));
            }
            Some(mut slot) => {
                let split = insert_rec(
                    &mut slot, key, val, dev, pool, hasher, block_size, max_int, &mut self.freed,
                )
                .await?;
                root = Some(match split {
                    None => slot,
                    Some((sep, right)) => {
                        let level = level_of(&slot) + 1;
                        Slot::Mem(Box::new(DNode::Internal {
                            level,
                            keys: alloc::vec![sep],
                            kids: alloc::vec![slot, right],
                        }))
                    }
                });
            }
        }
        self.root = root;
        Ok(())
    }

    /// Removes `key`; returns whether it was present. Does not rebalance (an
    /// emptied leaf is retained, like the on-disk delete).
    ///
    /// # Errors
    /// Device or verification errors.
    pub async fn delete<D: BlockDevice>(
        &mut self,
        key: &K,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
    ) -> Result<bool, StorageError> {
        let mut root = self.root.take();
        let removed = match &mut root {
            None => false,
            Some(slot) => delete_rec(slot, key, dev, pool, hasher, &mut self.freed).await?,
        };
        self.root = root;
        Ok(removed)
    }

    /// Writes the dirty shadow to disk bottom-up (each dirty node once) and
    /// returns the new root pointer plus the on-disk blocks this txg superseded.
    ///
    /// # Errors
    /// Allocation, device, or encoding errors.
    pub async fn serialize<A: Allocator, D: BlockDevice>(
        self,
        txg: u64,
        alloc: &mut A,
        dev: &D,
        pool: &mut BufferPool,
        hasher: Hasher,
    ) -> Result<(Option<BlockPtr>, Vec<BlockPtr>), StorageError> {
        let Self { root, freed } = self;
        let new_root = match root {
            None => None,
            Some(slot) => Some(serialize_rec(slot, txg, alloc, dev, pool, hasher).await?),
        };
        Ok((new_root, freed))
    }
}

fn search<K: Key, V: Value>(entries: &[(K, V)], key: &K) -> Option<V> {
    entries
        .binary_search_by(|(k, _)| k.cmp(key))
        .ok()
        .map(|i| entries[i].1.clone())
}

fn child_index<K: Key>(keys: &[K], key: &K) -> usize {
    keys.partition_point(|sep| *sep <= *key)
}

fn level_of<K, V>(slot: &Slot<K, V>) -> u8 {
    match slot {
        Slot::Mem(node) => match &**node {
            DNode::Leaf(_) => 0,
            DNode::Internal { level, .. } => *level,
        },
        // Only called on the root after `insert_rec` has faulted it in.
        Slot::Disk(_) => 0,
    }
}

/// Converts a `Disk` slot into a dirtied `Mem` node, recording the old block.
fn fault_in<'f, K: Key, V: Value, D: BlockDevice>(
    slot: &'f mut Slot<K, V>,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
    freed: &'f mut Vec<BlockPtr>,
) -> Fut<'f, ()> {
    Box::pin(async move {
        let ptr = match slot {
            Slot::Disk(ptr) => *ptr,
            Slot::Mem(_) => return Ok(()),
        };
        let dnode = match read_node::<K, V, D>(&ptr, dev, pool, hasher).await? {
            Node::Leaf(leaf) => DNode::Leaf(leaf.entries),
            Node::Internal(node) => DNode::Internal {
                level: node.level,
                keys: node.keys,
                kids: node.children.into_iter().map(Slot::Disk).collect(),
            },
        };
        *slot = Slot::Mem(Box::new(dnode));
        freed.push(ptr);
        Ok(())
    })
}

/// Range traversal over a shadow slot (`Mem` in memory, `Disk` via the device).
fn range_slot<'f, K: Key, V: Value, D: BlockDevice>(
    slot: &'f Slot<K, V>,
    start: &'f K,
    end: &'f K,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
    out: &'f mut Vec<(K, V)>,
) -> Fut<'f, ()> {
    Box::pin(async move {
        match slot {
            Slot::Disk(ptr) => range_disk::<K, V, D>(*ptr, start, end, dev, pool, hasher, out).await,
            Slot::Mem(node) => match &**node {
                DNode::Leaf(entries) => {
                    for (k, v) in entries {
                        if *k >= *start && *k < *end {
                            out.push((*k, v.clone()));
                        }
                    }
                    Ok(())
                }
                DNode::Internal { keys, kids, .. } => {
                    let n = keys.len();
                    for i in 0..=n {
                        let below_end = i == 0 || keys[i - 1] < *end;
                        let above_start = i == n || *start < keys[i];
                        if below_end && above_start {
                            range_slot::<K, V, D>(
                                &kids[i],
                                start,
                                end,
                                dev,
                                &mut *pool,
                                hasher,
                                &mut *out,
                            )
                            .await?;
                        }
                    }
                    Ok(())
                }
            },
        }
    })
}

/// Range traversal over an on-disk subtree (clean part of the shadow).
fn range_disk<'f, K: Key, V: Value, D: BlockDevice>(
    ptr: BlockPtr,
    start: &'f K,
    end: &'f K,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
    out: &'f mut Vec<(K, V)>,
) -> Fut<'f, ()> {
    Box::pin(async move {
        match read_node::<K, V, D>(&ptr, dev, pool, hasher).await? {
            Node::Leaf(leaf) => {
                for (k, v) in leaf.entries {
                    if k >= *start && k < *end {
                        out.push((k, v));
                    }
                }
            }
            Node::Internal(node) => {
                let n = node.keys.len();
                for i in 0..=n {
                    let below_end = i == 0 || node.keys[i - 1] < *end;
                    let above_start = i == n || *start < node.keys[i];
                    if below_end && above_start {
                        range_disk::<K, V, D>(
                            node.children[i],
                            start,
                            end,
                            dev,
                            &mut *pool,
                            hasher,
                            &mut *out,
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    })
}

/// Recursive `CoW` insert over the shadow. Returns `Some((separator, right))` if
/// this node split.
#[allow(clippy::too_many_arguments)]
fn insert_rec<'f, K: Key, V: Value, D: BlockDevice>(
    slot: &'f mut Slot<K, V>,
    key: K,
    val: V,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
    block_size: usize,
    max_int: usize,
    freed: &'f mut Vec<BlockPtr>,
) -> Fut<'f, Option<(K, Slot<K, V>)>> {
    Box::pin(async move {
        fault_in(&mut *slot, dev, &mut *pool, hasher, &mut *freed).await?;
        let node = match slot {
            Slot::Mem(node) => &mut **node,
            Slot::Disk(_) => unreachable!("faulted in above"),
        };
        match node {
            DNode::Leaf(entries) => {
                match entries.binary_search_by(|(k, _)| k.cmp(&key)) {
                    Ok(i) => entries[i].1 = val,
                    Err(i) => entries.insert(i, (key, val)),
                }
                if !leaf_fits::<K, V>(entries, block_size) {
                    let mid = leaf_split_index::<K, V>(entries);
                    let right = entries.split_off(mid);
                    let sep = right[0].0;
                    return Ok(Some((sep, Slot::Mem(Box::new(DNode::Leaf(right))))));
                }
                Ok(None)
            }
            DNode::Internal { level, keys, kids } => {
                let ci = child_index(keys, &key);
                let split = insert_rec(
                    &mut kids[ci],
                    key,
                    val,
                    dev,
                    &mut *pool,
                    hasher,
                    block_size,
                    max_int,
                    &mut *freed,
                )
                .await?;
                if let Some((sep, right)) = split {
                    keys.insert(ci, sep);
                    kids.insert(ci + 1, right);
                    if keys.len() > max_int {
                        let mid = keys.len() / 2;
                        let promote = keys[mid];
                        let right_keys = keys.split_off(mid + 1);
                        keys.pop();
                        let right_kids = kids.split_off(mid + 1);
                        let right = Slot::Mem(Box::new(DNode::Internal {
                            level: *level,
                            keys: right_keys,
                            kids: right_kids,
                        }));
                        return Ok(Some((promote, right)));
                    }
                }
                Ok(None)
            }
        }
    })
}

/// Recursive `CoW` delete over the shadow (no rebalance).
fn delete_rec<'f, K: Key, V: Value, D: BlockDevice>(
    slot: &'f mut Slot<K, V>,
    key: &'f K,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
    freed: &'f mut Vec<BlockPtr>,
) -> Fut<'f, bool> {
    Box::pin(async move {
        fault_in(&mut *slot, dev, &mut *pool, hasher, &mut *freed).await?;
        let node = match slot {
            Slot::Mem(node) => &mut **node,
            Slot::Disk(_) => unreachable!("faulted in above"),
        };
        match node {
            DNode::Leaf(entries) => match entries.binary_search_by(|(k, _)| k.cmp(key)) {
                Ok(i) => {
                    entries.remove(i);
                    Ok(true)
                }
                Err(_) => Ok(false),
            },
            DNode::Internal { keys, kids, .. } => {
                let ci = child_index(keys, key);
                delete_rec(&mut kids[ci], key, dev, &mut *pool, hasher, &mut *freed).await
            }
        }
    })
}

/// Recursively serializes a slot, writing dirty nodes children-first.
fn serialize_rec<'f, K: Key + 'f, V: Value + 'f, A: Allocator, D: BlockDevice>(
    slot: Slot<K, V>,
    txg: u64,
    alloc: &'f mut A,
    dev: &'f D,
    pool: &'f mut BufferPool,
    hasher: Hasher,
) -> Fut<'f, BlockPtr> {
    Box::pin(async move {
        match slot {
            Slot::Disk(ptr) => Ok(ptr),
            Slot::Mem(node) => match *node {
                DNode::Leaf(entries) => {
                    let node = Node::Leaf(Leaf {
                        generation: txg,
                        entries,
                    });
                    write_node(&node, txg, alloc, dev, pool, hasher).await
                }
                DNode::Internal { level, keys, kids } => {
                    let mut children = Vec::with_capacity(kids.len());
                    for kid in kids {
                        children.push(
                            serialize_rec(kid, txg, &mut *alloc, dev, &mut *pool, hasher).await?,
                        );
                    }
                    let node = Node::<K, V>::Internal(Internal {
                        level,
                        generation: txg,
                        keys,
                        children,
                    });
                    write_node(&node, txg, alloc, dev, pool, hasher).await
                }
            },
        }
    })
}
