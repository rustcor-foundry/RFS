//! POSIX-style filesystem over the copy-on-write tree (M7).
//!
//! Every object is a `u64` inode number. All filesystem state lives in one tree
//! under a composite [`FsKey`] `(object_id, kind, k2)`, so items for an object
//! are contiguous and range-scannable (the Btrfs/ZFS unified-key model):
//!
//! | kind | k2 | item |
//! |------|----|------|
//! | `SUPER`  | 0          | singleton (object 0): `next_ino`, `root_ino` |
//! | `INODE`  | 0          | per-object metadata |
//! | `DIRENT` | name hash  | a bucket of directory entries (collision-safe list) |
//!
//! Directory entries hash the name into `k2`; a bucket holds a *list* of
//! `(child, name)` so hash collisions are handled by exact-name match rather than
//! probing (no tombstone hazards). Each mutating call commits, so reads (lookup,
//! `readdir`, `getattr`) see a consistent committed state.
//!
//! File data (extents) and `read`/`write` are the next sub-increment; this layer
//! is the directory namespace + inodes.

use alloc::vec::Vec;

use crate::allocator::Allocator;
use crate::checksum::DigestMode;
use crate::device::BlockDevice;
use crate::error::StorageError;
use crate::tree::{BlockPtr, Key, MAX_CKSUM, Record, Value};
use crate::volume::Volume;

/// The root directory's inode number.
pub const ROOT_INO: u64 = 1;

const KIND_SUPER: u8 = 0;
const KIND_INODE: u8 = 1;
const KIND_DIRENT: u8 = 2;
/// File-data extents; also the exclusive upper bound for the `DIRENT` range.
const KIND_EXTENT: u8 = 3;

/// POSIX file-type bits (high bits of `mode`).
const S_IFMT: u32 = 0o17_0000;
/// Directory type bit.
pub const S_IFDIR: u32 = 0o04_0000;
/// Regular-file type bit.
pub const S_IFREG: u32 = 0o10_0000;
/// Symbolic-link type bit.
pub const S_IFLNK: u32 = 0o12_0000;

/// Composite key: items for one object are contiguous, ordered by `kind` then
/// `k2`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FsKey {
    /// Object (inode) id this item belongs to.
    pub object_id: u64,
    /// Item kind (`SUPER` / `INODE` / `DIRENT`).
    pub kind: u8,
    /// Kind-specific secondary key (e.g. a directory-entry name hash).
    pub k2: u64,
}

impl FsKey {
    const fn new(object_id: u64, kind: u8, k2: u64) -> Self {
        Self {
            object_id,
            kind,
            k2,
        }
    }
    const fn superblock() -> Self {
        Self::new(0, KIND_SUPER, 0)
    }
    const fn inode(ino: u64) -> Self {
        Self::new(ino, KIND_INODE, 0)
    }
    const fn dirent(dir: u64, name_hash: u64) -> Self {
        Self::new(dir, KIND_DIRENT, name_hash)
    }
    const fn extent(ino: u64, block_off: u64) -> Self {
        Self::new(ino, KIND_EXTENT, block_off)
    }
}

impl Record for FsKey {
    const SIZE: usize = 17;

    fn write(&self, out: &mut [u8]) {
        out[0..8].copy_from_slice(&self.object_id.to_le_bytes());
        out[8] = self.kind;
        out[9..17].copy_from_slice(&self.k2.to_le_bytes());
    }

    fn read(buf: &[u8]) -> Self {
        Self {
            object_id: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            kind: buf[8],
            k2: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
        }
    }
}

impl Key for FsKey {}

/// Per-object metadata. Times are seconds since the Unix epoch (the host injects
/// the clock via [`Filesystem::set_time`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Inode {
    /// Type + permission bits (POSIX `mode`).
    pub mode: u32,
    /// Hard-link count.
    pub nlink: u32,
    /// Size in bytes.
    pub size: u64,
    /// Access time.
    pub atime: u64,
    /// Modification time (data last changed).
    pub mtime: u64,
    /// Change time (metadata last changed).
    pub ctime: u64,
}

impl Inode {
    /// A fresh empty directory (`nlink` 2: itself and `.`).
    #[must_use]
    pub fn new_dir(mode: u32) -> Self {
        Self {
            mode: S_IFDIR | (mode & 0o7777),
            nlink: 2,
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
        }
    }

    /// A fresh empty regular file.
    #[must_use]
    pub fn new_file(mode: u32) -> Self {
        Self {
            mode: S_IFREG | (mode & 0o7777),
            nlink: 1,
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
        }
    }

    /// A fresh symbolic link (target stored as its data).
    #[must_use]
    pub fn new_symlink() -> Self {
        Self {
            mode: S_IFLNK | 0o777,
            nlink: 1,
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
        }
    }

    /// Whether this inode is a directory.
    #[must_use]
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// Whether this inode is a symbolic link.
    #[must_use]
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }

    /// Sets all three timestamps to `now` (for a freshly created object).
    #[must_use]
    fn stamped(mut self, now: u64) -> Self {
        self.atime = now;
        self.mtime = now;
        self.ctime = now;
        self
    }
}

/// One directory entry: a child inode and its name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirEntry {
    /// Child object id.
    pub ino: u64,
    /// Entry name (raw bytes, ≤ 255).
    pub name: Vec<u8>,
}

/// A tagged, variable-length value stored in the tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FsValue {
    /// Filesystem header (object 0): id allocator + root.
    Super {
        /// Next inode number to hand out.
        next_ino: u64,
        /// Root directory inode number.
        root_ino: u64,
    },
    /// An inode.
    Inode(Inode),
    /// A directory-entry bucket (one or more entries sharing a name hash).
    Dirent(Vec<DirEntry>),
    /// A file-data extent: a pointer to one data block.
    Extent(BlockPtr),
}

const TAG_SUPER: u8 = 0;
const TAG_INODE: u8 = 1;
const TAG_DIRENT: u8 = 2;
const TAG_EXTENT: u8 = 3;

