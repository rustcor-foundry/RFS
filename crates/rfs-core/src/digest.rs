//! Pluggable block digests with runtime acceleration dispatch.
//!
//! The Merkle tree stores a child's digest in its parent and verifies it on
//! read, so hashing is the hottest CPU path in the filesystem. This module keeps
//! the *algorithm* and the *implementation* (scalar vs vector) behind a single
//! [`Hasher`] so the rest of the engine never mentions either.
//!
//! # Acceleration
//!
//! [`SimdLevel`] is probed once via [`detect_simd`]. On RISC-V the intended plug
//! point is RVV (e.g. the K1 / `SpacemiT` X60: RVV 1.0, 256-bit VLEN) for a
//! vectorized BLAKE3 / xxh3 / Fletcher path; every algorithm always keeps a
//! scalar fallback for portability and for the desktop FUSE testbed. Real
//! speedups must be measured on silicon — streaming hashes are frequently memory
//! bandwidth bound, so width ratios do not translate directly to throughput.
//!
//! # Status
//!
//! `Fast64` (Fletcher-64) is implemented, scalar. `Blake3` is reserved behind a
//! future `blake3` feature; selecting it today yields
//! [`StorageError::Unsupported`] rather than a fake hash.

use crate::checksum::{DigestMode, fletcher64};
use crate::error::StorageError;

/// Detected SIMD capability of the current host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SimdLevel {
    /// No vector acceleration; portable scalar code.
    Scalar,
    /// RISC-V Vector (RVV) available.
    Rvv,
}

/// Probes the host for usable vector acceleration.
///
/// Today this always reports [`SimdLevel::Scalar`]; the RVV probe (reading the
/// vector CSRs / a boot-provided capability) lands with the vectorized hash
/// implementations.
#[must_use]
pub fn detect_simd() -> SimdLevel {
    SimdLevel::Scalar
}

/// The digest interface the tree layer depends on.
pub trait Digest {
    /// Output length in bytes for this algorithm.
    fn output_len(&self) -> usize;

    /// Hashes `data`, writing exactly `output_len()` bytes into `out`.
    ///
    /// # Panics
    /// If `out.len() < output_len()`.
    fn hash(&self, data: &[u8], out: &mut [u8]);
}

/// 64-bit non-cryptographic digest (Fletcher-64). The compact default: 8-byte
/// pointers, high fanout.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fast64;

impl Digest for Fast64 {
    fn output_len(&self) -> usize {
        8
    }

    fn hash(&self, data: &[u8], out: &mut [u8]) {
        out[..8].copy_from_slice(&fletcher64(data).to_le_bytes());
    }
}

/// A concrete, selected hasher: the volume's [`DigestMode`] bound to a runtime
/// [`SimdLevel`].
#[derive(Clone, Copy, Debug)]
pub struct Hasher {
    mode: DigestMode,
    simd: SimdLevel,
}

impl Hasher {
    /// Builds the hasher for a volume's digest mode, choosing the best available
    /// implementation.
    ///
    /// # Errors
    /// [`StorageError::Unsupported`] if the mode has no implementation in this
    /// build (currently `Blake3`).
    pub fn new(mode: DigestMode) -> Result<Self, StorageError> {
        match mode {
            DigestMode::Fast64 => Ok(Self {
                mode,
                simd: detect_simd(),
            }),
            // Reserved for the `blake3` feature; no scalar stand-in is provided
            // so callers cannot accidentally persist a non-BLAKE3 digest.
            DigestMode::Blake3 => Err(StorageError::Unsupported),
        }
    }

    /// The selected acceleration level.
    #[must_use]
    pub fn simd(&self) -> SimdLevel {
        self.simd
    }

    /// Output length for the selected mode.
    #[must_use]
    pub fn output_len(&self) -> usize {
        self.mode.output_len()
    }

    /// Computes the digest into `out` (must be at least `output_len()` bytes).
    ///
    /// # Panics
    /// If `out` is shorter than `output_len()`.
    pub fn hash(&self, data: &[u8], out: &mut [u8]) {
        match self.mode {
            DigestMode::Fast64 => match self.simd {
                // The RVV arm plugs in here; identical result, faster path.
                SimdLevel::Scalar | SimdLevel::Rvv => Fast64.hash(data, out),
            },
            // Unreachable: `new` rejects Blake3 until the feature exists.
            DigestMode::Blake3 => unreachable!("Blake3 hasher cannot be constructed yet"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Digest, Fast64, Hasher, SimdLevel, detect_simd};
    use crate::checksum::{DigestMode, fletcher64};
    use crate::error::StorageError;

    #[test]
    fn fast64_matches_fletcher() {
        let data = b"the quick brown fox";
        let mut out = [0u8; 8];
        Fast64.hash(data, &mut out);
        assert_eq!(u64::from_le_bytes(out), fletcher64(data));
    }

    #[test]
    fn hasher_fast64_roundtrip() {
        let h = Hasher::new(DigestMode::Fast64).unwrap();
        assert_eq!(h.output_len(), 8);
        assert_eq!(h.simd(), SimdLevel::Scalar);
        let mut out = [0u8; 8];
        h.hash(b"abc", &mut out);
        assert_eq!(u64::from_le_bytes(out), fletcher64(b"abc"));
    }

    #[test]
    fn blake3_mode_is_unsupported_for_now() {
        assert!(matches!(
            Hasher::new(DigestMode::Blake3),
            Err(StorageError::Unsupported)
        ));
    }

    #[test]
    fn simd_probe_defaults_scalar() {
        assert_eq!(detect_simd(), SimdLevel::Scalar);
    }
}
