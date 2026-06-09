//! The one seam the whole engine hangs off.
//!
//! The trait is intentionally tiny and **asynchronous**: Feox's storage path is
//! completion-driven and core-local (`!Send`), so the engine speaks `async`
//! natively rather than blocking. With native `async fn` in traits (stable since
//! Rust 1.75) and *static* dispatch via generics, this produces concrete,
//! anonymous, `!Send`-friendly futures — exactly the shape Feox wants — with no
//! `async-trait` allocation and no `dyn` overhead.

use core::future::Future;

use crate::error::StorageError;

/// A fixed-block-size storage device.
///
/// Addressing is in whole blocks (LBAs). Every buffer passed to [`read_block`]
/// / [`write_block`] must be exactly [`block_size`] bytes.
///
/// `&self` (not `&mut self`) is used deliberately: the engine holds the device
/// through shared references and relies on interior mutability in the
/// implementation. That matches Feox's core-local cell-based ownership model,
/// where a single core drives the queue without `&mut` aliasing friction.
///
/// [`read_block`]: BlockDevice::read_block
/// [`write_block`]: BlockDevice::write_block
/// [`block_size`]: BlockDevice::block_size
pub trait BlockDevice {
    /// Bytes per block. Constant for the lifetime of the device.
    fn block_size(&self) -> usize;

    /// Total number of addressable blocks.
    fn block_count(&self) -> u64;

    /// Reads block `lba` into `buf` (which must be exactly `block_size` bytes).
    fn read_block(
        &self,
        lba: u64,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<(), StorageError>>;

    /// Writes `buf` (exactly `block_size` bytes) to block `lba`.
    ///
    /// A successful return does **not** imply durability; call [`flush`] to
    /// establish a durability barrier.
    ///
    /// [`flush`]: BlockDevice::flush
    fn write_block(&self, lba: u64, buf: &[u8]) -> impl Future<Output = Result<(), StorageError>>;

    /// Establishes a durability barrier: all prior successful writes are on
    /// stable media once this resolves `Ok`.
    fn flush(&self) -> impl Future<Output = Result<(), StorageError>>;

    /// Reads `buf.len() / block_size` consecutive blocks starting at
    /// `start_lba`. `buf.len()` must be a whole multiple of the block size.
    ///
    /// This is the vectored / scatter-gather seam: the default fans out to
    /// per-block reads, but a real backend (`NVMe` SGL, RDMA scatter) overrides it
    /// to issue one transfer over a single registered, contiguous buffer.
    fn read_extent(
        &self,
        start_lba: u64,
        buf: &mut [u8],
    ) -> impl Future<Output = Result<(), StorageError>> {
        async move {
            let bs = self.block_size();
            if buf.is_empty() || !buf.len().is_multiple_of(bs) {
                return Err(StorageError::BufferSize);
            }
            for (i, chunk) in buf.chunks_mut(bs).enumerate() {
                self.read_block(start_lba + i as u64, chunk).await?;
            }
            Ok(())
        }
    }

    /// Writes `buf.len() / block_size` consecutive blocks starting at
    /// `start_lba`. Counterpart to [`read_extent`](BlockDevice::read_extent).
    fn write_extent(
        &self,
        start_lba: u64,
        buf: &[u8],
    ) -> impl Future<Output = Result<(), StorageError>> {
        async move {
            let bs = self.block_size();
            if buf.is_empty() || !buf.len().is_multiple_of(bs) {
                return Err(StorageError::BufferSize);
            }
            for (i, chunk) in buf.chunks(bs).enumerate() {
                self.write_block(start_lba + i as u64, chunk).await?;
            }
            Ok(())
        }
    }
}

/// Placement hint for write streams, mapped from the allocator's `SegKind` by a
/// higher layer and passed down to a device that supports `NVMe` FDP / ZNS
/// stream-style separation. Kept independent of the allocator to avoid an
/// upward layer dependency.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementHint(pub u16);

impl PlacementHint {
    /// Hot, frequently-rewritten metadata stream.
    pub const META: Self = Self(0);
    /// Cold, write-mostly-once data stream.
    pub const DATA: Self = Self(1);
}

/// Capability: the device can take per-write placement hints (`NVMe` FDP, ZNS
/// streams) so hot metadata and cold data land in distinct reuse units.
pub trait PlacementWrite: BlockDevice {
    /// Writes `buf` to `lba` with a placement hint.
    fn write_block_placed(
        &self,
        lba: u64,
        buf: &[u8],
        hint: PlacementHint,
    ) -> impl Future<Output = Result<(), StorageError>>;
}

/// Capability: the device can be told that a range of blocks is no longer in use
/// (`NVMe` Dataset Management / deallocate, i.e. TRIM). Wired to commit-time
/// segment reclamation so the FTL can recover space.
pub trait Deallocate: BlockDevice {
    /// Hints that `count` blocks starting at `start_lba` are free.
    fn deallocate(
        &self,
        start_lba: u64,
        count: u64,
    ) -> impl Future<Output = Result<(), StorageError>>;
}

/// Capability: the device exposes append-only zones (`NVMe` ZNS). A log-structured
/// allocator maps directly onto this — segment == zone, segment reclaim == zone
/// reset — eliminating device-side garbage collection.
pub trait ZonedDevice: BlockDevice {
    /// Number of blocks per zone (should match the allocator's segment size).
    fn zone_size_blocks(&self) -> u64;

    /// Resets a zone, making it empty and writable from its start again.
    fn reset_zone(&self, zone_index: u64) -> impl Future<Output = Result<(), StorageError>>;

    /// Appends `buf` at the zone's current write pointer, returning the LBA it
    /// landed at.
    fn append(
        &self,
        zone_index: u64,
        buf: &[u8],
    ) -> impl Future<Output = Result<u64, StorageError>>;
}
