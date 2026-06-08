//! The intent log (ZIL): low-latency durability for synchronous writes.
//!
//! A normal `commit` is the only thing that updates the tree on disk, but an
//! `fsync`-style write must be durable *now* without paying a full commit. The
//! ZIL records such operations in a small append-only log; on a crash they are
//! replayed on top of the last committed transaction, then folded into the next
//! normal commit. Consistency still comes from the superblock ring — the ZIL is
//! purely a latency shortcut.
//!
//! # Layout
//!
//! A fixed ring of [`ZIL_CAPACITY`] blocks living in the **reserved** region
//! (right after the superblock ring), so the allocator never touches it — no
//! chain pointers, no marking, no leaks. Record block `seq` lives at
//! `ZIL_START + seq % ZIL_CAPACITY` and is self-checksummed and tagged with
//! `(txg, seq)`. Each commit re-bases the log (records of an older `txg` are
//! ignored), and `seq` restarts at 0. Replay reads `seq = 0, 1, …` until a block
//! fails to validate (torn tail, stale `txg`, or sequence gap). When the ring
//! fills, the caller forces a commit to drain it.

use alloc::vec::Vec;

use crate::buffer::BufferPool;
use crate::checksum::fletcher64;
use crate::device::BlockDevice;
use crate::error::StorageError;
use crate::superblock::RING;
use crate::tree::{Key, Record};

/// First ZIL block (immediately after the superblock ring).
pub const ZIL_START: u64 = RING;
/// Number of blocks in the ZIL ring. The reserved region must cover
/// `ZIL_START + ZIL_CAPACITY` blocks.
pub const ZIL_CAPACITY: u64 = 32;

/// `b"RZIL"` little-endian.
const MAGIC: u32 = 0x4C49_5A52;
const OFF_MAGIC: usize = 0;
const OFF_COUNT: usize = 4;
const OFF_TXG: usize = 8;
const OFF_SEQ: usize = 16;
const HEADER_LEN: usize = 24;

const TAG_INSERT: u8 = 0;
const TAG_DELETE: u8 = 1;

/// A logged operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZilRecord<K, V> {
    /// Insert/update `key => val`.
    Insert(K, V),
    /// Remove `key`.
    Delete(K),
}

const fn slot_len<K: Record, V: Record>() -> usize {
    1 + K::SIZE + V::SIZE
}

/// Records that fit in one ZIL block.
#[must_use]
pub fn max_records<K: Record, V: Record>(block_size: usize) -> usize {
    (block_size - HEADER_LEN - 8) / slot_len::<K, V>()
}

const fn block_addr(seq: u64) -> u64 {
    ZIL_START + seq % ZIL_CAPACITY
}

fn encode<K: Key, V: Record>(
    records: &[ZilRecord<K, V>],
    txg: u64,
    seq: u64,
    buf: &mut [u8],
) -> Result<(), StorageError> {
    if records.len() > max_records::<K, V>(buf.len()) {
        return Err(StorageError::BufferSize);
    }
    buf.fill(0);
    buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    let count = u16::try_from(records.len()).map_err(|_| StorageError::BufferSize)?;
    buf[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&count.to_le_bytes());
    buf[OFF_TXG..OFF_TXG + 8].copy_from_slice(&txg.to_le_bytes());
    buf[OFF_SEQ..OFF_SEQ + 8].copy_from_slice(&seq.to_le_bytes());

    let stride = slot_len::<K, V>();
    for (i, rec) in records.iter().enumerate() {
        let o = HEADER_LEN + i * stride;
        match rec {
            ZilRecord::Insert(k, v) => {
                buf[o] = TAG_INSERT;
                k.write(&mut buf[o + 1..o + 1 + K::SIZE]);
                v.write(&mut buf[o + 1 + K::SIZE..o + stride]);
            }
            ZilRecord::Delete(k) => {
                buf[o] = TAG_DELETE;
                k.write(&mut buf[o + 1..o + 1 + K::SIZE]);
            }
        }
    }

    let len = buf.len();
    let sum = fletcher64(&buf[..len - 8]);
    buf[len - 8..].copy_from_slice(&sum.to_le_bytes());
    Ok(())
}

/// Returns the records of a valid block, or `None` for end-of-log (bad magic,
/// torn checksum, stale `txg`, or wrong `seq`).
fn decode<K: Key, V: Record>(buf: &[u8], expect_txg: u64, expect_seq: u64) -> Option<Vec<ZilRecord<K, V>>> {
    if u32::from_le_bytes(buf[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap()) != MAGIC {
        return None;
    }
    let len = buf.len();
    let stored = u64::from_le_bytes(buf[len - 8..].try_into().unwrap());
    if fletcher64(&buf[..len - 8]) != stored {
        return None;
    }
    if u64::from_le_bytes(buf[OFF_TXG..OFF_TXG + 8].try_into().unwrap()) != expect_txg {
        return None;
    }
    if u64::from_le_bytes(buf[OFF_SEQ..OFF_SEQ + 8].try_into().unwrap()) != expect_seq {
        return None;
    }
    let count = usize::from(u16::from_le_bytes(buf[OFF_COUNT..OFF_COUNT + 2].try_into().unwrap()));
    if count > max_records::<K, V>(len) {
        return None;
    }

    let stride = slot_len::<K, V>();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = HEADER_LEN + i * stride;
        let key = K::read(&buf[o + 1..o + 1 + K::SIZE]);
        match buf[o] {
            TAG_INSERT => out.push(ZilRecord::Insert(key, V::read(&buf[o + 1 + K::SIZE..o + stride]))),
            TAG_DELETE => out.push(ZilRecord::Delete(key)),
            _ => return None,
        }
    }
    Some(out)
}

/// Appends one record block at `seq` and flushes (the durability barrier).
///
/// # Errors
/// Device or capacity errors.
pub async fn append<K: Key, V: Record, D: BlockDevice>(
    records: &[ZilRecord<K, V>],
    txg: u64,
    seq: u64,
    dev: &D,
    pool: &mut BufferPool,
) -> Result<(), StorageError> {
    let mut buf = pool.acquire();
    if let Err(err) = encode(records, txg, seq, buf.as_mut_slice()) {
        pool.release(buf);
        return Err(err);
    }
    let write = dev.write_block(block_addr(seq), buf.as_slice()).await;
    pool.release(buf);
    write?;
    dev.flush().await
}

/// Reads the record block at `seq`, returning `None` at end-of-log.
///
/// # Errors
/// Device read errors.
pub async fn read<K: Key, V: Record, D: BlockDevice>(
    seq: u64,
    expect_txg: u64,
    dev: &D,
    pool: &mut BufferPool,
) -> Result<Option<Vec<ZilRecord<K, V>>>, StorageError> {
    let mut buf = pool.acquire();
    let read = dev.read_block(block_addr(seq), buf.as_mut_slice()).await;
    let outcome = match read {
        Ok(()) => Ok(decode::<K, V>(buf.as_slice(), expect_txg, seq)),
        Err(err) => Err(err),
    };
    pool.release(buf);
    outcome
}
