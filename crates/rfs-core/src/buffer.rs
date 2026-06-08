//! Aligned, poolable, registration-aware buffers.
//!
//! This replaces ad-hoc `Vec<u8>` scratch with buffers that are friendly to the
//! transports we want underneath the [`BlockDevice`] seam:
//!
//! * **Alignment** — DMA and `NVMe` want page-aligned, often hugepage-backed
//!   memory; [`AlignedBuf`] guarantees a chosen alignment ([`DMA_ALIGN`] by
//!   default).
//! * **Registration** — RDMA / NVMe-oF require memory to be *registered* with the
//!   NIC (pinned + DMA-mapped, yielding an `lkey`/`rkey`). A buffer carries an
//!   optional [`RegionKey`] so a transport can stash that handle on the very
//!   buffer the engine already owns, enabling true zero-copy.
//! * **Pooling** — [`BufferPool`] recycles block-sized buffers so the hot path
//!   neither allocates nor re-registers per I/O.
//!
//! Buffers are intentionally `!Send`/`!Sync` (they wrap a raw pointer), matching
//! Feox's core-local ownership model.
//!
//! [`BlockDevice`]: crate::device::BlockDevice

use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr::NonNull;
use core::slice;

/// Default buffer alignment: one 4 KiB page. Satisfies typical NVMe/DMA/RDMA
/// requirements.
pub const DMA_ALIGN: usize = 4096;

/// Handle to a registered memory region, filled in by an RDMA/NVMe-oF transport.
///
/// The engine never interprets these; it only carries them so a transport can
/// perform zero-copy, one-sided transfers against a buffer the engine owns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegionKey {
    /// Local access key.
    pub lkey: u32,
    /// Remote access key (for one-sided peer access during replication).
    pub rkey: u32,
    /// Address as seen by the remote peer.
    pub remote_addr: u64,
}

/// An owned, aligned, zeroed heap buffer with optional registration metadata.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    layout: Layout,
    len: usize,
    region: Option<RegionKey>,
}

impl AlignedBuf {
    /// Allocates a zeroed buffer of `len` bytes aligned to `align`.
    ///
    /// # Panics
    /// If `len == 0`, if `align` is not a power of two, or if the size/align
    /// combination is invalid for a [`Layout`].
    #[must_use]
    pub fn new(len: usize, align: usize) -> Self {
        assert!(len > 0, "AlignedBuf length must be non-zero");
        let layout = Layout::from_size_align(len, align).expect("valid buffer layout");
        // SAFETY: `layout` has non-zero size (asserted above), so `alloc_zeroed`
        // is being used correctly; we check the returned pointer for null below.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        Self {
            ptr,
            layout,
            len,
            region: None,
        }
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty (always false; length is non-zero).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Immutable view of the bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` is a valid, aligned allocation of `len` bytes that lives
        // as long as `self`; the returned slice borrows `self` immutably.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// Mutable view of the bytes.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, but the `&mut self` borrow guarantees exclusive
        // access for the returned slice's lifetime.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// The registration handle, if a transport has registered this buffer.
    #[must_use]
    pub fn region(&self) -> Option<RegionKey> {
        self.region
    }

    /// Records the registration handle for this buffer.
    pub fn set_region(&mut self, region: RegionKey) {
        self.region = Some(region);
    }

    /// Zeroes the contents (used when recycling through a pool).
    pub fn zero(&mut self) {
        self.as_mut_slice().fill(0);
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`layout` are exactly what `alloc_zeroed` returned in
        // `new` and have not been freed elsewhere (no `Clone`/`Copy`).
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// A recycler for fixed-size, aligned buffers.
///
/// Single-writer / core-local: `&mut self` throughout, no interior locking.
pub struct BufferPool {
    block_size: usize,
    align: usize,
    free: Vec<AlignedBuf>,
}

impl BufferPool {
    /// Creates a pool handing out `block_size`-byte buffers aligned to `align`.
    #[must_use]
    pub fn new(block_size: usize, align: usize) -> Self {
        Self {
            block_size,
            align,
            free: Vec::new(),
        }
    }

    /// Creates a pool with [`DMA_ALIGN`] alignment.
    #[must_use]
    pub fn for_block_size(block_size: usize) -> Self {
        Self::new(block_size, DMA_ALIGN)
    }

    /// Returns a zeroed buffer, reusing a recycled one when available.
    #[must_use]
    pub fn acquire(&mut self) -> AlignedBuf {
        match self.free.pop() {
            Some(mut buf) => {
                buf.zero();
                buf
            }
            None => AlignedBuf::new(self.block_size, self.align),
        }
    }

    /// Returns a buffer to the pool for reuse. Wrong-sized buffers are dropped.
    pub fn release(&mut self, buf: AlignedBuf) {
        if buf.len() == self.block_size {
            self.free.push(buf);
        }
    }

    /// Number of buffers currently idle in the pool.
    #[must_use]
    pub fn idle(&self) -> usize {
        self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{AlignedBuf, BufferPool, DMA_ALIGN, RegionKey};

    #[test]
    fn aligned_and_zeroed() {
        let buf = AlignedBuf::new(4096, DMA_ALIGN);
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.as_slice().as_ptr() as usize % DMA_ALIGN, 0);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn carries_region_handle() {
        let mut buf = AlignedBuf::new(512, 64);
        assert_eq!(buf.region(), None);
        let key = RegionKey { lkey: 7, rkey: 9, remote_addr: 0xdead_beef };
        buf.set_region(key);
        assert_eq!(buf.region(), Some(key));
    }

    #[test]
    fn pool_recycles_and_zeroes() {
        let mut pool = BufferPool::for_block_size(4096);
        let mut a = pool.acquire();
        a.as_mut_slice()[0] = 0xFF;
        pool.release(a);
        assert_eq!(pool.idle(), 1);
        let b = pool.acquire(); // same buffer, re-zeroed
        assert_eq!(pool.idle(), 0);
        assert_eq!(b.as_slice()[0], 0, "recycled buffer must be zeroed");
    }
}
