//! Every wrapper against a recorder that plays the kernel.
//!
//! [`Recorder`] implements [`Syscall`]: it notes each call's number and
//! registers and answers with whatever the test queued, which may read and
//! write the call's memory through [`Raw::memory`] and [`Raw::memory_mut`]
//! exactly as the kernel's `copy_from_user` and `copy_to_user` would. So each
//! test says, of one wrapper: this number, these registers in this order, a
//! pointer argument naming these bytes, and this decoding of the answer.

extern crate std;

use std::boxed::Box;
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::vec;
use std::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Requested, Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types;
use ferrix_native_abi::types::{
    DEVICE_INFO_BYTES, DEVICE_VIRTIO_PCI, DeviceBlock, IoMappingSpec, PACKET_SIGNAL, PACKET_USER,
    PIN_READ_ONLY, PortPacket,
};

use crate::call::{Raw, Syscall};
use crate::channel::{self, Channel, ReadError, Received};
use crate::chardev;
use crate::device::{Device, Interrupt, IoMapping, Limit};
use crate::error::{Error, decode, decode_handle};
use crate::handle::{Deadline, Object, OwnedHandle, rights_register};
use crate::job::{self, Job};
use crate::pending::{self, Process, Protection};
use crate::pin::{Addresses, Pin, PinAccess, device_address};
use crate::port::{self, Port};
use crate::vmo::{self, Vmo};

/// What the recorder does when a call arrives.
type Reply = Box<dyn for<'r, 'a> FnOnce(&'r mut Raw<'a>) -> usize>;

/// One call as it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Made {
    number: usize,
    args: [usize; 6],
}

/// A kernel that remembers and answers from a script.
#[derive(Default)]
struct Recorder {
    calls: RefCell<Vec<Made>>,
    replies: RefCell<VecDeque<Reply>>,
}

impl core::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recorder")
            .field("calls", &self.calls.borrow())
            .finish_non_exhaustive()
    }
}

impl Syscall for &Recorder {
    fn call(self, mut raw: Raw<'_>) -> usize {
        self.calls.borrow_mut().push(Made {
            number: raw.number(),
            args: raw.args(),
        });
        let reply = self.replies.borrow_mut().pop_front();
        reply.map_or(0, |reply| reply(&mut raw))
    }

    fn call_words(self, raw: Raw<'_>) -> Option<(usize, [usize; 3])> {
        Some((self.call(raw), [0; 3]))
    }
}

impl Recorder {
    /// Answer the next call with `reply`. Unscripted calls succeed with 0.
    fn answer(&self, reply: impl for<'r, 'a> FnOnce(&'r mut Raw<'a>) -> usize + 'static) {
        self.replies.borrow_mut().push_back(Box::new(reply));
    }

    /// Answer the next call with `value`.
    fn returns(&self, value: usize) {
        self.answer(move |_| value);
    }

    /// Fail the next call with `errno`.
    fn fails(&self, errno: Errno) {
        self.returns(errno.as_return_value().cast_unsigned());
    }

    /// Every call so far, forgotten.
    fn take(&self) -> Vec<Made> {
        core::mem::take(&mut *self.calls.borrow_mut())
    }

    /// The calls so far, as numbers only.
    fn numbers(&self) -> Vec<usize> {
        self.calls.borrow().iter().map(|made| made.number).collect()
    }
}

/// A call to `number` with these registers, the rest zero.
fn made(number: usize, leading: &[usize]) -> Made {
    let mut args = [0; 6];
    args[..leading.len()].copy_from_slice(leading);
    Made { number, args }
}

/// The 64-bit value a pointer argument names.
fn read_u64(raw: &Raw<'_>, address: usize) -> u64 {
    u64::from_ne_bytes(raw.memory(address, 8).unwrap().try_into().unwrap())
}

/// Write `bytes` where a pointer argument says.
fn write(raw: &mut Raw<'_>, address: usize, bytes: &[u8]) {
    raw.memory_mut(address, bytes.len())
        .expect("the kernel writes only where an output pointer names")
        .copy_from_slice(bytes);
}

