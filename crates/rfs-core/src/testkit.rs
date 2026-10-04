//! Test/fuzz support: an in-memory [`BlockDevice`] that can simulate power loss,
//! plus a minimal [`block_on`].
//!
//! This is the lower half of the testing pipeline from the design: a virtual
//! block device we can cut off mid-write to verify the engine always recovers a
//! safe state. It uses only `core` + `alloc`, so the same harness can later run
//! on-target, not just on the desktop.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::task::{Context, Poll, Waker};

use crate::allocator::{SegmentAllocator, SegmentGeom};
use crate::checksum::DigestMode;
use crate::device::{BlockDevice, Deallocate};
use crate::error::StorageError;
use crate::volume::Volume;
use crate::zil::ZIL_CAPACITY;

/// Drives a future to completion by polling in a loop.
///
/// The testkit's futures are always immediately ready (the media is in memory),
/// so this never actually spins. It exists so tests can call the engine's async
/// API without pulling in a real executor.
///
/// # Panics
/// Never, for testkit devices. (A future that genuinely pended would busy-loop;
/// that is acceptable for a test-only helper.)
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// An in-memory block device with optional torn-write injection.
pub struct MemDevice {
    block_size: usize,
    block_count: u64,
    media: RefCell<Vec<u8>>,
    /// Bytes this device will still accept before failing. `None` == unlimited.
    /// When the budget runs out mid-block, the partial bytes are left in the
    /// media (a torn write) and the op fails with [`StorageError::DeviceRemoved`].
    write_budget: Cell<Option<u64>>,
    /// Total blocks deallocated (TRIM) — lets tests observe the capability.
    trimmed: Cell<u64>,
    /// Count of `write_block` calls — lets tests observe write amplification.
    writes: Cell<u64>,
}

impl MemDevice {
    /// Creates a zeroed device of `block_count` blocks of `block_size` bytes.
    ///
    /// # Panics
    /// If the total device size (`block_size * block_count`) overflows `usize`.
    #[must_use]
    pub fn new(block_size: usize, block_count: u64) -> Self {
        let total = block_size
            .checked_mul(usize::try_from(block_count).expect("block_count fits usize"))
            .expect("device size fits usize");
        Self {
            block_size,
            block_count,
            media: RefCell::new(alloc::vec![0u8; total]),
            write_budget: Cell::new(None),
            trimmed: Cell::new(0),
            writes: Cell::new(0),
        }
    }

    /// Total number of blocks deallocated via [`Deallocate`] so far.
    #[must_use]
    pub fn trimmed_blocks(&self) -> u64 {
        self.trimmed.get()
    }

    /// Total number of `write_block` calls so far (write-amplification probe).
    #[must_use]
    pub fn write_count(&self) -> u64 {
        self.writes.get()
    }

    /// Sets a write budget in bytes; `None` clears the limit (writes succeed).
    pub fn set_write_budget(&self, bytes: Option<u64>) {
        self.write_budget.set(bytes);
    }

    /// Produces an independent copy of the current media (budget cleared).
    ///
    /// Used to fork "the disk as it would look after a crash at point X" so a
    /// test can crash at many points without re-deriving prior state.
    #[must_use]
    pub fn snapshot(&self) -> Self {
        Self {
            block_size: self.block_size,
            block_count: self.block_count,
            media: RefCell::new(self.media.borrow().clone()),
            write_budget: Cell::new(None),
            trimmed: Cell::new(self.trimmed.get()),
            writes: Cell::new(self.writes.get()),
        }
    }

    fn span(&self, lba: u64) -> Result<(usize, usize), StorageError> {
        if lba >= self.block_count {
            return Err(StorageError::OutOfBounds);
        }
        let start = usize::try_from(lba).map_err(|_| StorageError::OutOfBounds)? * self.block_size;
        Ok((start, start + self.block_size))
    }
}

// These are a deliberately trivial in-memory test double: the media is a
// Vec, so nothing awaits. clippy::unused_async_trait_impl suggests dropping
// `async` and returning `impl Future` instead, which would satisfy the lint
// but make a fake device read less like the real driver it stands in for.
#[allow(clippy::unused_async_trait_impl)]
impl BlockDevice for MemDevice {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn block_count(&self) -> u64 {
        self.block_count
    }

