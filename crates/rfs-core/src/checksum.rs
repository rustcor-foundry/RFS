//! Block integrity checksums.
//!
//! Milestone 1 uses Fletcher-64: cheap, endian-stable, and purpose-built for
//! detecting the torn/partial writes a power cut produces. It is **not**
//! cryptographic and is not meant to be.
//!
//! When the Merkle B-tree layer lands, tree nodes will carry a *cryptographic*
//! digest (BLAKE3) stored in the parent, per the ZFS self-healing pattern. That
//! will slot in behind a `Digest` trait without disturbing this module — the
//! superblock's torn-write detection has different requirements from the tree's
//! tamper/silent-corruption detection, so they need not share an algorithm.

/// Which integrity algorithm a volume uses for tree-node pointers.
///
/// Chosen once at format time and recorded in the superblock, so every interior
/// pointer in a volume is a fixed width. `Fast64` keeps pointers compact (24 B)
/// for high fanout; `Blake3` (48 B pointers) buys cryptographic strength,
/// content-addressing, and dedup at the cost of fanour/CPU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DigestMode {
    /// 64-bit non-cryptographic checksum (Fletcher-64 today; xxh3-class later).
    Fast64,
    /// 256-bit BLAKE3 (opt-in per volume).
    Blake3,
}

impl DigestMode {
    /// On-disk discriminant.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Fast64 => 0,
            Self::Blake3 => 1,
        }
    }

    /// Parses an on-disk discriminant; unknown values are rejected.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Fast64),
            1 => Some(Self::Blake3),
            _ => None,
        }
    }

    /// Checksum width in bytes for interior-node pointers in this mode.
    #[must_use]
    pub const fn output_len(self) -> usize {
        match self {
            Self::Fast64 => 8,
            Self::Blake3 => 32,
        }
    }
}

/// Computes the Fletcher-64 checksum of `data`.
///
/// Input is consumed in little-endian 32-bit words; a trailing partial word is
/// zero-padded, which is unambiguous here because checksummed regions are always
/// a fixed length (one device block minus the trailer).
#[must_use]
pub fn fletcher64(data: &[u8]) -> u64 {
    const MOD: u64 = 0xFFFF_FFFF;
    let mut sum1: u64 = 0;
    let mut sum2: u64 = 0;
    for chunk in data.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        let value = u64::from(u32::from_le_bytes(word));
        sum1 = (sum1 + value) % MOD;
        sum2 = (sum2 + sum1) % MOD;
    }
    (sum2 << 32) | sum1
}

#[cfg(test)]
mod tests {
    use super::fletcher64;

    #[test]
    fn detects_single_byte_flip() {
        let a = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut b = a;
        b[3] ^= 0x01;
        assert_ne!(fletcher64(&a), fletcher64(&b));
    }

    #[test]
    fn detects_word_swap() {
        // Fletcher's running second sum catches transposition that a plain
        // additive sum would miss.
        let a = [0xAAu8, 0, 0, 0, 0xBB, 0, 0, 0];
        let b = [0xBBu8, 0, 0, 0, 0xAA, 0, 0, 0];
        assert_ne!(fletcher64(&a), fletcher64(&b));
    }
}