/// A handle `sys` owns, of kind `T`.
fn owned(sys: &Recorder, value: u32) -> OwnedHandle<&Recorder> {
    OwnedHandle::from_raw(sys, Handle(value))
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn every_native_status_decodes_to_its_own_name_and_back() {
    let mut names = BTreeSet::new();
    for errno in status::ALL {
        let error = Error::from_errno(errno);
        assert!(!matches!(error, Error::Other(_)), "{errno:?} has no name");
        assert_eq!(error.errno(), Some(errno), "{error:?} round trip");
        assert!(names.insert(std::format!("{error:?}")), "{error:?} twice");
        assert_eq!(decode(errno.as_return_value().cast_unsigned()), Err(error));
    }
    assert_eq!(Error::from_errno(Errno::ENOSYS), Error::Unsupported);
    assert_eq!(Error::from_errno(Errno::ESRCH), Error::NoProcess);
    assert_eq!(Error::from_errno(Errno::EINTR), Error::Interrupted);
    assert_eq!(
        Error::from_errno(Errno::ENOENT),
        Error::Other(Errno::ENOENT)
    );
    assert_eq!(Error::Unexpected(3).errno(), None);
}

#[test]
fn only_the_errno_range_is_a_failure() {
    assert_eq!(decode(0), Ok(0));
    assert_eq!(decode(0x7FFF), Ok(0x7FFF));
    assert_eq!(decode(usize::MAX), Err(Error::Other(Errno(1))));
    assert_eq!(
        decode((-4095_isize).cast_unsigned()),
        Err(Error::Other(Errno(4095)))
    );
    let just_below = (-4096_isize).cast_unsigned();
    assert_eq!(
        decode(just_below),
        Ok(just_below),
        "an address, not an error"
    );
}

#[test]
fn a_handle_result_must_be_a_handle() {
    assert_eq!(decode_handle(0x1001), Ok(Handle(0x1001)));
    assert_eq!(decode_handle(0), Err(Error::Unexpected(0)));
    if let Ok(wide) = usize::try_from(u64::from(u32::MAX) + 1) {
        assert_eq!(
            decode_handle(wide),
            Err(Error::Unexpected(wide)),
            "never truncated"
        );
    }
    assert_eq!(
        decode_handle(status::BAD_HANDLE.as_return_value().cast_unsigned()),
        Err(Error::BadHandle)
    );
}

#[test]
fn rights_and_deadlines_and_protection_travel_as_the_abi_says() {
    assert_eq!(rights_register(Requested::Same), SAME_RIGHTS as usize);
    assert_eq!(
        rights_register(Requested::Exactly(Rights::READ | Rights::WAIT)),
        0x24
    );
    assert_eq!(Deadline::Never.bytes(), None);
    assert_eq!(Deadline::At(7).bytes(), Some(7_u64.to_ne_bytes()));
    assert_eq!(Protection::Read.register(), 1);
    assert_eq!(Protection::ReadWrite.register(), 3);
}

// ---------------------------------------------------------------------------
// The memory a call names
// ---------------------------------------------------------------------------

#[test]
fn a_raw_call_reaches_only_the_memory_it_borrows() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 9));
    let message = *b"abcdef";
    sys.answer(|raw| {
        let at = raw.args()[1];
        assert_eq!(raw.memory(at, 6), Some(&b"abcdef"[..]));
        assert_eq!(
            raw.memory(at + 2, 3),
            Some(&b"cde"[..]),
            "an interior piece"
        );
        assert_eq!(raw.memory(at + 4, 3), None, "a piece running past the end");
        assert_eq!(raw.memory(at - 1, 2), None, "a piece starting before it");
        assert_eq!(raw.memory(0, 1), None, "null");
        assert!(raw.memory_mut(at, 1).is_none(), "input is never writable");
        0
    });
    channel.write(&message).unwrap();
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

#[test]
fn a_handle_is_closed_once_when_dropped_and_never_when_given_up() {
    let sys = Recorder::default();
    drop(owned(&sys, 5));
    drop(OwnedHandle::from_raw(&sys, Handle::INVALID));
    assert_eq!(owned(&sys, 6).into_raw(), Handle(6));
    assert_eq!(sys.take(), [made(nr::HANDLE_CLOSE, &[5])]);

    sys.fails(status::BAD_HANDLE);
    assert_eq!(owned(&sys, 7).close(), Err(Error::BadHandle));
    assert_eq!(
        sys.take(),
        [made(nr::HANDLE_CLOSE, &[7])],
        "close is not repeated"
    );
}

#[test]
fn duplicate_asks_for_rights_and_owns_the_new_handle() {
    let sys = Recorder::default();
    let vmo = Vmo::from_owned(owned(&sys, 0x10));
    sys.returns(0x11);
    let copy = vmo.duplicate(Requested::Exactly(Rights::READ)).unwrap();
    assert_eq!(copy.handle(), Handle(0x11));
    sys.fails(status::ACCESS_DENIED);
    assert_eq!(
        vmo.duplicate(Requested::Same).unwrap_err(),
        Error::AccessDenied
    );
    drop(copy);
    assert_eq!(
        sys.take(),
        [
            made(nr::HANDLE_DUPLICATE, &[0x10, Rights::READ.0 as usize]),
            made(nr::HANDLE_DUPLICATE, &[0x10, SAME_RIGHTS as usize]),
            made(nr::HANDLE_CLOSE, &[0x11]),
        ]
    );
}

#[test]
fn replace_gives_up_the_original_only_when_it_succeeds() {
    let sys = Recorder::default();
    sys.returns(0x21);
    let job = Job::from_owned(owned(&sys, 0x20))
        .replace(Requested::Same)
        .unwrap();
    assert_eq!(job.handle(), Handle(0x21));

    sys.fails(status::ACCESS_DENIED);
    let (error, job) = job.replace(Requested::Exactly(Rights::ALL)).unwrap_err();
    assert_eq!(error, Error::AccessDenied);
    assert_eq!(
        job.handle(),
        Handle(0x21),
        "a failed replace changes nothing"
    );
    drop(job);
    assert_eq!(
        sys.take(),
        [
            made(nr::HANDLE_REPLACE, &[0x20, SAME_RIGHTS as usize]),
            made(nr::HANDLE_REPLACE, &[0x21, Rights::ALL.0 as usize]),
            made(nr::HANDLE_CLOSE, &[0x21]),
        ]
    );
}

#[test]
fn wait_one_names_its_deadline_and_reads_what_was_observed() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 3));
    sys.answer(|raw| {
        let [_, _, deadline, observed, ..] = raw.args();
        assert_eq!(read_u64(raw, deadline), 1_000);
        write(
            raw,
            observed,
            &(Signals::READABLE | Signals::PEER_CLOSED).0.to_ne_bytes(),
        );
        0
    });
    let seen = channel
        .wait_one(
            Signals::READABLE | Signals::PEER_CLOSED,
            Deadline::At(1_000),
        )
        .unwrap();
    assert_eq!(seen, Signals::READABLE | Signals::PEER_CLOSED);

    sys.fails(status::TIMED_OUT);
    let forever = channel.wait_one(Signals::READABLE, Deadline::Never);
    assert_eq!(forever, Err(Error::TimedOut));
    let calls = sys.take();
    assert_eq!(calls[0].number, nr::OBJECT_WAIT_ONE);
    assert_eq!(calls[0].args[..2], [3, 5]);
    assert_ne!(calls[0].args[2], 0);
    assert_eq!(
        calls[1].args[..3],
        [3, 1, 0],
        "a null deadline waits forever"
    );
    assert_ne!(calls[1].args[3], 0, "observed is always offered");
}