impl Value for FsValue {
    fn encoded_len(&self) -> usize {
        1 + match self {
            Self::Super { .. } => 16,
            Self::Inode(_) => 40,
            Self::Dirent(entries) => {
                2 + entries.iter().map(|e| 8 + 2 + e.name.len()).sum::<usize>()
            }
            Self::Extent(_) => 16 + MAX_CKSUM,
        }
    }

    fn encode(&self, out: &mut [u8]) {
        match self {
            Self::Super { next_ino, root_ino } => {
                out[0] = TAG_SUPER;
                out[1..9].copy_from_slice(&next_ino.to_le_bytes());
                out[9..17].copy_from_slice(&root_ino.to_le_bytes());
            }
            Self::Inode(i) => {
                out[0] = TAG_INODE;
                out[1..5].copy_from_slice(&i.mode.to_le_bytes());
                out[5..9].copy_from_slice(&i.nlink.to_le_bytes());
                out[9..17].copy_from_slice(&i.size.to_le_bytes());
                out[17..25].copy_from_slice(&i.atime.to_le_bytes());
                out[25..33].copy_from_slice(&i.mtime.to_le_bytes());
                out[33..41].copy_from_slice(&i.ctime.to_le_bytes());
            }
            Self::Dirent(entries) => {
                out[0] = TAG_DIRENT;
                let count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
                out[1..3].copy_from_slice(&count.to_le_bytes());
                let mut o = 3;
                for e in entries {
                    out[o..o + 8].copy_from_slice(&e.ino.to_le_bytes());
                    let nlen = u16::try_from(e.name.len()).unwrap_or(u16::MAX);
                    out[o + 8..o + 10].copy_from_slice(&nlen.to_le_bytes());
                    out[o + 10..o + 10 + e.name.len()].copy_from_slice(&e.name);
                    o += 10 + e.name.len();
                }
            }
            Self::Extent(ptr) => {
                out[0] = TAG_EXTENT;
                out[1..9].copy_from_slice(&ptr.addr.to_le_bytes());
                out[9..17].copy_from_slice(&ptr.birth_txg.to_le_bytes());
                out[17..17 + MAX_CKSUM].copy_from_slice(&ptr.checksum);
            }
        }
    }

    fn decode(buf: &[u8]) -> Self {
        match buf[0] {
            TAG_SUPER => Self::Super {
                next_ino: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                root_ino: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
            },
            TAG_INODE => Self::Inode(Inode {
                mode: u32::from_le_bytes(buf[1..5].try_into().unwrap()),
                nlink: u32::from_le_bytes(buf[5..9].try_into().unwrap()),
                size: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
                atime: u64::from_le_bytes(buf[17..25].try_into().unwrap()),
                mtime: u64::from_le_bytes(buf[25..33].try_into().unwrap()),
                ctime: u64::from_le_bytes(buf[33..41].try_into().unwrap()),
            }),
            TAG_EXTENT => {
                let mut checksum = [0u8; MAX_CKSUM];
                checksum.copy_from_slice(&buf[17..17 + MAX_CKSUM]);
                Self::Extent(BlockPtr {
                    addr: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                    birth_txg: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
                    checksum,
                })
            }
            _ => {
                let count = usize::from(u16::from_le_bytes(buf[1..3].try_into().unwrap()));
                let mut entries = Vec::with_capacity(count);
                let mut o = 3;
                for _ in 0..count {
                    let ino = u64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
                    let nlen = usize::from(u16::from_le_bytes(buf[o + 8..o + 10].try_into().unwrap()));
                    let name = buf[o + 10..o + 10 + nlen].to_vec();
                    entries.push(DirEntry { ino, name });
                    o += 10 + nlen;
                }
                Self::Dirent(entries)
            }
        }
    }

    fn referenced_blocks(&self, out: &mut Vec<u64>) {
        if let Self::Extent(ptr) = self {
            out.push(ptr.addr);
        }
    }
}

