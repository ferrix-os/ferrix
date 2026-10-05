//! Fuzz the block ring: the fuzzer plays one side, and the other is honest.
//!
//! From stage 10 the kernel and a ring-3 block driver share a ring VMO, and
//! each reads what the other wrote on every request: the kernel in ring 0,
//! with `overflow-checks` on, the driver with a device's DMA engine behind it.
//! Neither may trust the other, and this target holds each to that.
//!
//! # The input
//!
//! The first byte picks the honest side: even for the kernel, odd for the
//! driver. Then the geometry — entries and the device — and a sequence of
//! operations, each a byte and its arguments. The honest side's operations are
//! its real methods. The fuzzer's are writes anywhere in the ring: arbitrary
//! bytes at any offset, a value in any header field, or a plausible entry with
//! its tail bumped, which is what gets past the index checks to the entry
//! checks. Anything not supplied reads as zero.
//!
//! # The properties
//!
//! Not panicking is the floor. Beyond it, for the honest side:
//!
//! 1. **It never reads outside the ring**, nor **reads back a field it owns**
//!    once set up — its own indices and want-bell flag, the header it wrote or
//!    read once, the array it produces. The memory asserts both on every read.
//! 2. **An honest kernel never accepts an id twice**, nor one it does not have
//!    outstanding, and reports each exactly as it submitted it.
//! 3. **Corruption latches**: once reported, it is reported again.
//! 4. **Ending a ring fails exactly what is outstanding**, and the data VMO is
//!    never releasable until the reset is confirmed.
//! 5. **An honest driver hands on only requests that pass every check**, as an
//!    independent restatement of the rules decides them, and never loses count
//!    of what it holds.

#![no_main]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use ferrix_blkring::driver::{CompleteError, Consumed, DriverSide};
use ferrix_blkring::geometry::{Device, DeviceFlags};
use ferrix_blkring::kernel::{DataVmo, Ending, KernelSide, Slot, SubmitError};
use ferrix_blkring::layout::{
    COMPLETION_BYTES, Op, RawCompletion, RawSubmission, RingLayout, SUBMISSION_BYTES, Status,
    Submission, header, write_header,
};
use ferrix_blkring::RingMemory;
use libfuzzer_sys::fuzz_target;

/// The most operations one input may run.
const MAX_OPS: usize = 512;

struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn byte(&mut self) -> u8 {
        match self.0.split_first() {
            Some((&b, rest)) => {
                self.0 = rest;
                b
            }
            None => 0,
        }
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.byte(), self.byte(), self.byte(), self.byte()])
    }

    fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | (u64::from(self.u32()) << 32)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Honest {
    Kernel,
    Driver,
}

struct Shared {
    bytes: RefCell<Vec<u8>>,
    armed: Cell<bool>,
}

impl Shared {
    fn new(len: u64) -> Self {
        Self {
            bytes: RefCell::new(vec![0; len as usize]),
            armed: Cell::new(false),
        }
    }

    fn len(&self) -> usize {
        self.bytes.borrow().len()
    }

    /// The fuzzer's write: anywhere, clipped to the ring.
    fn poke(&self, at: usize, source: &[u8]) {
        let mut bytes = self.bytes.borrow_mut();
        for (i, b) in source.iter().enumerate() {
            if let Some(slot) = bytes.get_mut(at + i) {
                *slot = *b;
            }
        }
    }
}

struct Mem<'a> {
    shared: &'a Shared,
    honest: Honest,
    layout: RingLayout,
}

impl Mem<'_> {
    fn owns(&self, at: usize) -> bool {
        let within = |start: usize, len: usize| at >= start && at < start + len;
        if at < header::SUB_TAIL || within(header::RESERVED, header::RESERVED_BYTES) {
            return true;
        }
        let entries = self.layout.entries() as usize;
        match self.honest {
            Honest::Kernel => {
                within(header::SUB_TAIL, 4)
                    || within(header::SUB_HEAD, 4)
                    || within(header::COMP_HEAD, 4)
                    || within(header::COMP_WANT_BELL, 4)
                    || within(self.layout.sub_offset() as usize, entries * SUBMISSION_BYTES)
            }
            Honest::Driver => {
                within(header::SUB_HEAD, 4)
                    || within(header::COMP_TAIL, 4)
                    || within(header::SUB_WANT_BELL, 4)
                    || within(self.layout.comp_offset() as usize, entries * COMPLETION_BYTES)
            }
        }
    }
}

