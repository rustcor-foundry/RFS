//! Tree node on-disk codec and verified persistence.
//!
//! Layout is B+-tree style: a **leaf** (level 0) holds sorted `(key, value)`
//! entries; an **internal** node (level > 0) holds `n` separator keys and `n + 1`
//! child [`BlockPtr`]s. One node occupies exactly one device block; the unused
//! tail is zeroed so the whole-block checksum is deterministic.
//!
//! Every node is checksummed over its *entire block*; that checksum lives in the
//! parent's pointer (the root's in the superblock). [`read_node`] recomputes and
//! compares before trusting any bytes — the ZFS self-healing pattern.

use alloc::vec::Vec;

use super::ptr::{BlockPtr, MAX_CKSUM};
use super::{Key, Record, Value};
use crate::allocator::{Allocator, SegKind};
use crate::buffer::BufferPool;
use crate::device::BlockDevice;
use crate::digest::Hasher;
use crate::error::{CorruptKind, StorageError};

/// `b"RFND"` — tree node magic.
const NODE_MAGIC: u32 = 0x5246_4E44;

// Header: magic(4) level(1) flags(1) key_count(2) generation(8).
const OFF_MAGIC: usize = 0;
const OFF_LEVEL: usize = 4;
const OFF_COUNT: usize = 6;
const OFF_GENERATION: usize = 8;
const HEADER_LEN: usize = 16;

/// Bytes a leaf occupies on disk: the header plus, per entry, the fixed key, a
/// 2-byte length, and the variable value. Values are length-prefixed, so leaves
/// hold variable-size items (Btrfs "fixed key, variable data" model).
#[must_use]
pub fn leaf_used_bytes<K: Record, V: Value>(entries: &[(K, V)]) -> usize {
    HEADER_LEN
        + entries
            .iter()
            .map(|(_, v)| K::SIZE + 2 + v.encoded_len())
            .sum::<usize>()
}

/// Whether `entries` fit in a single leaf block.
#[must_use]
pub fn leaf_fits<K: Record, V: Value>(entries: &[(K, V)], block_size: usize) -> bool {
    leaf_used_bytes::<K, V>(entries) <= block_size
}

/// Byte-balanced split point (start index of the right half) for an overfull
/// leaf. Returns `1..entries.len()` when there are ≥ 2 entries.
#[must_use]
pub fn leaf_split_index<K: Record, V: Value>(entries: &[(K, V)]) -> usize {
    let total: usize = entries
        .iter()
        .map(|(_, v)| K::SIZE + 2 + v.encoded_len())
        .sum();
    let mut acc = 0usize;
    for (i, (_, v)) in entries.iter().enumerate() {
        acc += K::SIZE + 2 + v.encoded_len();
        if acc * 2 >= total && i + 1 < entries.len() {
            return i + 1;
        }
    }
    entries.len() / 2
}

/// Maximum separator keys that fit in an internal node (it also holds `n + 1`
/// pointers of `digest_len`-dependent width).
#[must_use]
pub fn max_internal_keys<K: Record>(block_size: usize, digest_len: usize) -> usize {
    let ptr = BlockPtr::encoded_len(digest_len);
    (block_size - HEADER_LEN - ptr) / (K::SIZE + ptr)
}

/// A leaf node: sorted `(key, value)` entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Leaf<K, V> {
    /// Transaction group that produced this node.
    pub generation: u64,
    /// Sorted ascending by key.
    pub entries: Vec<(K, V)>,
}

/// An internal node: `n` separator keys and `n + 1` child pointers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Internal<K> {
    /// Tree level (> 0).
    pub level: u8,
    /// Transaction group that produced this node.
    pub generation: u64,
    /// `n` separators, sorted ascending.
    pub keys: Vec<K>,
    /// `n + 1` children.
    pub children: Vec<BlockPtr>,
}

/// A decoded node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Node<K, V> {
    /// Level-0 leaf.
    Leaf(Leaf<K, V>),
    /// Internal node (level > 0).
    Internal(Internal<K>),
}

impl<K: Key, V: Value> Node<K, V> {
    /// Tree level (0 for a leaf).
    #[must_use]
    pub fn level(&self) -> u8 {
        match self {
            Self::Leaf(_) => 0,
            Self::Internal(n) => n.level,
        }
    }

    /// Serializes into a full block buffer, zeroing the unused tail.
    ///
    /// # Errors
    /// [`StorageError::BufferSize`] if the node does not fit, or
    /// [`CorruptKind::BadNode`] for a structurally invalid in-memory node
    /// (internal child count ≠ key count + 1, or a leaf used at level 0 only).
    pub fn encode(&self, buf: &mut [u8], digest_len: usize) -> Result<(), StorageError> {
        buf.fill(0);
        buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&NODE_MAGIC.to_le_bytes());

