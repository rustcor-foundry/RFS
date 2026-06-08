//! The atomic-commit primitive every later layer stands on.
//!
//! RFS is copy-on-write: a transaction never overwrites live data. It writes new
//! tree nodes to free space and then makes them live by publishing a new root.
//! "Publishing the root" *is* the commit, and it must be atomic across a power
//! cut. We get that the way ZFS does — not with a journal, but with a **ring of
//! superblock slots**:
//!
//! * Each commit bumps the transaction group number (`txg`) and writes the
//!   superblock to slot `txg % `[`RING`].
//! * Consecutive `txg`s land in distinct slots, so a torn write to the new slot
//!   cannot damage the previously committed slot.
//! * On open, we scan every slot, discard any that fail magic/checksum, and pick
//!   the survivor with the highest `txg`.
//!
//! The result: after a crash, [`open`] returns either the fully-committed new
//! state or the last good state — never a torn in-between.
//!
//! # Reserved layout
//!
//! Beyond `txg` and the root, the superblock reserves the anchors for layers not
//! yet built, so the format does not churn when they land:
//!
//! * [`Superblock::digest`] — per-volume integrity algorithm (fixed pointer
//!   width). See [`DigestMode`].
//! * [`Superblock::root_addr`] / `root_birth_txg` / `root_checksum` — the
//!   self-healing anchor of the Merkle tree (root checksum is stored max-width).
//! * [`Superblock::zil_head`] — head of the intent log replayed on mount.
//! * [`Superblock::snaplist_root`] — root of the snapshot directory.
//! * [`Superblock::flags`] — feature bits; e.g. a future `BACKREF_TREE` for the
//!   Btrfs-style escape hatch.

use crate::buffer::{AlignedBuf, DMA_ALIGN};
use crate::checksum::{DigestMode, fletcher64};
use crate::device::BlockDevice;
use crate::error::{CorruptKind, StorageError};

/// Number of slots in the superblock ring.
///
/// Two would suffice for single-commit crash safety (consecutive `txg`s differ),
/// but a little headroom tolerates a slot whose media block has gone bad.
pub const RING: u64 = 4;

/// `b"RFS\0SBLK"` little-endian — identifies a superblock block.
const MAGIC: u64 = 0x4B4C_4253_0053_4652;

/// On-disk format version understood by this build.
const VERSION: u32 = 1;

/// Width of the max-size checksum anchor stored in the superblock.
const ROOT_CKSUM_LEN: usize = 32;

/// Width of the volume identity UUID.
const VOLUME_UUID_LEN: usize = 16;

/// Feature flag: a Btrfs-style extent backref tree is present (snapshot escape
/// hatch). Reserved; not yet implemented.
pub const FLAG_BACKREF_TREE: u64 = 1 << 0;

/// Feature flag: this volume participates in block-level replication and uses
/// the multihost (MMP) fence fields. Reserved; not yet implemented.
pub const FLAG_REPLICATED: u64 = 1 << 1;

// Field byte offsets within the superblock block.
const OFF_MAGIC: usize = 0; // u64
const OFF_VERSION: usize = 8; // u32
const OFF_BLOCK_SIZE: usize = 12; // u32
const OFF_TXG: usize = 16; // u64
const OFF_FLAGS: usize = 24; // u64
const OFF_DIGEST: usize = 32; // u8 (+7 pad)
const OFF_ROOT_ADDR: usize = 40; // u64
const OFF_ROOT_BIRTH: usize = 48; // u64
const OFF_ROOT_CKSUM: usize = 56; // [u8; 32]
const OFF_ZIL_HEAD: usize = 88; // u64
const OFF_SNAPLIST: usize = 96; // u64
const OFF_VOLUME_UUID: usize = 104; // [u8; 16]
const OFF_OWNER_HOSTID: usize = 120; // u64
const OFF_MMP_SEQ: usize = 128; // u64
/// End of the fixed header; everything up to the trailing checksum is reserved.
const FIXED_END: usize = 136;

/// The persisted control block for the whole filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Superblock {
    /// Device block size this filesystem was created with.
    pub block_size: u32,
    /// Monotonic transaction-group counter. Higher == newer.
    pub txg: u64,
    /// Feature flags (reserved; e.g. a future Btrfs-style backref tree).
    pub flags: u64,
    /// Per-volume integrity algorithm; fixes interior-pointer width.
    pub digest: DigestMode,
    /// Block address of the live tree root. `0` means "no tree yet".
    pub root_addr: u64,
    /// `txg` in which the current root was born (snapshot reclamation).
    pub root_birth_txg: u64,
    /// Checksum of the root node (Merkle anchor). Stored max-width; `Fast64`
    /// uses the first 8 bytes and zero-fills the rest.
    pub root_checksum: [u8; ROOT_CKSUM_LEN],
    /// Block address of the intent-log head. `0` means "empty ZIL".
    pub zil_head: u64,
    /// Block address of the snapshot-directory root. `0` means "no snapshots".
    pub snaplist_root: u64,
    /// Stable volume identity, assigned by the provisioning layer. All-zero on a
    /// volume that has not yet been given an identity.
    pub volume_uuid: [u8; VOLUME_UUID_LEN],
    /// Multihost fence: id of the host that currently owns (has imported) the
    /// volume. `0` means unowned. Reserved for replication/failover.
    pub owner_hostid: u64,
    /// Multihost (MMP) heartbeat counter, bumped on each fence write so a peer
    /// can detect active use before importing. `0` means MMP disabled.
    pub mmp_seq: u64,
}

