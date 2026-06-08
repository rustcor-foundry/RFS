//! The block pointer — the convergence point of three decisions.
//!
//! Every link from a parent node to a child carries not just *where* the child
//! is, but *when it was born* (for snapshot reclamation) and *its checksum* (the
//! self-healing Merkle link). The on-disk width depends on the volume's digest
//! mode (8-byte fast / 32-byte BLAKE3); in memory we keep the max width and
//! encode only the active prefix.

/// Bytes of the fixed prefix: `addr` (8) + `birth_txg` (8).
pub const PTR_PREFIX: usize = 16;

/// Maximum checksum width carried in memory (BLAKE3).
pub const MAX_CKSUM: usize = 32;

/// A pointer from a parent node to a child block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockPtr {
    /// Block address of the child. `0` is the null pointer ("no child").
    pub addr: u64,
    /// Transaction group in which the child block was written. Used by
    /// birth-time snapshot reclamation.
    pub birth_txg: u64,
    /// Checksum of the child block's full contents (Merkle link). Only the first
    /// `digest_len` bytes are meaningful / persisted.
    pub checksum: [u8; MAX_CKSUM],
}

impl BlockPtr {
    /// The null pointer.
    pub const NULL: Self = Self {
        addr: 0,
        birth_txg: 0,
        checksum: [0u8; MAX_CKSUM],
    };

    /// Whether this is the null pointer.
    #[must_use]
    pub const fn is_null(&self) -> bool {
        self.addr == 0
    }

    /// On-disk size of a pointer for a given digest width.
    #[must_use]
    pub const fn encoded_len(digest_len: usize) -> usize {
        PTR_PREFIX + digest_len
    }

    /// Serializes into `out`, writing exactly `encoded_len(digest_len)` bytes.
    ///
    /// # Panics
    /// If `out` is shorter than `encoded_len(digest_len)` or `digest_len`
    /// exceeds [`MAX_CKSUM`].
    pub fn encode(&self, out: &mut [u8], digest_len: usize) {
        out[0..8].copy_from_slice(&self.addr.to_le_bytes());
        out[8..16].copy_from_slice(&self.birth_txg.to_le_bytes());
        out[PTR_PREFIX..PTR_PREFIX + digest_len].copy_from_slice(&self.checksum[..digest_len]);
    }

    /// Parses from `buf`, reading exactly `encoded_len(digest_len)` bytes.
    ///
    /// # Panics
    /// If `buf` is shorter than `encoded_len(digest_len)` or `digest_len`
    /// exceeds [`MAX_CKSUM`].
    #[must_use]
    pub fn decode(buf: &[u8], digest_len: usize) -> Self {
        let addr = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let birth_txg = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let mut checksum = [0u8; MAX_CKSUM];
        checksum[..digest_len].copy_from_slice(&buf[PTR_PREFIX..PTR_PREFIX + digest_len]);
        Self {
            addr,
            birth_txg,
            checksum,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockPtr, MAX_CKSUM};

    #[test]
    fn roundtrip_fast64_width() {
        let mut ptr = BlockPtr {
            addr: 0x1234,
            birth_txg: 7,
            checksum: [0u8; MAX_CKSUM],
        };
        ptr.checksum[..8].copy_from_slice(&0xABCD_1234_5678_9012u64.to_le_bytes());

        let mut buf = [0u8; 24];
        ptr.encode(&mut buf, 8);
        let back = BlockPtr::decode(&buf, 8);
        assert_eq!(ptr, back);
        assert_eq!(BlockPtr::encoded_len(8), 24);
    }

    #[test]
    fn roundtrip_blake3_width() {
        let mut checksum = [0u8; MAX_CKSUM];
        for (i, b) in checksum.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        let ptr = BlockPtr {
            addr: 99,
            birth_txg: 42,
            checksum,
        };
        let mut buf = [0u8; 48];
        ptr.encode(&mut buf, 32);
        assert_eq!(BlockPtr::decode(&buf, 32), ptr);
        assert_eq!(BlockPtr::encoded_len(32), 48);
    }

    #[test]
    fn null_pointer() {
        assert!(BlockPtr::NULL.is_null());
        assert!(!BlockPtr { addr: 1, ..BlockPtr::NULL }.is_null());
    }
}
