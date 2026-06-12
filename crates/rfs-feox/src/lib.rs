#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

//! `rfs-feox`: the Feox exokernel adapter — [`rfs_core::BlockDevice`] over
//! `feox-nvme`'s `QueueRing` data path (RFS milestone M5).
//!
//! The shape is exactly what `docs/FEOX-INTEGRATION.md` designed: each
//! `read_block`/`write_block`/`flush` submits one `NVMe` command on a
//! core-local queue ring and awaits its `NvmeIoFuture`. Because Feox's boot
//! storage path is polled (no MSI-X wiring yet), the await is wrapped in a
//! [`Pump`] future that drains the completion queue on every poll and
//! self-wakes while pending — so any polling executor (including
//! `feox-async`) makes progress without interrupt plumbing. When IRQ-driven
//! completions land, only the pump changes.
//!
//! Data movement uses one caller-provided, device-visible **bounce page**
//! (page-aligned, so a 4 KiB transfer is a single PRP1 and never needs
//! PRP2): writes copy into it before submit, reads copy out after
//! completion. Zero-copy registered-buffer I/O (`AlignedBuf` + `RegionKey`
//! over Feox physical-page capabilities) is the later optimization; the
//! bounce keeps the first adapter correct for any caller buffer alignment.
//!
//! The adapter always presents [`FS_BLOCK_SIZE`] (4 KiB) blocks — the size
//! the engine is validated at — by aggregating namespace LBAs: a 512-byte-
//! formatted namespace is driven with eight-LBA commands, a 4 KiB one with
//! single-LBA commands.
//!
//! The adapter is deliberately `!Send`/`!Sync` (raw bounce pointer +
//! core-local ring): one core owns the device, matching the engine's
//! `&self` + interior-mutability contract.

#[cfg(test)]
extern crate std;

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use feox_nvme::{
    NamespaceGeometry, NvmeCompletion, NvmeError, NvmeIoFuture, QueueRing, SubmissionQueueEntry,
};
use rfs_core::{BlockDevice, StorageError};

/// Largest block size the bounce path supports: one DMA page.
pub const MAX_BLOCK_SIZE: usize = 4096;

/// The filesystem block size the adapter presents, regardless of the
/// namespace's LBA format.
pub const FS_BLOCK_SIZE: usize = 4096;

/// A device-visible bounce page: the kernel-mapped pointer the adapter copies
/// through, and the address the controller DMAs to/from (identical under
/// Feox's identity map today; an IOMMU/`RegionKey` translation later).
#[derive(Clone, Copy, Debug)]
pub struct DmaPage {
    /// Kernel-visible mapping of the page.
    pub kernel: *mut u8,
    /// Device-visible (DMA) address of the same page.
    pub device: u64,
}

/// [`BlockDevice`] over one `NVMe` namespace driven through a
/// `feox_nvme::QueueRing`.
pub struct NvmeBlockDevice<const N: usize> {
    queue: RefCell<QueueRing<N>>,
    bounce: DmaPage,
    nsid: u32,
    /// Namespace LBAs per presented filesystem block.
    lbas_per_block: u64,
    /// Presented geometry: [`FS_BLOCK_SIZE`] blocks.
    block_count: u64,
}

impl<const N: usize> NvmeBlockDevice<N> {
    /// Wraps a live, registered I/O queue ring and namespace geometry.
    /// Returns `None` when the namespace LBA size does not divide
    /// [`FS_BLOCK_SIZE`] (the adapter aggregates whole LBAs per block).
    ///
    /// # Safety
    ///
    /// - `queue` must be a created, controller-registered I/O queue for the
    ///   namespace `nsid` belongs to, and this core must be the only driver
    ///   of it.
    /// - `bounce.kernel` must point at a readable+writable, **page-aligned**
    ///   buffer of at least [`MAX_BLOCK_SIZE`] bytes that the controller can
    ///   DMA at `bounce.device`, valid for the adapter's lifetime
    ///   (page-aligned so a 4 KiB transfer fits one PRP).
    #[must_use]
    pub unsafe fn new(
        queue: QueueRing<N>,
        nsid: u32,
        geometry: NamespaceGeometry,
        bounce: DmaPage,
    ) -> Option<Self> {
        if geometry.block_size == 0
            || geometry.block_size > FS_BLOCK_SIZE
            || !FS_BLOCK_SIZE.is_multiple_of(geometry.block_size)
        {
            return None;
        }
        let lbas_per_block = (FS_BLOCK_SIZE / geometry.block_size) as u64;
        Some(Self {
            queue: RefCell::new(queue),
            bounce,
            nsid,
            lbas_per_block,
            block_count: geometry.block_count / lbas_per_block,
        })
    }

