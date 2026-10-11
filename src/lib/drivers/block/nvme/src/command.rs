//! Commands and completions: the 64-byte submission queue entry, the
//! 16-byte completion queue entry, and the opcodes and fields this crate
//! sends (NVMe Base Specification 2.0, sections 3.3.3, 5 and the NVM Command
//! Set Specification, section 3.2).
//!
//! A submission queue entry, as every command lays it out:
//!
//! ```text
//!  0  1  opcode            24  8  PRP1
//!  1  1  flags (0: PRP)    32  8  PRP2
//!  2  2  command id        40  4  CDW10
//!  4  4  namespace id      44  4  CDW11
//!  8  8  reserved          48  4  CDW12
//! 16  8  metadata pointer  52 12  CDW13..CDW15
//! ```

/// Bytes in a submission queue entry.
pub const SQE_BYTES: usize = 64;
/// Bytes in a completion queue entry.
pub const CQE_BYTES: usize = 16;

/// Admin: Delete I/O Submission Queue.
pub const ADMIN_DELETE_SQ: u8 = 0x00;
/// Admin: Create I/O Submission Queue.
pub const ADMIN_CREATE_SQ: u8 = 0x01;
/// Admin: Delete I/O Completion Queue.
pub const ADMIN_DELETE_CQ: u8 = 0x04;
/// Admin: Create I/O Completion Queue.
pub const ADMIN_CREATE_CQ: u8 = 0x05;
/// Admin: Identify.
pub const ADMIN_IDENTIFY: u8 = 0x06;
/// Admin: Abort.
pub const ADMIN_ABORT: u8 = 0x08;
/// Admin: Set Features.
pub const ADMIN_SET_FEATURES: u8 = 0x09;
/// Admin: Get Features.
pub const ADMIN_GET_FEATURES: u8 = 0x0A;

/// NVM: Flush.
pub const NVM_FLUSH: u8 = 0x00;
/// NVM: Write.
pub const NVM_WRITE: u8 = 0x01;
/// NVM: Read.
pub const NVM_READ: u8 = 0x02;

/// Identify's CNS: one namespace.
pub const CNS_NAMESPACE: u32 = 0x00;
/// Identify's CNS: the controller.
pub const CNS_CONTROLLER: u32 = 0x01;
/// Identify's CNS: the active namespace ids above the one given.
pub const CNS_ACTIVE_NAMESPACES: u32 = 0x02;

/// Feature: Volatile Write Cache.
pub const FEATURE_WRITE_CACHE: u32 = 0x06;
/// Feature: Number of Queues.
pub const FEATURE_QUEUES: u32 = 0x07;
/// Feature: Autonomous Power State Transition.
pub const FEATURE_APST: u32 = 0x0C;
/// Feature: Host Memory Buffer.
pub const FEATURE_HOST_MEMORY: u32 = 0x0D;

/// Host Memory Buffer's CDW11 bit 0: enable host memory.
pub const HOST_MEMORY_ENABLE: u32 = 1 << 0;
/// Host Memory Buffer's CDW11 bit 1: the memory is the one given before
/// the reset, its contents kept.
pub const HOST_MEMORY_RETURN: u32 = 1 << 1;

/// A queue's flags, bit 0: its memory is physically contiguous.
pub const QUEUE_CONTIGUOUS: u32 = 1 << 0;
/// A completion queue's flags, bit 1: it interrupts.
pub const QUEUE_INTERRUPTS: u32 = 1 << 1;

/// A read's or write's CDW12 bit 30: force unit access.
pub const RW_FUA: u32 = 1 << 30;

/// One submission queue entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Command(
    /// The entry's bytes, as the controller reads them.
    pub [u8; SQE_BYTES],
);

impl Command {
    /// A command with `opcode`, everything else zero: PRPs, no fused
    /// operation.
    #[must_use]
    pub fn new(opcode: u8) -> Self {
        let mut bytes = [0_u8; SQE_BYTES];
        if let Some(first) = bytes.first_mut() {
            *first = opcode;
        }
        Command(bytes)
    }

    fn put(&mut self, at: usize, value: &[u8]) {
        if let Some(field) = self.0.get_mut(at..at + value.len()) {
            field.copy_from_slice(value);
        }
    }

    /// With the command id.
    #[must_use]
    pub fn id(mut self, id: u16) -> Self {
        self.put(2, &id.to_le_bytes());
        self
    }

    /// With the namespace id.
    #[must_use]
    pub fn namespace(mut self, nsid: u32) -> Self {
        self.put(4, &nsid.to_le_bytes());
        self
    }

    /// With the two PRP entries.
    #[must_use]
    pub fn prp(mut self, first: u64, second: u64) -> Self {
        self.put(24, &first.to_le_bytes());
        self.put(32, &second.to_le_bytes());
        self
    }

    /// With command dword `index`, 10 to 15.
    #[must_use]
    pub fn dword(mut self, index: usize, value: u32) -> Self {
        if (10..=15).contains(&index) {
            self.put(40 + (index - 10) * 4, &value.to_le_bytes());
        }
        self
    }

