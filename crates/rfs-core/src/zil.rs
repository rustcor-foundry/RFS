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
use crate::tree::{Key, Record, Value};

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

/// A logged operation. Records are variable-length on disk (the value is
/// length-prefixed), matching the tree's variable values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ZilRecord<K, V> {
    /// Insert/update `key => val`.
    Insert(K, V),
    /// Remove `key`.
    Delete(K),
}

/// On-disk length of one record: `tag(1) + key + vlen(2) + value`.
fn record_len<K: Record, V: Value>(rec: &ZilRecord<K, V>) -> usize {
    let vlen = match rec {
        ZilRecord::Insert(_, v) => v.encoded_len(),
        ZilRecord::Delete(_) => 0,
    };
    1 + K::SIZE + 2 + vlen
}

const fn block_addr(seq: u64) -> u64 {
    ZIL_START + seq % ZIL_CAPACITY
}

fn encode<K: Key, V: Value>(
    records: &[ZilRecord<K, V>],
    txg: u64,
    seq: u64,
    buf: &mut [u8],
) -> Result<(), StorageError> {
    let used: usize = HEADER_LEN + records.iter().map(record_len::<K, V>).sum::<usize>() + 8;
    if used > buf.len() {
        return Err(StorageError::BufferSize);
    }
    buf.fill(0);
    buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC.to_le_bytes());
    let count = u16::try_from(records.len()).map_err(|_| StorageError::BufferSize)?;
    buf[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&count.to_le_bytes());
    buf[OFF_TXG..OFF_TXG + 8].copy_from_slice(&txg.to_le_bytes());
    buf[OFF_SEQ..OFF_SEQ + 8].copy_from_slice(&seq.to_le_bytes());

    let mut o = HEADER_LEN;
    for rec in records {
        let (tag, key, vlen) = match rec {
            ZilRecord::Insert(k, v) => (TAG_INSERT, k, v.encoded_len()),
            ZilRecord::Delete(k) => (TAG_DELETE, k, 0),
        };
        buf[o] = tag;
        key.write(&mut buf[o + 1..o + 1 + K::SIZE]);
        let vlen16 = u16::try_from(vlen).map_err(|_| StorageError::BufferSize)?;
        buf[o + 1 + K::SIZE..o + 3 + K::SIZE].copy_from_slice(&vlen16.to_le_bytes());
        if let ZilRecord::Insert(_, v) = rec {
            v.encode(&mut buf[o + 3 + K::SIZE..o + 3 + K::SIZE + vlen]);
        }
        o += 3 + K::SIZE + vlen;
    }

    let len = buf.len();
    let sum = fletcher64(&buf[..len - 8]);
    buf[len - 8..].copy_from_slice(&sum.to_le_bytes());
    Ok(())
}

/// Returns the records of a valid block, or `None` for end-of-log (bad magic,
/// torn checksum, stale `txg`, wrong `seq`, or a malformed/overrunning record).
fn decode<K: Key, V: Value>(
    buf: &[u8],
    expect_txg: u64,
    expect_seq: u64,
) -> Option<Vec<ZilRecord<K, V>>> {
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
    let count = usize::from(u16::from_le_bytes(
        buf[OFF_COUNT..OFF_COUNT + 2].try_into().unwrap(),
    ));

    let payload_end = len - 8;
    let mut o = HEADER_LEN;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if o + 3 + K::SIZE > payload_end {
            return None;
        }
        let tag = buf[o];
        let key = K::read(&buf[o + 1..o + 1 + K::SIZE]);
        let vlen = usize::from(u16::from_le_bytes(
            buf[o + 1 + K::SIZE..o + 3 + K::SIZE].try_into().unwrap(),
        ));
        let vstart = o + 3 + K::SIZE;
        if vstart + vlen > payload_end {
            return None;
        }
        match tag {
            TAG_INSERT => out.push(ZilRecord::Insert(
                key,
                V::decode(&buf[vstart..vstart + vlen]),
            )),
            TAG_DELETE => out.push(ZilRecord::Delete(key)),
            _ => return None,
        }
        o = vstart + vlen;
    }
    Some(out)
}

/// Appends one record block at `seq` and flushes (the durability barrier).
///
/// # Errors
/// Device or capacity errors.
pub async fn append<K: Key, V: Value, D: BlockDevice>(
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
pub async fn read<K: Key, V: Value, D: BlockDevice>(
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