impl Superblock {
    /// A pristine superblock for a freshly formatted volume.
    #[must_use]
    pub const fn empty(block_size: u32, digest: DigestMode) -> Self {
        Self {
            block_size,
            txg: 1,
            flags: 0,
            digest,
            root_addr: 0,
            root_birth_txg: 0,
            root_checksum: [0u8; ROOT_CKSUM_LEN],
            zil_head: 0,
            snaplist_root: 0,
            volume_uuid: [0u8; VOLUME_UUID_LEN],
            owner_hostid: 0,
            mmp_seq: 0,
        }
    }

    /// Slot index this superblock will be written to.
    #[must_use]
    pub const fn slot(&self) -> u64 {
        self.txg % RING
    }

    /// Serializes `self` into `buf` (one full device block) and stamps the
    /// trailing checksum. `buf` must be at least `FIXED_END + 8` bytes.
    fn encode(&self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = 0;
        }
        buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC.to_le_bytes());
        buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        buf[OFF_BLOCK_SIZE..OFF_BLOCK_SIZE + 4].copy_from_slice(&self.block_size.to_le_bytes());
        buf[OFF_TXG..OFF_TXG + 8].copy_from_slice(&self.txg.to_le_bytes());
        buf[OFF_FLAGS..OFF_FLAGS + 8].copy_from_slice(&self.flags.to_le_bytes());
        buf[OFF_DIGEST] = self.digest.to_u8();
        buf[OFF_ROOT_ADDR..OFF_ROOT_ADDR + 8].copy_from_slice(&self.root_addr.to_le_bytes());
        buf[OFF_ROOT_BIRTH..OFF_ROOT_BIRTH + 8].copy_from_slice(&self.root_birth_txg.to_le_bytes());
        buf[OFF_ROOT_CKSUM..OFF_ROOT_CKSUM + ROOT_CKSUM_LEN].copy_from_slice(&self.root_checksum);
        buf[OFF_ZIL_HEAD..OFF_ZIL_HEAD + 8].copy_from_slice(&self.zil_head.to_le_bytes());
        buf[OFF_SNAPLIST..OFF_SNAPLIST + 8].copy_from_slice(&self.snaplist_root.to_le_bytes());
        buf[OFF_VOLUME_UUID..OFF_VOLUME_UUID + VOLUME_UUID_LEN].copy_from_slice(&self.volume_uuid);
        buf[OFF_OWNER_HOSTID..OFF_OWNER_HOSTID + 8].copy_from_slice(&self.owner_hostid.to_le_bytes());
        buf[OFF_MMP_SEQ..OFF_MMP_SEQ + 8].copy_from_slice(&self.mmp_seq.to_le_bytes());

        let len = buf.len();
        let sum = fletcher64(&buf[..len - 8]);
        buf[len - 8..].copy_from_slice(&sum.to_le_bytes());
    }

    /// Parses and validates one slot's block. Returns `Ok(None)` for an empty /
    /// never-written slot (zeroed magic), `Err` for a corrupt slot, and
    /// `Ok(Some(sb))` for a valid one.
    fn decode(buf: &[u8]) -> Result<Option<Self>, StorageError> {
        let magic = u64::from_le_bytes(buf[OFF_MAGIC..OFF_MAGIC + 8].try_into().unwrap());
        if magic == 0 {
            return Ok(None); // pristine slot, not an error
        }
        if magic != MAGIC {
            return Err(StorageError::Corrupt(CorruptKind::BadMagic));
        }

        let len = buf.len();
        let stored = u64::from_le_bytes(buf[len - 8..].try_into().unwrap());
        if fletcher64(&buf[..len - 8]) != stored {
            return Err(StorageError::Corrupt(CorruptKind::BadChecksum));
        }

        let version = u32::from_le_bytes(buf[OFF_VERSION..OFF_VERSION + 4].try_into().unwrap());
        if version != VERSION {
            return Err(StorageError::Corrupt(CorruptKind::UnsupportedVersion));
        }
        let digest = DigestMode::from_u8(buf[OFF_DIGEST])
            .ok_or(StorageError::Corrupt(CorruptKind::UnsupportedVersion))?;

        let mut root_checksum = [0u8; ROOT_CKSUM_LEN];
        root_checksum.copy_from_slice(&buf[OFF_ROOT_CKSUM..OFF_ROOT_CKSUM + ROOT_CKSUM_LEN]);

        Ok(Some(Self {
            block_size: u32::from_le_bytes(
                buf[OFF_BLOCK_SIZE..OFF_BLOCK_SIZE + 4].try_into().unwrap(),
            ),
            txg: u64::from_le_bytes(buf[OFF_TXG..OFF_TXG + 8].try_into().unwrap()),
            flags: u64::from_le_bytes(buf[OFF_FLAGS..OFF_FLAGS + 8].try_into().unwrap()),
            digest,
            root_addr: u64::from_le_bytes(buf[OFF_ROOT_ADDR..OFF_ROOT_ADDR + 8].try_into().unwrap()),
            root_birth_txg: u64::from_le_bytes(
                buf[OFF_ROOT_BIRTH..OFF_ROOT_BIRTH + 8].try_into().unwrap(),
            ),
            root_checksum,
            zil_head: u64::from_le_bytes(buf[OFF_ZIL_HEAD..OFF_ZIL_HEAD + 8].try_into().unwrap()),
            snaplist_root: u64::from_le_bytes(
                buf[OFF_SNAPLIST..OFF_SNAPLIST + 8].try_into().unwrap(),
            ),
            volume_uuid: buf[OFF_VOLUME_UUID..OFF_VOLUME_UUID + VOLUME_UUID_LEN]
                .try_into()
                .unwrap(),
            owner_hostid: u64::from_le_bytes(
                buf[OFF_OWNER_HOSTID..OFF_OWNER_HOSTID + 8].try_into().unwrap(),
            ),
            mmp_seq: u64::from_le_bytes(buf[OFF_MMP_SEQ..OFF_MMP_SEQ + 8].try_into().unwrap()),
        }))
    }
}