    /// The opcode.
    #[must_use]
    pub fn opcode(&self) -> u8 {
        self.0.first().copied().unwrap_or(0)
    }

    /// Identify, `cns` of namespace `nsid`, into the page at `page`.
    #[must_use]
    pub fn identify(cns: u32, nsid: u32, page: u64) -> Self {
        Command::new(ADMIN_IDENTIFY)
            .namespace(nsid)
            .prp(page, 0)
            .dword(10, cns)
    }

    /// Set Features `feature`, with CDW11 `value`.
    #[must_use]
    pub fn set_feature(feature: u32, value: u32) -> Self {
        Command::new(ADMIN_SET_FEATURES)
            .dword(10, feature)
            .dword(11, value)
    }

    /// Get Features `feature`, its current value.
    #[must_use]
    pub fn get_feature(feature: u32) -> Self {
        Command::new(ADMIN_GET_FEATURES).dword(10, feature)
    }

    /// Set Features, Host Memory Buffer: `flags` in CDW11, the buffer's
    /// size in pages, where its descriptor list is, and how many
    /// descriptors that holds.
    #[must_use]
    pub fn host_memory(flags: u32, pages: u32, list: u64, descriptors: u32) -> Self {
        Command::set_feature(FEATURE_HOST_MEMORY, flags)
            .dword(12, pages)
            .dword(13, list as u32)
            .dword(14, (list >> 32) as u32)
            .dword(15, descriptors)
    }

    /// Create I/O Completion Queue `queue` of `entries` entries in the page
    /// at `page`, interrupting through `vector`.
    #[must_use]
    pub fn create_cq(queue: u16, entries: u16, page: u64, vector: u16) -> Self {
        Command::new(ADMIN_CREATE_CQ)
            .prp(page, 0)
            .dword(
                10,
                (u32::from(entries.saturating_sub(1)) << 16) | u32::from(queue),
            )
            .dword(
                11,
                (u32::from(vector) << 16) | QUEUE_INTERRUPTS | QUEUE_CONTIGUOUS,
            )
    }

    /// Create I/O Submission Queue `queue` of `entries` entries in the page
    /// at `page`, completing into the completion queue of the same number.
    #[must_use]
    pub fn create_sq(queue: u16, entries: u16, page: u64) -> Self {
        Command::new(ADMIN_CREATE_SQ)
            .prp(page, 0)
            .dword(
                10,
                (u32::from(entries.saturating_sub(1)) << 16) | u32::from(queue),
            )
            .dword(11, (u32::from(queue) << 16) | QUEUE_CONTIGUOUS)
    }

    /// Abort command `id` of submission queue `queue`.
    #[must_use]
    pub fn abort(queue: u16, id: u16) -> Self {
        Command::new(ADMIN_ABORT).dword(10, (u32::from(id) << 16) | u32::from(queue))
    }

    /// Read or write `blocks` logical blocks, at least one and at most
    /// 65536, at `lba` of namespace `nsid`.
    #[must_use]
    pub fn transfer(opcode: u8, nsid: u32, lba: u64, blocks: u32, fua: bool) -> Self {
        let count = blocks.saturating_sub(1) & 0xFFFF;
        Command::new(opcode)
            .namespace(nsid)
            .dword(10, lba as u32)
            .dword(11, (lba >> 32) as u32)
            .dword(12, count | if fua { RW_FUA } else { 0 })
    }

    /// Flush namespace `nsid`'s volatile write cache.
    #[must_use]
    pub fn flush(nsid: u32) -> Self {
        Command::new(NVM_FLUSH).namespace(nsid)
    }
}

/// A completion's status field, without the phase bit: status code in bits
/// 7:0, status code type in 10:8, do-not-retry in bit 14.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Status(
    /// Bits 15:1 of the entry's last word.
    pub u16,
);

impl Status {
    /// Generic status, Invalid Command Opcode.
    pub const INVALID_OPCODE: Status = Status(0x01);
    /// Generic status, Command Abort Requested.
    pub const ABORT_REQUESTED: Status = Status(0x07);
    /// The do-not-retry bit.
    pub const DO_NOT_RETRY: u16 = 1 << 14;

    /// Whether the command succeeded.
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 & 0x7FF == 0
    }

    /// The status code type and status code, without the retry bits.
    #[must_use]
    pub const fn code(self) -> u16 {
        self.0 & 0x7FF
    }

    /// Whether the controller says the same command would fail again.
    #[must_use]
    pub const fn do_not_retry(self) -> bool {
        self.0 & Self::DO_NOT_RETRY != 0
    }
}

/// One completion queue entry.
///
/// ```text
///  0  4  command specific (DW0)    10  2  submission queue id
///  8  2  submission queue head     12  2  command id
///                                  14  2  phase (bit 0), status (15:1)
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Completion {
    /// DW0: what the command answers with.
    pub result: u32,
    /// How far the controller has read the submission queue.
    pub sq_head: u16,
    /// The command's id.
    pub id: u16,
    /// How it went.
    pub status: Status,
}
