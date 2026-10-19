//! The device a ring serves, the per-submission checks against it, and the
//! descriptor arithmetic behind `max_sectors`.
//!
//! These are pure functions of numbers, so the kernel, the driver and
//! `src/lib/drivers/block/virtio-blk` all compute them one way.
//!
//! # Why `max_sectors` depends on the page size
//!
//! The data VMO is pinned, and the pin query gives one device address per page;
//! the addresses are not contiguous. So the driver splits a payload region at
//! page boundaries into virtio data descriptors, and the kernel places regions
//! wherever it likes, with no alignment. A region of `len` bytes therefore
//! spans at most `(len + page - 2) / page + 1` pages — one more than its length
//! alone suggests, when it starts just before a page boundary — and one request
//! takes that many descriptors plus a header and a status. [`max_sectors`] is
//! the largest count for which that fits both the device's `seg_max` and its
//! queue, wherever the region starts.

use core::fmt;

use crate::layout::{FLAG_FUA, Op, RawSubmission, Submission};

/// The smallest logical block size: 512 bytes, virtio's unit.
pub const MIN_BLOCK_SIZE: u32 = 512;

/// The largest logical block size a ring may announce, as `src/lib/fs/common/block` accepts.
pub const MAX_BLOCK_SIZE: u32 = 65536;

/// What the device can do, from the features the driver *negotiated* — never
/// from the ones merely offered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeviceFlags(
    /// The flag word, as HELLO carries it.
    pub u32,
);

impl DeviceFlags {
    /// The device refuses writes.
    pub const READ_ONLY: DeviceFlags = DeviceFlags(1);
    /// The device has a volatile write cache and honours flush.
    pub const FLUSH: DeviceFlags = DeviceFlags(1 << 1);
    /// The device honours force unit access on a write.
    pub const FUA: DeviceFlags = DeviceFlags(1 << 2);
    /// Every flag v1 defines.
    pub const KNOWN: DeviceFlags = DeviceFlags(0b111);

    /// Whether every flag in `other` is set here.
    #[must_use]
    pub const fn contains(self, other: DeviceFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags in either.
    #[must_use]
    pub const fn union(self, other: DeviceFlags) -> DeviceFlags {
        DeviceFlags(self.0 | other.0)
    }
}

/// Why a device description was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceError {
    /// The block size is not a power of two in
    /// [`MIN_BLOCK_SIZE`]`..=`[`MAX_BLOCK_SIZE`].
    BlockSize,
    /// `max_sectors` is zero, so no read or write could be sent.
    NoSectors,
    /// A flag bit v1 does not define is set.
    UnknownFlags,
    /// The data VMO cannot hold one request of `max_sectors` sectors, so the
    /// largest request the driver announced would have nowhere to go.
    DataTooSmall,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            DeviceError::BlockSize => "the block size is not a power of two in 512..=65536",
            DeviceError::NoSectors => "max_sectors is zero",
            DeviceError::UnknownFlags => "the device flags have unknown bits",
            DeviceError::DataTooSmall => "the data VMO cannot hold one max_sectors request",
        })
    }
}

/// The device behind a ring, as HELLO describes it. A value of this type has
/// passed [`Device::new`]'s checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Device {
    block_size: u32,
    capacity: u64,
    max_sectors: u32,
    flags: DeviceFlags,
    data_vmo_size: u64,
}

impl Device {
    /// Describe a device.
    ///
    /// # Errors
    ///
    /// The [`DeviceError`] naming what is wrong with the description.
    pub const fn new(
        block_size: u32,
        capacity: u64,
        max_sectors: u32,
        flags: DeviceFlags,
        data_vmo_size: u64,
    ) -> Result<Self, DeviceError> {
        if block_size < MIN_BLOCK_SIZE
            || block_size > MAX_BLOCK_SIZE
            || !block_size.is_power_of_two()
        {
            return Err(DeviceError::BlockSize);
        }
        if max_sectors == 0 {
            return Err(DeviceError::NoSectors);
        }
        if flags.0 & !DeviceFlags::KNOWN.0 != 0 {
            return Err(DeviceError::UnknownFlags);
        }
        // Both factors are `u32`, so their product fits in a `u64`.
        let largest_request = max_sectors as u64 * block_size as u64;
        if data_vmo_size < largest_request {
            return Err(DeviceError::DataTooSmall);
        }
        Ok(Self {
            block_size,
            capacity,
            max_sectors,
            flags,
            data_vmo_size,
        })
    }

    /// Bytes in one logical sector.
    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Sectors on the device.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The most sectors one request may carry.
    #[must_use]
    pub const fn max_sectors(&self) -> u32 {
        self.max_sectors
    }

    /// What the device can do.
    #[must_use]
    pub const fn flags(&self) -> DeviceFlags {
        self.flags
    }

    /// Bytes in the data VMO.
    #[must_use]
    pub const fn data_vmo_size(&self) -> u64 {
        self.data_vmo_size
    }

    /// The payload length of a request of `count` sectors.
    #[must_use]
    pub const fn payload_len(&self, count: u32) -> u64 {
        // At most 2^32 × 2^16, so no overflow.
        count as u64 * self.block_size as u64
    }
}

/// Why the driver refused a submission. The kernel sees only the `REFUSED`
/// completion; this is for the driver's own log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidSubmission {
    /// `op` is not a known op.
    UnknownOp,
    /// The reserved field is not zero.
    ReservedNotZero,
    /// A flag bit other than FUA is set.
    UnknownFlags,
    /// FUA on something other than a write, or on a device that did not
    /// announce it.
    FuaNotAllowed,
    /// A read or write of zero sectors.
    Empty,
    /// A flush with a sector count.
    FlushWithCount,
    /// More sectors than `max_sectors`.
    TooManySectors,
    /// The range ends past the device, or its end overflows.
    PastCapacity,
    /// The payload region ends past the data VMO, or its end overflows.
    OutsideDataVmo,
}

