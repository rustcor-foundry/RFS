#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! `rfs-core` is the hardware-agnostic engine for RFS.
//!
//! It knows nothing about `NVMe`, SD cards, FUSE, or the Feox exokernel. It only
//! knows how to talk to a [`BlockDevice`]. The kernel-facing adapter (and the
//! desktop FUSE testbed) live in separate crates and merely *implement* that
//! trait.
//!
//! # Milestone 1 scope
//!
//! This first slice deliberately contains no B-tree and no allocator. It
//! establishes the two things every later layer depends on:
//!
//! 1. the async [`BlockDevice`] seam, and
//! 2. a crash-safe [`superblock`] ring with atomic commit.
//!
//! The accompanying [`testkit`] (enabled under `cfg(test)` or the `testkit`
//! feature) provides an in-memory device that can inject torn writes, plus the
//! property test that proves a power cut mid-commit always recovers a
//! consistent superblock.

extern crate alloc;

pub mod allocator;
pub mod buffer;
pub mod checksum;
pub mod device;
pub mod digest;
pub mod error;
pub mod fs;
pub mod snapshot;
pub mod superblock;
pub mod tree;
pub mod txg;
pub mod volume;
pub mod zil;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

#[cfg(test)]
mod sim;

pub use allocator::{AllocError, Allocator, SegKind, SegmentAllocator, SegmentGeom};
pub use buffer::{AlignedBuf, BufferPool, RegionKey};
pub use checksum::DigestMode;
pub use device::{BlockDevice, Deallocate, PlacementHint, PlacementWrite, ZonedDevice};
pub use digest::{Digest, Fast64, Hasher, SimdLevel};
pub use error::{CorruptKind, StorageError};
pub use fs::{Filesystem, FsKey, FsValue, Inode};
pub use snapshot::SnapEntry;
pub use superblock::Superblock;
pub use tree::{BlockPtr, Internal, Key, Leaf, Node, Record, Tree, Txn, Value};
pub use volume::Volume;
