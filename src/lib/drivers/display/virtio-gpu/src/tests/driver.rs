//! The driver against the fake device: bring-up, commands, a whole frame from
//! ATTACH to the device's screen, and a device that misbehaves.

use core::cell::RefCell;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use ferrix_displayctl::message::{Attach, FORMAT, Message, Rect as CtlRect, Status};
use ferrix_virtio::gpu::{
    CMD_GET_DISPLAY_INFO, CMD_RESOURCE_ATTACH_BACKING, CMD_RESOURCE_UNREF, CMD_SUBMIT_3D, Command,
    Context, DeviceError as Refusal, MemEntry, PAGE_SIZE, Response, backing_entries,
};
use ferrix_virtio::pci::FEATURE_VERSION_1;

use super::fake::{Bus, Device, Handle, PAGE, Region};
use crate::pipeline::{Pipeline, Request, Step};
use crate::{
    CONFIG_LOOK_EVERY, CONTROL_SLOTS, DeviceError, DevicePages, Done, Driver, ISR_CONFIG, Options,
    Parts, SubmitError, Teardown,
};

pub(super) type TestDriver = Driver<Handle, Region, Region>;

pub(super) fn build(mode: (u32, u32)) -> (Rc<Bus>, Rc<RefCell<Device>>, TestDriver) {
    let bus = Bus::new();
    let device = Rc::new(RefCell::new(Device::new(Rc::clone(&bus), mode)));
    let handle = Handle {
        device: Rc::clone(&device),
        doorbells: Rc::new(RefCell::new(0)),
        cursor_doorbells: Rc::new(RefCell::new(0)),
    };
    let parts = Parts {
        transport: handle,
        rings: bus.pin(2, false),
        // Two pages of slots, a page of large request, a page of response.
        area: bus.pin(4, true),
        cursor_rings: bus.pin(1, false),
        cursor_area: bus.pin(1, false),
    };
    let driver = Driver::init(
        parts,
        Options {
            reset_polls: 4,
            ..Options::default()
        },
    )
    .expect("the device comes up");
    (bus, device, driver)
}

/// Submit, let the device serve, take the outcome.
pub(super) fn run(
    driver: &mut TestDriver,
    device: &Rc<RefCell<Device>>,
    command: &Command<'_>,
) -> Done {
    driver.submit(command).expect("submitted");
    device.borrow_mut().serve();
    let _ = driver.on_interrupt().expect("a sound device");
    let done = driver
        .take_done()
        .expect("a sound response")
        .expect("a response");
    assert_eq!(done.command, command.code());
    assert_eq!(driver.take_done(), Ok(None), "one command, one completion");
    done
}

/// Every completion the device has made, in the order it made them.
fn take_all(driver: &mut TestDriver) -> Vec<Done> {
    let _ = driver.on_interrupt().expect("a sound device");
    let mut done = Vec::new();
    while let Some(one) = driver.take_done().expect("a sound response") {
        done.push(one);
    }
    done
}

#[test]
fn bring_up_negotiates_and_reads_the_displays() {
    let (_bus, device, mut driver) = build((1280, 800));
    assert!(driver.info().features & FEATURE_VERSION_1 != 0);
    assert_eq!(driver.info().features & (1 << 1), 0, "EDID is declined");
    assert_eq!(driver.info().config.num_scanouts, 1);

    let Ok(Response::DisplayInfo(scanouts)) =
        run(&mut driver, &device, &Command::GetDisplayInfo).result
    else {
        panic!("display info");
    };
    assert!(scanouts[0].enabled);
    assert_eq!(
        (scanouts[0].rect.width, scanouts[0].rect.height),
        (1280, 800)
    );
    assert_eq!(device.borrow().commands, [CMD_GET_DISPLAY_INFO]);
    assert!(
        device.borrow().protocol_errors.is_empty(),
        "{:?}",
        device.borrow().protocol_errors
    );
}