/// FNV-1a hash of a name → directory-entry `k2`.
fn name_hash(name: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Commit once this many blocks of changes have accumulated in the open txg,
/// bounding RAM/space while letting many operations coalesce into one commit.
const COMMIT_THRESHOLD_BLOCKS: u64 = 2048;

/// A mounted filesystem: a directory namespace over a [`Volume`].
///
/// Operations accumulate in the open transaction and are flushed lazily (at the
/// threshold, or on [`sync`](Filesystem::sync)) so many small ops coalesce into
/// one commit. Durability is explicit: call `sync` (the FUSE layer does so on
/// `fsync`/`flush`/unmount) — unsynced changes may be lost on a crash, as POSIX
/// allows.
pub struct Filesystem<A, D> {
    vol: Volume<FsKey, FsValue, A, D>,
    dirty: u64,
    /// Current wall-clock time (seconds since epoch), injected by the host.
    now: u64,
}

impl<A: Allocator, D: BlockDevice> Filesystem<A, D> {
    /// Formats a fresh filesystem with an empty root directory.
    ///
    /// # Errors
    /// Device / allocation errors.
    pub async fn format(dev: D, alloc: A, digest: DigestMode) -> Result<Self, StorageError> {
        let mut vol = Volume::format(dev, alloc, digest).await?;
        vol.insert(
            FsKey::superblock(),
            FsValue::Super {
                next_ino: ROOT_INO + 1,
                root_ino: ROOT_INO,
            },
        )
        .await?;
        vol.insert(FsKey::inode(ROOT_INO), FsValue::Inode(Inode::new_dir(0o755)))
            .await?;
        vol.commit().await?;
        Ok(Self { vol, dirty: 0, now: 0 })
    }

    /// Mounts an existing filesystem.
    ///
    /// # Errors
    /// Device errors or no valid superblock.
    pub async fn open(dev: D, alloc: A) -> Result<Self, StorageError> {
        Ok(Self {
            vol: Volume::open(dev, alloc).await?,
            dirty: 0,
            now: 0,
        })
    }

    /// Sets the wall-clock used to stamp timestamps (seconds since epoch). The
    /// host (e.g. the FUSE layer) calls this before mutating operations.
    pub fn set_time(&mut self, secs: u64) {
        self.now = secs;
    }

    /// Sets `ino`'s `atime`/`mtime` (each optional) and `ctime = now` (utimes).
    ///
    /// # Errors
    /// [`StorageError::NotFound`] or I/O.
    pub async fn set_times(
        &mut self,
        ino: u64,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<(), StorageError> {
        let mut inode = self.read_inode(ino).await?;
        if let Some(a) = atime {
            inode.atime = a;
        }
        if let Some(m) = mtime {
            inode.mtime = m;
        }
        inode.ctime = self.now;
        self.write_inode(ino, inode).await?;
        self.checkpoint(1).await
    }

    /// Bumps `ino`'s `mtime`/`ctime` to now (a content/metadata change).
    async fn bump_times(&mut self, ino: u64) -> Result<(), StorageError> {
        let mut inode = self.read_inode(ino).await?;
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.write_inode(ino, inode).await
    }

    /// Records `blocks` worth of change and commits if the batch is large enough.
    async fn checkpoint(&mut self, blocks: u64) -> Result<(), StorageError> {
        self.dirty += blocks;
        if self.dirty >= COMMIT_THRESHOLD_BLOCKS {
            self.vol.commit().await?;
            self.dirty = 0;
        }
        Ok(())
    }

    /// Flushes all pending changes to stable storage (the durability barrier).
    ///
    /// # Errors
    /// Device errors.
    pub async fn sync(&mut self) -> Result<(), StorageError> {
        if self.dirty > 0 {
            self.vol.commit().await?;
            self.dirty = 0;
        }
        Ok(())
    }

    /// Borrows the backing device (tests / fault injection).
    pub fn device(&self) -> &D {
        self.vol.device()
    }

    async fn alloc_ino(&mut self) -> Result<u64, StorageError> {
        let FsValue::Super { next_ino, root_ino } = self
            .vol
            .get(&FsKey::superblock())
            .await?
            .ok_or(StorageError::NotFound)?
        else {
            return Err(StorageError::Corrupt(crate::CorruptKind::BadNode));
        };
        self.vol
            .insert(
                FsKey::superblock(),
                FsValue::Super {
                    next_ino: next_ino + 1,
                    root_ino,
                },
            )
            .await?;
        Ok(next_ino)
    }

    async fn read_inode(&mut self, ino: u64) -> Result<Inode, StorageError> {
        match self.vol.get(&FsKey::inode(ino)).await? {
            Some(FsValue::Inode(i)) => Ok(i),
            _ => Err(StorageError::NotFound),
        }
    }

    async fn write_inode(&mut self, ino: u64, inode: Inode) -> Result<(), StorageError> {
        self.vol.insert(FsKey::inode(ino), FsValue::Inode(inode)).await
    }

    async fn bucket(&mut self, dir: u64, hash: u64) -> Result<Vec<DirEntry>, StorageError> {
        match self.vol.get(&FsKey::dirent(dir, hash)).await? {
            Some(FsValue::Dirent(entries)) => Ok(entries),
            _ => Ok(Vec::new()),
        }
    }

    /// Returns the child inode of `name` in directory `dir`, if present.
    ///
    /// # Errors
    /// Device / verification errors.
    pub async fn lookup(&mut self, dir: u64, name: &[u8]) -> Result<Option<u64>, StorageError> {
        let bucket = self.bucket(dir, name_hash(name)).await?;
        Ok(bucket.iter().find(|e| e.name == name).map(|e| e.ino))
    }

    /// Resolves a slash-free component path (sequence of names) from the root.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if any component is missing.
    pub async fn resolve(&mut self, components: &[&[u8]]) -> Result<u64, StorageError> {
        let mut ino = ROOT_INO;
        for comp in components {
            ino = self.lookup(ino, comp).await?.ok_or(StorageError::NotFound)?;
        }
        Ok(ino)
    }

    /// Inode metadata.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] if the inode does not exist.
    pub async fn getattr(&mut self, ino: u64) -> Result<Inode, StorageError> {
        self.read_inode(ino).await
    }

    async fn link(&mut self, dir: u64, name: &[u8], child: u64) -> Result<(), StorageError> {
        let hash = name_hash(name);
        let mut bucket = self.bucket(dir, hash).await?;
        if bucket.iter().any(|e| e.name == name) {
            return Err(StorageError::AlreadyExists);
        }
        bucket.push(DirEntry {
            ino: child,
            name: name.to_vec(),
        });
        self.vol.insert(FsKey::dirent(dir, hash), FsValue::Dirent(bucket)).await
    }

    async fn unlink_entry(&mut self, dir: u64, name: &[u8]) -> Result<u64, StorageError> {
        let hash = name_hash(name);
        let mut bucket = self.bucket(dir, hash).await?;
        let pos = bucket
            .iter()
            .position(|e| e.name == name)
            .ok_or(StorageError::NotFound)?;
        let child = bucket.remove(pos).ino;
        if bucket.is_empty() {
            self.vol.delete(&FsKey::dirent(dir, hash)).await?;
        } else {
            self.vol.insert(FsKey::dirent(dir, hash), FsValue::Dirent(bucket)).await?;
        }
        Ok(child)
    }

    /// Creates a subdirectory `name` under `parent`; returns its inode.
    ///
    /// # Errors
    /// [`StorageError::AlreadyExists`], [`StorageError::NotADirectory`], or I/O.
    pub async fn mkdir(&mut self, parent: u64, name: &[u8], mode: u32) -> Result<u64, StorageError> {
        let pinode = self.read_inode(parent).await?;
        if !pinode.is_dir() {
            return Err(StorageError::NotADirectory);
        }
        let ino = self.alloc_ino().await?;
        self.write_inode(ino, Inode::new_dir(mode).stamped(self.now)).await?;
        self.link(parent, name, ino).await?;
        // Parent gains a link from the child's "..", and its contents changed.
        let mut p = self.read_inode(parent).await?;
        p.nlink += 1;
        p.mtime = self.now;
        p.ctime = self.now;
        self.write_inode(parent, p).await?;
        self.checkpoint(1).await?;
        Ok(ino)
    }

    /// Creates an empty regular file `name` under `parent`; returns its inode.
    ///
    /// # Errors
    /// [`StorageError::AlreadyExists`], [`StorageError::NotADirectory`], or I/O.
    pub async fn create(&mut self, parent: u64, name: &[u8], mode: u32) -> Result<u64, StorageError> {
        let pinode = self.read_inode(parent).await?;
        if !pinode.is_dir() {
            return Err(StorageError::NotADirectory);
        }
        let ino = self.alloc_ino().await?;
        self.write_inode(ino, Inode::new_file(mode).stamped(self.now)).await?;
        self.link(parent, name, ino).await?;
        self.bump_times(parent).await?;
        self.checkpoint(1).await?;
        Ok(ino)
    }

    /// Lists `(name, ino)` of a directory, ascending by name hash.
    ///
    /// # Errors
    /// [`StorageError::NotADirectory`] or I/O.
    pub async fn readdir(&mut self, dir: u64) -> Result<Vec<(Vec<u8>, u64)>, StorageError> {
        if !self.read_inode(dir).await?.is_dir() {
            return Err(StorageError::NotADirectory);
        }
        let mut buckets = Vec::new();
        self.vol
            .range(
                FsKey::new(dir, KIND_DIRENT, 0),
                FsKey::new(dir, KIND_EXTENT, 0),
                &mut buckets,
            )
            .await?;
        let mut out = Vec::new();
        for (_, value) in buckets {
            if let FsValue::Dirent(entries) = value {
                for e in entries {
                    out.push((e.name, e.ino));
                }
            }
        }
        Ok(out)
    }

    /// Frees every data extent of `ino` (and its data blocks). Does not commit.
    async fn free_file_data(&mut self, ino: u64) -> Result<(), StorageError> {
        let mut exts = Vec::new();
        self.vol
            .range(
                FsKey::extent(ino, 0),
                FsKey::new(ino, KIND_EXTENT + 1, 0),
                &mut exts,
            )
            .await?;
        for (key, value) in exts {
            if let FsValue::Extent(ptr) = value {
                self.vol.free_data_block(ptr);
            }
            self.vol.delete(&key).await?;
        }
        Ok(())
    }

    /// Drops a link to `child`, freeing the inode (and a file's data) at nlink 0.
    /// Does not commit.
    async fn drop_link(&mut self, child: u64) -> Result<(), StorageError> {
        let mut inode = self.read_inode(child).await?;
        inode.nlink = inode.nlink.saturating_sub(1);
        if inode.nlink == 0 {
            if !inode.is_dir() {
                self.free_file_data(child).await?;
            }
            self.vol.delete(&FsKey::inode(child)).await?;
        } else {
            self.write_inode(child, inode).await?;
        }
        Ok(())
    }

    /// Adjusts an inode's link count by `delta` (clamped at 0). Does not commit.
    async fn adjust_nlink(&mut self, ino: u64, delta: i64) -> Result<(), StorageError> {
        let mut inode = self.read_inode(ino).await?;
        inode.nlink = u32::try_from(i64::from(inode.nlink) + delta).unwrap_or(0);
        self.write_inode(ino, inode).await
    }

    /// Removes a regular file `name` from `parent`, freeing its data at nlink 0.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] or I/O.
    pub async fn unlink(&mut self, parent: u64, name: &[u8]) -> Result<(), StorageError> {
        let child = self.unlink_entry(parent, name).await?;
        self.drop_link(child).await?;
        self.bump_times(parent).await?;
        self.checkpoint(1).await
    }

    /// Renames `(old_parent, old_name)` to `(new_parent, new_name)`, replacing an
    /// existing destination (empty dir or file). One atomic transaction.
    ///
    /// # Errors
    /// [`StorageError::NotFound`], [`StorageError::NotADirectory`] (type mismatch
    /// with the destination), [`StorageError::NotEmpty`], or I/O.
    pub async fn rename(
        &mut self,
        old_parent: u64,
        old_name: &[u8],
        new_parent: u64,
        new_name: &[u8],
    ) -> Result<(), StorageError> {
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }
        let src = self
            .lookup(old_parent, old_name)
            .await?
            .ok_or(StorageError::NotFound)?;
        let src_is_dir = self.read_inode(src).await?.is_dir();

        // Replace an existing destination, if any.
        if let Some(dst) = self.lookup(new_parent, new_name).await? {
            let dst_is_dir = self.read_inode(dst).await?.is_dir();
            if dst_is_dir != src_is_dir {
                return Err(StorageError::NotADirectory);
            }
            if dst_is_dir && !self.readdir(dst).await?.is_empty() {
                return Err(StorageError::NotEmpty);
            }
            self.unlink_entry(new_parent, new_name).await?;
            if dst_is_dir {
                self.vol.delete(&FsKey::inode(dst)).await?;
                self.adjust_nlink(new_parent, -1).await?; // dest dir's ".." gone
            } else {
                self.drop_link(dst).await?;
            }
        }

        // Move the source entry.
        self.unlink_entry(old_parent, old_name).await?;
        self.link(new_parent, new_name, src).await?;

        // A directory moving between parents re-homes its ".." link.
        if src_is_dir && old_parent != new_parent {
            self.adjust_nlink(old_parent, -1).await?;
            self.adjust_nlink(new_parent, 1).await?;
        }
        self.bump_times(old_parent).await?;
        if new_parent != old_parent {
            self.bump_times(new_parent).await?;
        }
        self.checkpoint(1).await
    }

    /// Removes an empty subdirectory `name` from `parent`.
    ///
    /// # Errors
    /// [`StorageError::NotFound`], [`StorageError::NotEmpty`], or I/O.
    pub async fn rmdir(&mut self, parent: u64, name: &[u8]) -> Result<(), StorageError> {
        let dir = self.lookup(parent, name).await?.ok_or(StorageError::NotFound)?;
        if !self.readdir(dir).await?.is_empty() {
            return Err(StorageError::NotEmpty);
        }
        self.unlink_entry(parent, name).await?;
        self.vol.delete(&FsKey::inode(dir)).await?;
        let mut p = self.read_inode(parent).await?;
        p.nlink = p.nlink.saturating_sub(1);
        p.mtime = self.now;
        p.ctime = self.now;
        self.write_inode(parent, p).await?;
        self.checkpoint(1).await
    }

    async fn get_extent(&mut self, ino: u64, block_off: u64) -> Result<Option<BlockPtr>, StorageError> {
        match self.vol.get(&FsKey::extent(ino, block_off)).await? {
            Some(FsValue::Extent(ptr)) => Ok(Some(ptr)),
            _ => Ok(None),
        }
    }

    /// Writes `data` at byte `offset` in file `ino`, extending it if needed.
    /// Block-granular copy-on-write: each touched block is read-modified into a
    /// fresh data block and the old one is freed (birth-gated) at commit.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] or I/O / verification errors.
    // Within-block offsets are bounded by the block size, which fits `usize`.
    #[allow(clippy::cast_possible_truncation)]
    pub async fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let bs = self.vol.block_size();
        let bs64 = bs as u64;
        let mut inode = self.read_inode(ino).await?;

        let mut written = 0usize;
        while written < data.len() {
            let pos = offset + written as u64;
            let block_off = (pos / bs64) * bs64;
            let within = (pos - block_off) as usize;
            let n = core::cmp::min(data.len() - written, bs - within);

            let mut buf = alloc::vec![0u8; bs];
            if let Some(old) = self.get_extent(ino, block_off).await? {
                self.vol.read_data_block(old, &mut buf).await?;
                self.vol.free_data_block(old);
            }
            buf[within..within + n].copy_from_slice(&data[written..written + n]);
            let ptr = self.vol.alloc_data_block(&buf).await?;
            self.vol
                .insert(FsKey::extent(ino, block_off), FsValue::Extent(ptr))
                .await?;
            written += n;
        }

        let end = offset + data.len() as u64;
        if end > inode.size {
            inode.size = end;
        }
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.write_inode(ino, inode).await?;
        let blocks = (data.len() / bs) as u64 + 1;
        self.checkpoint(blocks).await
    }

    /// Reads up to `len` bytes from byte `offset` of file `ino`. Bytes past EOF
    /// are omitted; unwritten holes read as zero.
    ///
    /// # Errors
    /// [`StorageError::NotFound`] or I/O / verification errors.
    // Within-block offsets are bounded by the block size, which fits `usize`.
    #[allow(clippy::cast_possible_truncation)]
    pub async fn read(&mut self, ino: u64, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        let bs = self.vol.block_size();
        let bs64 = bs as u64;
        let inode = self.read_inode(ino).await?;
        if offset >= inode.size {
            return Ok(Vec::new());
        }
        let end = core::cmp::min(offset + len as u64, inode.size);
        let mut out = alloc::vec![0u8; (end - offset) as usize];

        let mut pos = offset;
        while pos < end {
            let block_off = (pos / bs64) * bs64;
            let within = (pos - block_off) as usize;
            let n = core::cmp::min((end - pos) as usize, bs - within);
            if let Some(ptr) = self.get_extent(ino, block_off).await? {
                let mut buf = alloc::vec![0u8; bs];
                self.vol.read_data_block(ptr, &mut buf).await?;
                let o = (pos - offset) as usize;
                out[o..o + n].copy_from_slice(&buf[within..within + n]);
            }
            pos += n as u64;
        }
        Ok(out)
    }

    /// Sets the size of file `ino`, freeing any data blocks fully beyond it.
    /// (Growing leaves a sparse hole; shrinking reclaims trailing blocks.)
    ///
    /// # Errors
    /// [`StorageError::NotFound`] or I/O errors.
    // Within-block offsets are bounded by the block size, which fits `usize`.
    #[allow(clippy::cast_possible_truncation)]
    pub async fn truncate(&mut self, ino: u64, size: u64) -> Result<(), StorageError> {
        let bs64 = self.vol.block_size() as u64;
        let mut inode = self.read_inode(ino).await?;
        let drop_from = size.div_ceil(bs64) * bs64; // first block fully past `size`

        let mut exts = Vec::new();
        self.vol
            .range(
                FsKey::extent(ino, drop_from),
                FsKey::new(ino, KIND_EXTENT + 1, 0),
                &mut exts,
            )
            .await?;
        for (key, value) in exts {
            if let FsValue::Extent(ptr) = value {
                self.vol.free_data_block(ptr);
            }
            self.vol.delete(&key).await?;
        }

        // Zero the tail of the block straddling `size`, so a later grow reads
        // zeros there (POSIX) rather than stale bytes.
        let block_off = (size / bs64) * bs64;
        let within = (size - block_off) as usize;
        if within != 0
            && let Some(old) = self.get_extent(ino, block_off).await?
        {
            let bs = self.vol.block_size();
            let mut buf = alloc::vec![0u8; bs];
            self.vol.read_data_block(old, &mut buf).await?;
            buf[within..].fill(0);
            self.vol.free_data_block(old);
            let ptr = self.vol.alloc_data_block(&buf).await?;
            self.vol.insert(FsKey::extent(ino, block_off), FsValue::Extent(ptr)).await?;
        }

        inode.size = size;
        inode.mtime = self.now;
        inode.ctime = self.now;
        self.write_inode(ino, inode).await?;
        self.checkpoint(1).await
    }

    /// Creates a symbolic link `name` in `parent` pointing at `target` bytes.
    ///
    /// # Errors
    /// [`StorageError::AlreadyExists`], [`StorageError::NotADirectory`], or I/O.
    pub async fn symlink(&mut self, parent: u64, name: &[u8], target: &[u8]) -> Result<u64, StorageError> {
        if !self.read_inode(parent).await?.is_dir() {
            return Err(StorageError::NotADirectory);
        }
        let ino = self.alloc_ino().await?;
        self.write_inode(ino, Inode::new_symlink().stamped(self.now)).await?;
        self.write(ino, 0, target).await?; // store target as the link's data
        self.link(parent, name, ino).await?;
        self.bump_times(parent).await?;
        self.checkpoint(1).await?;
        Ok(ino)
    }

    /// Reads a symbolic link's target.
    ///
    /// # Errors
    /// [`StorageError::NotPermitted`] if `ino` is not a symlink, or I/O.
    pub async fn readlink(&mut self, ino: u64) -> Result<Vec<u8>, StorageError> {
        let inode = self.read_inode(ino).await?;
        if !inode.is_symlink() {
            return Err(StorageError::NotPermitted);
        }
        self.read(ino, 0, usize::try_from(inode.size).unwrap_or(usize::MAX)).await
    }

    /// Creates a hard link `name` in `parent` to the existing file `target`.
    ///
    /// # Errors
    /// [`StorageError::NotPermitted`] (linking a directory),
    /// [`StorageError::AlreadyExists`], or I/O.
    pub async fn hard_link(&mut self, parent: u64, name: &[u8], target: u64) -> Result<(), StorageError> {
        let mut inode = self.read_inode(target).await?;
        if inode.is_dir() {
            return Err(StorageError::NotPermitted);
        }
        self.link(parent, name, target).await?;
        inode.nlink += 1;
        inode.ctime = self.now;
        self.write_inode(target, inode).await?;
        self.bump_times(parent).await?;
        self.checkpoint(1).await
    }

    /// `(block_size, total_blocks, free_blocks)` for `statfs`/`df`.
    #[must_use]
    pub fn statfs(&self) -> (usize, u64, u64) {
        let (total, free) = self.vol.space();
        (self.vol.block_size(), total, free)
    }
}