    async fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        if buf.len() != self.block_size {
            return Err(StorageError::BufferSize);
        }
        let (start, end) = self.span(lba)?;
        buf.copy_from_slice(&self.media.borrow()[start..end]);
        Ok(())
    }

    async fn write_block(&self, lba: u64, buf: &[u8]) -> Result<(), StorageError> {
        if buf.len() != self.block_size {
            return Err(StorageError::BufferSize);
        }
        self.writes.set(self.writes.get() + 1);
        let (start, _end) = self.span(lba)?;

        // How many bytes are we allowed to actually commit before "power loss"?
        let writable = match self.write_budget.get() {
            None => self.block_size,
            Some(budget) => {
                let allowed = usize::try_from(budget)
                    .unwrap_or(usize::MAX)
                    .min(self.block_size);
                // Consume the budget; a short write trips the failure path below.
                self.write_budget
                    .set(Some(budget.saturating_sub(allowed as u64)));
                allowed
            }
        };

        {
            let mut media = self.media.borrow_mut();
            media[start..start + writable].copy_from_slice(&buf[..writable]);
        }

        if writable < self.block_size {
            // Torn write: partial bytes landed, then the device "vanished".
            return Err(StorageError::DeviceRemoved);
        }
        Ok(())
    }

    async fn flush(&self) -> Result<(), StorageError> {
        // Media is already the source of truth; nothing buffered above it.
        Ok(())
    }
}