impl fmt::Display for InvalidSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            InvalidSubmission::UnknownOp => "unknown op",
            InvalidSubmission::ReservedNotZero => "reserved field not zero",
            InvalidSubmission::UnknownFlags => "unknown flags",
            InvalidSubmission::FuaNotAllowed => "FUA not allowed here",
            InvalidSubmission::Empty => "a read or write of no sectors",
            InvalidSubmission::FlushWithCount => "a flush with a sector count",
            InvalidSubmission::TooManySectors => "more sectors than max_sectors",
            InvalidSubmission::PastCapacity => "range past the device",
            InvalidSubmission::OutsideDataVmo => "payload outside the data VMO",
        })
    }
}

/// Every check the driver makes on a submission it consumes.
///
/// The kernel makes the same checks on what it submits, so a request the
/// driver would refuse never costs a trip round the ring.
///
/// # Errors
///
/// The [`InvalidSubmission`] naming the first check that failed.
pub fn check_submission(
    raw: &RawSubmission,
    device: &Device,
) -> Result<Submission, InvalidSubmission> {
    let Some(op) = Op::from_raw(raw.op) else {
        return Err(InvalidSubmission::UnknownOp);
    };
    if raw.reserved != 0 {
        return Err(InvalidSubmission::ReservedNotZero);
    }
    if raw.flags & !FLAG_FUA != 0 {
        return Err(InvalidSubmission::UnknownFlags);
    }
    let fua = raw.flags & FLAG_FUA != 0;
    if fua && (op != Op::Write || !device.flags.contains(DeviceFlags::FUA)) {
        return Err(InvalidSubmission::FuaNotAllowed);
    }
    match (op, raw.count) {
        (Op::Flush, 0) | (Op::Read | Op::Write, 1..) => {}
        (Op::Flush, _) => return Err(InvalidSubmission::FlushWithCount),
        (Op::Read | Op::Write, 0) => return Err(InvalidSubmission::Empty),
    }
    if raw.count > device.max_sectors {
        return Err(InvalidSubmission::TooManySectors);
    }
    if !raw
        .sector
        .checked_add(u64::from(raw.count))
        .is_some_and(|end| end <= device.capacity)
    {
        return Err(InvalidSubmission::PastCapacity);
    }
    if !region_in_data_vmo(
        raw.data_offset,
        device.payload_len(raw.count),
        device.data_vmo_size,
    ) {
        return Err(InvalidSubmission::OutsideDataVmo);
    }
    Ok(Submission {
        id: raw.id,
        op,
        sector: raw.sector,
        count: raw.count,
        data_offset: raw.data_offset,
        fua,
    })
}

/// Whether `[data_offset, data_offset + len)` lies inside a data VMO of
/// `data_vmo_size` bytes, without the end overflowing.
#[must_use]
pub const fn region_in_data_vmo(data_offset: u64, len: u64, data_vmo_size: u64) -> bool {
    match data_offset.checked_add(len) {
        Some(end) => end <= data_vmo_size,
        None => false,
    }
}

/// How many pages of size `page_size` the region `[data_offset, data_offset +
/// len)` touches. Zero for an empty region.
///
/// `None` if `page_size` is not a power of two, or the region's end overflows.
#[must_use]
pub const fn pages_spanned(data_offset: u64, len: u64, page_size: u64) -> Option<u64> {
    if !page_size.is_power_of_two() {
        return None;
    }
    if len == 0 {
        return Some(0);
    }
    let Some(last) = data_offset.checked_add(len - 1) else {
        return None;
    };
    // `last >= data_offset`, and the difference in pages is far from `u64::MAX`.
    Some(last / page_size - data_offset / page_size + 1)
}

/// The virtio descriptors one request with this payload region takes: a
/// header, one per page spanned, and a status.
///
/// `None` if `page_size` is not a power of two, or the region's end overflows.
#[must_use]
pub const fn descriptors_for(data_offset: u64, len: u64, page_size: u64) -> Option<u64> {
    match pages_spanned(data_offset, len, page_size) {
        Some(pages) => pages.checked_add(2),
        None => None,
    }
}

/// The largest `max_sectors` a driver may announce: the most sectors for which
/// [`descriptors_for`] fits the device's queue, and the data descriptors fit
/// its `seg_max`, wherever in the data VMO the kernel puts the region.
///
/// Pass `u32::MAX` for `seg_max` if the device did not negotiate a limit.
/// Returns zero — which HELLO may not announce — if not even one sector fits,
/// or if `page_size` is not a power of two or `block_size` is zero.
#[must_use]
pub const fn max_sectors(seg_max: u32, queue_size: u16, page_size: u32, block_size: u32) -> u32 {
    if !page_size.is_power_of_two() || block_size == 0 {
        return 0;
    }
    let Some(by_queue) = (queue_size as u32).checked_sub(2) else {
        return 0;
    };
    let pages = if seg_max < by_queue {
        seg_max
    } else {
        by_queue
    };
    if pages == 0 {
        return 0;
    }
    // A region of `len` bytes spans at most `(len + page - 2) / page + 1`
    // pages, so `pages` pages hold any region of up to `(pages - 1) × page + 1`
    // bytes. `pages` is below 2^16 and `page_size` below 2^32, so no overflow.
    let bytes = (pages as u64 - 1) * page_size as u64 + 1;
    let sectors = bytes / block_size as u64;
    if sectors > u32::MAX as u64 {
        u32::MAX
    } else {
        sectors as u32
    }
}
