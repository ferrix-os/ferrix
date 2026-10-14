//! The ring, driven from both ends over one buffer.
//!
//! Every test is both sides stepped one after the other, so a barrier that
//! does nothing is correct here and nowhere else.

use alloc::vec;
use alloc::vec::Vec;

use crate::control::{Hello, Interface, InterfaceFlags, MAX_MESSAGE, Message, Refusal, Start};
use crate::driver::{DriverError, DriverSide};
use crate::kernel::{Completed, KernelSide, SubmitError};
use crate::layout::{
    COMPLETION_LEN, HEADER_LEN, HeaderError, Op, SUBMISSION_LEN, Status, Submission,
};
use crate::{Corruption, RingMemory};

/// How many entries the tests use.
const ENTRIES: u32 = 8;

/// How many bytes a slot holds in the tests.
const SLOT: u32 = 2048;

/// A ring in a vector, with a barrier that does nothing because the tests step
/// one side at a time.
struct Memory {
    /// The bytes.
    bytes: Vec<u8>,
}

impl Memory {
    /// A zeroed ring large enough for `ENTRIES` entries.
    fn new() -> Memory {
        Memory {
            bytes: vec![0; HEADER_LEN + ENTRIES as usize * (SUBMISSION_LEN + COMPLETION_LEN)],
        }
    }