#[test]
fn refusals_are_not_faults() {
    let (_bus, device, mut driver) = build((640, 480));
    assert_eq!(
        run(
            &mut driver,
            &device,
            &Command::ResourceUnref { resource_id: 42 }
        )
        .result,
        Err(Refusal::InvalidResourceId)
    );
    assert_eq!(driver.fault(), None);
    assert_eq!(device.borrow().commands.last(), Some(&CMD_RESOURCE_UNREF));
}

/// Every slot and the large place in flight at once behind one doorbell,
/// the next command told to wait, and the completions matched to their
/// tags however the device orders them.
#[test]
fn every_slot_in_flight_behind_one_doorbell_and_back_in_any_order() {
    let (_bus, device, mut driver) = build((640, 480));
    let doorbells = |driver: &TestDriver| *driver.transport().doorbells.borrow();
    let before = doorbells(&driver);
    for tag in 0..CONTROL_SLOTS as u64 {
        driver
            .post(
                tag,
                Context::NONE,
                &Command::ResourceUnref {
                    resource_id: 100 + tag as u32,
                },
                &[],
            )
            .expect("a free slot");
    }
    assert_eq!(driver.free_slots(), 0);
    // Too many entries for a slot: the large place takes it.
    let entries = vec![
        MemEntry {
            addr: 0x1000,
            length: 4096
        };
        100
    ];
    let long = Command::ResourceAttachBacking {
        resource_id: 1,
        entries: &entries,
    };
    driver
        .post(99, Context::NONE, &long, &[])
        .expect("the large place");
    assert_eq!(
        driver.post(7, Context::NONE, &Command::GetDisplayInfo, &[]),
        Err(SubmitError::Busy)
    );
    assert_eq!(
        driver.post(7, Context::NONE, &long, &[]),
        Err(SubmitError::Busy)
    );
    assert_eq!(doorbells(&driver), before, "posting rings nothing");
    driver.kick();
    driver.kick();
    assert_eq!(
        doorbells(&driver),
        before + 1,
        "one doorbell for all of them"
    );

    device.borrow_mut().complete_backwards = true;
    device.borrow_mut().serve();
    let done = take_all(&mut driver);
    let tags: Vec<u64> = done.iter().map(|done| done.tag).collect();
    let mut expected: Vec<u64> = (0..CONTROL_SLOTS as u64).chain([99]).collect();
    expected.reverse();
    assert_eq!(tags, expected);
    for one in &done {
        let code = if one.tag == 99 {
            CMD_RESOURCE_ATTACH_BACKING
        } else {
            CMD_RESOURCE_UNREF
        };
        assert_eq!(one.command, code);
        assert_eq!(
            one.result,
            Err(Refusal::InvalidResourceId),
            "no such resource"
        );
    }
    assert!(!driver.is_busy());
    assert_eq!(driver.free_slots(), CONTROL_SLOTS);
    assert!(
        device.borrow().protocol_errors.is_empty(),
        "{:?}",
        device.borrow().protocol_errors
    );
    // And the slots are good for more.
    let Ok(Response::DisplayInfo(_)) = run(&mut driver, &device, &Command::GetDisplayInfo).result
    else {
        panic!("display info");
    };
}

/// A command stream is read where its writer left it: the request is the
/// header, and the stream's scattered pages follow it in the chain.
#[test]
fn a_stream_is_read_where_it_lies() {
    let (bus, device, mut driver) = build((640, 480));
    let work = bus.pin(3, true);
    let stream: Vec<u8> = (0..PAGE + 40)
        .map(|index| (index * 31 % 251) as u8)
        .collect();
    work.write_bytes(PAGE, &stream);
    let pages = work.device_pages();
    let following = [(pages[1], PAGE as u32), (pages[2], 40)];
    driver
        .post(
            5,
            Context::NONE,
            &Command::Submit3dHeader {
                size: stream.len() as u32,
            },
            &following,
        )
        .expect("posted");
    driver.kick();
    device.borrow_mut().serve();
    let done = take_all(&mut driver);
    assert_eq!(done.len(), 1);
    assert_eq!((done[0].tag, done[0].command), (5, CMD_SUBMIT_3D));
    assert!(done[0].result.is_ok(), "{:?}", done[0].result);
    assert_eq!(device.borrow().streams, [stream]);
    assert!(
        device.borrow().protocol_errors.is_empty(),
        "{:?}",
        device.borrow().protocol_errors
    );
}

