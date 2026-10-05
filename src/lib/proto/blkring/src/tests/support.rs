//! The shared buffer, fixtures and a seeded generator.

use core::cell::{Cell, RefCell};
use std::vec;
use std::vec::Vec;

use crate::RingMemory;
use crate::driver::DriverSide;
use crate::geometry::{Device, DeviceFlags};
use crate::identity::{DiskName, Identity, Location};
use crate::kernel::{KernelSide, Slot};
use crate::layout::{COMPLETION_BYTES, RingLayout, SUBMISSION_BYTES, Submission, header};

/// Bytes in a sector of the fixture device.
pub(super) const BLOCK: u64 = 512;

/// The fixture device: a 512-byte-sector disk of 2^20 sectors that announces
/// flush and FUA, 64 sectors a request, and a 16 MiB data VMO.
pub(super) fn device() -> Device {
    Device::new(
        512,
        1 << 20,
        64,
        DeviceFlags::FLUSH.union(DeviceFlags::FUA),
        1 << 24,
    )
    .expect("the fixture device is valid")
}

/// The fixture disk: `vda` at 0000:00:03.0, with a serial.
pub(super) fn identity() -> Identity {
    Identity {
        location: Location::new(0, 0, 3 << 3),
        serial: *b"ferrix-test-disk\0\0\0\0",
        name: DiskName::for_index(0).expect("vda"),
    }
}

/// A one-sector read of sector `id * 8` into a region of its own.
pub(super) fn read(id: u64) -> Submission {
    Submission::read(id, id * 8, 1, id * BLOCK)
}

/// Which side a memory handle belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Who {
    Kernel,
    Driver,
}

/// The ring VMO both sides share.
pub(super) struct Shared {
    bytes: RefCell<Vec<u8>>,
    own_reads: Cell<usize>,
    armed: Cell<bool>,
}

impl Shared {
    pub(super) fn new(len: u64) -> Self {
        Self {
            bytes: RefCell::new(vec![0; usize::try_from(len).expect("a test ring fits")]),
            own_reads: Cell::new(0),
            armed: Cell::new(false),
        }
    }

    pub(super) fn memory(&self, who: Who, layout: RingLayout) -> Mem<'_> {
        Mem {
            shared: self,
            who,
            layout,
        }
    }

    /// Start counting reads of a side's own fields: after setup, when the
    /// kernel has read the driver's header once.
    pub(super) fn arm(&self) {
        self.armed.set(true);
    }

    pub(super) fn own_reads(&self) -> usize {
        self.own_reads.get()
    }

    pub(super) fn peek(&self, at: usize, len: usize) -> Vec<u8> {
        self.bytes.borrow()[at..at + len].to_vec()
    }

    pub(super) fn peek_u32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.peek(at, 4).try_into().expect("four bytes"))
    }

    /// Write as the peer would: counted against nobody.
    pub(super) fn poke(&self, at: usize, bytes: &[u8]) {
        self.bytes.borrow_mut()[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub(super) fn poke_u32(&self, at: usize, value: u32) {
        self.poke(at, &value.to_le_bytes());
    }
}

/// One side's handle on the shared ring.
pub(super) struct Mem<'a> {
    shared: &'a Shared,
    who: Who,
    layout: RingLayout,
}

impl Mem<'_> {
    /// Whether `at` is in a field this side writes, or read once at setup.
    fn owns(&self, at: usize) -> bool {
        let within = |start: usize, len: usize| at >= start && at < start + len;
        if at < header::SUB_TAIL || within(header::RESERVED, header::RESERVED_BYTES) {
            return true;
        }
        let entries = self.layout.entries() as usize;
        match self.who {
            // The kernel never needs the driver's `sub_head` either, and the
            // count holds it to that.
            Who::Kernel => {
                within(header::SUB_TAIL, 4)
                    || within(header::SUB_HEAD, 4)
                    || within(header::COMP_HEAD, 4)
                    || within(header::COMP_WANT_BELL, 4)
                    || within(
                        self.layout.sub_offset() as usize,
                        entries * SUBMISSION_BYTES,
                    )
            }
            Who::Driver => {
                within(header::SUB_HEAD, 4)
                    || within(header::COMP_TAIL, 4)
                    || within(header::SUB_WANT_BELL, 4)
                    || within(
                        self.layout.comp_offset() as usize,
                        entries * COMPLETION_BYTES,
                    )
            }
        }
    }
}

impl RingMemory for Mem<'_> {
    fn read_u8(&self, offset: usize) -> u8 {
        if self.shared.armed.get() && self.owns(offset) {
            self.shared.own_reads.set(self.shared.own_reads.get() + 1);
        }
        *self
            .shared
            .bytes
            .borrow()
            .get(offset)
            .expect("a read inside the ring")
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        *self
            .shared
            .bytes
            .borrow_mut()
            .get_mut(offset)
            .expect("a write inside the ring") = value;
    }

    fn barrier(&self) {}

    // One side at a time touches this memory, so bytes compose.
    fn read_u16(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)])
    }

    fn read_u32(&self, offset: usize) -> u32 {
        u32::from(self.read_u16(offset)) | (u32::from(self.read_u16(offset + 2)) << 16)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        let [low, high] = value.to_le_bytes();
        self.write_u8(offset, low);
        self.write_u8(offset + 1, high);
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.write_u16(offset, value as u16);
        self.write_u16(offset + 2, (value >> 16) as u16);
    }
}

/// Build a driver, then a kernel attached to its ring, run `body`, and check
/// that neither side read back a field of its own.
pub(super) fn with_pair<R>(
    entries: u32,
    body: impl FnOnce(&mut KernelSide<'_, Mem<'_>>, &mut DriverSide<Mem<'_>>, &Shared) -> R,
) -> R {
    let layout = RingLayout::standard(entries).expect("a valid entry count");
    let shared = Shared::new(layout.ring_bytes());
    let mut driver = DriverSide::new(
        shared.memory(Who::Driver, layout),
        layout.ring_bytes(),
        layout,
        device(),
    )
    .expect("the standard layout fits its own size");
    let mut slots = vec![Slot::EMPTY; entries as usize];
    let mut kernel = KernelSide::attach(
        shared.memory(Who::Kernel, layout),
        layout.ring_bytes(),
        device(),
        &mut slots,
    )
    .expect("a fresh header attaches");
    shared.arm();
    let result = body(&mut kernel, &mut driver, &shared);
    assert_eq!(
        shared.own_reads(),
        0,
        "a side read back one of its own fields"
    );
    result
}

/// xorshift64*, so a failing seed replays exactly.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(super) fn u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub(super) fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.u64() % bound }
    }

    pub(super) fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}