#[test]
fn wait_async_carries_its_key_through_a_pointer() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 3));
    let port = Port::from_owned(owned(&sys, 4));
    sys.answer(|raw| {
        assert_eq!(raw.args()[..3], [3, 4, Signals::PEER_CLOSED.0 as usize]);
        assert_eq!(read_u64(raw, raw.args()[3]), 0xFEED);
        0
    });
    channel
        .wait_async(&port, Signals::PEER_CLOSED, 0xFEED)
        .unwrap();
    assert_eq!(sys.numbers()[0], nr::OBJECT_WAIT_ASYNC);
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

#[test]
fn channel_create_reads_both_handles_from_its_output() {
    let sys = Recorder::default();
    sys.answer(|raw| {
        let out = raw.args()[0];
        write(raw, out, &[0x31, 0, 0, 0, 0x32, 0, 0, 0]);
        0
    });
    let (left, right) = channel::create(&sys).unwrap();
    assert_eq!(
        (left.handle(), right.handle()),
        (Handle(0x31), Handle(0x32))
    );
    drop((left, right));
    assert_eq!(
        sys.numbers(),
        [nr::CHANNEL_CREATE, nr::HANDLE_CLOSE, nr::HANDLE_CLOSE]
    );
    assert_eq!(sys.take()[0].args[1..], [0; 5]);

    sys.answer(|raw| {
        let out = raw.args()[0];
        write(raw, out, &[0x41, 0, 0, 0, 0, 0, 0, 0]);
        0
    });
    assert_eq!(channel::create(&sys).unwrap_err(), Error::Unexpected(0));
    assert_eq!(
        sys.take()[1],
        made(nr::HANDLE_CLOSE, &[0x41]),
        "no half-made pair leaks"
    );
    sys.fails(status::NO_HANDLES);
    assert_eq!(channel::create(&sys).unwrap_err(), Error::NoHandles);
}

#[test]
fn channel_write_sends_bytes_and_no_handles() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 7));
    sys.answer(|raw| {
        let [handle, bytes, count, handles, handle_count, _] = raw.args();
        assert_eq!((handle, count, handles, handle_count), (7, 4, 0, 0));
        assert_eq!(raw.memory(bytes, 4), Some(&b"ping"[..]));
        0
    });
    channel.write(b"ping").unwrap();
    channel.write(&[]).unwrap();
    sys.fails(status::PEER_CLOSED);
    assert_eq!(channel.write(b"x"), Err(Error::PeerClosed));
    let calls = sys.take();
    assert_eq!(calls[0].number, nr::CHANNEL_WRITE);
    assert_eq!(
        calls[1],
        made(nr::CHANNEL_WRITE, &[7, 0, 0, 0, 0]),
        "empty is null"
    );
}

#[test]
fn handles_leave_with_a_message_only_if_it_is_sent() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 7));
    sys.answer(|raw| {
        let [_, bytes, count, handles, handle_count, _] = raw.args();
        assert_eq!((count, handle_count), (2, 2));
        assert_eq!(raw.memory(bytes, 2), Some(&b"hi"[..]));
        assert_eq!(
            raw.memory(handles, 8),
            Some(&[0x51, 0, 0, 0, 0x52, 0, 0, 0][..])
        );
        0
    });
    channel
        .write_with(b"hi", [owned(&sys, 0x51), owned(&sys, 0x52)])
        .unwrap();
    assert_eq!(
        sys.numbers(),
        [nr::CHANNEL_WRITE],
        "sent handles are not closed"
    );

    sys.fails(status::ACCESS_DENIED);
    let (error, back) = channel.write_with(b"hi", [owned(&sys, 0x53)]).unwrap_err();
    assert_eq!(error, Error::AccessDenied);
    assert_eq!(back[0].raw(), Handle(0x53));
    drop(back);
    assert_eq!(sys.take()[2], made(nr::HANDLE_CLOSE, &[0x53]));
}

#[test]
fn channel_read_fills_bytes_and_handles_and_reports_sizes() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 7));
    sys.answer(|raw| {
        let [handle, bytes, capacity, handles, handle_capacity, actual] = raw.args();
        assert_eq!((handle, capacity, handle_capacity), (7, 16, 2));
        write(raw, bytes, b"pong");
        write(raw, handles, &[0x61, 0, 0, 0]);
        write(raw, actual, &[4, 0, 0, 0, 1, 0, 0, 0]);
        0
    });
    let mut bytes = [0_u8; 16];
    let mut handles = [Handle::INVALID; 2];
    let got = channel.read(&mut bytes, &mut handles).unwrap();
    assert_eq!(
        got,
        Received {
            bytes: 4,
            handles: 1
        }
    );
    assert_eq!(&bytes[..4], b"pong");
    assert_eq!(handles, [Handle(0x61), Handle::INVALID]);
    assert_eq!(sys.take()[0].number, nr::CHANNEL_READ);
}

#[test]
fn a_message_too_large_is_reported_and_left_queued() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 7));
    sys.answer(|raw| {
        let actual = raw.args()[5];
        assert_eq!((raw.args()[1], raw.args()[3]), (0, 0), "no buffers offered");
        write(raw, actual, &[200, 0, 0, 0, 3, 0, 0, 0]);
        status::BUFFER_TOO_SMALL.as_return_value().cast_unsigned()
    });
    let too_small = channel.read(&mut [], &mut []);
    assert_eq!(
        too_small,
        Err(ReadError::TooSmall {
            bytes: 200,
            handles: 3
        })
    );
    sys.fails(status::SHOULD_WAIT);
    let empty = channel.read(&mut [], &mut []);
    assert_eq!(empty, Err(ReadError::Failed(Error::ShouldWait)));
}