#[cfg(test)]
mod tests {
    use super::{Filesystem, ROOT_INO};
    use crate::allocator::{SegmentAllocator, SegmentGeom};
    use crate::checksum::DigestMode;
    use crate::error::StorageError;
    use crate::testkit::{MemDevice, block_on};

    const BS: usize = 4096;
    const BLOCKS: u64 = 16_384;

    fn alloc() -> SegmentAllocator {
        SegmentAllocator::new(SegmentGeom::new(BLOCKS, 64, 1).unwrap())
    }

    type Fs = Filesystem<SegmentAllocator, MemDevice>;

    fn fresh() -> Fs {
        block_on(Filesystem::format(MemDevice::new(BS, BLOCKS), alloc(), DigestMode::Fast64)).unwrap()
    }

    fn names(mut entries: alloc::vec::Vec<(alloc::vec::Vec<u8>, u64)>) -> alloc::vec::Vec<alloc::vec::Vec<u8>> {
        entries.sort();
        entries.into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn mkdir_create_lookup_readdir() {
        let mut fs = fresh();
        let docs = block_on(fs.mkdir(ROOT_INO, b"docs", 0o755)).unwrap();
        let readme = block_on(fs.create(ROOT_INO, b"README", 0o644)).unwrap();
        let notes = block_on(fs.create(docs, b"notes.txt", 0o644)).unwrap();

        assert_eq!(block_on(fs.lookup(ROOT_INO, b"docs")).unwrap(), Some(docs));
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"README")).unwrap(), Some(readme));
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"missing")).unwrap(), None);
        assert_eq!(block_on(fs.resolve(&[b"docs", b"notes.txt"])).unwrap(), notes);

        assert_eq!(names(block_on(fs.readdir(ROOT_INO)).unwrap()), [b"README".to_vec(), b"docs".to_vec()]);
        assert_eq!(names(block_on(fs.readdir(docs)).unwrap()), [b"notes.txt".to_vec()]);

        assert!(block_on(fs.getattr(docs)).unwrap().is_dir());
        assert!(!block_on(fs.getattr(readme)).unwrap().is_dir());
    }

    #[test]
    fn duplicate_name_rejected() {
        let mut fs = fresh();
        block_on(fs.create(ROOT_INO, b"a", 0o644)).unwrap();
        assert!(matches!(
            block_on(fs.create(ROOT_INO, b"a", 0o644)),
            Err(StorageError::AlreadyExists)
        ));
    }

    #[test]
    fn unlink_and_rmdir() {
        let mut fs = fresh();
        block_on(fs.create(ROOT_INO, b"f", 0o644)).unwrap();
        let d = block_on(fs.mkdir(ROOT_INO, b"d", 0o755)).unwrap();
        block_on(fs.create(d, b"inner", 0o644)).unwrap();

        block_on(fs.unlink(ROOT_INO, b"f")).unwrap();
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"f")).unwrap(), None);

        // Non-empty dir cannot be removed.
        assert!(matches!(block_on(fs.rmdir(ROOT_INO, b"d")), Err(StorageError::NotEmpty)));
        block_on(fs.unlink(d, b"inner")).unwrap();
        block_on(fs.rmdir(ROOT_INO, b"d")).unwrap();
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"d")).unwrap(), None);
        assert!(block_on(fs.readdir(ROOT_INO)).unwrap().is_empty());
    }

    #[test]
    fn survives_remount() {
        let media;
        let docs;
        {
            let mut fs = fresh();
            docs = block_on(fs.mkdir(ROOT_INO, b"docs", 0o755)).unwrap();
            block_on(fs.create(docs, b"a", 0o644)).unwrap();
            block_on(fs.create(docs, b"b", 0o644)).unwrap();
            block_on(fs.sync()).unwrap();
            media = fs.device().snapshot();
        }
        let mut fs: Fs = block_on(Filesystem::open(media, alloc())).unwrap();
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"docs")).unwrap(), Some(docs));
        assert_eq!(names(block_on(fs.readdir(docs)).unwrap()), [b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn file_write_read_spanning_blocks_and_holes() {
        let mut fs = fresh();
        let f = block_on(fs.create(ROOT_INO, b"data", 0o644)).unwrap();

        // Write across a block boundary (BS=4096): offset 4090, 20 bytes.
        let payload: alloc::vec::Vec<u8> = (0..20u8).collect();
        block_on(fs.write(f, 4090, &payload)).unwrap();
        assert_eq!(block_on(fs.getattr(f)).unwrap().size, 4110);

        // Read it back exactly.
        assert_eq!(block_on(fs.read(f, 4090, 20)).unwrap(), payload);
        // The gap [0, 4090) is a hole → zeros.
        assert_eq!(block_on(fs.read(f, 0, 8)).unwrap(), [0u8; 8]);
        // Read past EOF is bounded by size.
        assert_eq!(block_on(fs.read(f, 4100, 1000)).unwrap().len(), 10);

        // Overwrite within the first written block.
        block_on(fs.write(f, 4090, &[0xAA, 0xBB])).unwrap();
        let r = block_on(fs.read(f, 4090, 4)).unwrap();
        assert_eq!(r, [0xAA, 0xBB, 2, 3]);
    }

    #[test]
    fn file_data_survives_remount() {
        let media;
        let f;
        {
            let mut fs = fresh();
            f = block_on(fs.create(ROOT_INO, b"big", 0o644)).unwrap();
            let blob: alloc::vec::Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
            block_on(fs.write(f, 0, &blob)).unwrap();
            block_on(fs.sync()).unwrap();
            media = fs.device().snapshot();
        }
        let mut fs: Fs = block_on(Filesystem::open(media, alloc())).unwrap();
        let got = block_on(fs.read(f, 0, 10_000)).unwrap();
        assert_eq!(got.len(), 10_000);
        assert!(got.iter().enumerate().all(|(i, &b)| b == (i % 256) as u8));
    }

    #[test]
    fn truncate_shrinks_and_grows() {
        let mut fs = fresh();
        let f = block_on(fs.create(ROOT_INO, b"t", 0o644)).unwrap();
        let blob = alloc::vec![7u8; 20_000];
        block_on(fs.write(f, 0, &blob)).unwrap();

        block_on(fs.truncate(f, 5)).unwrap();
        assert_eq!(block_on(fs.getattr(f)).unwrap().size, 5);
        assert_eq!(block_on(fs.read(f, 0, 100)).unwrap(), [7u8; 5]);

        // Grow back: the gap is a hole (zeros), not the old data.
        block_on(fs.truncate(f, 10)).unwrap();
        assert_eq!(block_on(fs.read(f, 0, 100)).unwrap(), [7, 7, 7, 7, 7, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn symlink_roundtrip() {
        let mut fs = fresh();
        let s = block_on(fs.symlink(ROOT_INO, b"link", b"/some/where/target")).unwrap();
        assert!(block_on(fs.getattr(s)).unwrap().is_symlink());
        assert_eq!(block_on(fs.readlink(s)).unwrap(), b"/some/where/target");
        let f = block_on(fs.create(ROOT_INO, b"f", 0o644)).unwrap();
        assert!(matches!(block_on(fs.readlink(f)), Err(StorageError::NotPermitted)));
    }

    #[test]
    fn hard_link_shares_inode() {
        let mut fs = fresh();
        let a = block_on(fs.create(ROOT_INO, b"a", 0o644)).unwrap();
        block_on(fs.write(a, 0, b"shared")).unwrap();
        block_on(fs.hard_link(ROOT_INO, b"b", a)).unwrap();

        let b = block_on(fs.lookup(ROOT_INO, b"b")).unwrap().unwrap();
        assert_eq!(b, a, "hard link shares the inode");
        assert_eq!(block_on(fs.getattr(a)).unwrap().nlink, 2);

        // A write via one name is visible via the other.
        block_on(fs.write(a, 0, b"UPDATED")).unwrap();
        assert_eq!(block_on(fs.read(b, 0, 100)).unwrap(), b"UPDATED");

        // Unlinking one name keeps the inode (nlink drops to 1).
        block_on(fs.unlink(ROOT_INO, b"a")).unwrap();
        assert_eq!(block_on(fs.getattr(b)).unwrap().nlink, 1);
        assert_eq!(block_on(fs.read(b, 0, 100)).unwrap(), b"UPDATED");

        // Directories cannot be hard-linked.
        let d = block_on(fs.mkdir(ROOT_INO, b"d", 0o755)).unwrap();
        assert!(matches!(
            block_on(fs.hard_link(ROOT_INO, b"e", d)),
            Err(StorageError::NotPermitted)
        ));
    }

    #[test]
    fn timestamps_track_operations() {
        let mut fs = fresh();
        fs.set_time(1000);
        let f = block_on(fs.create(ROOT_INO, b"a", 0o644)).unwrap();
        let i = block_on(fs.getattr(f)).unwrap();
        assert_eq!((i.atime, i.mtime, i.ctime), (1000, 1000, 1000), "new file stamped");
        assert_eq!(block_on(fs.getattr(ROOT_INO)).unwrap().mtime, 1000, "parent mtime bumped");

        fs.set_time(2000);
        block_on(fs.write(f, 0, b"x")).unwrap();
        let i2 = block_on(fs.getattr(f)).unwrap();
        assert_eq!(i2.mtime, 2000, "write bumps mtime");
        assert_eq!(i2.atime, 1000, "write leaves atime");

        fs.set_time(3000);
        block_on(fs.set_times(f, Some(500), Some(600))).unwrap();
        let i3 = block_on(fs.getattr(f)).unwrap();
        assert_eq!((i3.atime, i3.mtime, i3.ctime), (500, 600, 3000), "utimes sets a/mtime, ctime=now");
    }

    #[test]
    fn statfs_reports_and_tracks_space() {
        let mut fs = fresh();
        let (bs, total, free0) = fs.statfs();
        assert_eq!(bs, 4096);
        assert!(total > 0 && free0 > 0 && free0 <= total);

        let f = block_on(fs.create(ROOT_INO, b"big", 0o644)).unwrap();
        block_on(fs.write(f, 0, &alloc::vec![1u8; 2_000_000])).unwrap();
        let (_, _, free1) = fs.statfs();
        assert!(free1 < free0, "writing ~2 MiB should reduce free space: {free0} -> {free1}");
    }

    #[test]
    fn metadata_ops_batch_until_sync() {
        let mut fs = fresh();
        let before = fs.device().write_count();
        for i in 0..100u64 {
            let name = alloc::format!("d{i}");
            block_on(fs.mkdir(ROOT_INO, name.as_bytes(), 0o755)).unwrap();
        }
        // No sync yet: everything is still in the in-memory txg shadow, so the
        // 100 mkdirs caused (almost) no device writes — they coalesce.
        let during = fs.device().write_count() - before;
        assert!(during < 20, "expected batching, got {during} device writes for 100 mkdirs");

        block_on(fs.sync()).unwrap();
        assert!(fs.device().write_count() > before, "sync must flush to device");
        // And all 100 are durable + visible.
        assert_eq!(block_on(fs.readdir(ROOT_INO)).unwrap().len(), 100);
    }

    #[test]
    fn rename_moves_files_and_dirs() {
        let mut fs = fresh();
        let a = block_on(fs.mkdir(ROOT_INO, b"a", 0o755)).unwrap();
        let b = block_on(fs.mkdir(ROOT_INO, b"b", 0o755)).unwrap();
        let f = block_on(fs.create(a, b"f", 0o644)).unwrap();
        block_on(fs.write(f, 0, b"payload")).unwrap();

        // Move file a/f -> b/g, content preserved, old gone.
        block_on(fs.rename(a, b"f", b, b"g")).unwrap();
        assert_eq!(block_on(fs.lookup(a, b"f")).unwrap(), None);
        let g = block_on(fs.lookup(b, b"g")).unwrap().unwrap();
        assert_eq!(g, f);
        assert_eq!(block_on(fs.read(g, 0, 100)).unwrap(), b"payload");

        // Move a (with a nested file) under b: b/a/inner reachable.
        block_on(fs.create(a, b"inner", 0o644)).unwrap();
        block_on(fs.rename(ROOT_INO, b"a", b, b"a")).unwrap();
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"a")).unwrap(), None);
        assert_eq!(block_on(fs.resolve(&[b"b", b"a", b"inner"])).unwrap() > 0, true);
    }

    #[test]
    fn rename_replaces_and_rejects() {
        let mut fs = fresh();
        let src = block_on(fs.create(ROOT_INO, b"src", 0o644)).unwrap();
        block_on(fs.write(src, 0, b"NEW")).unwrap();
        let dst = block_on(fs.create(ROOT_INO, b"dst", 0o644)).unwrap();
        block_on(fs.write(dst, 0, b"OLD-and-longer")).unwrap();

        // Replace dst with src.
        block_on(fs.rename(ROOT_INO, b"src", ROOT_INO, b"dst")).unwrap();
        assert_eq!(block_on(fs.lookup(ROOT_INO, b"src")).unwrap(), None);
        let d = block_on(fs.lookup(ROOT_INO, b"dst")).unwrap().unwrap();
        assert_eq!(d, src);
        assert_eq!(block_on(fs.read(d, 0, 100)).unwrap(), b"NEW");

        // Type mismatch and non-empty-dir rejection.
        let dir = block_on(fs.mkdir(ROOT_INO, b"dir", 0o755)).unwrap();
        block_on(fs.create(dir, b"child", 0o644)).unwrap();
        assert!(matches!(
            block_on(fs.rename(ROOT_INO, b"dst", ROOT_INO, b"dir")),
            Err(StorageError::NotADirectory)
        ));
        let dir2 = block_on(fs.mkdir(ROOT_INO, b"dir2", 0o755)).unwrap();
        let _ = dir2;
        assert!(matches!(
            block_on(fs.rename(ROOT_INO, b"dir2", ROOT_INO, b"dir")),
            Err(StorageError::NotEmpty)
        ));
    }

    #[test]
    fn unlink_reclaims_data_blocks() {
        // Device is 64 MiB; cycle far more than that through a reused name. If
        // unlink leaked data extents this would hit NoSpace and panic on unwrap.
        let mut fs = fresh();
        let blob = alloc::vec![0x5Au8; 1 << 20]; // 1 MiB (256 blocks)
        for _ in 0..200 {
            let f = block_on(fs.create(ROOT_INO, b"tmp", 0o644)).unwrap();
            block_on(fs.write(f, 0, &blob)).unwrap();
            block_on(fs.unlink(ROOT_INO, b"tmp")).unwrap();
        }
        // ~200 MiB cycled through a 64 MiB device — only possible if reclaimed.
        assert!(block_on(fs.readdir(ROOT_INO)).unwrap().is_empty());
    }

    #[test]
    fn many_files_with_data_survive_remount() {
        // Mirrors the FUSE torture in-process: a multi-level tree of files with
        // varied-size data, verified byte-for-byte after remount.
        let media;
        {
            let mut fs = fresh();
            let d = block_on(fs.mkdir(ROOT_INO, b"d", 0o755)).unwrap();
            for i in 0..80u64 {
                let parent = if i % 2 == 0 { ROOT_INO } else { d };
                let name = alloc::format!("f{i}");
                let f = block_on(fs.create(parent, name.as_bytes(), 0o644)).unwrap();
                let len = usize::try_from((i * 37) % 5000 + 1).unwrap();
                let byte = u8::try_from(i % 251).unwrap();
                block_on(fs.write(f, 0, &alloc::vec![byte; len])).unwrap();
            }
            block_on(fs.sync()).unwrap();
            media = fs.device().snapshot();
        }

        let mut fs: Fs = block_on(Filesystem::open(media, alloc())).unwrap();
        // 40 even-numbered files at root + the "d" directory.
        assert_eq!(block_on(fs.readdir(ROOT_INO)).unwrap().len(), 41);
        let d = block_on(fs.lookup(ROOT_INO, b"d")).unwrap().unwrap();
        assert_eq!(block_on(fs.readdir(d)).unwrap().len(), 40);

        for i in 0..80u64 {
            let parent = if i % 2 == 0 { ROOT_INO } else { d };
            let name = alloc::format!("f{i}");
            let f = block_on(fs.lookup(parent, name.as_bytes())).unwrap().unwrap();
            let len = usize::try_from((i * 37) % 5000 + 1).unwrap();
            let byte = u8::try_from(i % 251).unwrap();
            let got = block_on(fs.read(f, 0, len + 16)).unwrap();
            assert_eq!(got.len(), len, "file f{i} size after remount");
            assert!(got.iter().all(|&b| b == byte), "file f{i} content after remount");
        }
    }

    #[test]
    fn many_entries_span_multiple_leaves() {
        // Enough entries (and a small-ish tree) to force splits in the dirent range.
        let mut fs = fresh();
        for i in 0..200u64 {
            let name = alloc::format!("file{i:04}");
            block_on(fs.create(ROOT_INO, name.as_bytes(), 0o644)).unwrap();
        }
        let listing = block_on(fs.readdir(ROOT_INO)).unwrap();
        assert_eq!(listing.len(), 200);
        // Every created name is present and resolves.
        for i in 0..200u64 {
            let name = alloc::format!("file{i:04}");
            assert!(block_on(fs.lookup(ROOT_INO, name.as_bytes())).unwrap().is_some());
        }
    }
}
