//! Copy-on-write Merkle B-tree.
//!
//! On-disk shapes: [`BlockPtr`] and the [`Node`] codec, with verify-on-read
//! wired through the full stack ([`write_node`] / [`read_node`]) and `CoW`
//! insert/delete/lookup ([`Tree`]).
//!
//! **Keys are fixed-size** ([`Record`]) so internal-node fan-out is simple;
//! **values are variable-length** ([`Value`], Btrfs "fixed key, variable item
//! data" model) so the VFS can store inodes, directory entries, and extents of
//! differing sizes in one tree. Leaves pack length-prefixed values and split by
//! bytes; internal nodes stay fixed-width.

mod btree;
mod node;
mod ptr;

pub use btree::{Tree, Txn};
pub use node::{
    Internal, Leaf, Node, leaf_fits, leaf_split_index, leaf_used_bytes, max_internal_keys,
    read_node, write_node,
};
pub use ptr::{BlockPtr, MAX_CKSUM, PTR_PREFIX};

/// A fixed-size, byte-serializable record — used for keys.
pub trait Record: Copy {
    /// On-disk size in bytes.
    const SIZE: usize;

    /// Serializes `self` into `out` (`out.len()` must be ≥ [`SIZE`](Record::SIZE)).
    fn write(&self, out: &mut [u8]);

    /// Parses from `buf` (`buf.len()` must be ≥ [`SIZE`](Record::SIZE)).
    fn read(buf: &[u8]) -> Self;
}

/// A key: an ordered fixed-size record. The tree keeps keys sorted ascending.
pub trait Key: Record + Ord {}

/// A variable-length, byte-serializable value stored in a leaf.
pub trait Value: Clone {
    /// On-disk length in bytes of this value.
    fn encoded_len(&self) -> usize;

    /// Serializes `self` into `out` (`out.len()` must be ≥ [`encoded_len`](Value::encoded_len)).
    fn encode(&self, out: &mut [u8]);

    /// Parses from `buf`, which is exactly the stored byte slice.
    fn decode(buf: &[u8]) -> Self;
}

impl Record for u64 {
    const SIZE: usize = 8;

    fn write(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.to_le_bytes());
    }

    fn read(buf: &[u8]) -> Self {
        Self::from_le_bytes(buf[..8].try_into().unwrap())
    }
}

impl Key for u64 {}

impl Value for u64 {
    fn encoded_len(&self) -> usize {
        8
    }

    fn encode(&self, out: &mut [u8]) {
        out[..8].copy_from_slice(&self.to_le_bytes());
    }

    fn decode(buf: &[u8]) -> Self {
        Self::from_le_bytes(buf[..8].try_into().unwrap())
    }
}
