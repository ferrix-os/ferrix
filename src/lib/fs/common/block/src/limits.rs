//! What the device can do, and how the queue is tuned.

use core::fmt;

use crate::request::{Flags, Op};

/// The smallest logical block size a block device reports.
pub const MIN_BLOCK_SIZE: u32 = 512;

/// The largest logical block size Linux accepts from a device.
pub const MAX_BLOCK_SIZE: u32 = 65536;

/// What one device accepts, as its driver reports it.
///
/// Built only through [`Limits::new`], so every field the queue divides a
/// decision by is known to be non-zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    logical_block_size: u32,
    capacity: u64,
    max_sectors: u32,
    max_parts: u32,
    queue_depth: u32,
}

/// Why a set of limits describes no device the queue could drive.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitsError {
    /// The logical block size is not a power of two between 512 and 65536.
    BlockSize,
    /// A command may carry no sectors.
    MaxSectors,
    /// A command may carry no parts.
    MaxParts,
    /// The device accepts no commands at once.
    QueueDepth,
}

impl fmt::Display for LimitsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LimitsError::BlockSize => "logical block size is not a power of two in 512..=65536",
            LimitsError::MaxSectors => "maximum sectors per command is zero",
            LimitsError::MaxParts => "maximum parts per command is zero",
            LimitsError::QueueDepth => "queue depth is zero",
        })
    }
}

impl Limits {
    /// A device with the given logical block size in bytes, capacity in
    /// logical sectors, the most sectors and the most parts one command may
    /// carry, and the most commands it runs at once.
    ///
    /// A capacity of zero is accepted: such a device can still be flushed.
    pub const fn new(
        logical_block_size: u32,
        capacity: u64,
        max_sectors: u32,
        max_parts: u32,
        queue_depth: u32,
    ) -> Result<Self, LimitsError> {
        if !logical_block_size.is_power_of_two()
            || logical_block_size < MIN_BLOCK_SIZE
            || logical_block_size > MAX_BLOCK_SIZE
        {
            return Err(LimitsError::BlockSize);
        }
        if max_sectors == 0 {
            return Err(LimitsError::MaxSectors);
        }
        if max_parts == 0 {
            return Err(LimitsError::MaxParts);
        }
        if queue_depth == 0 {
            return Err(LimitsError::QueueDepth);
        }
        Ok(Limits {
            logical_block_size,
            capacity,
            max_sectors,
            max_parts,
            queue_depth,
        })
    }

    /// Bytes in one logical sector.
    #[must_use]
    pub const fn logical_block_size(&self) -> u32 {
        self.logical_block_size
    }

    /// The number of logical sectors on the device.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The most sectors one dispatched command may carry.
    #[must_use]
    pub const fn max_sectors(&self) -> u32 {
        self.max_sectors
    }

    /// The most requests one dispatched command may carry.
    #[must_use]
    pub const fn max_parts(&self) -> u32 {
        self.max_parts
    }

    /// The most commands the device runs at once.
    #[must_use]
    pub const fn queue_depth(&self) -> u32 {
        self.queue_depth
    }

    /// The length in bytes of `count` logical sectors.
    ///
    /// A `u32` count times a block size of at most 2¹⁶ fits a `u64` with room
    /// to spare, so this cannot saturate; saturating says so without an
    /// overflow check that could never fire.
    #[must_use]
    pub const fn bytes(&self, count: u32) -> u64 {
        (count as u64).saturating_mul(self.logical_block_size as u64)
    }
}

/// How the queue is tuned. Times are in the caller's ticks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// How long a read, or a `sync` write or discard, may wait before it
    /// leaves ahead of the elevator.
    pub read_expiry: u64,
    /// How long an asynchronous write or discard may wait.
    pub write_expiry: u64,
    /// While plugged, the number of queued requests at which the queue
    /// dispatches anyway. Zero makes plugging have no effect.
    pub plug_threshold: usize,
    /// The most requests that may be submitted and not yet completed. Past it
    /// [`Queue::submit`](crate::Queue::submit) refuses, which is how a caller
    /// producing I/O faster than the device finishes it is made to wait.
    pub max_requests: usize,
}

impl Default for Config {
    /// `mq-deadline`'s expiries — half a second for reads, five for writes —
    /// on the assumption that a tick is a millisecond; the plug threshold of
    /// Linux's `BLK_MAX_REQUEST_COUNT`; and room for 256 requests.
    fn default() -> Self {
        Config {
            read_expiry: 500,
            write_expiry: 5000,
            plug_threshold: 16,
            max_requests: 256,
        }
    }
}

impl Config {
    /// For a device with no seek cost, which is every disk behind a block
    /// ring (a virtio disk): a read, or a `sync` write or discard, waits
    /// behind the elevator at most 25 ms, not [`Config::default`]'s 500.
    ///
    /// The rule: the read expiry is the longest a read can starve behind the
    /// elevator. With readers adding requests ahead of the head faster than
    /// it wraps, a request behind it waits the whole expiry. `mq-deadline`'s
    /// half second is what a spinning disk gives up to keep the head moving
    /// one way; a device with no head gets nothing for it. On a ring disk
    /// the default made the 99th percentile of 32 concurrent 4 KiB reads 112
    /// to 228 ms under KVM; 25 ms made it 28 to 35 ms with the median
    /// unchanged (`docs/roadmap/stage-11-block-core-btrfs-read.md`, the seam
    /// measured, 1). Asynchronous writes keep their five seconds.
    #[must_use]
    pub const fn fast_device() -> Self {
        Config {
            read_expiry: 25,
            write_expiry: 5000,
            plug_threshold: 16,
            max_requests: 256,
        }
    }

    /// How long a request with this op and these flags may wait.
    #[must_use]
    pub const fn expiry(&self, op: Op, flags: Flags) -> u64 {
        match op {
            Op::Read | Op::Flush => self.read_expiry,
            Op::Write | Op::Discard if flags.sync => self.read_expiry,
            Op::Write | Op::Discard => self.write_expiry,
        }
    }
}
