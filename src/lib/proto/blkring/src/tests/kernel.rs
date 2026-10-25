//! The kernel's checks: setup (§4.1), indices (§4.2), completions (§4.3), its
//! own bound, its outstanding-id table, and a driver scribbling on its fields.

use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use ferrix_native_abi::status;
use ferrix_native_abi::types::{PACKET_USER, PortPacket};

use super::support::{Rng, Shared, Who, device, read, with_pair};
use crate::bell::{BELL_COMPLETE, BELL_SUBMIT, Rung, Wait, port_queue_rung, rung};
use crate::driver::{Consumed, DriverSide};
use crate::kernel::{AttachError, Ending, KernelSide, Slot, SubmitError, Table};
use crate::layout::{HeaderError, RingLayout, Status, Submission, completion, header};
use crate::{Corruption, InvalidSubmission};

type Mutation = fn(&Shared);

fn attach_after(mutate: impl FnOnce(&Shared)) -> Option<AttachError> {
    let layout = RingLayout::standard(4).expect("valid");
    let shared = Shared::new(layout.ring_bytes());
    let _driver = DriverSide::new(
        shared.memory(Who::Driver, layout),
        layout.ring_bytes(),
        layout,
        device(),
    )
    .expect("fits");
    mutate(&shared);
    let mut slots = [Slot::EMPTY; 4];
    KernelSide::attach(
        shared.memory(Who::Kernel, layout),
        layout.ring_bytes(),
        device(),
        &mut slots,
    )
    .err()
}

#[test]
fn setup_refuses_every_header_the_specification_refuses() {
    let cases: [(Mutation, HeaderError); 12] = [
        (|s| s.poke(header::MAGIC, b"FXBQ"), HeaderError::BadMagic),
        (
            |s| s.poke(header::VERSION, &[2, 0]),
            HeaderError::BadVersion,
        ),
        (
            |s| s.poke(header::FLAGS, &[1, 0]),
            HeaderError::UnknownFlags,
        ),
        (|s| s.poke_u32(header::ENTRIES, 3), HeaderError::BadEntries),
        (
            |s| s.poke_u32(header::ENTRIES, 8192),
            HeaderError::BadEntries,
        ),
        (
            |s| s.poke_u32(header::SUB_OFFSET, 32),
            HeaderError::OverlapsHeader,
        ),
        (
            |s| s.poke_u32(header::COMP_OFFSET, 4096),
            HeaderError::OutsideRing,
        ),
        (
            |s| s.poke_u32(header::COMP_OFFSET, 64),
            HeaderError::ArraysOverlap,
        ),
        (
            |s| s.poke(header::RESERVED + 6, &[1]),
            HeaderError::ReservedNotZero,
        ),
        (|s| s.poke_u32(header::SUB_HEAD, 1), HeaderError::NotFresh),
        (|s| s.poke_u32(header::COMP_TAIL, 1), HeaderError::NotFresh),
        (
            |s| s.poke_u32(header::SUB_WANT_BELL, 1),
            HeaderError::NotFresh,
        ),
    ];
    for (mutate, error) in cases {
        assert_eq!(
            attach_after(mutate),
            Some(AttachError::Header(error)),
            "{error:?}"
        );
    }
    assert_eq!(attach_after(|_| {}), None, "a fresh header");
}

#[test]
fn setup_refuses_a_ring_smaller_than_its_header_says_and_no_storage() {
    let layout = RingLayout::standard(4).expect("valid");
    let shared = Shared::new(layout.ring_bytes());
    let _driver = DriverSide::new(
        shared.memory(Who::Driver, layout),
        layout.ring_bytes(),
        layout,
        device(),
    )
    .expect("fits");
    let mut slots = [Slot::EMPTY; 4];
    let small = KernelSide::attach(
        shared.memory(Who::Kernel, layout),
        layout.ring_bytes() - 1,
        device(),
        &mut slots,
    );
    assert_eq!(
        small.err(),
        Some(AttachError::Header(HeaderError::OutsideRing)),
        "a byte short"
    );
    let none = KernelSide::attach(
        shared.memory(Who::Kernel, layout),
        layout.ring_bytes(),
        device(),
        &mut [],
    );
    assert_eq!(none.err(), Some(AttachError::NoStorage), "no storage");
}

