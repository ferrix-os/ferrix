//! The loop, with the ring's own kernel end on one side and a fake device on
//! the other.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_netring::RingMemory;
use ferrix_netring::driver::DriverSide;
use ferrix_netring::kernel::{Completed, KernelSide};
use ferrix_netring::layout::{COMPLETION_LEN, HEADER_LEN, Op, SUBMISSION_LEN, Status};
use ferrix_virtio_net::{DeviceError, Event, Frame, SubmitError};

use crate::{Fault, Nic, Serve};

/// How many entries the tests' ring has.
const ENTRIES: u32 = 8;

/// How many bytes a slot holds.
const SLOT: u32 = 2048;

/// Bytes a test memory holds.
struct Memory {
    /// The bytes.
    bytes: Vec<u8>,
}

impl Memory {
    /// A zeroed memory of `len` bytes.
    fn new(len: usize) -> Memory {
        Memory {
            bytes: vec![0; len],
        }
    }
}

impl RingMemory for Memory {
    fn read_u8(&self, offset: usize) -> u8 {
        self.bytes.get(offset).copied().unwrap_or(0)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        if let Some(slot) = self.bytes.get_mut(offset) {
            *slot = value;
        }
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

/// A device that does what the test tells it to.
struct FakeNic {
    /// The transmit region.
    transmit: Vec<u8>,
    /// The receive region.
    receive: Vec<u8>,
    /// Frames handed over, in order.
    submitted: Vec<(u64, Vec<u8>)>,
    /// Events waiting to be drained.
    events: VecDeque<Event>,
    /// Buffers given back.
    released: Vec<u16>,
    /// Whether the queue refuses the next frame.
    full: bool,
    /// Whether the device is broken.
    broken: bool,
    /// The longest frame a transmit slot holds.
    capacity: u32,
}

impl FakeNic {
    /// A device with room for everything.
    fn new() -> FakeNic {
        FakeNic {
            transmit: vec![0; ENTRIES as usize * SLOT as usize],
            // More buffers than the ring has slots, as a device's ring has.
            receive: vec![0; 64 * SLOT as usize],
            submitted: Vec::new(),
            events: VecDeque::new(),
            released: Vec::new(),
            full: false,
            broken: false,
            capacity: SLOT,
        }
    }

    /// Say a frame arrived in `buffer`, carrying `bytes`.
    fn arrive(&mut self, buffer: u16, bytes: &[u8]) {
        let offset = u64::from(buffer) * u64::from(SLOT);
        let at = offset as usize;
        if let Some(slot) = self.receive.get_mut(at..at + bytes.len()) {
            slot.copy_from_slice(bytes);
        }
        self.events.push_back(Event::Received {
            buffer,
            offset,
            len: bytes.len() as u32,
        });
    }

    /// Say the frame with that id went out.
    fn sent(&mut self, id: u64) {
        self.events.push_back(Event::Sent { id });
    }
}

impl Nic for FakeNic {
    fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError> {
        if self.broken {
            return Err(SubmitError::Device(DeviceError::NeedsReset));
        }
        if self.full {
            return Err(SubmitError::QueueFull);
        }
        let at = frame.offset as usize;
        let bytes = self
            .transmit
            .get(at..at + frame.len as usize)
            .unwrap_or_default()
            .to_vec();
        self.submitted.push((frame.id, bytes));
        Ok(())
    }

    fn release(&mut self, buffer: u16) -> Result<(), DeviceError> {
        self.released.push(buffer);
        Ok(())
    }

    fn drain(&mut self, out: &mut [Event], receive_room: usize) -> Result<usize, DeviceError> {
        if self.broken {
            return Err(DeviceError::NeedsReset);
        }
        let mut count = 0;
        let mut received = 0;
        let mut kept = VecDeque::new();
        while count < out.len() {
            let Some(event) = self.events.pop_front() else {
                break;
            };
            if let Event::Received { .. } = event {
                if received >= receive_room || !kept.is_empty() {
                    // Held in the device's ring, in order.
                    kept.push_back(event);
                    continue;
                }
                received += 1;
            }
            if let Some(slot) = out.get_mut(count) {
                *slot = event;
            }
            count += 1;
        }
        kept.append(&mut self.events);
        self.events = kept;
        Ok(count)
    }

    fn write_transmit(&mut self, offset: u64, frame: &[u8]) {
        let at = offset as usize;
        if let Some(slot) = self.transmit.get_mut(at..at + frame.len()) {
            slot.copy_from_slice(frame);
        }
    }

    fn read_receive(&self, offset: u64, out: &mut [u8]) {
        let at = offset as usize;
        let len = out.len();
        if let Some(slot) = self.receive.get(at..at + len) {
            out.copy_from_slice(slot);
        }
    }

    fn transmit_slot(&self, index: u32) -> (u64, u32) {
        (u64::from(index) * u64::from(SLOT), self.capacity)
    }
}

/// A ring with both ends, a data area, a device and the loop.
struct Bench {
    /// The ring VMO.
    ring: Memory,
    /// The data VMO.
    data: Memory,
    /// The driver's end.
    side: DriverSide,
    /// The kernel's end.
    kernel: KernelSide,
    /// The device.
    nic: FakeNic,
    /// The loop.
    serve: Serve,
}

impl Bench {
    /// Everything, wired together.
    fn new() -> Bench {
        let ring_bytes = HEADER_LEN + ENTRIES as usize * (SUBMISSION_LEN + COMPLETION_LEN);
        let mut ring = Memory::new(ring_bytes);
        let side = DriverSide::create(&mut ring, ring_bytes, ENTRIES, SLOT).expect("a fresh ring");
        let data_bytes = ENTRIES as usize * SLOT as usize;
        let kernel = KernelSide::attach(&ring, ring_bytes, data_bytes).expect("the header");
        Bench {
            ring,
            data: Memory::new(data_bytes),
            side,
            kernel,
            nic: FakeNic::new(),
            serve: Serve::new(),
        }
    }

    /// The kernel posts a slot for the device to fill.
    fn post_receive(&mut self) -> u32 {
        let slot = self.kernel.free_slot().expect("a free slot");
        self.kernel
            .submit(&mut self.ring, slot, Op::Receive, 0)
            .expect("submitted");
        let _ = self.kernel.publish(&mut self.ring);
        slot
    }

    /// The kernel asks for a frame to be sent.
    fn transmit(&mut self, frame: &[u8]) -> u32 {
        let slot = self.kernel.free_slot().expect("a free slot");
        let offset = self
            .kernel
            .layout()
            .slot_offset(slot)
            .expect("a slot in the ring");
        for (at, byte) in frame.iter().enumerate() {
            self.data.write_u8(offset + at, *byte);
        }
        self.kernel
            .submit(&mut self.ring, slot, Op::Transmit, frame.len() as u32)
            .expect("submitted");
        let _ = self.kernel.publish(&mut self.ring);
        slot
    }

    /// One turn of the loop.
    fn turn(&mut self) -> Result<crate::Turn, Fault> {
        self.serve.turn(
            &mut self.ring,
            &mut self.data,
            &mut self.side,
            &mut self.nic,
        )
    }

    /// What the kernel drained.
    fn drain(&mut self) -> Vec<Completed> {
        let _ = self.side.publish(&mut self.ring);
        let mut done = [Completed {
            slot: 0,
            op: Op::Transmit,
            length: 0,
            status: Status::Ok,
        }; 16];
        let drained = self
            .kernel
            .drain(&mut self.ring, &mut done)
            .expect("the completion ring");
        done.iter().take(drained.taken).copied().collect()
    }

    /// The bytes in a ring slot.
    fn slot_bytes(&self, slot: u32, len: usize) -> Vec<u8> {
        let offset = self.kernel.layout().slot_offset(slot).expect("a slot");
        (0..len).map(|at| self.data.read_u8(offset + at)).collect()
    }
}

#[test]
fn a_frame_the_kernel_submits_reaches_the_device() {
    let mut bench = Bench::new();
    let slot = bench.transmit(b"a frame going out");
    let turn = bench.turn().expect("a turn");
    assert_eq!(turn.sent, 1);
    assert_eq!(bench.nic.submitted.len(), 1);
    assert_eq!(
        bench
            .nic
            .submitted
            .first()
            .map(|(id, bytes)| (*id, bytes.as_slice())),
        Some((u64::from(slot), b"a frame going out".as_slice()))
    );
}

#[test]
fn the_slot_is_answered_when_the_device_says_the_frame_went() {
    let mut bench = Bench::new();
    let slot = bench.transmit(b"gone");
    let _ = bench.turn().expect("a turn");
    assert!(
        bench.drain().is_empty(),
        "nothing is answered until it goes"
    );
    bench.nic.sent(u64::from(slot));
    let _ = bench.turn().expect("a turn");
    let done = bench.drain();
    assert_eq!(done.len(), 1);
    assert_eq!(
        done.first()
            .map(|entry| (entry.slot, entry.op, entry.status)),
        Some((slot, Op::Transmit, Status::Ok))
    );
}

#[test]
fn a_frame_that_arrives_lands_in_a_posted_slot() {
    let mut bench = Bench::new();
    let slot = bench.post_receive();
    let _ = bench.turn().expect("a turn");
    assert_eq!(bench.serve.posted(), 1);
    bench.nic.arrive(0, b"a frame coming in");
    let turn = bench.turn().expect("a turn");
    assert_eq!(turn.received, 1);
    let done = bench.drain();
    assert_eq!(
        done.first()
            .map(|entry| (entry.slot, entry.op, entry.length)),
        Some((slot, Op::Receive, 17))
    );
    assert_eq!(bench.slot_bytes(slot, 17), b"a frame coming in");
}

#[test]
fn the_device_gets_its_buffer_back_the_moment_the_bytes_are_copied() {
    let mut bench = Bench::new();
    let _ = bench.post_receive();
    let _ = bench.turn().expect("a turn");
    bench.nic.arrive(3, b"hello");
    let _ = bench.turn().expect("a turn");
    assert_eq!(bench.nic.released, alloc::vec![3]);
}

#[test]
fn a_frame_with_no_slot_waiting_stays_with_the_device_until_one_is_posted() {
    let mut bench = Bench::new();
    bench.nic.arrive(1, b"nowhere to go yet");
    let turn = bench.turn().expect("a turn");
    assert_eq!((turn.received, turn.dropped), (0, 0));
    assert!(
        bench.nic.released.is_empty(),
        "a frame with no slot is left in the device's ring, not thrown away"
    );
    let slot = bench.post_receive();
    let turn = bench.turn().expect("a turn");
    assert_eq!((turn.received, turn.dropped), (1, 0));
    assert_eq!(bench.nic.released, alloc::vec![1]);
    let done = bench.drain();
    assert_eq!(
        done.first().map(|entry| (entry.slot, entry.length)),
        Some((slot, 17))
    );
    assert_eq!(bench.slot_bytes(slot, 17), b"nowhere to go yet");
}

/// A burst far past the slots posted -- a TCP sender's window arriving at
/// line rate -- loses nothing and keeps its order: the device holds what the
/// ring has no room for (2026-10-22: dropping it cost a download 85 % of its
/// rate).
#[test]
fn a_burst_past_the_posted_slots_waits_in_the_device_and_arrives_in_order() {
    let mut bench = Bench::new();
    for _ in 0..4 {
        let _ = bench.post_receive();
    }
    for index in 0..40_u16 {
        bench.nic.arrive(index, &[index as u8; 20]);
    }
    let mut landed = Vec::new();
    for _ in 0..10 {
        let turn = bench.turn().expect("a turn");
        assert_eq!(turn.received, 4, "as many as there are slots");
        for entry in bench.drain() {
            landed.extend(bench.slot_bytes(entry.slot, 1));
        }
        for _ in 0..4 {
            let _ = bench.post_receive();
        }
    }
    assert_eq!(bench.serve.counters().dropped, 0);
    assert_eq!(landed, (0..40_u8).collect::<Vec<_>>());
    assert_eq!(bench.nic.released.len(), 40);
}

#[test]
fn slots_are_filled_oldest_first() {
    let mut bench = Bench::new();
    let first = bench.post_receive();
    let second = bench.post_receive();
    let _ = bench.turn().expect("a turn");
    bench.nic.arrive(0, b"one");
    bench.nic.arrive(1, b"two");
    let _ = bench.turn().expect("a turn");
    let done = bench.drain();
    assert_eq!(
        done.iter().map(|entry| entry.slot).collect::<Vec<_>>(),
        alloc::vec![first, second]
    );
    assert_eq!(bench.slot_bytes(first, 3), b"one");
    assert_eq!(bench.slot_bytes(second, 3), b"two");
}

#[test]
fn a_frame_the_queue_will_not_take_waits_and_nothing_else_is_taken() {
    let mut bench = Bench::new();
    bench.nic.full = true;
    let held = bench.transmit(b"waiting");
    let behind = bench.transmit(b"behind it");
    let turn = bench.turn().expect("a turn");
    assert_eq!(turn.sent, 0);
    assert!(bench.nic.submitted.is_empty(), "a full queue takes nothing");
    // Both are owed a completion: `take` advanced the ring's head for the
    // whole batch, so the second one waits behind the first rather than being
    // forgotten. Keeping only one is a lost packet, which is what the first
    // version of this did.
    assert_eq!(bench.serve.counters().deferred, 2);

    bench.nic.full = false;
    let turn = bench.turn().expect("a turn");
    assert_eq!(turn.sent, 2, "both waited, and both go");
    assert_eq!(
        bench
            .nic
            .submitted
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        alloc::vec![u64::from(held), u64::from(behind)],
        "they go in the order the kernel asked for them"
    );
}

/// A driver that will not sleep is a driver that spins, and the case where
/// that happens is the one where nothing it can do will help: a frame waiting
/// for the device's queue stops the loop taking any submission, so the ring
/// stays full and the ring's own answer is `Pending` for ever. Only the
/// device's interrupt can break it, so the answer must be to sleep.
#[test]
fn a_throttled_loop_sleeps_rather_than_spinning_on_a_ring_it_will_not_drain() {
    let mut bench = Bench::new();
    bench.nic.full = true;
    let _held = bench.transmit(b"waiting");
    let _turn = bench.turn().expect("a turn");
    assert!(
        bench.serve.throttled(),
        "the frame is waiting for the device"
    );

    // More for the kernel to send, which the loop will not take while the
    // first frame waits. The ring is not empty, so asking it would answer
    // `Pending` and the caller would turn again, and again.
    let _behind = bench.transmit(b"behind it");
    assert!(
        matches!(
            bench
                .side
                .prepare_to_sleep(&mut bench.ring)
                .expect("a ring answer"),
            ferrix_netring::Wait::Pending(_)
        ),
        "the ring has submissions, so on its own it says do not sleep"
    );
    let wait = bench
        .serve
        .before_sleep(&mut bench.ring, &mut bench.side)
        .expect("a verdict");
    assert_eq!(
        wait,
        ferrix_netring::Wait::Sleep,
        "a throttled loop sleeps and waits for the device"
    );

    // Once the device takes them, the ring decides again.
    bench.nic.full = false;
    let turn = bench.turn().expect("a turn");
    assert_eq!(turn.sent, 2);
    assert!(!bench.serve.throttled());
    let wait = bench
        .serve
        .before_sleep(&mut bench.ring, &mut bench.side)
        .expect("a verdict");
    assert_eq!(wait, ferrix_netring::Wait::Sleep, "and the ring is empty");
}

/// The loop remembers no more receive slots than a ring can have, so `Serve`
/// is a few hundred bytes rather than kilobytes on a driver's stack.
#[test]
fn the_slot_queue_is_the_rings_own_limit() {
    assert_eq!(
        crate::MAX_POSTED,
        ferrix_netring::layout::MAX_ENTRIES as usize
    );
    assert!(size_of::<Serve>() < 2048, "{}", size_of::<Serve>());
}

#[test]
fn a_frame_longer_than_the_device_carries_is_answered_rather_than_held() {
    let mut bench = Bench::new();
    let slot = bench.kernel.free_slot().expect("a free slot");
    // Longer than a device slot, which the ring itself would refuse -- so it
    // is written straight into the submission ring.
    bench
        .kernel
        .submit(&mut bench.ring, slot, Op::Transmit, SLOT)
        .expect("submitted");
    let _ = bench.kernel.publish(&mut bench.ring);
    let mut nic = FakeNic::new();
    // A device that carries less than the ring's slot holds.
    nic.capacity = 8;
    bench.nic = nic;
    let _ = bench.turn().expect("a turn");
    let done = bench.drain();
    assert_eq!(done.len(), 1, "a frame that cannot go is still answered");
}

#[test]
fn a_broken_device_stops_the_loop() {
    let mut bench = Bench::new();
    bench.nic.broken = true;
    let answer = bench.turn();
    assert!(matches!(answer, Err(Fault::Device(_))));
}

#[test]
fn a_hundred_frames_cross_in_both_directions_without_losing_one() {
    let mut bench = Bench::new();
    let mut sent = 0_u32;
    let mut received = 0_u32;
    let mut buffer = 0_u16;
    for round in 0..100_u32 {
        // Keep a receive slot posted and a frame going out.
        if bench.kernel.free_slot().is_some() {
            let _ = bench.post_receive();
        }
        if bench.kernel.free_slot().is_some() {
            let body = alloc::vec![round as u8; 64];
            let _ = bench.transmit(&body);
        }
        let _ = bench.turn().expect("a turn");
        // The device answers everything it was given, and delivers one frame.
        let ids: Vec<u64> = bench.nic.submitted.iter().map(|(id, _)| *id).collect();
        bench.nic.submitted.clear();
        for id in ids {
            bench.nic.sent(id);
            sent += 1;
        }
        if bench.serve.posted() > 0 {
            bench.nic.arrive(buffer, &alloc::vec![round as u8; 32]);
            buffer = (buffer + 1) % 8;
            received += 1;
        }
        let _ = bench.turn().expect("a turn");
        let _ = bench.drain();
    }
    assert_eq!(bench.serve.counters().sent, u64::from(sent));
    assert_eq!(bench.serve.counters().received, u64::from(received));
    assert_eq!(bench.serve.counters().dropped, 0);
    assert!(
        sent > 50 && received > 50,
        "{sent} sent and {received} received"
    );
}