        match self {
            Self::Leaf(leaf) => {
                if !leaf_fits::<K, V>(&leaf.entries, buf.len()) {
                    return Err(StorageError::BufferSize);
                }
                let count = u16::try_from(leaf.entries.len())
                    .map_err(|_| StorageError::BufferSize)?;
                buf[OFF_LEVEL] = 0;
                buf[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&count.to_le_bytes());
                buf[OFF_GENERATION..OFF_GENERATION + 8]
                    .copy_from_slice(&leaf.generation.to_le_bytes());
                // Fixed key + 2-byte length per item in a header array, then the
                // variable value bytes packed in order after the headers.
                let mut hdr = HEADER_LEN;
                let mut data = HEADER_LEN + leaf.entries.len() * (K::SIZE + 2);
                for (k, v) in &leaf.entries {
                    let vlen = v.encoded_len();
                    let vlen16 = u16::try_from(vlen).map_err(|_| StorageError::BufferSize)?;
                    k.write(&mut buf[hdr..hdr + K::SIZE]);
                    buf[hdr + K::SIZE..hdr + K::SIZE + 2].copy_from_slice(&vlen16.to_le_bytes());
                    v.encode(&mut buf[data..data + vlen]);
                    hdr += K::SIZE + 2;
                    data += vlen;
                }
            }
            Self::Internal(node) => {
                if node.level == 0 {
                    return Err(StorageError::Corrupt(CorruptKind::BadNode));
                }
                if node.children.len() != node.keys.len() + 1 {
                    return Err(StorageError::Corrupt(CorruptKind::BadNode));
                }
                if node.keys.len() > max_internal_keys::<K>(buf.len(), digest_len) {
                    return Err(StorageError::BufferSize);
                }
                let count =
                    u16::try_from(node.keys.len()).map_err(|_| StorageError::BufferSize)?;
                buf[OFF_LEVEL] = node.level;
                buf[OFF_COUNT..OFF_COUNT + 2].copy_from_slice(&count.to_le_bytes());
                buf[OFF_GENERATION..OFF_GENERATION + 8]
                    .copy_from_slice(&node.generation.to_le_bytes());

                let ptr_len = BlockPtr::encoded_len(digest_len);
                let keys_bytes = node.keys.len() * K::SIZE;
                for (i, k) in node.keys.iter().enumerate() {
                    let off = HEADER_LEN + i * K::SIZE;
                    k.write(&mut buf[off..off + K::SIZE]);
                }
                for (j, child) in node.children.iter().enumerate() {
                    let off = HEADER_LEN + keys_bytes + j * ptr_len;
                    child.encode(&mut buf[off..off + ptr_len], digest_len);
                }
            }
        }
        Ok(())
    }

    /// Parses a node from a full block buffer.
    ///
    /// Counts read from the (untrusted) buffer are bounds-checked against block
    /// capacity before any payload is read, so a corrupt header cannot cause an
    /// out-of-bounds read.
    ///
    /// # Errors
    /// [`CorruptKind::BadMagic`] or [`CorruptKind::BadNode`].
    ///
    /// # Panics
    /// If `buf` is smaller than one block's header, or `digest_len` exceeds
    /// [`MAX_CKSUM`] — both caller invariants (`buf` is a full device block).
    pub fn decode(buf: &[u8], digest_len: usize) -> Result<Self, StorageError> {
        let magic = u32::from_le_bytes(buf[OFF_MAGIC..OFF_MAGIC + 4].try_into().unwrap());
        if magic != NODE_MAGIC {
            return Err(StorageError::Corrupt(CorruptKind::BadMagic));
        }
        let level = buf[OFF_LEVEL];
        let count =
            usize::from(u16::from_le_bytes(buf[OFF_COUNT..OFF_COUNT + 2].try_into().unwrap()));
        let generation =
            u64::from_le_bytes(buf[OFF_GENERATION..OFF_GENERATION + 8].try_into().unwrap());

        if level == 0 {
            // Bounds-check the header array, then read each length-prefixed value.
            let bad = StorageError::Corrupt(CorruptKind::BadNode);
            let headers_end = HEADER_LEN + count * (K::SIZE + 2);
            if headers_end > buf.len() {
                return Err(bad);
            }
            let mut entries = Vec::with_capacity(count);
            let mut hdr = HEADER_LEN;
            let mut data = headers_end;
            for _ in 0..count {
                let k = K::read(&buf[hdr..hdr + K::SIZE]);
                let vlen = usize::from(u16::from_le_bytes(
                    buf[hdr + K::SIZE..hdr + K::SIZE + 2].try_into().unwrap(),
                ));
                let end = data.checked_add(vlen).ok_or(bad)?;
                if end > buf.len() {
                    return Err(bad);
                }
                entries.push((k, V::decode(&buf[data..end])));
                hdr += K::SIZE + 2;
                data = end;
            }
            Ok(Self::Leaf(Leaf {
                generation,
                entries,
            }))
        } else {
            if count > max_internal_keys::<K>(buf.len(), digest_len) {
                return Err(StorageError::Corrupt(CorruptKind::BadNode));
            }
            let ptr_len = BlockPtr::encoded_len(digest_len);
            let keys_bytes = count * K::SIZE;
            let mut keys = Vec::with_capacity(count);
            for i in 0..count {
                let off = HEADER_LEN + i * K::SIZE;
                keys.push(K::read(&buf[off..off + K::SIZE]));
            }
            let mut children = Vec::with_capacity(count + 1);
            for j in 0..=count {
                let off = HEADER_LEN + keys_bytes + j * ptr_len;
                children.push(BlockPtr::decode(&buf[off..off + ptr_len], digest_len));
            }
            Ok(Self::Internal(Internal {
                level,
                generation,
                keys,
                children,
            }))
        }
    }
}