    /// How long it is.
    fn len(&self) -> usize {
        self.bytes.len()
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

/// A ring with both ends attached.
fn pair() -> (Memory, DriverSide, KernelSide) {
    let mut memory = Memory::new();
    let size = memory.len();
    let driver = DriverSide::create(&mut memory, size, ENTRIES, SLOT).expect("a fresh ring");
    let kernel = KernelSide::attach(&memory, size, ENTRIES as usize * SLOT as usize)
        .expect("the header the driver wrote");
    (memory, driver, kernel)
}

#[test]
fn a_fresh_ring_is_one_the_kernel_takes_up() {
    let (memory, driver, kernel) = pair();
    assert_eq!(kernel.layout().entries(), ENTRIES);
    assert_eq!(kernel.layout().slot_bytes(), SLOT);
    assert_eq!(driver.layout(), kernel.layout());
    assert_eq!(memory.read_u8(0), b'F');
    assert_eq!(kernel.outstanding(), 0);
}

#[test]
fn a_ring_too_small_for_its_arrays_is_refused() {
    let mut memory = Memory::new();
    let answer = DriverSide::create(&mut memory, HEADER_LEN + 8, ENTRIES, SLOT);
    assert_eq!(answer.err(), Some(HeaderError::Arrays));
}

#[test]
fn the_entry_count_must_be_a_power_of_two_in_range() {
    let mut memory = Memory::new();
    let size = memory.len();
    for bad in [0, 1, 3, 6, 8192] {
        assert_eq!(
            DriverSide::create(&mut memory, size, bad, SLOT).err(),
            Some(HeaderError::Entries(bad)),
            "{bad} entries should be refused"
        );
    }
}

#[test]
fn a_slot_must_be_a_cache_line_multiple_in_range() {
    let mut memory = Memory::new();
    let size = memory.len();
    for bad in [0, 32, 100, 131_072] {
        assert_eq!(
            DriverSide::create(&mut memory, size, ENTRIES, bad).err(),
            Some(HeaderError::SlotBytes(bad)),
            "a slot of {bad} bytes should be refused"
        );
    }
}

#[test]
fn a_header_without_the_magic_is_not_a_ring() {
    let (mut memory, _driver, _kernel) = pair();
    memory.write_u8(0, b'X');
    let size = memory.len();
    assert_eq!(
        KernelSide::attach(&memory, size, ENTRIES as usize * SLOT as usize).err(),
        Some(crate::kernel::AttachError::Header(HeaderError::NotARing))
    );
}

#[test]
fn a_header_of_another_version_is_refused() {
    let (mut memory, _driver, _kernel) = pair();
    memory.write_u16(4, 2);
    let size = memory.len();
    assert_eq!(
        KernelSide::attach(&memory, size, ENTRIES as usize * SLOT as usize).err(),
        Some(crate::kernel::AttachError::Header(HeaderError::Version(2)))
    );
}

#[test]
fn a_data_area_too_small_for_its_slots_is_refused() {
    let (memory, _driver, _kernel) = pair();
    let size = memory.len();
    assert_eq!(
        KernelSide::attach(&memory, size, ENTRIES as usize * SLOT as usize - 1).err(),
        Some(crate::kernel::AttachError::DataTooSmall)
    );
}

#[test]
fn a_frame_submitted_is_the_frame_taken() {
    let (mut memory, mut driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("a fresh ring has free slots");
    kernel
        .submit(&mut memory, slot, Op::Transmit, 60)
        .expect("a slot nobody holds");
    assert!(
        kernel.publish(&mut memory).is_none(),
        "nobody asked to be rung"
    );

    let mut taken = [Submission {
        slot: 0,
        length: 0,
        op: Op::Transmit,
    }; 4];
    let consumed = driver
        .take(&mut memory, &mut taken)
        .expect("the submission");
    assert_eq!(consumed.taken, 1);
    assert!(!consumed.more);
    assert_eq!(
        taken.first(),
        Some(&Submission {
            slot,
            length: 60,
            op: Op::Transmit
        })
    );
}

#[test]
fn a_completion_frees_the_slot_it_answers() {
    let (mut memory, mut driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Receive, 0)
        .expect("submitted");
    let _ = kernel.publish(&mut memory);
    assert!(kernel.is_outstanding(slot));
    assert_eq!(kernel.outstanding(), 1);

    let mut taken = [Submission {
        slot: 0,
        length: 0,
        op: Op::Transmit,
    }; 4];
    let _ = driver.take(&mut memory, &mut taken).expect("taken");
    driver
        .complete(&mut memory, slot, 1_500, Status::Ok)
        .expect("completed");
    let _ = driver.publish(&mut memory);

    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 4];
    let drained = kernel.drain(&mut memory, &mut done).expect("drained");
    assert_eq!(drained.taken, 1);
    assert_eq!(
        done.first(),
        Some(&Completed {
            slot,
            op: Op::Receive,
            length: 1_500,
            status: Status::Ok
        })
    );
    assert!(!kernel.is_outstanding(slot));
    assert_eq!(kernel.outstanding(), 0);
}

#[test]
fn a_slot_cannot_be_submitted_twice() {
    let (mut memory, _driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Transmit, 10)
        .expect("the first");
    assert_eq!(
        kernel.submit(&mut memory, slot, Op::Transmit, 10),
        Err(SubmitError::SlotBusy)
    );
}

#[test]
fn a_frame_longer_than_a_slot_is_refused() {
    let (mut memory, _driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("free");
    assert_eq!(
        kernel.submit(&mut memory, slot, Op::Transmit, SLOT + 1),
        Err(SubmitError::TooLong)
    );
}

#[test]
fn a_ring_with_every_slot_out_takes_no_more() {
    let (mut memory, _driver, mut kernel) = pair();
    for _ in 0..ENTRIES {
        let slot = kernel.free_slot().expect("a slot while any is free");
        kernel
            .submit(&mut memory, slot, Op::Receive, 0)
            .expect("submitted");
    }
    assert_eq!(kernel.free_slot(), None);
    assert_eq!(kernel.outstanding(), ENTRIES);
}

#[test]
fn every_slot_goes_out_and_comes_back() {
    let (mut memory, mut driver, mut kernel) = pair();
    for round in 0..4_u32 {
        for _ in 0..ENTRIES {
            let slot = kernel.free_slot().expect("free");
            kernel
                .submit(&mut memory, slot, Op::Transmit, round)
                .expect("submitted");
        }
        let _ = kernel.publish(&mut memory);
        let mut taken = [Submission {
            slot: 0,
            length: 0,
            op: Op::Transmit,
        }; ENTRIES as usize];
        let consumed = driver.take(&mut memory, &mut taken).expect("taken");
        assert_eq!(consumed.taken, ENTRIES as usize);
        for entry in &taken {
            driver
                .complete(&mut memory, entry.slot, entry.length, Status::Ok)
                .expect("completed");
        }
        let _ = driver.publish(&mut memory);
        let mut done = [Completed {
            slot: 0,
            op: Op::Transmit,
            length: 0,
            status: Status::Ok,
        }; ENTRIES as usize];
        let drained = kernel.drain(&mut memory, &mut done).expect("drained");
        assert_eq!(drained.taken, ENTRIES as usize);
        assert_eq!(kernel.outstanding(), 0);
    }
}

#[test]
fn a_drain_into_a_short_buffer_says_there_is_more() {
    let (mut memory, mut driver, mut kernel) = pair();
    for _ in 0..4 {
        let slot = kernel.free_slot().expect("free");
        kernel
            .submit(&mut memory, slot, Op::Receive, 0)
            .expect("submitted");
    }
    let _ = kernel.publish(&mut memory);
    let mut taken = [Submission {
        slot: 0,
        length: 0,
        op: Op::Transmit,
    }; 4];
    let _ = driver.take(&mut memory, &mut taken).expect("taken");
    for entry in &taken {
        driver
            .complete(&mut memory, entry.slot, 64, Status::Ok)
            .expect("completed");
    }
    let _ = driver.publish(&mut memory);

    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 2];
    let drained = kernel.drain(&mut memory, &mut done).expect("drained");
    assert_eq!(drained.taken, 2);
    assert!(drained.more);
}

#[test]
fn a_completion_for_a_slot_nobody_submitted_is_corruption() {
    let (mut memory, mut driver, mut kernel) = pair();
    driver
        .complete(&mut memory, 3, 64, Status::Ok)
        .expect("the driver may write what it likes");
    let _ = driver.publish(&mut memory);
    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 2];
    assert_eq!(
        kernel.drain(&mut memory, &mut done),
        Err(Corruption::UnknownSlot)
    );
    // Corruption is terminal: the same answer from every later call.
    assert_eq!(
        kernel.drain(&mut memory, &mut done),
        Err(Corruption::UnknownSlot)
    );
    assert_eq!(kernel.corruption(), Some(Corruption::UnknownSlot));
}

#[test]
fn a_completion_naming_a_slot_the_ring_has_not_is_corruption() {
    let (mut memory, _driver, mut kernel) = pair();
    let layout = *kernel.layout();
    // Written by hand, because the driver's own `complete` refuses it.
    memory.write_u32(layout.completion_at(0), ENTRIES + 1);
    memory.write_u32(layout.completion_at(0) + 4, 0);
    memory.write_u32(layout.completion_at(0) + 8, 0);
    memory.write_u32(layout.comp_tail(), 1);
    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 2];
    assert_eq!(
        kernel.drain(&mut memory, &mut done),
        Err(Corruption::SlotOutOfRange)
    );
}

