//! Mounts an RFS [`Filesystem`] as a real Linux directory via FUSE.
//!
//! Usage: `rfs-fuse <image-file> <mountpoint>`. A fresh/empty image is formatted;
//! an existing one is reopened. The core engine stays `no_std`; this binary
//! provides a file-backed [`BlockDevice`], a trivial `block_on` (the engine's
//! futures are immediately ready over synchronous I/O), and translation between
//! FUSE calls and the engine's filesystem API.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem as FuseFs, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, Request,
};

use rfs_core::allocator::{SegmentAllocator, SegmentGeom};
use rfs_core::checksum::DigestMode;
use rfs_core::device::BlockDevice;
use rfs_core::error::StorageError;
use rfs_core::fs::{Filesystem, Inode, ROOT_INO};

const BLOCK_SIZE: usize = 4096;
const BLOCKS_PER_SEGMENT: u32 = 512;
const RESERVED_SEGMENTS: u32 = 1;
const TTL: Duration = Duration::from_secs(1);

/// Drives an immediately-ready future to completion (no real executor needed).
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// A `BlockDevice` backed by a regular file (one image = one volume).
struct FileDevice {
    file: std::fs::File,
    blocks: u64,
}

impl FileDevice {
    fn offset(&self, lba: u64) -> Result<u64, StorageError> {
        if lba >= self.blocks {
            return Err(StorageError::OutOfBounds);
        }
        Ok(lba * BLOCK_SIZE as u64)
    }
}

impl BlockDevice for FileDevice {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
    fn block_count(&self) -> u64 {
        self.blocks
    }
    async fn read_block(&self, lba: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let off = self.offset(lba)?;
        self.file.read_exact_at(buf, off).map_err(|_| StorageError::Io)
    }
    async fn write_block(&self, lba: u64, buf: &[u8]) -> Result<(), StorageError> {
        let off = self.offset(lba)?;
        self.file.write_all_at(buf, off).map_err(|_| StorageError::Io)
    }
    async fn flush(&self) -> Result<(), StorageError> {
        self.file.sync_data().map_err(|_| StorageError::Io)
    }
}

fn geom(blocks: u64) -> SegmentGeom {
    SegmentGeom::new(blocks, BLOCKS_PER_SEGMENT, RESERVED_SEGMENTS)
        .expect("image too small for RFS geometry")
}

/// Maps engine errors to errno for FUSE replies.
fn errno(err: &StorageError) -> i32 {
    match err {
        StorageError::NotFound => libc::ENOENT,
        StorageError::AlreadyExists => libc::EEXIST,
        StorageError::NotADirectory => libc::ENOTDIR,
        StorageError::NotEmpty => libc::ENOTEMPTY,
        StorageError::NotPermitted => libc::EPERM,
        _ => libc::EIO,
    }
}

/// Seconds since the Unix epoch (0 if the clock is before the epoch).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Adapter {
    fs: Filesystem<SegmentAllocator, FileDevice>,
    uid: u32,
    gid: u32,
}

impl Adapter {
    /// Injects the current wall clock before a mutating operation.
    fn tick(&mut self) {
        self.fs.set_time(now_secs());
    }

    fn attr(&self, ino: u64, inode: &Inode) -> FileAttr {
        let kind = if inode.is_dir() {
            FileType::Directory
        } else if inode.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };
        FileAttr {
            ino,
            size: inode.size,
            blocks: inode.size.div_ceil(512),
            atime: UNIX_EPOCH + Duration::from_secs(inode.atime),
            mtime: UNIX_EPOCH + Duration::from_secs(inode.mtime),
            ctime: UNIX_EPOCH + Duration::from_secs(inode.ctime),
            crtime: UNIX_EPOCH + Duration::from_secs(inode.ctime),
            kind,
            perm: (inode.mode & 0o7777) as u16,
            nlink: inode.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE as u32,
            flags: 0,
        }
    }
}