#[test]
fn setup_zeroes_the_kernels_own_fields_whatever_they_held() {
    let layout = RingLayout::standard(4).expect("valid");
    let shared = Shared::new(layout.ring_bytes());
    let _driver = DriverSide::new(
        shared.memory(Who::Driver, layout),
        layout.ring_bytes(),
        layout,
        device(),
    )
    .expect("fits");
    for at in [header::SUB_TAIL, header::COMP_HEAD, header::COMP_WANT_BELL] {
        shared.poke_u32(at, 0xFFFF_FFFF);
    }
    let mut slots = [Slot::EMPTY; 4];
    let kernel = KernelSide::attach(
        shared.memory(Who::Kernel, layout),
        layout.ring_bytes(),
        device(),
        &mut slots,
    );
    assert!(
        kernel.is_ok(),
        "the kernel's fields are not the driver's to set"
    );
    for at in [header::SUB_TAIL, header::COMP_HEAD, header::COMP_WANT_BELL] {
        assert_eq!(shared.peek_u32(at), 0, "offset {at}");
    }
}

#[test]
fn a_completion_tail_more_than_a_ring_ahead_is_corruption_and_latches() {
    for tail in [5, u32::MAX] {
        with_pair(4, |kernel, _driver, shared| {
            kernel.submit(read(1)).expect("room");
            shared.poke_u32(header::COMP_TAIL, tail);
            assert_eq!(kernel.poll(), Err(Corruption::TailOverrun), "tail {tail}");
            assert_eq!(kernel.poll(), Err(Corruption::TailOverrun), "latched");
            assert_eq!(
                kernel.prepare_to_sleep(),
                Err(Corruption::TailOverrun),
                "latched"
            );
            assert_eq!(
                kernel.submit(read(2)),
                Err(SubmitError::Corrupt(Corruption::TailOverrun)),
                "no new submissions"
            );
            assert_eq!(kernel.publish(), None, "nothing more published");
        });
    }
}

/// Submit `ids`, have the driver take and complete them all, and publish.
fn exchange(
    kernel: &mut KernelSide<'_, super::support::Mem<'_>>,
    driver: &mut DriverSide<super::support::Mem<'_>>,
    ids: &[u64],
) {
    for &id in ids {
        kernel.submit(read(id)).expect("room");
    }
    let _ = kernel.publish();
    for &id in ids {
        assert_eq!(
            driver.consume(),
            Ok(Some(Consumed::Request(read(id)))),
            "consume {id}"
        );
    }
    for &id in ids {
        driver.complete(id, Status::Ok, 512).expect("held");
    }
    let _ = driver.publish();
}

#[test]
fn a_completion_tail_that_moves_backwards_is_corruption() {
    with_pair(4, |kernel, driver, shared| {
        exchange(kernel, driver, &[1, 2, 3]);
        assert_eq!(
            kernel.poll().map(|c| c.map(|c| c.submission.id)),
            Ok(Some(1)),
            "first"
        );
        shared.poke_u32(header::COMP_TAIL, 2);
        assert_eq!(kernel.poll(), Err(Corruption::TailBackwards), "3 then 2");
        let mut failed: Vec<u64> = kernel.end(Ending::DriverDied).map(|s| s.id).collect();
        failed.sort_unstable();
        assert_eq!(failed, [2, 3], "everything still outstanding fails");
    });
}

#[test]
fn a_completion_for_an_id_not_outstanding_is_corruption() {
    with_pair(4, |kernel, driver, _shared| {
        kernel.submit(read(1)).expect("room");
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "consumed");
        driver.complete(99, Status::Ok, 512).expect("held");
        let _ = driver.publish();
        assert_eq!(kernel.poll(), Err(Corruption::UnknownId), "never submitted");
        assert_eq!(
            kernel
                .end(Ending::DriverDied)
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            [1],
            "1 fails"
        );
    });
}

