//! In-process micro-benchmark of the `rfs-core` engine.
//!
//! This drives the filesystem directly over an in-memory `MemDevice` (the
//! `testkit` device): no FUSE, no syscalls, no real disk. It therefore measures
//! the *engine itself* — the CoW Merkle B+-tree, segment allocator, checksums
//! and txg commit path — which is the number most relevant to the eventual
//! in-kernel / exokernel target. The FUSE adapter (`rfs-fuse`) plus `fio` give
//! the "usable today" figure; this gives the ceiling.
//!
//! Build/run:  `cargo run --release --features bench --bin rfs-bench`
//! Args (all optional):  `--mb <seq MiB> --rand <ops> --files <n> --fsync <n>`

use std::future::Future;
use std::time::Instant;

use rfs_core::allocator::{SegmentAllocator, SegmentGeom};
use rfs_core::checksum::DigestMode;
use rfs_core::fs::{Filesystem, ROOT_INO, S_IFREG};
use rfs_core::testkit::MemDevice;

const BLOCK_SIZE: usize = 4096;
const BLOCKS_PER_SEGMENT: u32 = 512;
const RESERVED_SEGMENTS: u32 = 1;

/// Minimal executor: the in-memory device is always immediately ready, so this
/// poll loop never actually spins (same approach as the FUSE adapter).
fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Tiny xorshift64* PRNG — deterministic, no external deps.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

type Fs = Filesystem<SegmentAllocator, MemDevice>;

/// Fresh, freshly-formatted filesystem on a `bytes`-sized RAM device.
fn fresh_fs(bytes: u64) -> Fs {
    let blocks = bytes / BLOCK_SIZE as u64;
    let dev = MemDevice::new(BLOCK_SIZE, blocks);
    let geom = SegmentGeom::new(blocks, BLOCKS_PER_SEGMENT, RESERVED_SEGMENTS)
        .expect("geometry");
    block_on(Filesystem::format(dev, SegmentAllocator::new(geom), DigestMode::Fast64))
        .expect("format")
}

fn mbps(bytes: u64, secs: f64) -> f64 {
    (bytes as f64) / secs / 1.0e6
}

fn row(label: &str, value: f64, unit: &str) {
    println!("{label:<28} {value:>12.1} {unit}");
}

