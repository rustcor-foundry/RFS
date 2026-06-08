//! Segment allocator (the F2FS-borrowed layer).
//!
//! The device is carved into large fixed-size **segments** (e.g. 2 MiB). Within
//! a segment, allocation is a pure append — a write pointer that only moves
//! forward — so the on-media write stream stays sequential, which is what flash
//! wants. Whole segments are the unit of reclamation.
//!
//! # Two lessons encoded here
//!
//! * **Hot/cold isolation (F2FS).** There is a separate *active* segment per
//!   [`SegKind`]: constantly-churning metadata never shares a segment with
//!   write-once user data, so metadata thrash cannot trigger garbage collection
//!   across cold data.
//! * **Deferred free (copy-on-write correctness).** [`free`](Allocator::free) never makes
//!   a block immediately reusable. A block orphaned by a copy-on-write update
//!   must stay readable until the new root that no longer references it is
//!   durably committed — otherwise a crash mid-transaction rolls back to a root
//!   pointing at reused space. So a freed segment becomes reusable only at
//!   [`commit`](Allocator::commit), which models reaching the durability
//!   barrier. (Snapshot-aware "is this block truly dead?" logic lives one layer
//!   up, in the transaction/object layer; the allocator stays dumb about
//!   snapshots but honors the txg barrier.)
//!
//! Allocation is synchronous and takes `&mut self`: unlike [`BlockDevice`], the
//! allocator is touched only by the single writer, so there is no aliasing or
//! concurrency to model here. Actual block I/O happens separately through the
//! async device.
//!
//! # M2 scope
//!
//! The allocator state is in-memory. Persisting it (a log-structured space map,
//! or rebuilding by mark-and-sweep on mount) is a later milestone. M2 reclaims
//! the common case — a closed segment whose last live block was freed — but does
//! not yet *relocate* the few survivors out of a mostly-empty segment (cleaning);
//! that is also later.
//!
//! [`BlockDevice`]: crate::device::BlockDevice

use alloc::vec::Vec;

/// Allocation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AllocError {
    /// No free segment is available to satisfy the request.
    NoSpace,
    /// The block address is outside the device or inside the reserved prefix.
    OutOfRange,
    /// The block was not currently allocated (double free / freeing free space).
    NotAllocated,
}

/// Which log a block belongs to. Separate active segments keep these from
/// sharing media, isolating churn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegKind {
    /// Hot, frequently rewritten metadata (tree interior, inodes, ...).
    Meta,
    /// Cold, write-mostly-once user data.
    Data,
}

impl SegKind {
    /// Number of distinct logs. (F2FS uses six; we start with two.)
    const COUNT: usize = 2;

    const fn index(self) -> usize {
        match self {
            Self::Meta => 0,
            Self::Data => 1,
        }
    }
}

/// The allocation interface the upper layers (notably the copy-on-write
/// `B-tree`) depend on.
pub trait Allocator {
    /// Allocates one block for the given log, returning its absolute address.
    ///
    /// # Errors
    /// [`AllocError::NoSpace`] if no segment can be opened.
    fn alloc(&mut self, kind: SegKind) -> Result<u64, AllocError>;

    /// Stages `block` for freeing. The block stops being live immediately (so
    /// double frees are caught), but its segment is not reusable until
    /// [`commit`](Allocator::commit).
    ///
    /// # Errors
    /// [`AllocError::OutOfRange`] or [`AllocError::NotAllocated`].
    fn free(&mut self, block: u64) -> Result<(), AllocError>;

    /// Marks the durability barrier as reached: segments emptied since the last
    /// commit become reusable.
    fn commit(&mut self);

    /// Marks `block` as live during mount-time recovery (mark-and-sweep). Call
    /// once per reachable block, before any [`alloc`](Allocator::alloc) /
    /// [`free`](Allocator::free), then [`finish_rebuild`](Allocator::finish_rebuild).
    ///
    /// # Errors
    /// [`AllocError::OutOfRange`] for an out-of-device or reserved block.
    fn mark_live(&mut self, block: u64) -> Result<(), AllocError>;