#[test]
fn a_duplicate_completion_is_corruption() {
    with_pair(4, |kernel, driver, _shared| {
        kernel.submit(read(1)).expect("room");
        kernel.submit(read(2)).expect("room");
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "1");
        assert!(matches!(driver.consume(), Ok(Some(_))), "2");
        driver.complete(1, Status::Ok, 512).expect("held");
        driver
            .complete(1, Status::Ok, 512)
            .expect("held, as far as the driver counts");
        let _ = driver.publish();
        assert_eq!(
            kernel.poll().map(|c| c.map(|c| c.submission.id)),
            Ok(Some(1)),
            "the first answer"
        );
        assert_eq!(
            kernel.poll(),
            Err(Corruption::UnknownId),
            "the second answer to the same id"
        );
        assert_eq!(
            kernel
                .end(Ending::DriverDied)
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            [2],
            "2 fails"
        );
    });
}

#[test]
fn a_completion_with_an_unknown_status_is_corruption() {
    with_pair(4, |kernel, driver, shared| {
        exchange(kernel, driver, &[1]);
        let at = kernel.layout().completion_at(0) + completion::STATUS;
        shared.poke_u32(at, 5);
        assert_eq!(kernel.poll(), Err(Corruption::UnknownStatus), "status 5");
    });
}

#[test]
fn a_completion_claiming_more_bytes_than_it_carried_is_corruption() {
    with_pair(4, |kernel, driver, _shared| {
        kernel.submit(read(1)).expect("room");
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "consumed");
        driver.complete(1, Status::Ok, 513).expect("held");
        let _ = driver.publish();
        assert_eq!(
            kernel.poll(),
            Err(Corruption::BytesDoneTooLarge),
            "513 of 512"
        );
    });
}

#[test]
fn an_ok_write_or_flush_that_did_less_than_it_carried_is_an_io_error() {
    let cases = [
        (
            Submission::write(1, 0, 2, 0),
            Status::Ok,
            512,
            Status::IoError,
        ),
        (Submission::write(2, 0, 2, 0), Status::Ok, 1024, Status::Ok),
        (Submission::read(3, 0, 2, 0), Status::Ok, 512, Status::Ok),
        (Submission::flush(4), Status::Ok, 0, Status::Ok),
        (
            Submission::write(5, 0, 2, 0),
            Status::IoError,
            0,
            Status::IoError,
        ),
        (
            Submission::write(6, 0, 1, 0),
            Status::ReadOnly,
            0,
            Status::ReadOnly,
        ),
    ];
    with_pair(8, |kernel, driver, _shared| {
        for (submission, sent, bytes, seen) in cases {
            kernel.submit(submission).expect("room");
            let _ = kernel.publish();
            assert_eq!(
                driver.consume(),
                Ok(Some(Consumed::Request(submission))),
                "{submission:?}"
            );
            driver.complete(submission.id, sent, bytes).expect("held");
            let _ = driver.publish();
            let completed = kernel.poll().expect("honest").expect("pending");
            assert_eq!(
                (completed.status, completed.bytes_done),
                (seen, bytes),
                "{submission:?}"
            );
        }
    });
}

#[test]
fn the_kernel_keeps_at_most_entries_outstanding_and_sends_only_what_the_driver_accepts() {
    with_pair(2, |kernel, driver, _shared| {
        assert_eq!(kernel.capacity(), 2, "entries");
        kernel.submit(read(1)).expect("room");
        assert_eq!(
            kernel.submit(read(1)),
            Err(SubmitError::DuplicateId),
            "same id"
        );
        kernel.submit(read(2)).expect("room");
        assert_eq!(
            kernel.submit(read(3)),
            Err(SubmitError::Full),
            "entries outstanding"
        );
        assert_eq!(
            kernel.submit(Submission::read(4, 0, 0, 0)),
            Err(SubmitError::Invalid(InvalidSubmission::Empty)),
            "the kernel holds itself to the driver's checks"
        );
        let _ = kernel.publish();
        assert!(matches!(driver.consume(), Ok(Some(_))), "1");
        assert_eq!(
            kernel.submit(read(3)),
            Err(SubmitError::Full),
            "consumed is still outstanding"
        );
        driver.complete(1, Status::Ok, 512).expect("held");
        let _ = driver.publish();
        assert!(matches!(kernel.poll(), Ok(Some(_))), "1 completes");
        kernel.submit(read(3)).expect("room again");
        assert_eq!(kernel.outstanding(), 2, "2 and 3");
    });
}