#[test]
fn a_request_larger_than_the_area_is_refused() {
    let (_bus, _device, mut driver) = build((640, 480));
    // The large place is a page: room for (4096 - 32) / 16 entries.
    let entries = vec![
        MemEntry {
            addr: 0x1000,
            length: 4096
        };
        480
    ];
    assert_eq!(
        driver.submit(&Command::ResourceAttachBacking {
            resource_id: 1,
            entries: &entries
        }),
        Err(SubmitError::TooLarge)
    );
    assert!(!driver.is_busy());
}

/// Drive the pipeline and the driver together until the pipeline is idle,
/// playing the glue: pins come from the card region, replies are collected.
fn pump(
    pipeline: &mut Pipeline,
    driver: &mut TestDriver,
    device: &Rc<RefCell<Device>>,
    card: &Region,
    replies: &mut Vec<Message>,
) {
    use crate::DevicePages;
    let mut entries = [MemEntry::default(); 64];
    let mut pins = 0;
    for _ in 0..64 {
        let snapshot = entries;
        match pipeline.next(&snapshot) {
            Step::Pin { offset, length, .. } => {
                let first = (offset / PAGE_SIZE) as usize;
                let pages = (length / PAGE_SIZE) as usize;
                let count =
                    backing_entries(&card.device_pages()[first..first + pages], &mut entries)
                        .expect("pinned pages make entries");
                pins += 1;
                pipeline.pinned(Ok(count)).expect("waiting");
            }
            Step::Submit(command) => {
                let done = run(driver, device, &command);
                pipeline.done(done.result).expect("waiting");
            }
            Step::Unpin { .. } => pins -= 1,
            Step::Cursor {
                scanout,
                resource,
                hot_x,
                hot_y,
            } => {
                driver
                    .update_cursor(scanout, resource, hot_x, hot_y)
                    .expect("posted");
                device.borrow_mut().serve_cursor();
            }
            Step::Reply(message) => replies.push(message),
            Step::Wait => panic!("nothing is outstanding here"),
            Step::Idle => break,
        }
    }
    let _ = pins;
}

/// What [`first_frame`] leaves: the device, the driver, the card, the
/// pipeline, the pixels and the replies so far.
type Frame = (
    Rc<RefCell<Device>>,
    TestDriver,
    Region,
    Pipeline,
    Vec<u8>,
    Vec<Message>,
);

/// A 64 × 16 buffer on scattered card pages, attached, shown and flushed
/// once: the driver, the device, the card, the pipeline, the pixels and the
/// replies so far.
fn first_frame() -> Frame {
    let (bus, device, mut driver) = build((64, 16));
    // The card VMO: eight pages, scattered, as the core's allocator and the
    // IOMMU might leave them. The buffer is its first two.
    let card = bus.pin(8, true);
    let mut pixels = Vec::new();
    for index in 0..64u32 * 16 {
        pixels.extend_from_slice(&(0xFF00_0000 | (index * 7919)).to_le_bytes());
    }
    card.write_bytes(0, &pixels);

    let mut pipeline = Pipeline::new();
    let mut replies = Vec::new();
    let attach = Attach {
        buffer: 1,
        format: FORMAT,
        offset: 0,
        length: 2 * PAGE_SIZE,
        width: 64,
        height: 16,
        stride: 256,
    };
    let whole = CtlRect {
        x: 0,
        y: 0,
        width: 64,
        height: 16,
    };
    for request in [
        Request::Attach(attach),
        Request::Scanout {
            scanout: 0,
            buffer: 1,
            rect: whole,
        },
        Request::Flush {
            buffer: 1,
            sequence: 1,
            rect: whole,
        },
    ] {
        pipeline.push(request).expect("room");
    }
    pump(&mut pipeline, &mut driver, &device, &card, &mut replies);

    assert_eq!(
        replies,
        [
            Message::Attached {
                buffer: 1,
                status: Status::Ok
            },
            Message::Flipped {
                sequence: 1,
                status: Status::Ok
            },
        ]
    );
    assert_eq!(
        device.borrow().screen,
        pixels,
        "the screen shows the buffer"
    );
    (device, driver, card, pipeline, pixels, replies)
}

