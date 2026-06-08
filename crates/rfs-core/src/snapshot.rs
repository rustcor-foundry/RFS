//! The snapshot directory.
//!
//! A snapshot is just a preserved tree root tagged with the txg it was taken at
//! (the ZFS model). The directory is a single self-checksummed block — its own
//! Fletcher-64 trailer rather than a parent pointer, since the superblock only
//! reserves an address (`snaplist_root`) for it. It is written copy-on-write
//! like everything else; the old block becomes unreferenced and is reclaimed on
//! the next mount.
//!
//! One block currently bounds the snapshot count (e.g. ~63 at 4 KiB); a chained
//! / B-tree directory is a later extension.

use alloc::vec::Vec;

use crate::allocator::{Allocator, SegKind};
use crate::buffer::{AlignedBuf, BufferPool};
use crate::checksum::fletcher64;
use crate::device::BlockDevice;
use crate::error::{CorruptKind, StorageError};
use crate::tree::{BlockPtr, MAX_CKSUM};

/// `b"RSNP"` little-endian — snapshot directory magic.
const MAGIC: u32 = 0x504E_5352;
const OFF_MAGIC: usize = 0;
const OFF_COUNT: usize = 4;
const HEADER_LEN: usize = 8;
/// id(8) + txg(8) + root.addr(8) + root.birth(8) + root.checksum(32).
const ENTRY_LEN: usize = 64;

/// One snapshot: an id, the txg it captured, and the root it pins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapEntry {
    /// Stable snapshot identifier.
    pub id: u64,
    /// Transaction group captured (blocks born ≤ this are pinned by it).
    pub txg: u64,
    /// The preserved tree root (null for a snapshot of an empty tree).
    pub root: BlockPtr,
}

/// Maximum snapshots that fit in one directory block.
#[must_use]
pub fn max_entries(block_size: usize) -> usize {
    (block_size - HEADER_LEN - 8) / ENTRY_LEN
}

fn encode(entries: &[SnapEntry], buf: &mut [u8]) -> Result<(), StorageError> {
    if entries.len() > max_entries(buf.len()) {
        return Err(StorageError::BufferSize);
    }
    buf.fill(0);
    buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    let count = u32::try_from(entries.len()).map_err(|_| StorageError::BufferSize)?;
    buf[OFF_COUNT..OFF_COUNT + 4].copy_from_slice(&count.to_le_bytes());
    for (i, e) in entries.iter().enumerate() {
        let o = HEADER_LEN + i * ENTRY_LEN;
        buf[o..o + 8].copy_from_slice(&e.id.to_le_bytes());
        buf[o + 8..o + 16].copy_from_slice(&e.txg.to_le_bytes());
        buf[o + 16..o + 24].copy_from_slice(&e.root.addr.to_le_bytes());
        buf[o + 24..o + 32].copy_from_slice(&e.root.birth_txg.to_le_bytes());
        buf[o + 32..o + 64].copy_from_slice(&e.root.checksum[..MAX_CKSUM]);
    }
    let len = buf.len();
    let sum = fletcher64(&buf[..len - 8]);
    buf[len - 8..].copy_from_slice(&sum.to_le_bytes());
    Ok(())
}

fn decode(buf: &[u8]) -> Result<Vec<SnapEntry>, StorageError> {
    let magic = u32::from_le_bytes(buf[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap());
    if magic != MAGIC {
        return Err(StorageError::Corrupt(CorruptKind::BadMagic));
    }
    let len = buf.len();
    let stored = u64::from_le_bytes(buf[len - 8..].try_into().unwrap());
    if fletcher64(&buf[..len - 8]) != stored {
        return Err(StorageError::Corrupt(CorruptKind::BadChecksum));
    }
    let count = usize::try_from(u32::from_le_bytes(
        buf[OFF_COUNT..OFF_COUNT + 4].try_into().unwrap(),
    ))
    .map_err(|_| StorageError::Corrupt(CorruptKind::BadNode))?;
    if count > max_entries(len) {
        return Err(StorageError::Corrupt(CorruptKind::BadNode));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = HEADER_LEN + i * ENTRY_LEN;
        let id = u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
        let txg = u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap());
        let addr = u64::from_le_bytes(buf[o + 16..o + 24].try_into().unwrap());
        let birth_txg = u64::from_le_bytes(buf[o + 24..o + 32].try_into().unwrap());
        let mut checksum = [0u8; MAX_CKSUM];
        checksum.copy_from_slice(&buf[o + 32..o + 64]);
        out.push(SnapEntry {
            id,
            txg,
            root: BlockPtr {
                addr,
                birth_txg,
                checksum,
            },
        });
    }
    Ok(out)
}

/// Writes the directory to a fresh block (copy-on-write) and returns its addr.
///
/// # Errors
/// Allocation, device, or capacity errors.
pub async fn write<A: Allocator, D: BlockDevice>(
    entries: &[SnapEntry],
    alloc: &mut A,
    dev: &D,
    pool: &mut BufferPool,
) -> Result<u64, StorageError> {
    let mut buf = pool.acquire();
    if let Err(err) = encode(entries, buf.as_mut_slice()) {
        pool.release(buf);
        return Err(err);
    }
    let addr = match alloc.alloc(SegKind::Meta) {
        Ok(addr) => addr,
        Err(err) => {
            pool.release(buf);
            return Err(err.into());
        }
    };
    let write = dev.write_block(addr, buf.as_slice()).await;
    pool.release(buf);
    write?;
    Ok(addr)
}

/// Reads and validates the directory at `addr`.
///
/// # Errors
/// Device errors, [`CorruptKind::BadMagic`], or [`CorruptKind::BadChecksum`].
pub async fn read<D: BlockDevice>(
    addr: u64,
    dev: &D,
    pool: &mut BufferPool,
) -> Result<Vec<SnapEntry>, StorageError> {
    let mut buf = pool.acquire();
    let outcome = read_into(addr, dev, &mut buf).await;
    pool.release(buf);
    outcome
}

async fn read_into<D: BlockDevice>(
    addr: u64,
    dev: &D,
    buf: &mut AlignedBuf,
) -> Result<Vec<SnapEntry>, StorageError> {
    dev.read_block(addr, buf.as_mut_slice()).await?;
    decode(buf.as_slice())
}
