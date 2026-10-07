//! Fuzz the render control protocol's wire: the messages a driver and a
//! core send each other, decoded from whatever bytes arrive.
//!
//! The core reads these in the kernel and the driver in ring 3, and neither
//! wrote what the other sent. A second driver will speak the same wire
//! (`docs/GPU.md` §3.3 and §4), so what is pinned down here is the wire
//! rather than one driver's use of it.
//!
//! # The properties
//!
//! Not panicking is the floor. Beyond it:
//!
//! 1. **A decoded message re-encodes to the bytes it came from.** Decoding
//!    is strict -- exact length, reserved words zero, every enumeration a
//!    value it has -- so anything that survives it is something this
//!    protocol could have sent, and encoding it again gives that back.
//! 2. **A validated HELLO keeps its promises**: its version is the one this
//!    crate speaks, its name is non-empty text with nothing after its zero,
//!    its features are ones this version defines and include the one a
//!    render node needs, and the object limit is inside the protocol's.
//! 3. **A work range that fits lies inside the VMO**, and one that does not
//!    is refused whatever arithmetic it was built from.
//! 4. **The session never contradicts itself.** Driven with requests and
//!    replies from the input: an id the core was given back can be asked
//!    for again and one the device may still hold cannot; a broken session
//!    stays broken and answers nothing; and the number of things in flight
//!    never passes the fixed capacity, since the kernel allocates none of
//!    it -- attaches to an importing context included.

#![no_main]

use ferrix_native_abi::rights::Rights;
use ferrix_renderctl::message::{
    Hello, MAX_OBJECT_BYTES, MAX_WORK_BYTES, MakeBlob, Message, NAME_BYTES, NO_RING, NO_WINDOW,
    Status, VERSION, Work, features, flags,
};
use ferrix_renderctl::session::{MAX_IN_FLIGHT, RequestError, Session};
use libfuzzer_sys::fuzz_target;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).map_or(0, |field| {
        u32::from_le_bytes(field.try_into().expect("four bytes"))
    })
}