    fn check(&self, lba: u64, len: usize) -> Result<(), StorageError> {
        if len != FS_BLOCK_SIZE {
            return Err(StorageError::BufferSize);
        }
        if lba >= self.block_count {
            return Err(StorageError::OutOfBounds);
        }
        Ok(())
    }

    /// Starting namespace LBA for a presented block.
    fn slba(&self, lba: u64) -> u64 {
        lba * self.lbas_per_block
    }

    /// Zero-based LBA count per presented block (`NVMe` NLB encoding).
    #[allow(clippy::cast_possible_truncation)] // lbas_per_block <= 4096/512
    fn nlb(&self) -> u16 {
        (self.lbas_per_block - 1) as u16
    }

    fn copy_into_bounce(&self, data: &[u8]) {
        // SAFETY: `new` guarantees the bounce page holds MAX_BLOCK_SIZE
        // bytes and `check` capped `data` at the block size.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.bounce.kernel, data.len()) };
    }

    fn copy_from_bounce(&self, out: &mut [u8]) {
        // SAFETY: as above, in the other direction.
        unsafe { core::ptr::copy_nonoverlapping(self.bounce.kernel, out.as_mut_ptr(), out.len()) };
    }

    /// Submits `command` and pumps the ring until its completion resolves.
    async fn run(&self, command: SubmissionQueueEntry) -> Result<(), StorageError> {
        let (_cid, future) = self.queue.borrow_mut().submit(command).map_err(map_error)?;
        Pump {
            queue: &self.queue,
            inner: future,
        }
        .await
        .map_err(map_error)
        .map(|_completion| ())
    }
}

/// Maps transport errors onto the engine's storage errors. Capability
/// revocation and device removal stay first-class; everything else is I/O.
fn map_error(error: NvmeError) -> StorageError {
    match error {
        NvmeError::DeviceRemoved => StorageError::DeviceRemoved,
        NvmeError::CapabilityRevoked => StorageError::CapabilityRevoked,
        NvmeError::QueueFull | NvmeError::CommandFailed(_) | NvmeError::StaleSlot => {
            StorageError::Io
        }
    }
}

/// Drives one submitted command on a polled ring: every poll first drains the
/// completion queue, then polls the inner future; while pending it self-wakes
/// so polling executors keep re-polling without interrupt wiring.
struct Pump<'queue, const N: usize> {
    queue: &'queue RefCell<QueueRing<N>>,
    inner: NvmeIoFuture<N>,
}