#[test]
fn a_frame_reaches_the_screen() {
    let (device, _driver, _card, _pipeline, pixels, replies) = first_frame();
    assert_eq!(
        replies,
        [
            Message::Attached {
                buffer: 1,
                status: Status::Ok
            },
            Message::Flipped {
                sequence: 1,
                status: Status::Ok
            },
        ]
    );
    assert_eq!(
        device.borrow().screen,
        pixels,
        "the screen shows the buffer"
    );
    assert_eq!(
        device.borrow().resources[&1].backing.len(),
        2,
        "two scattered pages, two entries"
    );
}

#[test]
fn a_partial_flush_then_off_and_detach() {
    let (device, mut driver, card, mut pipeline, pixels, mut replies) = first_frame();
    // A partial flush after a change moves only that rectangle.
    let mut changed = pixels;
    for pixel in changed[256 * 5..256 * 6].chunks_mut(4).skip(10).take(4) {
        pixel.copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }
    card.write_bytes(0, &changed);
    pipeline
        .push(Request::Flush {
            buffer: 1,
            sequence: 2,
            rect: CtlRect {
                x: 10,
                y: 5,
                width: 4,
                height: 1,
            },
        })
        .expect("room");
    pump(&mut pipeline, &mut driver, &device, &card, &mut replies);
    assert_eq!(device.borrow().screen, changed);

    // Off, then detach.
    for request in [
        Request::Scanout {
            scanout: 0,
            buffer: 0,
            rect: CtlRect::default(),
        },
        Request::Detach { buffer: 1 },
    ] {
        pipeline.push(request).expect("room");
    }
    pump(&mut pipeline, &mut driver, &device, &card, &mut replies);
    assert_eq!(
        replies.last(),
        Some(&Message::Detached {
            buffer: 1,
            status: Status::Ok
        })
    );
    assert!(device.borrow().resources.is_empty());
    assert!(
        device.borrow().protocol_errors.is_empty(),
        "{:?}",
        device.borrow().protocol_errors
    );
}

#[test]
fn a_device_that_breaks_the_protocol_is_failed() {
    type Setup = fn(&mut Device);
    let cases: [(Setup, DeviceError); 2] = [
        (
            |device| device.misbehave.written = Some(8),
            DeviceError::Protocol(ferrix_virtio::gpu::GpuError::ResponseTooShort(8)),
        ),
        (
            |device| device.misbehave.response = Some(0x1107),
            DeviceError::Protocol(ferrix_virtio::gpu::GpuError::UnknownResponse(0x1107)),
        ),
    ];
    for (misbehave, expected) in cases {
        let (_bus, device, mut driver) = build((64, 16));
        misbehave(&mut device.borrow_mut());
        driver
            .submit(&Command::ResourceUnref { resource_id: 1 })
            .expect("submitted");
        device.borrow_mut().serve();
        let taken = driver.on_interrupt().and_then(|_| driver.take_done());
        assert_eq!(taken, Err(expected));
        assert_eq!(driver.fault(), Some(expected));
        assert_eq!(
            driver.submit(&Command::GetDisplayInfo),
            Err(SubmitError::Broken)
        );
        assert!(matches!(driver.shutdown(), Teardown::Released(_)));
    }
}

