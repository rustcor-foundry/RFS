//! Copy-on-write Merkle B-tree (M3).
//!
//! This increment freezes the on-disk shapes — [`BlockPtr`] and the [`Node`]
//! codec — and wires verify-on-read through the full stack ([`write_node`] /
//! [`read_node`]). Tree operations (`CoW` insert / split / lookup) build on top in
//! the next increment.
//!
//! Keys and values are fixed-size via the [`Record`] trait; variable-length
//! records (e.g. inline file tails) are a later extension.

mod node;
mod ptr;

pub use node::{Internal, Leaf, Node, max_internal_keys, max_leaf_entries, read_node, write_node};
pub use ptr::{BlockPtr, MAX_CKSUM, PTR_PREFIX};

/// A fixed-size, byte-serializable record (keys and values).
pub trait Record: Copy {
    /// On-disk size in bytes.
    const SIZE: usize;

    /// Serializes `self` into `out` (`out.len()` must be ≥ [`SIZE`](Record::SIZE)).
    fn write(&self, out: &mut [u8]);

    /// Parses from `buf` (`buf.len()` must be ≥ [`SIZE`](Record::SIZE)).
    fn read(buf: &[u8]) -> Self;
}

/// A key: an ordered record. The tree keeps keys sorted ascending.
pub trait Key: Record + Ord {}

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
