# M5 — Feox exokernel integration (design)

RFS's first target is the **Feox** exokernel on RISC-V. The integration is a thin
`rfs-feox` adapter that implements `rfs_core::BlockDevice` over Feox's NVMe stack.
This document specifies that adapter and the work `feox-nvme` must land before it
can move bytes.

## Why the seam already fits

The `BlockDevice` seam was shaped for exactly this from day one:

- **Async, completion-driven.** `feox-nvme`'s `NvmeIoFuture` resolves to
  `Result<NvmeCompletion, NvmeError>` — our `async fn read_block/write_block`
  await one such future per op. No bridge, no executor mismatch.
- **Core-local `!Send`.** `NvmeIoFuture` is `!Send` (polled only on the owning
  core); our static-dispatch futures are `!Send`-friendly. Same ownership model.
- **Capability-aware.** `NvmeError::{DeviceRemoved, CapabilityRevoked, StaleSlot}`
  map straight onto `StorageError::{DeviceRemoved, CapabilityRevoked, Io}` — both
  already first-class in the engine.
- **Registered, aligned buffers.** `AlignedBuf` is page-aligned and already carries
  an optional `RegionKey` (lkey/rkey/remote addr) — the slot for the DMA/IOMMU
  handle Feox mints from a physical-page capability.

## Current state of `feox-nvme` (the blocker)

Today `feox-nvme` is an **inflight-tracking model only**: `NvmeQueuePair::submit()`
takes *no arguments* and returns `(cid, NvmeIoFuture)`; `complete(NvmeCompletion)`
resolves the tracked future with a status. There is no command payload (opcode /
LBA / buffer), no submission-queue entry, no doorbell, no completion-queue
processing, and no namespace geometry. So it correctly models command-ID
lifetimes and waker behaviour, but **cannot transfer data** — a functional
`BlockDevice` is not yet possible.

## The adapter (`rfs-feox`), once the data path exists

```text
NvmeBlockDevice {
    ns: NamespaceCap,           // capability to one NVMe namespace
    queue: RefCell<NvmeQueuePair<N>>,   // core-local IO queue (interior mut → &self)
    block_size: usize,          // from Identify Namespace (LBA format)
    block_count: u64,           // NSZE
}

impl BlockDevice for NvmeBlockDevice {
    async fn read_block(&self, lba, buf) {
        // buf is a registered AlignedBuf → DMA address via buf.region()
        let (_cid, fut) = self.queue.borrow_mut()
            .submit(NvmeCommand::read(self.ns, lba, 1, dma_of(buf)))?;
        match fut.await { Ok(c) if c.succeeded() => Ok(()), Ok(c) => Err(map(c)), Err(e) => Err(map(e)) }
    }
    async fn write_block(&self, lba, buf) { /* WRITE (0x01), same shape */ }
    async fn flush(&self)               { /* FLUSH (0x00) → durability barrier */ }
}
```

`SegmentGeom` is built from the namespace block size/capacity; segment size is
chosen to align to the device's optimal write/zone size. Everything above the
seam — allocator, CoW tree, txg/ZIL, snapshots, the VFS — is unchanged.

## What `feox-nvme` must add (the gap list)

1. **Command descriptor on submit.** `submit(cmd)` carrying `{ opcode, nsid,
   slba, nlb, prp1/prp2 | sgl }` — the 64-byte NVMe command, including data-buffer
   pointers.
2. **Submission path.** Write the SQE into the IO submission queue and ring the
   tail doorbell (MMIO).
3. **Completion path.** Poll the IO completion queue (or take an MSI-X IRQ),
   decode CQEs, and call `complete(NvmeCompletion)` — the half that already exists.
4. **DMA buffers.** Physical/IOMMU addresses for `AlignedBuf` regions, minted from
   Feox physical-page capabilities (Feox already has `cap_request` for those).
5. **Bring-up.** Admin queue: create IO SQ/CQ pair; `Identify Namespace` for block
   size + capacity.

(1)–(3) are squarely Feox-side NVMe-driver work; the RFS adapter is small and
mechanical once they exist.

## Until then

RFS runs and is validated on the other `BlockDevice` backends:

- **in-memory** crash-injecting device (unit tests + the model-checked fuzzer),
- **file-backed** device under FUSE on Linux (mounts as a real directory).

The async / `!Send` / capability / registered-buffer seams mean the Feox NVMe
backend is a drop-in: no change to the engine, only a new `BlockDevice` impl.