#[test]
fn a_completion_longer_than_a_slot_is_corruption() {
    let (mut memory, _driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Receive, 0)
        .expect("submitted");
    let layout = *kernel.layout();
    memory.write_u32(layout.completion_at(0), slot);
    memory.write_u32(layout.completion_at(0) + 4, SLOT + 1);
    memory.write_u32(layout.completion_at(0) + 8, 0);
    memory.write_u32(layout.comp_tail(), 1);
    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 2];
    assert_eq!(
        kernel.drain(&mut memory, &mut done),
        Err(Corruption::LengthTooLarge)
    );
}

#[test]
fn a_tail_that_ran_away_is_corruption() {
    let (mut memory, _driver, mut kernel) = pair();
    let layout = *kernel.layout();
    memory.write_u32(layout.comp_tail(), ENTRIES + 1);
    let mut done = [Completed {
        slot: 0,
        op: Op::Transmit,
        length: 0,
        status: Status::Ok,
    }; 2];
    assert_eq!(
        kernel.drain(&mut memory, &mut done),
        Err(Corruption::TailOverrun)
    );
}

#[test]
fn a_submission_with_an_unknown_operation_is_corruption() {
    let (mut memory, mut driver, _kernel) = pair();
    let layout = *driver.layout();
    memory.write_u32(layout.submission_at(0), 0);
    memory.write_u32(layout.submission_at(0) + 4, 0);
    memory.write_u8(layout.submission_at(0) + 8, 7);
    memory.write_u32(layout.sub_tail(), 1);
    let mut taken = [Submission {
        slot: 0,
        length: 0,
        op: Op::Transmit,
    }; 2];
    assert_eq!(
        driver.take(&mut memory, &mut taken),
        Err(Corruption::UnknownOp)
    );
}