impl<const N: usize> Future for Pump<'_, N> {
    type Output = Result<NvmeCompletion, NvmeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.queue.borrow_mut().process_completions();
        match Pin::new(&mut this.inner).poll(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

impl<const N: usize> BlockDevice for NvmeBlockDevice<N> {
    fn block_size(&self) -> usize {
        FS_BLOCK_SIZE
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    async fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.check(lba, buf.len())?;
        let command = SubmissionQueueEntry::nvm_read(
            self.nsid,
            self.slba(lba),
            self.nlb(),
            self.bounce.device,
            0,
        );
        self.run(command).await?;
        self.copy_from_bounce(buf);
        Ok(())
    }

    async fn write_block(&self, lba: u64, buf: &[u8]) -> Result<(), StorageError> {
        self.check(lba, buf.len())?;
        self.copy_into_bounce(buf);
        let command = SubmissionQueueEntry::nvm_write(
            self.nsid,
            self.slba(lba),
            self.nlb(),
            self.bounce.device,
            0,
        );
        self.run(command).await
    }

    fn flush(&self) -> impl Future<Output = Result<(), StorageError>> {
        self.run(SubmissionQueueEntry::nvm_flush(self.nsid, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::{DmaPage, MAX_BLOCK_SIZE, NvmeBlockDevice};
    use core::future::Future;
    use core::pin::pin;
    use core::ptr::NonNull;
    use core::task::{Context, Poll, Waker};
    use feox_nvme::{
        CompletionQueueEntry, ControllerRegisters, NamespaceGeometry, QueueRing,
        SubmissionQueueEntry,
    };
    use rfs_core::{BlockDevice, StorageError};

    /// A fake controller: a register bank for doorbells plus ring + bounce
    /// memory the test injects CQEs into.
    struct Rig {
        _bar: std::vec::Vec<u8>,
        sq: std::boxed::Box<[SubmissionQueueEntry; 4]>,
        cq: std::boxed::Box<[CompletionQueueEntry; 4]>,
        bounce: std::vec::Vec<u8>,
        cq_injected: usize,
    }

    impl Rig {
        fn new() -> (Self, NvmeBlockDevice<4>) {
            let mut bar = std::vec![0u8; 0x1100];
            let mut sq = std::boxed::Box::new([SubmissionQueueEntry::default(); 4]);
            let mut cq = std::boxed::Box::new([CompletionQueueEntry::default(); 4]);
            let mut bounce = std::vec![0u8; MAX_BLOCK_SIZE];
            // SAFETY (test): the bank covers the doorbell window; rings hold
            // four entries; all allocations outlive the device via Rig.
            let regs = unsafe { ControllerRegisters::new(bar.as_mut_ptr()) };
            let ring: QueueRing<4> = unsafe {
                QueueRing::new(
                    regs,
                    1,
                    0,
                    NonNull::new(sq.as_mut_ptr()).unwrap(),
                    NonNull::new(cq.as_mut_ptr()).unwrap(),
                )
            };
            let page = DmaPage {
                kernel: bounce.as_mut_ptr(),
                device: 0xD000,
            };
            let geometry = NamespaceGeometry {
                block_count: 64,
                block_size: 512,
            };
            // SAFETY (test): ring + bounce page live in Rig for the device's
            // lifetime; single-threaded test.
            let device = unsafe { NvmeBlockDevice::new(ring, 1, geometry, page) }.unwrap();
            (
                Self {
                    _bar: bar,
                    sq,
                    cq,
                    bounce,
                    cq_injected: 0,
                },
                device,
            )
        }

        /// Pretends the controller completed the most recent command.
        fn inject_completion(&mut self, cid: u16, status_field: u16) {
            let slot = self.cq_injected % 4;
            let phase = u32::from(self.cq_injected / 4 % 2 == 0); // 1,1,1,1,0,0,...
            self.cq[slot].dw3 = u32::from(cid) | (phase << 16) | (u32::from(status_field) << 17);
            self.cq_injected += 1;
        }

        /// Writes through the bounce page's raw pointer (the aliasing-safe
        /// way to play the controller's DMA engine).
        fn dma_fill(&mut self, data: &[u8]) {
            // SAFETY (test): within the MAX_BLOCK_SIZE bounce allocation.
            unsafe {
                core::ptr::copy_nonoverlapping(data.as_ptr(), self.bounce.as_mut_ptr(), data.len());
            }
        }

        fn dma_peek(&self, len: usize) -> std::vec::Vec<u8> {
            let mut out = std::vec![0u8; len];
            // SAFETY (test): within the bounce allocation.
            unsafe {
                core::ptr::copy_nonoverlapping(self.bounce.as_ptr(), out.as_mut_ptr(), len);
            }
            out
        }
    }

    fn poll_once<F: Future>(future: &mut core::pin::Pin<&mut F>) -> Poll<F::Output> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        future.as_mut().poll(&mut cx)
    }

    #[test]
    fn read_block_round_trips_through_the_bounce() {
        let (mut rig, device) = Rig::new();
        // 64 LBAs of 512 bytes present as 8 blocks of 4096.
        assert_eq!(device.block_size(), 4096);
        assert_eq!(device.block_count(), 8);
        let mut out = [0u8; 4096];
        let pattern: std::vec::Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        {
            let mut future = pin!(device.read_block(7, &mut out));

            // First poll: command submitted, no completion yet.
            assert!(poll_once(&mut future).is_pending());
            assert_eq!(rig.sq[0].cdw0 & 0xFF, 0x02, "NVM Read opcode");
            assert_eq!(rig.sq[0].nsid, 1);
            assert_eq!(rig.sq[0].cdw10, 56, "SLBA = block 7 * 8 LBAs");
            assert_eq!(rig.sq[0].cdw12, 7, "NLB = 8 LBAs, zero-based");
            assert_eq!(rig.sq[0].prp1, 0xD000, "bounce DMA address");

            // The "controller" lands data in the bounce and completes CID 0.
            rig.dma_fill(&pattern);
            rig.inject_completion(0, 0);

            match poll_once(&mut future) {
                Poll::Ready(Ok(())) => {}
                other => panic!("expected Ok, got {other:?}"),
            }
        }
        assert_eq!(&out[..], &pattern[..]);
    }

    #[test]
    fn write_block_stages_data_before_submit_and_flush_uses_opcode_zero() {
        let (mut rig, device) = Rig::new();
        let data: std::vec::Vec<u8> = (0..4096u32).map(|i| (i % 199) as u8 ^ 0x5A).collect();
        {
            let mut future = pin!(device.write_block(3, &data));
            assert!(poll_once(&mut future).is_pending());
            assert_eq!(rig.sq[0].cdw0 & 0xFF, 0x01, "NVM Write opcode");
            assert_eq!(rig.sq[0].cdw10, 24, "SLBA = block 3 * 8 LBAs");
            assert_eq!(rig.dma_peek(4096), data, "data staged before the doorbell");
            rig.inject_completion(0, 0);
            assert!(matches!(poll_once(&mut future), Poll::Ready(Ok(()))));
        }
        {
            let mut future = pin!(device.flush());
            assert!(poll_once(&mut future).is_pending());
            assert_eq!(rig.sq[1].cdw0 & 0xFF, 0x00, "NVM Flush opcode");
            rig.inject_completion(0, 0);
            assert!(matches!(poll_once(&mut future), Poll::Ready(Ok(()))));
        }
    }

    #[test]
    fn failures_and_bounds_map_to_engine_errors() {
        let (mut rig, device) = Rig::new();

        // Out-of-bounds and wrong-size requests fail without touching the ring.
        let mut buf = [0u8; 4096];
        {
            let mut oob = pin!(device.read_block(8, &mut buf));
            assert!(matches!(
                poll_once(&mut oob),
                Poll::Ready(Err(StorageError::OutOfBounds))
            ));
        }
        {
            let mut small = [0u8; 256];
            let mut sized = pin!(device.read_block(0, &mut small));
            assert!(matches!(
                poll_once(&mut sized),
                Poll::Ready(Err(StorageError::BufferSize))
            ));
        }

        // A failed completion surfaces as an I/O error.
        {
            let mut failing = pin!(device.read_block(0, &mut buf));
            assert!(poll_once(&mut failing).is_pending());
            rig.inject_completion(0, 0x0002); // generic command error
            assert!(matches!(
                poll_once(&mut failing),
                Poll::Ready(Err(StorageError::Io))
            ));
        }
    }

    #[test]
    fn read_extent_spans_blocks_over_the_adapter() {
        let (mut rig, device) = Rig::new();
        let mut out = std::vec![0u8; 8192];
        {
            let mut future = pin!(device.read_extent(2, &mut out));

            // Block 1 of 2.
            assert!(poll_once(&mut future).is_pending());
            assert_eq!(rig.sq[0].cdw10, 16, "SLBA = block 2 * 8 LBAs");
            rig.dma_fill(&[0xAA; 4096]);
            rig.inject_completion(0, 0);
            // Completing block 1 lets the extent submit block 2.
            assert!(poll_once(&mut future).is_pending());
            assert_eq!(rig.sq[1].cdw10, 24, "SLBA = block 3 * 8 LBAs");
            rig.dma_fill(&[0xBB; 4096]);
            rig.inject_completion(0, 0);
            assert!(matches!(poll_once(&mut future), Poll::Ready(Ok(()))));
        }
        assert!(out[..4096].iter().all(|&b| b == 0xAA));
        assert!(out[4096..].iter().all(|&b| b == 0xBB));
    }
}