fn main() {
    // --- args -------------------------------------------------------------
    let mut seq_mb: u64 = 256;
    let mut rand_ops: u64 = 50_000;
    let mut n_files: u64 = 20_000;
    let mut fsync_ops: u64 = 5_000;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        match a.as_str() {
            "--mb" => seq_mb = val(),
            "--rand" => rand_ops = val(),
            "--files" => n_files = val(),
            "--fsync" => fsync_ops = val(),
            _ => {}
        }
    }

    println!("# rfs-core engine micro-benchmark (in-memory device, no FUSE)");
    println!("# block={BLOCK_SIZE}B  seq={seq_mb}MiB  rand={rand_ops}x4K  files={n_files}  fsync={fsync_ops}");
    println!("{:<28} {:>12} unit", "metric", "value");
    println!("{}", "-".repeat(48));

    // Device sized to hold the live set plus CoW churn headroom.
    let data_dev = (seq_mb * 1024 * 1024 * 4).max(256 * 1024 * 1024);
    let chunk = 64 * 1024usize; // 64 KiB application write size

    // --- sequential write -------------------------------------------------
    {
        let mut fs = fresh_fs(data_dev);
        let ino = block_on(fs.create(ROOT_INO, b"seq", S_IFREG | 0o644)).unwrap();
        let buf = vec![0xABu8; chunk];
        let total = seq_mb * 1024 * 1024;
        let t = Instant::now();
        let mut off = 0u64;
        while off < total {
            block_on(fs.write(ino, off, &buf)).unwrap();
            off += chunk as u64;
        }
        block_on(fs.sync()).unwrap();
        let s = t.elapsed().as_secs_f64();
        row("seq write", mbps(total, s), "MB/s");

        // --- sequential read (same fs, data resident) ---------------------
        let t = Instant::now();
        let mut off = 0u64;
        while off < total {
            let v = block_on(fs.read(ino, off, chunk)).unwrap();
            std::hint::black_box(&v);
            off += chunk as u64;
        }
        let s = t.elapsed().as_secs_f64();
        row("seq read", mbps(total, s), "MB/s");
    }

    // --- random 4K write / read ------------------------------------------
    {
        let mut fs = fresh_fs(data_dev);
        let ino = block_on(fs.create(ROOT_INO, b"rand", S_IFREG | 0o644)).unwrap();
        let file_bytes = (seq_mb * 1024 * 1024).max(64 * 1024 * 1024);
        let nblocks = file_bytes / BLOCK_SIZE as u64;
        // Lay the file down first so random writes are overwrites (CoW path).
        let zero = vec![0u8; BLOCK_SIZE];
        let mut off = 0u64;
        while off < file_bytes {
            block_on(fs.write(ino, off, &zero)).unwrap();
            off += BLOCK_SIZE as u64;
        }
        block_on(fs.sync()).unwrap();

        let blk = vec![0xCDu8; BLOCK_SIZE];
        let mut rng = Rng(0x1234_5678_9abc_def0);
        let t = Instant::now();
        for _ in 0..rand_ops {
            let b = rng.next() % nblocks;
            block_on(fs.write(ino, b * BLOCK_SIZE as u64, &blk)).unwrap();
        }
        block_on(fs.sync()).unwrap();
        let s = t.elapsed().as_secs_f64();
        row("rand 4K write", rand_ops as f64 / s, "IOPS");

        let mut rng = Rng(0x0fed_cba9_8765_4321);
        let t = Instant::now();
        for _ in 0..rand_ops {
            let b = rng.next() % nblocks;
            let v = block_on(fs.read(ino, b * BLOCK_SIZE as u64, BLOCK_SIZE)).unwrap();
            std::hint::black_box(&v);
        }
        let s = t.elapsed().as_secs_f64();
        row("rand 4K read", rand_ops as f64 / s, "IOPS");
    }

    // --- metadata: create / stat / unlink --------------------------------
    {
        let mut fs = fresh_fs(256 * 1024 * 1024);
        let mut names: Vec<Vec<u8>> = Vec::with_capacity(n_files as usize);
        for i in 0..n_files {
            names.push(format!("f{i:08}").into_bytes());
        }
        let t = Instant::now();
        for n in &names {
            block_on(fs.create(ROOT_INO, n, S_IFREG | 0o644)).unwrap();
        }
        block_on(fs.sync()).unwrap();
        let s = t.elapsed().as_secs_f64();
        row("create (empty files)", n_files as f64 / s, "ops/s");

        let t = Instant::now();
        for n in &names {
            let ino = block_on(fs.lookup(ROOT_INO, n)).unwrap().unwrap();
            let a = block_on(fs.getattr(ino)).unwrap();
            std::hint::black_box(&a);
        }
        let s = t.elapsed().as_secs_f64();
        row("lookup+stat", n_files as f64 / s, "ops/s");

        let t = Instant::now();
        for n in &names {
            block_on(fs.unlink(ROOT_INO, n)).unwrap();
        }
        block_on(fs.sync()).unwrap();
        let s = t.elapsed().as_secs_f64();
        row("unlink", n_files as f64 / s, "ops/s");
    }

    // --- fsync latency: overwrite 4K + durable commit, repeated -----------
    {
        let mut fs = fresh_fs(256 * 1024 * 1024);
        let ino = block_on(fs.create(ROOT_INO, b"log", S_IFREG | 0o644)).unwrap();
        let blk = vec![0x5Au8; BLOCK_SIZE];
        let t = Instant::now();
        for _ in 0..fsync_ops {
            block_on(fs.write(ino, 0, &blk)).unwrap();
            block_on(fs.sync()).unwrap();
        }
        let s = t.elapsed().as_secs_f64();
        row("fsync (4K + commit)", fsync_ops as f64 / s, "ops/s");
        row("fsync mean latency", s / fsync_ops as f64 * 1.0e6, "us");
    }

    println!("{}", "-".repeat(48));
    println!("# done");
}