#[test]
fn the_driver_refuses_to_complete_a_slot_the_ring_has_not() {
    let (mut memory, mut driver, _kernel) = pair();
    assert_eq!(
        driver.complete(&mut memory, ENTRIES, 0, Status::Ok),
        Err(DriverError::SlotOutOfRange)
    );
    assert_eq!(
        driver.complete(&mut memory, 0, SLOT + 1, Status::Ok),
        Err(DriverError::LengthTooLarge)
    );
}

#[test]
fn the_want_bell_handshake_rings_only_when_asked() {
    let (mut memory, mut driver, mut kernel) = pair();
    // The driver asks to be rung and finds nothing, so it sleeps.
    assert_eq!(
        driver.prepare_to_sleep(&mut memory),
        Ok(crate::bell::Wait::Sleep)
    );
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Transmit, 4)
        .expect("submitted");
    let bell = kernel.publish(&mut memory).expect("the driver asked");
    assert_eq!(bell.key(), crate::bell::BELL_SUBMIT);
    driver.woke(&mut memory);

    // Now nobody is asking, so publishing rings nothing.
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Transmit, 4)
        .expect("submitted");
    assert!(kernel.publish(&mut memory).is_none());
}

#[test]
fn a_side_about_to_sleep_that_finds_work_does_not() {
    let (mut memory, mut driver, mut kernel) = pair();
    let slot = kernel.free_slot().expect("free");
    kernel
        .submit(&mut memory, slot, Op::Transmit, 4)
        .expect("submitted");
    let _ = kernel.publish(&mut memory);
    assert_eq!(
        driver.prepare_to_sleep(&mut memory),
        Ok(crate::bell::Wait::Pending(1))
    );
}

#[test]
fn abandoning_gives_every_slot_back() {
    let (mut memory, _driver, mut kernel) = pair();
    for _ in 0..3 {
        let slot = kernel.free_slot().expect("free");
        kernel
            .submit(&mut memory, slot, Op::Receive, 0)
            .expect("submitted");
    }
    assert_eq!(kernel.abandon(), 3);
    assert_eq!(kernel.outstanding(), 0);
    assert_eq!(kernel.free_slot(), Some(0));
}

#[test]
fn a_slot_offset_is_its_index_times_the_slot_size() {
    let (memory, _driver, kernel) = pair();
    let _ = memory;
    assert_eq!(kernel.layout().slot_offset(0), Ok(0));
    assert_eq!(kernel.layout().slot_offset(3), Ok(3 * SLOT as usize));
    assert_eq!(
        kernel.layout().slot_offset(ENTRIES),
        Err(Corruption::SlotOutOfRange)
    );
    assert_eq!(
        kernel.layout().data_bytes(),
        ENTRIES as usize * SLOT as usize
    );
}