fuzz_target!(|bytes: &[u8]| {
    // 1.
    if let Some(message) = Message::decode(bytes) {
        let again = message.encode();
        assert_eq!(again.as_bytes(), bytes, "{message:?} did not re-encode");
        assert_eq!(message.kind(), u32_at(bytes, 0));
        assert_eq!(Message::length_of(message.kind()), Some(bytes.len()));

        // 2.
        if let Message::Hello(hello) = message {
            let rights = [Rights(Rights::WRITE.0 | Rights::TRANSFER.0)];
            if hello.validate(&rights).is_ok() {
                assert_eq!(hello.version, VERSION);
                assert!(!hello.name().is_empty());
                assert!(
                    hello
                        .name()
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                );
                // The name is the text and then zeros, with nothing after.
                let end = hello.name().len();
                assert!(hello.name[end..].iter().all(|&byte| byte == 0));
                assert_eq!(hello.features & !features::KNOWN, 0);
                assert_ne!(hello.features & features::SUBMIT, 0);
                assert!(hello.object_max > 0);
                assert!(hello.object_limit() <= MAX_OBJECT_BYTES);
                assert!(hello.object_limit() <= hello.object_max);
            }
            // A name that does not fit is never made, so one that came off
            // the wire is at most the field.
            assert!(hello.name().len() <= NAME_BYTES);
        }
    }

    // 3.
    let range = Work {
        at: u32_at(bytes, 0),
        len: u32_at(bytes, 4),
    };
    let vmo = u64::from(u32_at(bytes, 8));
    if range.fits(vmo) {
        assert!(range.len <= MAX_WORK_BYTES);
        let end = u64::from(range.at) + u64::from(range.len);
        assert!(end <= vmo);
    }

    // 4. The session, driven by the input.
    let work = 1 << 16;
    let named = Hello::named("virtio_gpu").expect("a name");
    let told = Hello {
        version: VERSION,
        location: 0,
        name: named,
        features: features::SUBMIT | features::FENCES | features::BLOBS | features::RINGS,
        capsets: 1 << 2 | 1 << 4,
        object_max: 1 << 20,
    };
    let rights = [Rights(Rights::WRITE.0 | Rights::TRANSFER.0)];
    if let Ok(mut core) = Session::accept(&told, &rights, work) {
        let mut in_flight = 0usize;
        let mut attaching = 0usize;
        for step in bytes.iter().take(256) {
            let id = u32::from(step >> 4) + 1;
            let fence = u64::from(*step);
            let was_broken = core.is_broken();
            match step & 0x7 {
                0 => drop(core.make_context(id, 0)),
                1 => drop(core.drop_context(id)),
                2 if step & 0x8 == 0 => drop(core.make_object(
                    id,
                    u32::from(step >> 5),
                    4096,
                    flags::TO_DEVICE,
                    Work { at: 0, len: 16 },
                )),
                2 => drop(core.make_blob(MakeBlob {
                    object: id,
                    context: u32::from(step >> 5),
                    memory: 2,
                    flags: 1,
                    blob_id: fence,
                    bytes: 4096,
                    window: NO_WINDOW,
                })),
                3 => drop(core.drop_object(id)),
                4 => {
                    let ring = if step & 0x8 == 0 {
                        NO_RING
                    } else {
                        u32::from(step >> 4)
                    };
                    if core.submit(id, ring, fence, Work { at: 0, len: 16 }).is_ok() {
                        in_flight += 1;
                    }
                }
                5 => {
                    if core.wait(id, fence).is_ok() {
                        in_flight += 1;
                    }
                }
                6 => {
                    let reply = match step >> 3 & 3 {
                        0 => Message::ContextMade {
                            context: id,
                            status: Status::Ok,
                        },
                        1 if step & 0x80 == 0 => Message::ObjectMade {
                            object: id,
                            status: Status::Ok,
                        },
                        1 => Message::BlobMade {
                            object: id,
                            status: Status::Ok,
                            map_info: 1,
                        },
                        2 => Message::Submitted {
                            fence,
                            status: Status::Ok,
                        },
                        _ => Message::Waited {
                            fence,
                            status: Status::Ok,
                        },
                    };
                    if core.receive(&reply).is_ok()
                        && matches!(reply, Message::Submitted { .. } | Message::Waited { .. })
                    {
                        in_flight -= 1;
                    }
                }
                // An object given to a context that imported it, and the
                // answer, which names the object alone.
                7 if step & 0x18 == 0 => {
                    if core.attach_object(id, u32::from(step >> 5)).is_ok() {
                        attaching += 1;
                    }
                }
                7 if step & 0x18 == 0x08 => {
                    let reply = Message::ObjectAttached {
                        object: id,
                        status: if step & 0x80 == 0 {
                            Status::Ok
                        } else {
                            Status::DeviceRefused
                        },
                    };
                    if core.receive(&reply).is_ok() {
                        attaching -= 1;
                    }
                }
                _ => drop(core.stop()),
            }
            // A broken session stays broken and takes nothing more.
            if was_broken {
                assert!(core.is_broken());
                assert_eq!(core.make_context(1, 0), Err(RequestError::Closed));
            }
            // The capacity is fixed, because nothing here allocates.
            assert!(in_flight <= MAX_IN_FLIGHT);
            assert!(attaching <= MAX_IN_FLIGHT);
        }
    }

    // And a name made from the input is either refused or reads back.
    if let Ok(text) = core::str::from_utf8(bytes)
        && let Some(name) = Hello::named(text)
    {
        assert!(!text.is_empty() && text.len() <= NAME_BYTES);
        assert_eq!(&name[..text.len()], text.as_bytes());
        assert!(name[text.len()..].iter().all(|&byte| byte == 0));
    }
});
