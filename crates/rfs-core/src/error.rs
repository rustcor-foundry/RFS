//! Error types shared across the engine.

/// Why a piece of stored state failed to validate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CorruptKind {
    /// The block did not start with the expected RFS magic.
    BadMagic,
    /// The block's stored checksum did not match its contents.
    BadChecksum,
    /// The on-disk format version is newer than this build understands.
    UnsupportedVersion,
    /// No slot in the superblock ring validated.
    NoValidSuperblock,
    /// A tree node failed structural validation (bad magic or impossible counts).
    BadNode,
}

/// Errors surfaced by the storage stack.
///
/// `CapabilityRevoked` and `DeviceRemoved` are first-class because the target
/// is a capability-oriented exokernel (Feox): the device under us can vanish or
/// have its access capability pulled at any time, and the engine must treat that
/// as an ordinary, recoverable outcome rather than a panic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StorageError {
    /// The requested LBA is outside the device.
    OutOfBounds,
    /// The caller's buffer length does not equal the device block size.
    BufferSize,
    /// The backing device disappeared (hot-unplug, surprise removal).
    DeviceRemoved,
    /// The capability authorizing this access was revoked.
    CapabilityRevoked,
    /// A lower-level transport error occurred.
    Io,
    /// The requested capability/algorithm is not available in this build or on
    /// this device (e.g. a digest mode without its feature, or a zoned op on a
    /// non-zoned device).
    Unsupported,
    /// The allocator has no free space.
    NoSpace,
    /// A referenced object (e.g. a snapshot id) does not exist.
    NotFound,
    /// An object that must not already exist does (e.g. a duplicate name).
    AlreadyExists,
    /// An operation required a directory (or a non-directory) and got the other.
    NotADirectory,
    /// A directory operation requires the directory to be empty.
    NotEmpty,
    /// The operation is not permitted (e.g. hard-linking a directory).
    NotPermitted,
    /// Stored state failed structural validation.
    Corrupt(CorruptKind),
}

impl From<crate::allocator::AllocError> for StorageError {
    fn from(err: crate::allocator::AllocError) -> Self {
        match err {
            crate::allocator::AllocError::NoSpace => Self::NoSpace,
            crate::allocator::AllocError::OutOfRange | crate::allocator::AllocError::NotAllocated => {
                Self::Io
            }
        }
    }
}