#[test]
fn a_read_offers_no_more_handle_room_than_a_message_can_use() {
    let sys = Recorder::default();
    let channel = Channel::from_owned(owned(&sys, 7));
    let mut handles = vec![Handle::INVALID; 100];
    let _ = channel.read(&mut [], &mut handles);
    assert_eq!(sys.take()[0].args[4], 64);
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

#[test]
fn a_port_packet_is_laid_out_as_the_kernel_writes_one() {
    let sys = Recorder::default();
    sys.returns(0x71);
    let port = port::create(&sys).unwrap();
    sys.answer(|raw| {
        let packet = raw.memory(raw.args()[1], 32).unwrap();
        assert_eq!(packet[..8], 9_u64.to_ne_bytes());
        assert_eq!(packet[8..12], PACKET_USER.to_ne_bytes());
        assert_eq!(packet[16..24], 1_u64.to_ne_bytes());
        assert_eq!(packet[24..], 2_u64.to_ne_bytes());
        0
    });
    port.queue(9, [1, 2]).unwrap();
    let sent = PortPacket {
        key: 3,
        kind: PACKET_SIGNAL,
        signals: 4,
        data: [5, 6],
    };
    sys.answer(move |raw| {
        assert_eq!(raw.args()[1], 0, "no deadline");
        let out = raw.args()[2];
        write(raw, out, &port::packet_bytes(&sent));
        0
    });
    assert_eq!(port.wait(Deadline::Never).unwrap(), sent);
    assert_eq!(
        sys.numbers(),
        [nr::PORT_CREATE, nr::PORT_QUEUE, nr::PORT_WAIT]
    );
    assert_eq!(sys.take()[0].args, [0; 6]);
}

// ---------------------------------------------------------------------------
// VMOs
// ---------------------------------------------------------------------------

#[test]
fn vmo_calls_pass_offsets_through_pointers() {
    let sys = Recorder::default();
    sys.returns(0x81);
    let vmo = vmo::create(&sys, 5000).unwrap();
    sys.answer(|raw| {
        let [_, buffer, count, offset, ..] = raw.args();
        assert_eq!((count, read_u64(raw, offset)), (3, 4096));
        write(raw, buffer, b"xyz");
        0
    });
    let mut buffer = [0_u8; 3];
    vmo.read(&mut buffer, 4096).unwrap();
    assert_eq!(&buffer, b"xyz");
    sys.answer(|raw| {
        let [_, buffer, count, offset, ..] = raw.args();
        assert_eq!(raw.memory(buffer, count), Some(&b"ab"[..]));
        assert_eq!(read_u64(raw, offset), 1);
        0
    });
    vmo.write(b"ab", 1).unwrap();
    sys.answer(|raw| {
        let out = raw.args()[1];
        write(raw, out, &8192_u64.to_ne_bytes());
        0
    });
    assert_eq!(vmo.size().unwrap(), 8192);
    let calls = sys.take();
    assert_eq!(calls[0], made(nr::VMO_CREATE, &[5000]));
    let numbers: Vec<usize> = calls.iter().map(|made| made.number).collect();
    assert_eq!(
        numbers,
        [
            nr::VMO_CREATE,
            nr::VMO_READ,
            nr::VMO_WRITE,
            nr::VMO_GET_SIZE
        ]
    );
}

#[test]
fn vmo_map_lets_the_kernel_choose_the_address() {
    let sys = Recorder::default();
    let vmo = Vmo::from_owned(owned(&sys, 0x81));
    sys.answer(|raw| {
        let [handle, address, length, protection, offset, _] = raw.args();
        assert_eq!((handle, address, length, protection), (0x81, 0, 0x2000, 3));
        assert_eq!(read_u64(raw, offset), 0x1000);
        0x7000_0000
    });
    assert_eq!(
        vmo.map(None, 0x2000, Protection::ReadWrite, 0x1000),
        Ok(0x7000_0000)
    );
    sys.fails(Errno::ENOSYS);
    assert_eq!(
        vmo.map(None, 0x1000, Protection::Read, 0),
        Err(Error::Unsupported)
    );
    let calls = sys.take();
    assert_eq!(calls[0].number, nr::VMO_MAP);
    assert_eq!(calls[1].args[3], 1, "MAP_READ alone");
}

// ---------------------------------------------------------------------------
// Jobs and devices
// ---------------------------------------------------------------------------

#[test]
fn job_calls_name_the_job() {
    let sys = Recorder::default();
    let root = Job::from_owned(owned(&sys, 0x91));
    sys.returns(0x92);
    let child = root.create_child().unwrap();
    child.kill().unwrap();
    sys.fails(status::BAD_STATE);
    assert_eq!(root.create_child().unwrap_err(), Error::BadState);
    drop(child);
    assert_eq!(
        sys.take(),
        [
            made(nr::JOB_CREATE, &[0x91]),
            made(nr::JOB_KILL, &[0x92]),
            made(nr::JOB_CREATE, &[0x91]),
            made(nr::HANDLE_CLOSE, &[0x92]),
        ]
    );
}

#[test]
fn a_job_is_limited_and_read_back_through_pointers() {
    let sys = Recorder::default();
    let job = Job::from_owned(owned(&sys, 0x94));
    sys.answer(|raw| {
        let [_, resource, at, ..] = raw.args();
        assert_eq!(resource as u64, types::JOB_TASKS);
        assert_eq!(read_u64(raw, at), 10, "the limit, through a pointer");
        0
    });
    job.set_limit(job::Resource::Tasks, 10).unwrap();
    sys.answer(|raw| {
        let [_, resource, out, ..] = raw.args();
        assert_eq!(resource as u64, types::JOB_MEMORY);
        let mut words = [0u8; 24];
        for (chunk, value) in words.chunks_exact_mut(8).zip([4096u64, 8192, 3]) {
            chunk.copy_from_slice(&value.to_ne_bytes());
        }
        write(raw, out, &words);
        0
    });
    assert_eq!(
        job.quota(job::Resource::Memory).unwrap(),
        job::Quota {
            used: 4096,
            limit: 8192,
            refused: 3
        }
    );
    sys.fails(status::INVALID_ARGS);
    assert_eq!(
        job.set_limit(job::Resource::CpuWeight, 0).unwrap_err(),
        Error::InvalidArgs
    );
    let numbers = sys.numbers();
    assert_eq!(
        numbers,
        [nr::JOB_SET_LIMIT, nr::JOB_GET_QUOTA, nr::JOB_SET_LIMIT]
    );
}

#[test]
fn a_job_is_had_for_a_cgroup_by_its_descriptor() {
    let sys = Recorder::default();
    sys.returns(0x93);
    let job = job::for_cgroup(&sys, 5, Requested::Exactly(Rights::WAIT)).unwrap();
    sys.fails(status::ACCESS_DENIED);
    assert_eq!(
        job::for_cgroup(&sys, -1, Requested::Same).unwrap_err(),
        Error::AccessDenied
    );
    drop(job);
    assert_eq!(
        sys.take(),
        [
            made(nr::JOB_FOR_CGROUP, &[5, Rights::WAIT.0 as usize]),
            made(nr::JOB_FOR_CGROUP, &[0xFFFF_FFFF, SAME_RIGHTS as usize]),
            made(nr::HANDLE_CLOSE, &[0x93]),
        ]
    );
}

#[test]
fn a_device_hands_out_interrupts_and_apertures() {
    let sys = Recorder::default();
    let device = Device::from_owned(owned(&sys, 0xA1));
    let port = Port::from_owned(owned(&sys, 0xA2));
    sys.returns(0xA3);
    let interrupt: Interrupt<_> = device.interrupt(2).unwrap();
    sys.answer(|raw| {
        assert_eq!(raw.args()[..2], [0xA3, 0xA2]);
        assert_eq!(read_u64(raw, raw.args()[2]), 77);
        0
    });
    interrupt.bind(&port, 77).unwrap();
    interrupt.ack().unwrap();
    sys.answer(|raw| {
        let spec = raw.args()[1];
        assert_eq!(
            (read_u64(raw, spec), read_u64(raw, spec + 8)),
            (0xFEB0_0000, 0x1000)
        );
        0xA4
    });
    let spec = IoMappingSpec {
        phys: 0xFEB0_0000,
        len: 0x1000,
    };
    let mapping: IoMapping<_> = device.io_mapping(spec).unwrap();
    sys.returns(0x5000_0000);
    assert_eq!(mapping.map(None), Ok(0x5000_0000));
    assert_eq!(mapping.map(Some(0x6000_0000)), Ok(0));
    let calls = sys.take();
    assert_eq!(calls[0], made(nr::INTERRUPT_CREATE, &[0xA1, 2]));
    assert_eq!(calls[1].number, nr::INTERRUPT_BIND);
    assert_eq!(calls[2], made(nr::INTERRUPT_ACK, &[0xA3]));
    assert_eq!(calls[3].number, nr::IO_MAPPING_CREATE);
    assert_eq!(calls[4], made(nr::IO_MAPPING_MAP, &[0xA4, 0]));
    assert_eq!(calls[5], made(nr::IO_MAPPING_MAP, &[0xA4, 0x6000_0000]));
}

/// `device_aperture` passes the handle and index in registers and reads the
/// kernel's 32 bytes back whole, an 8 GiB length above 4 GiB included; the
/// configuration calls pass handle, offset and width, a read's value comes
/// back through memory, so a read of all ones is not an error on a 32-bit
/// machine, and a refused write is the kernel's status.
///
/// Verifies: L.device.24
#[test]
fn a_device_reports_an_aperture_whole_and_reaches_its_configuration_window() {
    let sys = Recorder::default();
    let device = Device::from_owned(owned(&sys, 0xA8));
    sys.answer(|raw| {
        let mut bytes = [0_u8; types::APERTURE_INFO_BYTES];
        bytes[0..8].copy_from_slice(&0x0000_0008_0000_0000_u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&0x0000_0002_0000_0000_u64.to_ne_bytes());
        bytes[16] = 2;
        bytes[17] = types::APERTURE_PREFETCHABLE | types::APERTURE_BAR_64;
        bytes[24..32].copy_from_slice(&0x4000_u64.to_ne_bytes());
        write(raw, raw.args()[2], &bytes);
        0
    });
    let aperture = device.aperture(1).expect("an aperture");
    assert_eq!(aperture.phys, 0x8_0000_0000, "above 4 GiB");
    assert_eq!(aperture.len, 0x2_0000_0000, "8 GiB, not cut");
    assert_eq!((aperture.bar, aperture.offset), (2, 0x4000));
    assert_eq!(
        aperture.flags,
        types::APERTURE_PREFETCHABLE | types::APERTURE_BAR_64
    );

    sys.answer(|raw| {
        write(raw, raw.args()[3], &u32::MAX.to_ne_bytes());
        0
    });
    assert_eq!(
        device.config_read(0x100, 4),
        Ok(u32::MAX),
        "all ones is a value"
    );
    sys.fails(Errno::EACCES);
    assert_eq!(device.config_write(0x10, 4, 0), Err(Error::AccessDenied));
    sys.returns(0);
    assert_eq!(device.config_write(0x48, 2, 0x1234), Ok(()));
    let calls = sys.take();
    assert_eq!(calls[0].number, nr::DEVICE_APERTURE);
    assert_eq!(calls[0].args[..2], [0xA8, 1]);
    assert_eq!(calls[1].number, nr::DEVICE_CONFIG_READ);
    assert_eq!(calls[1].args[..3], [0xA8, 0x100, 4]);
    assert_eq!(calls[2], made(nr::DEVICE_CONFIG_WRITE, &[0xA8, 0x10, 4, 0]));
    assert_eq!(
        calls[3],
        made(nr::DEVICE_CONFIG_WRITE, &[0xA8, 0x48, 2, 0x1234])
    );
}

#[test]
fn device_info_reads_the_kernel_bytes_back_and_quiesce_takes_the_handle() {
    let sys = Recorder::default();
    sys.answer(|raw| {
        let mut bytes = [0_u8; DEVICE_INFO_BYTES];
        bytes[0..8].copy_from_slice(&0x8000_0000_u64.to_ne_bytes());
        bytes[8..12].copy_from_slice(&0x10_u32.to_ne_bytes());
        bytes[12..16].copy_from_slice(&56_u32.to_ne_bytes());
        bytes[64..68].copy_from_slice(&0x0001_0318_u32.to_ne_bytes());
        bytes[72..76].copy_from_slice(&2_u32.to_ne_bytes());
        bytes[84..86].copy_from_slice(&0x1AF4_u16.to_ne_bytes());
        bytes[86..88].copy_from_slice(&0x1042_u16.to_ne_bytes());
        bytes[90..92].copy_from_slice(&DEVICE_VIRTIO_PCI.to_ne_bytes());
        bytes[92..94].copy_from_slice(&0x1AF4_u16.to_ne_bytes());
        bytes[94..96].copy_from_slice(&0x1100_u16.to_ne_bytes());
        write(raw, raw.args()[1], &bytes);
        0
    });
    let device = Device::from_owned(owned(&sys, 0xA7));
    let info = device.info().expect("an info");
    assert_eq!(info.common.phys, 0x8000_0000, "common phys");
    assert_eq!(info.common.offset, 0x10, "common offset");
    assert_eq!(info.common.length, 56, "common length");
    assert_eq!(info.location, 0x0001_0318, "location");
    assert_eq!(info.apertures, 2, "apertures");
    assert_eq!(
        (info.vendor_id, info.device_id),
        (0x1AF4, 0x1042),
        "identity"
    );
    assert_eq!(info.virtio, DEVICE_VIRTIO_PCI, "virtio");
    assert_eq!(
        (info.subsystem_vendor_id, info.subsystem_id),
        (0x1AF4, 0x1100),
        "subsystem, in the last four bytes"
    );
    assert_eq!(
        info.notify,
        DeviceBlock::default(),
        "a block never written is zero"
    );

    sys.returns(0);
    device.quiesce().expect("quiesced");
    let calls = sys.take();
    assert_eq!(calls[0].number, nr::DEVICE_INFO);
    assert_eq!(calls[0].args[0], 0xA7, "the device handle first");
    assert_eq!(calls[1], made(nr::DEVICE_QUIESCE, &[0xA7]));

    // The clock: asked, then set, the rate coming back as the value.
    sys.returns(74_250_000);
    assert_eq!(device.clock(75_000_000, false), Ok(74_250_000), "rounded");
    sys.returns(49_500_000);
    assert_eq!(device.clock(49_500_000, true), Ok(49_500_000), "set");
    let calls = sys.take();
    assert_eq!(
        calls[0],
        made(nr::DEVICE_CLOCK, &[0xA7, 75_000_000, 0]),
        "asked"
    );
    assert_eq!(
        calls[1],
        made(
            nr::DEVICE_CLOCK,
            &[0xA7, 49_500_000, types::CLOCK_SET as usize]
        ),
        "set"
    );
}

/// The isolated-interrupts mark and `device_isolation` travel by their
/// numbers.
#[test]
fn the_isolated_interrupts_mark_travels_by_its_number() {
    let sys = Recorder::default();
    let device = Device::from_owned(owned(&sys, 0xA7));
    sys.returns(0);
    assert_eq!(device.set_limit(Limit::IsolatedInterrupts, 1), Ok(()));
    sys.returns(3);
    assert_eq!(device.isolation(), Ok(3));
    let calls = sys.take();
    assert_eq!(
        calls[0],
        made(
            nr::DEVICE_SET_LIMIT,
            &[0xA7, types::DEVICE_LIMIT_ISOLATED_INTERRUPTS as usize, 1]
        )
    );
    assert_eq!(calls[1], made(nr::DEVICE_ISOLATION, &[0xA7]));
}

/// The pin budget: set, refused, and read back, each limit by its number.
#[test]
fn device_limits_travel_by_their_numbers() {
    let sys = Recorder::default();
    let device = Device::from_owned(owned(&sys, 0xA7));
    sys.returns(0);
    assert_eq!(device.set_limit(Limit::PinPages, 262_144), Ok(()), "set");
    sys.fails(status::BAD_STATE);
    assert_eq!(
        device.set_limit(Limit::PinPages, 1),
        Err(Error::BadState),
        "refused under live pins"
    );
    sys.returns(1_048_576);
    assert_eq!(device.limit(Limit::PinCeiling), Ok(1_048_576), "ceiling");
    sys.returns(524_288);
    assert_eq!(device.limit(Limit::PinRoom), Ok(524_288), "room");
    let calls = sys.take();
    assert_eq!(
        calls[0],
        made(
            nr::DEVICE_SET_LIMIT,
            &[0xA7, types::DEVICE_LIMIT_PIN_PAGES as usize, 262_144]
        )
    );
    assert_eq!(
        calls[2],
        made(
            nr::DEVICE_GET_LIMIT,
            &[0xA7, types::DEVICE_LIMIT_PIN_CEILING as usize]
        )
    );
    assert_eq!(
        calls[3],
        made(
            nr::DEVICE_GET_LIMIT,
            &[0xA7, types::DEVICE_LIMIT_PIN_ROOM as usize]
        )
    );
    assert_eq!(
        Error::from_errno(status::LIMIT_REACHED),
        Error::LimitReached,
        "a pin past its budget has a name of its own"
    );
}

// ---------------------------------------------------------------------------
// Process creation, now on the table
// ---------------------------------------------------------------------------

#[test]
fn process_creation_numbers_are_the_tables() {
    assert_eq!(
        nr::decode(pending::PROCESS_CREATE),
        Some(nr::NativeCall::ProcessCreate)
    );
    assert_eq!(
        nr::decode(pending::PROCESS_START),
        Some(nr::NativeCall::ProcessStart)
    );
}

#[test]
fn process_creation_makes_the_real_calls_and_surfaces_enosys() {
    let sys = Recorder::default();
    let job = Job::from_owned(owned(&sys, 0xB1));
    let elf = Vmo::from_owned(owned(&sys, 0xB2));
    sys.answer(|raw| {
        let [job, elf, name, len, ..] = raw.args();
        assert_eq!((job, elf, len), (0xB1, 0xB2, 6));
        assert_eq!(raw.memory(name, len), Some(&b"devmgr"[..]));
        0xB3
    });
    let process: Process<_> = pending::create_process(&job, &elf, "devmgr").unwrap();
    sys.fails(Errno::ENOSYS);
    let (error, bootstrap) = process.start(owned(&sys, 0xB4)).unwrap_err();
    assert_eq!(error, Error::Unsupported);
    assert_eq!(
        bootstrap.raw(),
        Handle(0xB4),
        "the bootstrap handle comes back"
    );
    process.start(bootstrap).unwrap();
    let calls = sys.take();
    assert_eq!(calls[0].number, pending::PROCESS_CREATE);
    assert_eq!(calls[1], made(pending::PROCESS_START, &[0xB3, 0xB4]));
    assert_eq!(calls[2], made(pending::PROCESS_START, &[0xB3, 0xB4]));
    assert_eq!(
        sys.numbers(),
        [],
        "a started bootstrap handle is not closed here"
    );

    sys.fails(Errno::ENOSYS);
    assert_eq!(
        pending::create_process(&job, &elf, "x").unwrap_err(),
        Error::Unsupported
    );
}

#[test]
fn a_process_is_watched_for_its_end_through_wait_async() {
    let sys = Recorder::default();
    let process = Process::from_owned(owned(&sys, 0xB5));
    let port = Port::from_owned(owned(&sys, 0xB6));
    sys.answer(|raw| {
        let [process, port, signals, key, ..] = raw.args();
        assert_eq!((process, port), (0xB5, 0xB6));
        assert_eq!(signals, Signals::TERMINATED.0 as usize);
        assert_eq!(read_u64(raw, key), 0x5EED);
        0
    });
    process.notify_on_exit(&port, 0x5EED).unwrap();
    assert_eq!(
        sys.take()[0].number,
        nr::OBJECT_WAIT_ASYNC,
        "no call of its own"
    );
}

// ---------------------------------------------------------------------------
// Pinning
// ---------------------------------------------------------------------------

#[test]
fn a_pin_names_the_device_first_and_passes_its_range_in_registers() {
    let sys = Recorder::default();
    let device = Device::from_owned(owned(&sys, 0xC2));
    let vmo = Vmo::from_owned(owned(&sys, 0xC1));
    sys.returns(0xC3);
    let pin: Pin<_> = device
        .pin(&vmo, 0x1000, 0x3000, PinAccess::ReadWrite)
        .unwrap();
    assert_eq!(pin.handle(), Handle(0xC3));
    sys.fails(status::ALREADY_BOUND);
    let refused = device.pin(&vmo, 0, 0x1000, PinAccess::ReadOnly);
    assert_eq!(refused.unwrap_err(), Error::AlreadyBound);
    drop(pin);
    assert_eq!(
        sys.take(),
        [
            made(nr::VMO_PIN, &[0xC2, 0xC1, 0x1000, 0x3000, 0]),
            made(
                nr::VMO_PIN,
                &[0xC2, 0xC1, 0, 0x1000, PIN_READ_ONLY as usize]
            ),
            made(nr::HANDLE_CLOSE, &[0xC3]),
        ]
    );
}

#[test]
fn an_address_query_tells_a_short_buffer_from_a_complete_one() {
    let sys = Recorder::default();
    let pin = Pin::from_owned(owned(&sys, 0xC3));
    let pages: Vec<u8> = [0x8000_u64, 0x9000, 0xF000]
        .iter()
        .flat_map(|address| address.to_ne_bytes())
        .collect();
    for capacity in [2_usize, 4] {
        let pages = pages.clone();
        sys.answer(move |raw| {
            let [pin, out, offered, ..] = raw.args();
            assert_eq!((pin, offered), (0xC3, capacity));
            let fits = capacity.min(3) * 8;
            write(raw, out, &pages[..fits]);
            3
        });
    }

    let mut short = [[0_u8; 8]; 2];
    let found = pin.addresses(&mut short).unwrap();
    assert_eq!(
        found,
        Addresses {
            pages: 3,
            written: 2
        }
    );
    assert!(!found.is_complete(), "three pages, room for two");
    assert_eq!(short.map(device_address), [0x8000, 0x9000]);

    let mut room = [[0_u8; 8]; 4];
    let found = pin.addresses(&mut room).unwrap();
    assert!(found.is_complete());
    assert_eq!(room.map(device_address), [0x8000, 0x9000, 0xF000, 0]);

    sys.fails(status::ACCESS_DENIED);
    assert_eq!(pin.addresses(&mut []), Err(Error::AccessDenied));
    assert_eq!(
        sys.take()[2],
        made(nr::VMO_PIN_ADDRESSES, &[0xC3, 0, 0]),
        "empty is null"
    );
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

#[test]
fn every_call_in_the_native_table_has_a_wrapper() {
    let sys = Recorder::default();
    let handle = || owned(&sys, 1);
    let port = Port::from_owned(handle());
    let channel = Channel::from_owned(handle());
    let vmo = Vmo::from_owned(handle());
    let job = Job::from_owned(handle());
    let device = Device::from_owned(handle());
    let interrupt = Interrupt::from_owned(handle());
    let mapping = IoMapping::from_owned(handle());

    let _ = handle().close();
    let _ = channel.duplicate(Requested::Same);
    let _ = handle().replace(Requested::Same);
    let _ = channel.wait_one(Signals::READABLE, Deadline::Never);
    let _ = channel.wait_async(&port, Signals::READABLE, 0);
    let _ = channel::create(&sys);
    let _ = channel.write(b"");
    let _ = channel.read(&mut [], &mut []);
    let _ = channel.write_read(None);
    let _ = port::create(&sys);
    let _ = port.queue(0, [0, 0]);
    let _ = port.wait(Deadline::Never);
    let _ = vmo::create(&sys, 1);
    let _ = vmo.read(&mut [], 0);
    let _ = vmo.write(&[], 0);
    let _ = vmo.size();
    let _ = vmo.map(None, 1, Protection::Read, 0);
    let _ = job.create_child();
    let _ = job.kill();
    let _ = job::for_cgroup(&sys, 0, Requested::Same);
    let _ = job.set_limit(job::Resource::Tasks, 1);
    let _ = job.quota(job::Resource::Tasks);
    let _ = device.interrupt(0);
    let _ = device.block_ring();
    let _ = device.net_ring();
    let _ = device.display_control();
    let _ = device.render_control();
    let _ = device.input_control();
    let _ = device.sound_control();
    let _ = device.chardev_control();
    let _ = chardev::reply(&channel, 1, 0, 0);
    let _ = chardev::copy_in(&channel, 1, 0, &mut []);
    let _ = chardev::copy_out(&channel, 1, 0, &[]);
    let _ = chardev::file(&channel, 1, 3);
    let _ = device.log_control();
    let _ = device.info();
    let _ = device.quiesce();
    let _ = device.clock(1, false);
    let _ = device.aperture(0);
    let _ = device.config_read(0, 4);
    let _ = device.config_write(0x48, 4, 0);
    let _ = device.set_limit(Limit::PinPages, 1);
    let _ = device.limit(Limit::PinRoom);
    let _ = device.isolation();
    let _ = pending::create_process(&job, &vmo, "x");
    let _ = Process::from_owned(handle()).start(handle());
    let _ = Process::from_owned(handle()).status();
    let _ = pending::give_bootstrap(&sys, 2, handle());
    let _ = pending::take_bootstrap(&sys);
    let _ = pending::start_devmgr(&handle(), &job);
    let _ = pending::audit_read(&sys, &handle(), 0, 0, &mut []);
    let _ = port.descriptor(true);
    let _ = interrupt.bind(&port, 0);
    let _ = interrupt.ack();
    let _ = device.io_mapping(IoMappingSpec::default());
    let _ = mapping.map(None);
    let _ = mapping.map_combining(None);
    let _ = device.pin(&vmo, 0, 1, PinAccess::ReadOnly);
    let _ = Pin::from_owned(handle()).addresses(&mut []);

    let wrapped: BTreeSet<usize> = sys.numbers().into_iter().collect();
    let table: BTreeSet<usize> = nr::ALL.iter().map(|&call| nr::number(call)).collect();
    let missing: Vec<_> = table.difference(&wrapped).collect();
    assert!(missing.is_empty(), "no wrapper makes {missing:#x?}");
}

/// `Words::of`, word by word since it stopped calling `memcpy`, packs every
/// length a `channel_write_read` may send exactly as the one copy into a
/// zeroed buffer did, its bytes come back through `Words::bytes`, and a
/// message longer than three words is refused.
#[test]
fn words_of_packs_every_length_as_one_copy_would() {
    let source: Vec<u8> = (1..=25_u8).map(|byte| byte.wrapping_mul(37)).collect();
    for len in 0..=channel::WRITE_READ_BYTES {
        let bytes = &source[..len];
        let mut padded = [0_u8; channel::WRITE_READ_BYTES];
        padded[..len].copy_from_slice(bytes);
        let expected: Vec<usize> = padded
            .chunks_exact(size_of::<usize>())
            .map(|chunk| usize::from_ne_bytes(chunk.try_into().unwrap()))
            .collect();
        let words = channel::Words::of(bytes).unwrap();
        assert_eq!(words.as_slice(), expected.as_slice(), "length {len}");
        let back = channel::Words { len, words }.bytes();
        assert_eq!(&back[..len], bytes, "length {len}");
        assert!(back[len..].iter().all(|&byte| byte == 0), "length {len}");
    }
    assert_eq!(channel::Words::of(&source[..channel::WRITE_READ_BYTES + 1]), None);
}