// These are a deliberately trivial in-memory test double: the media is a
// Vec, so nothing awaits. clippy::unused_async_trait_impl suggests dropping
// `async` and returning `impl Future` instead, which would satisfy the lint
// but make a fake device read less like the real driver it stands in for.
#[allow(clippy::unused_async_trait_impl)]
impl Deallocate for MemDevice {
    async fn deallocate(&self, start_lba: u64, count: u64) -> Result<(), StorageError> {
        let end = start_lba
            .checked_add(count)
            .ok_or(StorageError::OutOfBounds)?;
        if end > self.block_count {
            return Err(StorageError::OutOfBounds);
        }
        // A real SSD discards the range; the model just records the hint.
        self.trimmed.set(self.trimmed.get() + count);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Crash-recovery fuzz driver (shared by the in-crate simulation and the
// libFuzzer target under `fuzz/`).
// ---------------------------------------------------------------------------

const FUZZ_BS: usize = 256;
const FUZZ_BLOCKS: u64 = 65_536;
const FUZZ_KEYS: u64 = 48;

type FuzzVol = Volume<u64, u64, SegmentAllocator, MemDevice>;

fn fuzz_alloc() -> SegmentAllocator {
    SegmentAllocator::new(SegmentGeom::new(FUZZ_BLOCKS, 64, 1).unwrap())
}

fn next_byte(data: &[u8], pos: &mut usize) -> u8 {
    let b = data.get(*pos).copied().unwrap_or(0);
    *pos += 1;
    b
}

fn fuzz_check(vol: &mut FuzzVol, model: &BTreeMap<u64, u64>) {
    for k in 0..FUZZ_KEYS {
        assert_eq!(
            block_on(vol.get(&k)).unwrap(),
            model.get(&k).copied(),
            "key {k} mismatch vs durability model"
        );
    }
}

fn fuzz_drain(
    vol: &mut FuzzVol,
    working: &BTreeMap<u64, u64>,
    durable: &mut BTreeMap<u64, u64>,
    pending: &mut u64,
) {
    if *pending >= ZIL_CAPACITY - 1 {
        block_on(vol.commit()).unwrap();
        durable.clone_from(working);
        *pending = 0;
    }
}

/// Drives a [`Volume`] from an arbitrary byte string, model-checking that crash
/// recovery always yields the last committed state plus `fsync`'d (ZIL) ops —
/// never a torn/corrupt in-between. Each byte stream decodes to a sequence of
/// insert / delete / sync / commit / crash steps.
///
/// Shared by the deterministic simulation test and the `cargo fuzz` target.
///
/// # Panics
/// On any divergence from the durability model (that *is* the test).
pub fn fuzz_crash_recovery(data: &[u8]) {
    let dev = MemDevice::new(FUZZ_BS, FUZZ_BLOCKS);
    let mut vol: FuzzVol = block_on(Volume::format(dev, fuzz_alloc(), DigestMode::Fast64)).unwrap();

    let mut working: BTreeMap<u64, u64> = BTreeMap::new();
    let mut durable: BTreeMap<u64, u64> = BTreeMap::new();
    let mut pending: u64 = 0;
    let mut pos = 0usize;
    let mut steps = 0u32;

    while pos < data.len() && steps < 4096 {
        steps += 1;
        let op = next_byte(data, &mut pos);
        let key = u64::from(next_byte(data, &mut pos)) % FUZZ_KEYS;
        match op % 8 {
            0..=2 => {
                let v = u64::from(next_byte(data, &mut pos));
                block_on(vol.insert(key, v)).unwrap();
                working.insert(key, v);
            }
            3 => {
                block_on(vol.delete(&key)).unwrap();
                working.remove(&key);
            }
            4 => {
                fuzz_drain(&mut vol, &working, &mut durable, &mut pending);
                let v = u64::from(next_byte(data, &mut pos));
                block_on(vol.sync_insert(key, v)).unwrap();
                working.insert(key, v);
                durable.insert(key, v);
                pending += 1;
            }
            5 => {
                fuzz_drain(&mut vol, &working, &mut durable, &mut pending);
                block_on(vol.sync_delete(&key)).unwrap();
                working.remove(&key);
                durable.remove(&key);
                pending += 1;
            }
            6 => {
                block_on(vol.commit()).unwrap();
                durable.clone_from(&working);
                pending = 0;
            }
            _ => {
                let tear = next_byte(data, &mut pos);
                if tear & 1 == 0 {
                    // Tear a commit mid-write: it must leave no trace.
                    vol.device().set_write_budget(Some(u64::from(tear) + 1));
                    let _ = block_on(vol.commit());
                }
                let media = vol.device().snapshot();
                vol = block_on(Volume::open(media, fuzz_alloc())).unwrap();
                fuzz_check(&mut vol, &durable);
                working.clone_from(&durable);
            }
        }
    }

    let media = vol.device().snapshot();
    let mut vol = block_on(Volume::open(media, fuzz_alloc())).unwrap();
    fuzz_check(&mut vol, &durable);
}

#[cfg(test)]
mod tests {
    use super::{MemDevice, block_on};
    use crate::checksum::DigestMode;
    use crate::device::{BlockDevice, Deallocate};
    use crate::error::StorageError;
    use crate::superblock::{self, RING, Superblock};

    const BS: usize = 4096;

    #[test]
    fn vectored_extent_roundtrip() {
        let dev = MemDevice::new(BS, 64);
        let mut src = alloc::vec![0u8; BS * 3];
        for (i, b) in src.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        block_on(dev.write_extent(10, &src)).unwrap();
        let mut dst = alloc::vec![0u8; BS * 3];
        block_on(dev.read_extent(10, &mut dst)).unwrap();
        assert_eq!(src, dst);
    }

    #[test]
    fn extent_rejects_misaligned_length() {
        let dev = MemDevice::new(BS, 64);
        let mut buf = alloc::vec![0u8; BS + 1];
        assert_eq!(
            block_on(dev.read_extent(0, &mut buf)),
            Err(StorageError::BufferSize)
        );
    }

    #[test]
    fn deallocate_counts_and_bounds_check() {
        let dev = MemDevice::new(BS, 64);
        block_on(dev.deallocate(0, 8)).unwrap();
        assert_eq!(dev.trimmed_blocks(), 8);
        assert_eq!(
            block_on(dev.deallocate(60, 10)),
            Err(StorageError::OutOfBounds)
        );
    }

    #[test]
    fn format_then_open_roundtrips() {
        let dev = MemDevice::new(BS, 64);
        let made = block_on(superblock::format(&dev, DigestMode::Fast64)).unwrap();
        let read = block_on(superblock::open(&dev)).unwrap();
        assert_eq!(made, read);
        assert_eq!(read.txg, 1);
        assert_eq!(read.root_addr, 0);
    }

    #[test]
    fn commit_advances_to_newest_txg() {
        let dev = MemDevice::new(BS, 64);
        let mut sb = block_on(superblock::format(&dev, DigestMode::Fast64)).unwrap();
        for _ in 0..10 {
            sb.txg += 1;
            sb.root_addr = sb.txg * 100;
            block_on(superblock::commit(&dev, &sb)).unwrap();
        }
        let read = block_on(superblock::open(&dev)).unwrap();
        assert_eq!(read.txg, 11);
        assert_eq!(read.root_addr, 1100);
    }

    /// The milestone-1 invariant: a power cut at *any* byte offset during a
    /// commit recovers either the new state or the previous state — never a
    /// torn, checksum-failing in-between.
    #[test]
    fn power_cut_during_commit_is_always_consistent() {
        // Build a known-good state at txg = 5.
        let dev = MemDevice::new(BS, 64);
        let mut sb = block_on(superblock::format(&dev, DigestMode::Fast64)).unwrap();
        for _ in 0..4 {
            sb.txg += 1;
            sb.root_addr = sb.txg * 100;
            block_on(superblock::commit(&dev, &sb)).unwrap();
        }
        let good = block_on(superblock::open(&dev)).unwrap();
        assert_eq!(good.txg, 5);

        let mut next = good;
        next.txg += 1; // 6
        next.root_addr = 999;
        // Sanity: the torn slot is not the slot holding `good`.
        assert_ne!(good.txg % RING, next.txg % RING);

        for budget in 0..=BS as u64 {
            let crashed = dev.snapshot();
            crashed.set_write_budget(Some(budget));
            // May fail (torn) or succeed (full block written); either is fine.
            let _ = block_on(superblock::commit(&crashed, &next));

            let recovered: Superblock =
                block_on(superblock::open(&crashed)).expect("a valid superblock must survive");

            assert!(
                recovered.txg == good.txg || recovered.txg == next.txg,
                "budget {budget}: recovered txg {} not in {{{}, {}}}",
                recovered.txg,
                good.txg,
                next.txg,
            );
            if recovered.txg == next.txg {
                assert_eq!(recovered.root_addr, 999, "budget {budget}: full commit");
            } else {
                assert_eq!(
                    recovered.root_addr, good.root_addr,
                    "budget {budget}: rollback"
                );
            }
        }
    }
}