/// A hello a driver would send.
fn hello() -> Hello {
    Hello {
        version: crate::layout::VERSION,
        entries: ENTRIES,
        slot_bytes: SLOT,
        interface: Interface::new(
            b"eth0",
            [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            1500,
            InterfaceFlags {
                carrier: true,
                broadcast: true,
                multicast: true,
            },
        ),
    }
}

#[test]
fn a_hello_a_driver_would_send_is_one_the_kernel_takes() {
    assert_eq!(hello().validate(), Ok(()));
}

#[test]
fn a_hello_is_refused_for_the_first_reason_that_applies() {
    let mut wrong = hello();
    wrong.version = 2;
    wrong.entries = 3;
    assert_eq!(wrong.validate(), Err(Refusal::Version));

    let mut wrong = hello();
    wrong.entries = 3;
    wrong.slot_bytes = 1;
    assert_eq!(wrong.validate(), Err(Refusal::Entries));

    let mut wrong = hello();
    wrong.slot_bytes = 100;
    assert_eq!(wrong.validate(), Err(Refusal::SlotBytes));

    let mut wrong = hello();
    wrong.interface = Interface::new(b"", [0; 6], 1500, InterfaceFlags::default());
    assert_eq!(wrong.validate(), Err(Refusal::Name));

    let mut wrong = hello();
    wrong.interface = Interface::new(b"eth 0", [0; 6], 1500, InterfaceFlags::default());
    assert_eq!(wrong.validate(), Err(Refusal::Name));

    let mut wrong = hello();
    wrong.interface = Interface::new(b"eth0", [0; 6], 60, InterfaceFlags::default());
    assert_eq!(wrong.validate(), Err(Refusal::Mtu));

    // An MTU a slot cannot carry, frame header included.
    let mut wrong = hello();
    wrong.interface = Interface::new(b"eth0", [0; 6], SLOT, InterfaceFlags::default());
    assert_eq!(wrong.validate(), Err(Refusal::Mtu));
}

#[test]
fn every_message_survives_the_trip_through_bytes() {
    let messages = [
        Message::Start(Start {
            index: 2,
            location: 0x0001_0203,
        }),
        Message::Hello(hello()),
        Message::Ready,
        Message::Refused(Refusal::NameInUse),
        Message::Link(true),
        Message::Link(false),
        Message::Stop,
        Message::Stopped,
    ];
    let mut bytes = [0_u8; MAX_MESSAGE];
    for message in messages {
        let written = message.encode(&mut bytes).expect("room for every message");
        assert!(written <= MAX_MESSAGE);
        let read = Message::decode(bytes.get(..written).expect("what was written"));
        assert_eq!(read, Ok(message));
    }
}

#[test]
fn a_message_cut_short_is_refused_rather_than_guessed() {
    let mut bytes = [0_u8; MAX_MESSAGE];
    let written = Message::Hello(hello()).encode(&mut bytes).expect("room");
    for cut in 0..written {
        let answer = Message::decode(bytes.get(..cut).unwrap_or_default());
        assert!(
            answer.is_err(),
            "a hello cut to {cut} bytes should not decode"
        );
    }
}

#[test]
fn a_kind_this_version_has_not_is_refused_by_name() {
    assert_eq!(
        Message::decode(&[99]),
        Err(crate::control::MessageError::UnknownKind(99))
    );
    assert_eq!(
        Message::decode(&[]),
        Err(crate::control::MessageError::Truncated)
    );
}

#[test]
fn a_refusal_code_this_version_has_not_is_malformed() {
    assert_eq!(
        Message::decode(&[4, 99]),
        Err(crate::control::MessageError::Malformed)
    );
}

#[test]
fn an_interface_name_longer_than_the_field_is_cut_rather_than_refused() {
    let interface = Interface::new(
        b"a-very-long-interface-name",
        [0; 6],
        1500,
        InterfaceFlags::default(),
    );
    assert_eq!(interface.name().len(), crate::control::MAX_NAME);
    assert_eq!(interface.name(), b"a-very-long-int");
}

#[test]
fn the_flags_survive_the_trip_through_bits() {
    for carrier in [false, true] {
        for broadcast in [false, true] {
            for multicast in [false, true] {
                let flags = InterfaceFlags {
                    carrier,
                    broadcast,
                    multicast,
                };
                assert_eq!(InterfaceFlags::from_bits(flags.bits()), flags);
            }
        }
    }
}