impl RingMemory for Mem<'_> {
    fn read_u8(&self, offset: usize) -> u8 {
        assert!(
            !(self.shared.armed.get() && self.owns(offset)),
            "the honest side read back its own byte {offset}"
        );
        *self
            .shared
            .bytes
            .borrow()
            .get(offset)
            .unwrap_or_else(|| panic!("read outside the ring at {offset}"))
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        *self
            .shared
            .bytes
            .borrow_mut()
            .get_mut(offset)
            .unwrap_or_else(|| panic!("write outside the ring at {offset}")) = value;
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

fn device(input: &mut Input<'_>) -> Device {
    let block_size = 512 << (input.byte() % 5);
    let capacity = 1 + u64::from(input.u32());
    let max_sectors = 1 + u32::from(input.byte() % 64);
    let flags = DeviceFlags(u32::from(input.byte() % 8));
    // At least one request of `max_sectors`, as `Device::new` requires, plus up
    // to 64 MiB more.
    let data_vmo_size = u64::from(max_sectors) * u64::from(block_size) + u64::from(input.u16()) * 1024;
    Device::new(block_size, capacity, max_sectors, flags, data_vmo_size).expect("valid by construction")
}

/// The fuzzer writes somewhere in the ring.
fn scribble(input: &mut Input<'_>, shared: &Shared) {
    match input.byte() % 3 {
        0 => {
            let at = usize::from(input.u16()) % shared.len();
            let len = usize::from(input.byte() % 16);
            let bytes: Vec<u8> = (0..len).map(|_| input.byte()).collect();
            shared.poke(at, &bytes);
        }
        1 => {
            let field = header::SUB_TAIL + 4 * usize::from(input.byte() % 6);
            shared.poke(field, &input.u32().to_le_bytes());
        }
        _ => {
            let field = header::SUB_TAIL + 4 * usize::from(input.byte() % 6);
            let old = shared.bytes.borrow()[field..field + 4].to_vec();
            let old = u32::from_le_bytes(old.try_into().expect("four"));
            let delta = u32::from(input.byte() % 8);
            let new = if input.byte() & 1 == 0 { old.wrapping_add(delta) } else { old.wrapping_sub(delta) };
            shared.poke(field, &new.to_le_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// The honest kernel
// ---------------------------------------------------------------------------

#[derive(Default)]
struct KernelModel {
    next_id: u64,
    outstanding: BTreeMap<u64, Submission>,
    completed: BTreeSet<u64>,
    confirmed: bool,
    fuzz_tail: u32,
}

fn submission(input: &mut Input<'_>, device: &Device, id: u64) -> Submission {
    let op = input.byte() % 3;
    let sector = u64::from(input.u32()) % (device.capacity() + 1);
    let count = u32::from(input.byte()) % (device.max_sectors() + 2);
    let data_offset = u64::from(input.u32()) % (device.data_vmo_size() + 1);
    match op {
        0 => Submission::flush(id),
        1 => Submission::read(id, sector, count, data_offset),
        _ if input.byte() & 1 == 1 => Submission::write(id, sector, count, data_offset).with_fua(),
        _ => Submission::write(id, sector, count, data_offset),
    }
}

fn fuzz_kernel(input: &mut Input<'_>) {
    let entries = 2_u32 << (input.byte() % 4);
    let layout = RingLayout::standard(entries).expect("valid");
    let shared = Shared::new(layout.ring_bytes());
    let device = device(input);
    write_header(&mut Mem { shared: &shared, honest: Honest::Driver, layout }, &layout);
    if input.byte() & 1 == 1 {
        scribble(input, &shared);
    }
    let mut slots = vec![Slot::EMPTY; 1 + usize::from(input.byte() % 32)];
    let memory = Mem { shared: &shared, honest: Honest::Kernel, layout };
    let Ok(mut kernel) = KernelSide::attach(memory, layout.ring_bytes(), device, &mut slots) else {
        return;
    };
    shared.armed.set(true);
    let mut model = KernelModel { next_id: 1, ..KernelModel::default() };
    for _ in 0..MAX_OPS {
        if input.0.is_empty() {
            break;
        }
        kernel_step(input, &mut kernel, &mut model, &shared, layout);
        let vmo = kernel.data_vmo();
        assert!(model.confirmed || vmo != DataVmo::Releasable, "releasable before the reset");
        assert_eq!(kernel.outstanding(), model.outstanding.len(), "the table and the model agree");
    }
}

fn kernel_step(
    input: &mut Input<'_>,
    kernel: &mut KernelSide<'_, Mem<'_>>,
    model: &mut KernelModel,
    shared: &Shared,
    layout: RingLayout,
) {
    match input.byte() % 10 {
        0 | 1 => {
            let id = model.next_id;
            model.next_id += 1;
            let s = submission(input, kernel.device(), id);
            match kernel.submit(s) {
                Ok(()) => assert!(model.outstanding.insert(id, s).is_none(), "id reused"),
                Err(SubmitError::Full) => assert!(kernel.outstanding() >= kernel.capacity(), "false full"),
                Err(_) => {}
            }
        }
        2 => {
            let _ = kernel.publish();
        }
        3 | 4 => kernel_poll(kernel, model),
        5 => {
            let _ = kernel.prepare_to_sleep();
        }
        6 => kernel.woke(),
        7 => scribble(input, shared),
        8 => {
            let id = match model.outstanding.keys().nth(usize::from(input.byte())) {
                Some(&id) if input.byte() & 1 == 0 => id,
                _ => input.u64(),
            };
            let completion = RawCompletion {
                id,
                bytes_done: u64::from(input.u32()),
                status: u32::from(input.byte() % 6),
                // Anything: the kernel reads it as a measurement and never
                // judges or acts on it.
                device_ticks: input.u32(),
            };
            completion.write_to(&mut Mem { shared, honest: Honest::Driver, layout }, layout.completion_at(model.fuzz_tail));
            model.fuzz_tail = model.fuzz_tail.wrapping_add(1);
            shared.poke(header::COMP_TAIL, &model.fuzz_tail.to_le_bytes());
        }
        _ => kernel_lifecycle(input, kernel, model),
    }
}

fn kernel_poll(kernel: &mut KernelSide<'_, Mem<'_>>, model: &mut KernelModel) {
    match kernel.poll() {
        Ok(Some(completed)) => {
            let id = completed.submission.id;
            let submitted = model.outstanding.remove(&id).expect("accepted an id that is not outstanding");
            assert_eq!(submitted, completed.submission, "reported as submitted");
            assert!(model.completed.insert(id), "accepted an id twice");
            let len = kernel.device().payload_len(submitted.count);
            assert!(completed.bytes_done <= len, "more bytes than carried");
            if completed.status == Status::Ok && submitted.op != Op::Read {
                assert_eq!(completed.bytes_done, len, "a short OK write");
            }
        }
        Ok(None) => {}
        Err(corruption) => {
            assert_eq!(kernel.corruption(), Some(corruption), "recorded");
            assert_eq!(kernel.poll(), Err(corruption), "latched");
        }
    }
}

fn kernel_lifecycle(input: &mut Input<'_>, kernel: &mut KernelSide<'_, Mem<'_>>, model: &mut KernelModel) {
    match input.byte() % 4 {
        0 => kernel.stop(),
        choice @ (1 | 2) => {
            let ending = if choice == 1 { Ending::Stopped } else { Ending::DriverDied };
            let drained: BTreeSet<u64> = kernel.end(ending).map(|s| s.id).collect();
            let expected: BTreeSet<u64> = model.outstanding.keys().copied().collect();
            assert_eq!(drained, expected, "end fails exactly what is outstanding");
            model.outstanding.clear();
            let vmo = kernel.data_vmo();
            assert!(vmo == DataVmo::HeldUntilReset || (model.confirmed && vmo == DataVmo::Releasable), "held");
        }
        _ => {
            let ended = kernel.data_vmo() != DataVmo::InUse;
            let confirmed = kernel.confirm_reset().is_ok();
            assert_eq!(confirmed, ended, "only an ended ring is confirmed");
            model.confirmed |= confirmed;
        }
    }
}

// ---------------------------------------------------------------------------
// The honest driver
// ---------------------------------------------------------------------------

fn check_valid(s: &Submission, device: &Device) {
    match s.op {
        Op::Flush => assert_eq!(s.count, 0, "a flush with a count"),
        Op::Read | Op::Write => assert!(s.count > 0 && s.count <= device.max_sectors(), "a bad count"),
    }
    assert!(!s.fua || (s.op == Op::Write && device.flags().contains(DeviceFlags::FUA)), "bad FUA");
    assert!(
        u128::from(s.sector) + u128::from(s.count) <= u128::from(device.capacity()),
        "past the device"
    );
    let len = u128::from(s.count) * u128::from(device.block_size());
    assert!(u128::from(s.data_offset) + len <= u128::from(device.data_vmo_size()), "outside the data VMO");
}

fn fuzz_driver(input: &mut Input<'_>) {
    let entries = 2_u32 << (input.byte() % 4);
    let layout = RingLayout::standard(entries).expect("valid");
    let shared = Shared::new(layout.ring_bytes());
    let device = device(input);
    let memory = Mem { shared: &shared, honest: Honest::Driver, layout };
    let mut driver = DriverSide::new(memory, layout.ring_bytes(), layout, device).expect("fits");
    shared.armed.set(true);
    let mut held: Vec<Submission> = Vec::new();
    let mut fuzz_tail = 0_u32;
    for _ in 0..MAX_OPS {
        if input.0.is_empty() {
            break;
        }
        driver_step(input, &mut driver, &mut held, &mut fuzz_tail, &shared, layout);
        if driver.corruption().is_none() {
            assert_eq!(driver.held(), held.len() as u64, "the driver lost count");
        }
    }
}

fn driver_step(
    input: &mut Input<'_>,
    driver: &mut DriverSide<Mem<'_>>,
    held: &mut Vec<Submission>,
    fuzz_tail: &mut u32,
    shared: &Shared,
    layout: RingLayout,
) {
    match input.byte() % 9 {
        0 | 1 => match driver.consume() {
            Ok(Some(Consumed::Request(s))) => {
                check_valid(&s, driver.device());
                held.push(s);
            }
            Ok(Some(Consumed::Refused { .. }) | None) => {}
            Err(corruption) => {
                assert_eq!(driver.corruption(), Some(corruption), "recorded");
                assert_eq!(driver.consume(), Err(corruption), "latched");
            }
        },
        2 | 3 => {
            if held.is_empty() {
                return;
            }
            let s = held.swap_remove(usize::from(input.byte()) % held.len());
            let status = [Status::Ok, Status::IoError, Status::Unsupported][usize::from(input.byte() % 3)];
            let bytes = driver.device().payload_len(s.count);
            match driver.complete(s.id, status, bytes) {
                Ok(()) => {}
                Err(CompleteError::Corrupt(corruption)) => {
                    assert_eq!(driver.corruption(), Some(corruption), "recorded");
                }
                Err(CompleteError::NotHeld) => panic!("the driver lost count of what it holds"),
            }
        }
        4 => {
            let _ = driver.publish();
        }
        5 => {
            let _ = driver.prepare_to_sleep();
        }
        6 => driver.woke(),
        7 => scribble(input, shared),
        _ => {
            let raw = RawSubmission {
                id: u64::from(input.u16()),
                sector: u64::from(input.u32()) % (driver.device().capacity() + 2),
                data_offset: u64::from(input.u32()) % (driver.device().data_vmo_size() + 2),
                count: u32::from(input.byte()) % (driver.device().max_sectors() + 2),
                op: input.byte() % 5,
                flags: input.byte() % 4,
                reserved: u16::from(input.byte() == 0xFF),
            };
            raw.write_to(&mut Mem { shared, honest: Honest::Kernel, layout }, layout.submission_at(*fuzz_tail));
            *fuzz_tail = fuzz_tail.wrapping_add(1);
            shared.poke(header::SUB_TAIL, &fuzz_tail.to_le_bytes());
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = Input(data);
    if input.byte() & 1 == 0 {
        fuzz_kernel(&mut input);
    } else {
        fuzz_driver(&mut input);
    }
});