#[test]
fn a_driver_scribbling_on_the_kernels_fields_moves_nothing() {
    with_pair(4, |kernel, driver, shared| {
        exchange(kernel, driver, &[1, 2, 3]);
        assert_eq!(
            kernel.poll().map(|c| c.map(|c| c.submission.id)),
            Ok(Some(1)),
            "1"
        );
        for at in [
            header::SUB_TAIL,
            header::COMP_HEAD,
            header::COMP_WANT_BELL,
            header::ENTRIES,
            header::SUB_OFFSET,
            header::COMP_OFFSET,
        ] {
            shared.poke_u32(at, 0xDEAD_BEEF);
        }
        shared.poke(header::MAGIC, b"XXXX");
        for id in [2, 3] {
            assert_eq!(
                kernel.poll().map(|c| c.map(|c| c.submission.id)),
                Ok(Some(id)),
                "{id}"
            );
        }
        assert_eq!(kernel.poll(), Ok(None), "nothing more");
        assert_eq!(
            shared.peek_u32(header::COMP_HEAD),
            3,
            "the kernel's own head"
        );
        kernel.submit(read(4)).expect("room");
        assert_eq!(kernel.publish(), None, "the driver did not ask");
        assert_eq!(
            shared.peek_u32(header::SUB_TAIL),
            4,
            "the kernel's own tail"
        );
        exchange_tail(kernel, driver, 4);
    });
}

fn exchange_tail(
    kernel: &mut KernelSide<'_, super::support::Mem<'_>>,
    driver: &mut DriverSide<super::support::Mem<'_>>,
    id: u64,
) {
    assert_eq!(
        driver.consume(),
        Ok(Some(Consumed::Request(read(id)))),
        "consume {id}"
    );
    driver.complete(id, Status::Ok, 512).expect("held");
    let _ = driver.publish();
    assert_eq!(
        kernel.poll().map(|c| c.map(|c| c.submission.id)),
        Ok(Some(id)),
        "complete {id}"
    );
}

#[test]
fn full_counts_as_rung() {
    assert_eq!(port_queue_rung(Ok(())), Ok(Rung::Queued), "queued");
    assert_eq!(
        port_queue_rung(Err(status::SHOULD_WAIT)),
        Ok(Rung::Full),
        "full is rung"
    );
    assert_eq!(
        port_queue_rung(Err(status::BAD_HANDLE)),
        Err(status::BAD_HANDLE),
        "a bad handle is an error"
    );
    #[derive(Debug, PartialEq)]
    enum PortError {
        Full,
        Closed,
    }
    let full = |error: &PortError| *error == PortError::Full;
    assert_eq!(
        rung(Err(PortError::Full), full),
        Ok(Rung::Full),
        "the kernel's Full"
    );
    assert_eq!(
        rung(Err(PortError::Closed), full),
        Err(PortError::Closed),
        "other errors"
    );
}