/// Writes `node` to a freshly allocated block and returns a pointer to it,
/// stamped with `txg` and the node's checksum.
///
/// This is the `CoW` write primitive: it never overwrites; it allocates new space
/// (leaves → cold `Data` log, internal nodes → hot `Meta` log, the F2FS
/// isolation), writes, and hands back a pointer the caller bubbles up.
///
/// # Errors
/// Allocation, device, or encoding errors.
pub async fn write_node<K, V, A, D>(
    node: &Node<K, V>,
    txg: u64,
    alloc: &mut A,
    dev: &D,
    pool: &mut BufferPool,
    hasher: Hasher,
) -> Result<BlockPtr, StorageError>
where
    K: Key,
    V: Value,
    A: Allocator,
    D: BlockDevice,
{
    let mut buf = pool.acquire();
    let result = encode_and_address(node, txg, alloc, hasher, buf.as_mut_slice());
    let ptr = match result {
        Ok(ptr) => ptr,
        Err(err) => {
            pool.release(buf);
            return Err(err);
        }
    };
    let write = dev.write_block(ptr.addr, buf.as_slice()).await;
    pool.release(buf);
    write?;
    Ok(ptr)
}

/// Encodes into `buf`, hashes it, allocates a block, and builds the pointer.
/// Split out so the buffer can be released on every path.
fn encode_and_address<K, V, A>(
    node: &Node<K, V>,
    txg: u64,
    alloc: &mut A,
    hasher: Hasher,
    buf: &mut [u8],
) -> Result<BlockPtr, StorageError>
where
    K: Key,
    V: Value,
    A: Allocator,
{
    node.encode(buf, hasher.output_len())?;
    let mut checksum = [0u8; MAX_CKSUM];
    hasher.hash(buf, &mut checksum);
    let kind = match node {
        Node::Leaf(_) => SegKind::Data,
        Node::Internal(_) => SegKind::Meta,
    };
    let addr = alloc.alloc(kind)?;
    Ok(BlockPtr {
        addr,
        birth_txg: txg,
        checksum,
    })
}

/// Reads the block `ptr` references, verifies its checksum against `ptr`, and
/// decodes it. Returns [`CorruptKind::BadChecksum`] on a mismatch — the
/// self-healing trigger (a caller with a replica can then fetch the good copy).
///
/// # Errors
/// Device errors, [`CorruptKind::BadChecksum`], or decode errors.
pub async fn read_node<K, V, D>(
    ptr: &BlockPtr,
    dev: &D,
    pool: &mut BufferPool,
    hasher: Hasher,
) -> Result<Node<K, V>, StorageError>
where
    K: Key,
    V: Value,
    D: BlockDevice,
{
    let mut buf = pool.acquire();
    let outcome = read_verify_decode::<K, V, D>(ptr, dev, hasher, &mut buf).await;
    pool.release(buf);
    outcome
}

async fn read_verify_decode<K, V, D>(
    ptr: &BlockPtr,
    dev: &D,
    hasher: Hasher,
    buf: &mut crate::buffer::AlignedBuf,
) -> Result<Node<K, V>, StorageError>
where
    K: Key,
    V: Value,
    D: BlockDevice,
{
    dev.read_block(ptr.addr, buf.as_mut_slice()).await?;
    let mut got = [0u8; MAX_CKSUM];
    hasher.hash(buf.as_slice(), &mut got);
    let len = hasher.output_len();
    if got[..len] != ptr.checksum[..len] {
        return Err(StorageError::Corrupt(CorruptKind::BadChecksum));
    }
    Node::decode(buf.as_slice(), len)
}