    /// Finalizes recovery: any segment holding a live block becomes closed
    /// (immutable until it empties); the rest become free and reusable.
    /// Unreferenced (garbage) blocks in fully-dead segments are reclaimed.
    fn finish_rebuild(&mut self);

    /// Clears all accounting back to "everything free" (except the reserved
    /// prefix) so an in-place mark-and-sweep (`reset` → `mark_live`* →
    /// `finish_rebuild`) can recompute free space — e.g. after deleting a
    /// snapshot.
    fn reset(&mut self);
}

/// Static description of how the device is divided into segments.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentGeom {
    /// Blocks per segment (e.g. 512 for 2 MiB segments of 4 KiB blocks).
    pub blocks_per_segment: u32,
    /// Total number of whole segments on the device.
    pub segment_count: u32,
    /// Segments at the start reserved for the superblock ring and allocator
    /// metadata; never handed out.
    pub reserved_segments: u32,
}

impl SegmentGeom {
    /// Derives geometry from a device block count and a desired segment size.
    ///
    /// Any tail of blocks that does not fill a whole segment is left unmanaged.
    ///
    /// # Errors
    /// [`AllocError::OutOfRange`] if the geometry is degenerate (zero-size
    /// segments, or fewer segments than the reserved prefix).
    pub fn new(
        block_count: u64,
        blocks_per_segment: u32,
        reserved_segments: u32,
    ) -> Result<Self, AllocError> {
        if blocks_per_segment == 0 {
            return Err(AllocError::OutOfRange);
        }
        let segment_count =
            u32::try_from(block_count / u64::from(blocks_per_segment)).unwrap_or(u32::MAX);
        if segment_count <= reserved_segments {
            return Err(AllocError::OutOfRange);
        }
        Ok(Self {
            blocks_per_segment,
            segment_count,
            reserved_segments,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SegState {
    /// Reserved prefix; never allocated.
    Reserved,
    /// Available to be opened.
    Free,
    /// Currently the active append target for some kind.
    Open,
    /// Filled and no longer the active target; may still hold live blocks.
    Closed,
}

#[derive(Clone, Copy, Debug)]
struct SegmentInfo {
    state: SegState,
    /// Next free block index within the segment (the append pointer).
    write_ptr: u32,
    /// Count of currently-live blocks in the segment.
    valid: u32,
}

/// Compact one-bit-per-block liveness map.
struct Bitmap {
    words: Vec<u64>,
}

impl Bitmap {
    fn new(bits: usize) -> Self {
        Self {
            words: alloc::vec![0u64; bits.div_ceil(64)],
        }
    }

    fn get(&self, index: usize) -> bool {
        self.words[index / 64] & (1u64 << (index % 64)) != 0
    }

    fn set(&mut self, index: usize) {
        self.words[index / 64] |= 1u64 << (index % 64);
    }

    fn clear(&mut self, index: usize) {
        self.words[index / 64] &= !(1u64 << (index % 64));
    }
}

/// F2FS-style segment allocator with hot/cold isolation and deferred frees.
pub struct SegmentAllocator {
    geom: SegmentGeom,
    segments: Vec<SegmentInfo>,
    bitmap: Bitmap,
    active: [Option<u32>; SegKind::COUNT],
    /// Segments touched by frees since the last commit (dedup at commit time).
    dirty: Vec<u32>,
    free_segments: u32,
}

impl SegmentAllocator {
    /// Creates an allocator over `geom`, with every non-reserved segment free.
    ///
    /// # Panics
    /// If the total block count (`segment_count * blocks_per_segment`) overflows
    /// `usize` on this target.
    #[must_use]
    pub fn new(geom: SegmentGeom) -> Self {
        let total_blocks = usize::try_from(
            u64::from(geom.segment_count) * u64::from(geom.blocks_per_segment),
        )
        .expect("device block count fits usize");

        let mut segments = Vec::with_capacity(geom.segment_count as usize);
        for seg in 0..geom.segment_count {
            let state = if seg < geom.reserved_segments {
                SegState::Reserved
            } else {
                SegState::Free
            };
            segments.push(SegmentInfo {
                state,
                write_ptr: 0,
                valid: 0,
            });
        }

        Self {
            free_segments: geom.segment_count - geom.reserved_segments,
            geom,
            segments,
            bitmap: Bitmap::new(total_blocks),
            active: [None; SegKind::COUNT],
            dirty: Vec::new(),
        }
    }

    /// Number of segments currently free (available to open).
    #[must_use]
    pub fn free_segment_count(&self) -> u32 {
        self.free_segments
    }

    /// Whether `block` is currently live. Useful for tests and scrubbing.
    #[must_use]
    pub fn is_allocated(&self, block: u64) -> bool {
        usize::try_from(block).is_ok_and(|b| b < self.bitmap_len() && self.bitmap.get(b))
    }

    fn bitmap_len(&self) -> usize {
        self.geom.segment_count as usize * self.geom.blocks_per_segment as usize
    }

    fn first_free_segment(&self) -> Option<u32> {
        self.segments
            .iter()
            .position(|s| s.state == SegState::Free)
            .and_then(|p| u32::try_from(p).ok())
    }

    fn seg_of(&self, block: u64) -> Option<u32> {
        let seg = block / u64::from(self.geom.blocks_per_segment);
        u32::try_from(seg)
            .ok()
            .filter(|&s| s < self.geom.segment_count)
    }
}

impl Allocator for SegmentAllocator {
    fn alloc(&mut self, kind: SegKind) -> Result<u64, AllocError> {
        let k = kind.index();
        let bps = self.geom.blocks_per_segment;

        // Retire a full active segment so the search below opens a fresh one.
        if let Some(seg) = self.active[k]
            && self.segments[seg as usize].write_ptr >= bps
        {
            self.segments[seg as usize].state = SegState::Closed;
            self.active[k] = None;
        }

        // Ensure there is an active segment with room.
        let seg = if let Some(seg) = self.active[k] {
            seg
        } else {
            let seg = self.first_free_segment().ok_or(AllocError::NoSpace)?;
            self.segments[seg as usize].state = SegState::Open;
            self.segments[seg as usize].write_ptr = 0;
            self.active[k] = Some(seg);
            self.free_segments -= 1;
            seg
        };

        let info = &mut self.segments[seg as usize];
        let within = info.write_ptr;
        info.write_ptr += 1;
        info.valid += 1;

        let block = u64::from(seg) * u64::from(bps) + u64::from(within);
        self.bitmap.set(usize::try_from(block).unwrap());
        Ok(block)
    }

    fn free(&mut self, block: u64) -> Result<(), AllocError> {
        let seg = self.seg_of(block).ok_or(AllocError::OutOfRange)?;
        if self.segments[seg as usize].state == SegState::Reserved {
            return Err(AllocError::OutOfRange);
        }
        let idx = usize::try_from(block).map_err(|_| AllocError::OutOfRange)?;
        if idx >= self.bitmap_len() || !self.bitmap.get(idx) {
            return Err(AllocError::NotAllocated);
        }

        // The block stops being live now (so a second free is rejected), but the
        // segment is not made reusable until commit().
        self.bitmap.clear(idx);
        self.segments[seg as usize].valid -= 1;
        self.dirty.push(seg);
        Ok(())
    }

    fn commit(&mut self) {
        self.dirty.sort_unstable();
        self.dirty.dedup();
        let dirty = core::mem::take(&mut self.dirty);
        for seg in dirty {
            let info = &mut self.segments[seg as usize];
            let is_active = self.active.contains(&Some(seg));
            if info.valid == 0 && info.state == SegState::Closed && !is_active {
                info.state = SegState::Free;
                info.write_ptr = 0;
                self.free_segments += 1;
            }
        }
    }

    fn mark_live(&mut self, block: u64) -> Result<(), AllocError> {
        let seg = self.seg_of(block).ok_or(AllocError::OutOfRange)?;
        if self.segments[seg as usize].state == SegState::Reserved {
            return Err(AllocError::OutOfRange);
        }
        let idx = usize::try_from(block).map_err(|_| AllocError::OutOfRange)?;
        if idx >= self.bitmap_len() {
            return Err(AllocError::OutOfRange);
        }
        if !self.bitmap.get(idx) {
            self.bitmap.set(idx);
            self.segments[seg as usize].valid += 1;
        }
        Ok(())
    }

    fn finish_rebuild(&mut self) {
        let bps = self.geom.blocks_per_segment;
        self.active = [None; SegKind::COUNT];
        let mut free = 0u32;
        for seg in &mut self.segments {
            if seg.state == SegState::Reserved {
                continue;
            }
            if seg.valid > 0 {
                seg.state = SegState::Closed;
                seg.write_ptr = bps;
            } else {
                seg.state = SegState::Free;
                seg.write_ptr = 0;
                free += 1;
            }
        }
        self.free_segments = free;
    }

    fn reset(&mut self) {
        for seg in &mut self.segments {
            if seg.state != SegState::Reserved {
                seg.state = SegState::Free;
                seg.write_ptr = 0;
                seg.valid = 0;
            }
        }
        for word in &mut self.bitmap.words {
            *word = 0;
        }
        self.active = [None; SegKind::COUNT];
        self.free_segments = self.geom.segment_count - self.geom.reserved_segments;
        self.dirty.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{AllocError, Allocator, SegKind, SegmentAllocator, SegmentGeom};

    fn alloc_n(a: &mut SegmentAllocator, kind: SegKind, n: usize) -> alloc::vec::Vec<u64> {
        (0..n).map(|_| a.alloc(kind).unwrap()).collect()
    }

    fn geom() -> SegmentGeom {
        // 8 segments of 4 blocks; segment 0 reserved (superblock ring lives there).
        SegmentGeom::new(32, 4, 1).unwrap()
    }

    #[test]
    fn appends_sequentially_then_opens_next_segment() {
        let mut a = SegmentAllocator::new(geom());
        let blocks = alloc_n(&mut a, SegKind::Data, 5);
        // First four fill one segment (sequential), fifth jumps to a new segment.
        assert_eq!(&blocks[..4], &[blocks[0], blocks[0] + 1, blocks[0] + 2, blocks[0] + 3]);
        assert_ne!(blocks[4] / 4, blocks[3] / 4, "5th block must be in a new segment");
        assert!(blocks.iter().all(|&b| b >= 4), "reserved segment 0 never handed out");
    }

    #[test]
    fn meta_and_data_never_share_a_segment() {
        let mut a = SegmentAllocator::new(geom());
        let m = a.alloc(SegKind::Meta).unwrap();
        let d = a.alloc(SegKind::Data).unwrap();
        assert_ne!(m / 4, d / 4, "hot meta and cold data must be isolated");
    }

    #[test]
    fn free_is_deferred_until_commit() {
        // Fill segment 1 entirely, then free it all. Until commit, the segment is
        // not reclaimed, so allocation does not reuse those addresses.
        let mut a = SegmentAllocator::new(geom());
        let seg1 = alloc_n(&mut a, SegKind::Data, 4); // fills one segment
        let _seg2_first = a.alloc(SegKind::Data).unwrap(); // forces seg1 closed
        let free_before = a.free_segment_count();

        for &b in &seg1 {
            a.free(b).unwrap();
        }
        assert_eq!(
            a.free_segment_count(),
            free_before,
            "freed segment must NOT be reusable before commit"
        );
        assert!(seg1.iter().all(|&b| !a.is_allocated(b)), "blocks no longer live");

        a.commit();
        assert_eq!(
            a.free_segment_count(),
            free_before + 1,
            "emptied segment reclaimed at commit"
        );
    }

    #[test]
    fn double_free_and_out_of_range_are_rejected() {
        let mut a = SegmentAllocator::new(geom());
        let b = a.alloc(SegKind::Data).unwrap();
        a.free(b).unwrap();
        assert_eq!(a.free(b), Err(AllocError::NotAllocated));
        assert_eq!(a.free(0), Err(AllocError::OutOfRange)); // reserved segment
        assert_eq!(a.free(10_000), Err(AllocError::OutOfRange));
    }

    #[test]
    fn exhaustion_reports_no_space() {
        let mut a = SegmentAllocator::new(geom());
        // 7 usable segments * 4 blocks = 28 allocatable blocks.
        let ok = alloc_n(&mut a, SegKind::Data, 28);
        assert_eq!(ok.len(), 28);
        assert_eq!(a.alloc(SegKind::Data), Err(AllocError::NoSpace));
    }
}