/// Initializes a brand-new filesystem: writes the first superblock (`txg = 1`)
/// with the chosen `digest` mode and flushes. The device must have at least
/// [`RING`] blocks and a block size of at least `FIXED_END + 8` bytes.
///
/// # Errors
/// Propagates any device error, or [`StorageError::BufferSize`] if the device
/// block is too small to hold a superblock.
pub async fn format<D: BlockDevice>(
    dev: &D,
    digest: DigestMode,
) -> Result<Superblock, StorageError> {
    let block_size = u32::try_from(dev.block_size()).map_err(|_| StorageError::BufferSize)?;
    let sb = Superblock::empty(block_size, digest);
    commit(dev, &sb).await?;
    Ok(sb)
}

/// Atomically publishes `sb` by writing it to slot `sb.txg % RING` and flushing.
///
/// The flush is the durability barrier: when this resolves `Ok`, the new `txg`
/// is the one [`open`] will recover. The caller is responsible for bumping
/// `sb.txg` before each commit.
///
/// # Errors
/// Propagates device errors and [`StorageError::BufferSize`] for an undersized
/// block.
pub async fn commit<D: BlockDevice>(dev: &D, sb: &Superblock) -> Result<(), StorageError> {
    let bs = dev.block_size();
    if bs < FIXED_END + 8 {
        return Err(StorageError::BufferSize);
    }
    let mut block = alloc_block(bs);
    sb.encode(block.as_mut_slice());
    dev.write_block(sb.slot(), block.as_slice()).await?;
    dev.flush().await
}

/// Recovers the newest consistent superblock by scanning the whole ring.
///
/// Corrupt or torn slots are skipped; the highest valid `txg` wins. This is the
/// crash-recovery entry point.
///
/// # Errors
/// Returns [`CorruptKind::NoValidSuperblock`] if no slot validates, or any
/// device read error.
pub async fn open<D: BlockDevice>(dev: &D) -> Result<Superblock, StorageError> {
    let bs = dev.block_size();
    let mut block = alloc_block(bs);
    let mut best: Option<Superblock> = None;

    for slot in 0..RING {
        dev.read_block(slot, block.as_mut_slice()).await?;
        // A torn/corrupt slot must not abort recovery — skip it and keep the
        // best valid slot found so far. (let-chain: edition 2024.)
        if let Ok(Some(sb)) = Superblock::decode(block.as_slice())
            && best.is_none_or(|b| sb.txg > b.txg)
        {
            best = Some(sb);
        }
    }

    best.ok_or(StorageError::Corrupt(CorruptKind::NoValidSuperblock))
}

/// Allocates a zeroed, DMA-aligned one-block scratch buffer.
///
/// The hot tree paths (M3+) will pull from a `BufferPool` to avoid per-op
/// allocation and to reuse registered memory; the superblock path is cold, so a
/// fresh aligned buffer is fine here.
fn alloc_block(bs: usize) -> AlignedBuf {
    AlignedBuf::new(bs, DMA_ALIGN)
}