#[test]
fn a_producer_rings_only_when_the_consumer_asked_and_there_is_something_new() {
    with_pair(4, |kernel, driver, shared| {
        kernel.submit(read(1)).expect("room");
        assert_eq!(kernel.publish(), None, "the driver did not ask");
        assert_eq!(
            driver.prepare_to_sleep(),
            Ok(Wait::Pending(1)),
            "the driver looks again"
        );
        assert_eq!(
            shared.peek_u32(header::SUB_WANT_BELL),
            0,
            "and stops asking"
        );
        assert!(matches!(driver.consume(), Ok(Some(_))), "1");
        assert_eq!(driver.prepare_to_sleep(), Ok(Wait::Sleep), "nothing left");
        assert_eq!(shared.peek_u32(header::SUB_WANT_BELL), 1, "asking");
        assert_eq!(kernel.publish(), None, "nothing new to publish");
        kernel.submit(read(2)).expect("room");
        let bell = kernel.publish().expect("the driver asked");
        assert_eq!((bell.key(), bell.tail()), (BELL_SUBMIT, 2), "submit bell");
        let expected = PortPacket {
            key: 1,
            kind: PACKET_USER,
            signals: 0,
            data: [2, 0],
        };
        assert_eq!(bell.packet(), expected, "the packet");
        driver.woke();
        assert_eq!(shared.peek_u32(header::SUB_WANT_BELL), 0, "awake");

        driver.complete(1, Status::Ok, 512).expect("held");
        assert_eq!(
            kernel.prepare_to_sleep(),
            Ok(Wait::Sleep),
            "not published yet"
        );
        let bell = driver.publish().expect("the kernel asked");
        assert_eq!(
            (bell.key(), bell.packet().data),
            (BELL_COMPLETE, [1, 0]),
            "complete bell"
        );
        kernel.woke();
        assert!(matches!(kernel.poll(), Ok(Some(_))), "1 completes");
    });
}

#[test]
fn the_outstanding_table_agrees_with_a_map() {
    let seeds = if cfg!(miri) { 4 } else { 200 };
    for seed in 0..seeds {
        let mut rng = Rng::new(seed);
        let size = 1 + rng.below(9) as usize;
        let mut slots = vec![Slot::EMPTY; size];
        let mut table = Table::new(&mut slots, size);
        let mut map = BTreeMap::new();
        for _ in 0..200 {
            let id = rng.below(4 * size as u64);
            table_step(&mut table, &mut map, rng.below(3), id, size);
            assert_eq!(table.len(), map.len(), "seed {seed}: length");
        }
    }
}

fn table_step(table: &mut Table<'_>, map: &mut BTreeMap<u64, ()>, op: u64, id: u64, size: usize) {
    match op {
        0 => match table.insert(Submission::flush(id)) {
            Ok(()) => assert!(map.insert(id, ()).is_none(), "inserted a duplicate {id}"),
            Err(SubmitError::DuplicateId) => {
                assert!(map.contains_key(&id), "a false duplicate {id}");
            }
            Err(SubmitError::Full) => assert_eq!(map.len(), size, "a false full"),
            Err(other) => panic!("{other:?}"),
        },
        1 => assert_eq!(
            table.remove(id).map(|s| s.id),
            map.remove(&id).map(|()| id),
            "remove {id}"
        ),
        _ => assert_eq!(
            table.find(id).map(|s| s.id),
            map.contains_key(&id).then_some(id),
            "find {id}"
        ),
    }
}

/// A driver that answered anything within the patience keeps its disk; one
/// that answered nothing for a whole patience while a request waits loses it.
#[test]
fn a_disk_goes_silent_only_when_nothing_was_answered_for_a_whole_patience() {
    use crate::kernel::gone_silent;
    const PATIENCE: u64 = 30_000_000_000;
    // One request timed out, but the driver answered another 10 s ago.
    assert!(!gone_silent(
        100_000_000_000,
        90_000_000_000,
        true,
        PATIENCE
    ));
    // Nothing answered for exactly the patience, or longer.
    assert!(gone_silent(
        130_000_000_000,
        100_000_000_000,
        true,
        PATIENCE
    ));
    assert!(gone_silent(
        500_000_000_000,
        100_000_000_000,
        true,
        PATIENCE
    ));
    // Nothing waits: an idle disk is not a dead one.
    assert!(!gone_silent(
        500_000_000_000,
        100_000_000_000,
        false,
        PATIENCE
    ));
    // A clock that went back is never silence.
    assert!(!gone_silent(
        50_000_000_000,
        100_000_000_000,
        true,
        PATIENCE
    ));
    // Just short of the patience.
    assert!(!gone_silent(
        129_999_999_999,
        100_000_000_000,
        true,
        PATIENCE
    ));
}