#[test]
fn a_display_change_raises_the_control_queues_interrupt_and_is_asked_once() {
    let (_bus, device, mut driver) = build((64, 16));
    // The transport's vector is the control queue's, and a host resizing a
    // window is only heard of if the configuration shares it.
    assert_eq!(device.borrow().config_vector, 1);
    assert!(!driver.display_changed());
    device.borrow_mut().config_mut()[0..4].copy_from_slice(&1u32.to_le_bytes());
    assert!(driver.display_changed());
    assert!(!driver.display_changed());
    // An event that is not a display's is taken and is not one.
    device.borrow_mut().config_mut()[0..4].copy_from_slice(&2u32.to_le_bytes());
    assert!(!driver.display_changed());
}

#[test]
fn display_events_are_read_and_cleared() {
    let (_bus, device, mut driver) = build((64, 16));
    device.borrow_mut().config_mut()[0..4].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(driver.take_events(), 1);
    assert_eq!(driver.take_events(), 0);
    let _ = PAGE;
}

/// A device that needs a reset says so with a configuration change, which
/// under MSI-X shares the control queue's vector: an interrupt that brings
/// no completion. The driver reads the status then, and not on an interrupt
/// that brought one -- but on every [`CONFIG_LOOK_EVERY`]th all the same,
/// and whenever asked after a wait ([`Driver::check_needs_reset`]).
#[test]
fn a_device_that_needs_a_reset_is_failed_at_its_next_quiet_interrupt() {
    let (_bus, device, mut driver) = build((64, 16));
    device.borrow_mut().misbehave.needs_reset = true;
    driver
        .submit(&Command::ResourceUnref { resource_id: 1 })
        .expect("submitted");
    device.borrow_mut().serve();
    let reads = device.borrow().status_reads.get();
    assert!(
        driver.on_interrupt().is_ok(),
        "an interrupt with a completion"
    );
    assert_eq!(device.borrow().status_reads.get(), reads, "status not read");
    assert!(driver.take_done().expect("taken").is_some());
    assert_eq!(driver.on_interrupt(), Err(DeviceError::NeedsReset));
    assert_eq!(driver.fault(), Some(DeviceError::NeedsReset));
    assert_eq!(
        driver.submit(&Command::GetDisplayInfo),
        Err(SubmitError::Broken)
    );
    assert!(matches!(driver.shutdown(), Teardown::Released(_)));

    let (_bus, device, mut driver) = build((64, 16));
    device.borrow_mut().misbehave.needs_reset = true;
    assert_eq!(driver.check_needs_reset(), Err(DeviceError::NeedsReset));
    assert_eq!(driver.fault(), Some(DeviceError::NeedsReset));
}

/// Interrupts that bring completions read no register but the queue's own
/// memory, except every [`CONFIG_LOOK_EVERY`]th; one that brings none reads
/// the status and says the configuration may have changed.
#[test]
fn only_a_quiet_or_a_periodic_interrupt_reads_the_device_status() {
    let (_bus, device, mut driver) = build((64, 16));
    let before = device.borrow().status_reads.get();
    let mut looked = 0;
    for _ in 0..CONFIG_LOOK_EVERY * 2 {
        driver
            .submit(&Command::ResourceUnref { resource_id: 1 })
            .expect("submitted");
        device.borrow_mut().serve();
        let isr = driver.on_interrupt().expect("interrupt");
        if isr & ISR_CONFIG != 0 {
            looked += 1;
        }
        while driver.take_done().expect("taken").is_some() {}
    }
    assert_eq!(looked, 2, "every CONFIG_LOOK_EVERYth interrupt looks");
    assert_eq!(device.borrow().status_reads.get() - before, 2);
    let isr = driver.on_interrupt().expect("a quiet interrupt");
    assert_ne!(
        isr & ISR_CONFIG,
        0,
        "a quiet interrupt may be a configuration change"
    );
    assert_eq!(device.borrow().status_reads.get() - before, 3);
}