#[cfg(test)]
mod tests {
    use super::{Internal, Leaf, Node, leaf_fits, max_internal_keys, read_node, write_node};
    use crate::allocator::{SegmentAllocator, SegmentGeom};
    use crate::buffer::BufferPool;
    use crate::checksum::DigestMode;
    use crate::device::BlockDevice;
    use crate::digest::Hasher;
    use crate::error::{CorruptKind, StorageError};
    use crate::testkit::{MemDevice, block_on};
    use crate::tree::ptr::BlockPtr;

    const BS: usize = 4096;

    fn leaf(entries: &[(u64, u64)]) -> Node<u64, u64> {
        Node::Leaf(Leaf {
            generation: 5,
            entries: entries.to_vec(),
        })
    }

    fn internal(keys: &[u64], children: &[BlockPtr]) -> Node<u64, u64> {
        Node::Internal(Internal {
            level: 1,
            generation: 5,
            keys: keys.to_vec(),
            children: children.to_vec(),
        })
    }

    #[test]
    fn leaf_codec_roundtrip() {
        let node = leaf(&[(1, 10), (2, 20), (9, 90)]);
        let mut buf = [0u8; BS];
        node.encode(&mut buf, 8).unwrap();
        assert_eq!(Node::<u64, u64>::decode(&buf, 8).unwrap(), node);
    }

    #[test]
    fn internal_codec_roundtrip() {
        let mut c0 = BlockPtr::NULL;
        c0.addr = 100;
        let mut c1 = BlockPtr::NULL;
        c1.addr = 200;
        let mut c2 = BlockPtr::NULL;
        c2.addr = 300;
        let node = internal(&[5, 15], &[c0, c1, c2]);
        let mut buf = [0u8; BS];
        node.encode(&mut buf, 8).unwrap();
        assert_eq!(Node::<u64, u64>::decode(&buf, 8).unwrap(), node);
    }

    #[test]
    fn capacities_are_sane() {
        // Leaf is byte-based now: header(16) + N*(key 8 + len 2 + value 8) <= 4096.
        let full: alloc::vec::Vec<(u64, u64)> = (0..200u64).map(|k| (k, k)).collect();
        assert!(leaf_fits::<u64, u64>(&full, BS));
        let toobig: alloc::vec::Vec<(u64, u64)> = (0..300u64).map(|k| (k, k)).collect();
        assert!(!leaf_fits::<u64, u64>(&toobig, BS));
        // internal: ptr 24, (4096-16-24)/(8+24) = 4056/32 = 126.
        assert_eq!(max_internal_keys::<u64>(BS, 8), 126);
    }

    #[test]
    fn decode_rejects_impossible_count() {
        let mut buf = [0u8; BS];
        let node = leaf(&[(1, 1)]);
        node.encode(&mut buf, 8).unwrap();
        // Forge an absurd key_count.
        buf[6..8].copy_from_slice(&60000u16.to_le_bytes());
        assert_eq!(
            Node::<u64, u64>::decode(&buf, 8),
            Err(StorageError::Corrupt(CorruptKind::BadNode))
        );
    }

    fn harness() -> (MemDevice, SegmentAllocator, BufferPool, Hasher) {
        let dev = MemDevice::new(BS, 256);
        let geom = SegmentGeom::new(256, 8, 1).unwrap();
        let alloc = SegmentAllocator::new(geom);
        let pool = BufferPool::for_block_size(BS);
        let hasher = Hasher::new(DigestMode::Fast64).unwrap();
        (dev, alloc, pool, hasher)
    }

    #[test]
    fn persist_and_verify_roundtrip() {
        let (dev, mut alloc, mut pool, hasher) = harness();
        let node = leaf(&[(1, 11), (2, 22), (3, 33)]);
        let ptr = block_on(write_node(&node, 7, &mut alloc, &dev, &mut pool, hasher)).unwrap();
        assert_eq!(ptr.birth_txg, 7);
        let back: Node<u64, u64> =
            block_on(read_node(&ptr, &dev, &mut pool, hasher)).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn corruption_is_detected_on_read() {
        let (dev, mut alloc, mut pool, hasher) = harness();
        let node = leaf(&[(1, 11), (2, 22)]);
        let ptr = block_on(write_node(&node, 1, &mut alloc, &dev, &mut pool, hasher)).unwrap();

        // Flip a byte on the media under the node — silent corruption.
        let mut victim = [0u8; BS];
        block_on(dev.read_block(ptr.addr, &mut victim)).unwrap();
        victim[HEADER_LEN_TEST] ^= 0xFF;
        block_on(dev.write_block(ptr.addr, &victim)).unwrap();

        let result: Result<Node<u64, u64>, _> =
            block_on(read_node(&ptr, &dev, &mut pool, hasher));
        assert_eq!(
            result,
            Err(StorageError::Corrupt(CorruptKind::BadChecksum)),
            "parent-stored checksum must catch silent corruption"
        );
    }

    const HEADER_LEN_TEST: usize = super::HEADER_LEN + 1;
}
