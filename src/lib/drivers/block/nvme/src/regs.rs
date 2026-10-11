//! The controller's registers: offsets, and the bits this crate reads and
//! writes (NVMe Base Specification 2.0, section 3.1.3; the same offsets and
//! bits since 1.0).

/// Controller Capabilities, 64 bits.
pub const CAP: u32 = 0x00;
/// Version.
pub const VS: u32 = 0x08;
/// Controller Configuration.
pub const CC: u32 = 0x14;
/// Controller Status.
pub const CSTS: u32 = 0x1C;
/// Admin Queue Attributes.
pub const AQA: u32 = 0x24;
/// Admin Submission Queue Base Address, 64 bits.
pub const ASQ: u32 = 0x28;
/// Admin Completion Queue Base Address, 64 bits.
pub const ACQ: u32 = 0x30;
/// The first doorbell: submission queue 0's tail.
pub const DOORBELLS: u32 = 0x1000;

/// `CC.EN`: the controller processes commands.
pub const CC_ENABLE: u32 = 1 << 0;
/// `CC.CSS`, bits 6:4. Zero selects the NVM command set.
pub const CC_CSS_MASK: u32 = 7 << 4;
/// `CC.MPS`, bits 10:7: the host's page is `2^(12 + MPS)` bytes.
pub const CC_MPS_MASK: u32 = 0xF << 7;
/// `CC.AMS`, bits 13:11. Zero selects round robin.
pub const CC_AMS_MASK: u32 = 7 << 11;
/// `CC.SHN`, bits 15:14.
pub const CC_SHN_MASK: u32 = 3 << 14;
/// `CC.SHN` = 01b: a normal shutdown is coming.
pub const CC_SHN_NORMAL: u32 = 1 << 14;
/// `CC.IOSQES`, bits 19:16, as this crate sets it: 64-byte entries.
pub const CC_IOSQES: u32 = 6 << 16;
/// `CC.IOCQES`, bits 23:20, as this crate sets it: 16-byte entries.
pub const CC_IOCQES: u32 = 4 << 20;

/// `CSTS.RDY`: the controller is ready after `CC.EN` was set.
pub const CSTS_READY: u32 = 1 << 0;
/// `CSTS.CFS`: the controller failed and needs a reset.
pub const CSTS_FATAL: u32 = 1 << 1;
/// `CSTS.SHST`, bits 3:2.
pub const CSTS_SHST_MASK: u32 = 3 << 2;
/// `CSTS.SHST` = 10b: shutdown processing is complete.
pub const CSTS_SHST_COMPLETE: u32 = 2 << 2;
/// What every register of a function that left the bus reads as.
pub const GONE: u32 = u32::MAX;

/// The log2 of the one page size this crate runs a controller with.
pub const PAGE_SHIFT: u32 = 12;
/// That page, in bytes: `CC.MPS` = 0.
pub const PAGE: usize = 1 << PAGE_SHIFT;

/// Controller Capabilities, taken apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capabilities(
    /// The register, as read.
    pub u64,
);

impl Capabilities {
    /// `MQES`: the most entries a queue may have, less one.
    #[must_use]
    pub const fn max_queue_entries(self) -> u32 {
        (self.0 & 0xFFFF) as u32 + 1
    }

    /// `TO`: the longest `CSTS.RDY` takes to follow `CC.EN`, in nanoseconds.
    /// The field counts 500 ms units; a controller that says zero is given
    /// one unit.
    #[must_use]
    pub const fn ready_timeout_nanos(self) -> u64 {
        let units = (self.0 >> 24) & 0xFF;
        (if units == 0 { 1 } else { units }) * 500_000_000
    }

    /// `DSTRD`: each doorbell is `4 << DSTRD` bytes from the one before.
    #[must_use]
    pub const fn doorbell_stride(self) -> u32 {
        4 << ((self.0 >> 32) & 0xF)
    }

    /// `CSS` bit 0: the NVM command set is supported.
    #[must_use]
    pub const fn nvm_command_set(self) -> bool {
        (self.0 >> 37) & 1 != 0
    }

    /// `MPSMIN`: the smallest host page is `2^(12 + MPSMIN)` bytes.
    #[must_use]
    pub const fn min_page_shift(self) -> u32 {
        PAGE_SHIFT + ((self.0 >> 48) & 0xF) as u32
    }

    /// `MPSMAX`: the largest host page is `2^(12 + MPSMAX)` bytes.
    #[must_use]
    pub const fn max_page_shift(self) -> u32 {
        PAGE_SHIFT + ((self.0 >> 52) & 0xF) as u32
    }
}

/// Where submission queue `queue`'s tail doorbell is, with `stride` bytes
/// between doorbells.
#[must_use]
pub const fn sq_doorbell(queue: u16, stride: u32) -> u32 {
    DOORBELLS + (2 * queue as u32) * stride
}

/// Where completion queue `queue`'s head doorbell is.
#[must_use]
pub const fn cq_doorbell(queue: u16, stride: u32) -> u32 {
    DOORBELLS + (2 * queue as u32 + 1) * stride
}