impl FuseFs for Adapter {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match block_on(self.fs.lookup(parent, name.as_bytes())) {
            Ok(Some(ino)) => match block_on(self.fs.getattr(ino)) {
                Ok(inode) => reply.entry(&TTL, &self.attr(ino, &inode), 0),
                Err(e) => reply.error(errno(&e)),
            },
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match block_on(self.fs.getattr(ino)) {
            Ok(inode) => reply.attr(&TTL, &self.attr(ino, &inode)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        self.tick();
        match block_on(self.fs.mkdir(parent, name.as_bytes(), mode)) {
            Ok(ino) => match block_on(self.fs.getattr(ino)) {
                Ok(inode) => reply.entry(&TTL, &self.attr(ino, &inode), 0),
                Err(e) => reply.error(errno(&e)),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        self.tick();
        match block_on(self.fs.create(parent, name.as_bytes(), mode)) {
            Ok(ino) => match block_on(self.fs.getattr(ino)) {
                Ok(inode) => reply.created(&TTL, &self.attr(ino, &inode), 0, 0, 0),
                Err(e) => reply.error(errno(&e)),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        match block_on(self.fs.read(ino, offset.max(0) as u64, size as usize)) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        self.tick();
        match block_on(self.fs.write(ino, offset.max(0) as u64, data)) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        self.tick();
        if let Some(new_size) = size {
            if let Err(e) = block_on(self.fs.truncate(ino, new_size)) {
                reply.error(errno(&e));
                return;
            }
        }
        if atime.is_some() || mtime.is_some() {
            let to_secs = |t: fuser::TimeOrNow| match t {
                fuser::TimeOrNow::Now => now_secs(),
                fuser::TimeOrNow::SpecificTime(st) => st
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            if let Err(e) = block_on(self.fs.set_times(ino, atime.map(to_secs), mtime.map(to_secs))) {
                reply.error(errno(&e));
                return;
            }
        }
        match block_on(self.fs.getattr(ino)) {
            Ok(inode) => reply.attr(&TTL, &self.attr(ino, &inode)),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.tick();
        match block_on(self.fs.unlink(parent, name.as_bytes())) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.tick();
        match block_on(self.fs.rmdir(parent, name.as_bytes())) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        self.tick();
        match block_on(self.fs.rename(
            parent,
            name.as_bytes(),
            newparent,
            newname.as_bytes(),
        )) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
        match block_on(self.fs.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn fsync(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match block_on(self.fs.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn fsyncdir(&mut self, _req: &Request, _ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        match block_on(self.fs.sync()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn destroy(&mut self) {
        let _ = block_on(self.fs.sync()); // flush on unmount
    }

    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        let (bsize, total, free) = self.fs.statfs();
        let bsize = bsize as u32;
        // blocks, bfree, bavail, files, ffree, bsize, namelen, frsize
        reply.statfs(total, free, free, 0, free, bsize, 255, bsize);
    }

    fn symlink(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        self.tick();
        let target = link.as_os_str().as_bytes();
        match block_on(self.fs.symlink(parent, name.as_bytes(), target)) {
            Ok(ino) => match block_on(self.fs.getattr(ino)) {
                Ok(inode) => reply.entry(&TTL, &self.attr(ino, &inode), 0),
                Err(e) => reply.error(errno(&e)),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        match block_on(self.fs.readlink(ino)) {
            Ok(target) => reply.data(&target),
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        self.tick();
        match block_on(self.fs.hard_link(newparent, newname.as_bytes(), ino)) {
            Ok(()) => match block_on(self.fs.getattr(ino)) {
                Ok(inode) => reply.entry(&TTL, &self.attr(ino, &inode), 0),
                Err(e) => reply.error(errno(&e)),
            },
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let mut listing: Vec<(u64, FileType, Vec<u8>)> = vec![
            (ino, FileType::Directory, b".".to_vec()),
            (ROOT_INO, FileType::Directory, b"..".to_vec()),
        ];
        match block_on(self.fs.readdir(ino)) {
            Ok(entries) => {
                for (name, child) in entries {
                    let kind = match block_on(self.fs.getattr(child)) {
                        Ok(i) if i.is_dir() => FileType::Directory,
                        _ => FileType::RegularFile,
                    };
                    listing.push((child, kind, name));
                }
            }
            Err(e) => {
                reply.error(errno(&e));
                return;
            }
        }
        for (i, (cino, kind, name)) in listing.into_iter().enumerate().skip(offset as usize) {
            if reply.add(cino, (i + 1) as i64, kind, OsStr::from_bytes(&name)) {
                break;
            }
        }
        reply.ok();
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(image), Some(mountpoint)) = (args.next(), args.next()) else {
        eprintln!("usage: rfs-fuse <image-file> <mountpoint>");
        std::process::exit(2);
    };

    let existing = std::fs::metadata(&image).map(|m| m.len() > 0).unwrap_or(false);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&image)
        .expect("open image");

    // Size a new image to 64 MiB.
    if !existing {
        file.set_len(64 * 1024 * 1024).expect("size image");
    }
    let blocks = file.metadata().expect("stat image").len() / BLOCK_SIZE as u64;
    let dev = FileDevice { file, blocks };

    let fs = if existing {
        block_on(Filesystem::open(dev, SegmentAllocator::new(geom(blocks)))).expect("open fs")
    } else {
        block_on(Filesystem::format(
            dev,
            SegmentAllocator::new(geom(blocks)),
            DigestMode::Fast64,
        ))
        .expect("format fs")
    };

    // SAFETY: getuid/getgid are always-safe libc calls.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let adapter = Adapter { fs, uid, gid };

    println!("rfs-fuse: mounting {image} at {mountpoint}");
    fuser::mount2(adapter, &mountpoint, &[MountOption::FSName("rfs".into())]).expect("mount");
}
