//! Fuzz the display control protocol: its messages and the core's session.
//!
//! The kernel's display core reads these messages from a ring-3 driver, so
//! every byte of them is the driver's word, and the session is what stands
//! between a lying driver and the core's buffer bookkeeping.
//!
//! # The properties
//!
//! Not panicking is the floor. Beyond it:
//!
//! 1. **Decoding is exact**: bytes that decode encode back to the same bytes.
//! 2. **The session keeps its word**: driven by an arbitrary mix of the core's
//!    requests and the driver's replies, it never accepts a FLIPPED out of
//!    order, never reports a buffer attached that it did not ask to attach,
//!    and once broken refuses everything.
//! 3. **An EDID's refresh is bounded**: whatever bytes a device answers
//!    `GET_EDID` with, the refresh read from them is 0 or one a scanout may
//!    report.

#![no_main]

use std::collections::BTreeSet;

use ferrix_displayctl::edid::preferred_refresh_mhz;
use ferrix_displayctl::message::{
    Attach, FORMAT, Hello, MAX_REFRESH_MHZ, MAX_SCANOUTS, Message, PAGE_SIZE, Rect, ScanoutMode,
    Status, Timings, VERSION,
};
use ferrix_displayctl::session::{Event, Session};
use libfuzzer_sys::fuzz_target;

const CARD: u64 = 64 * PAGE_SIZE;

fn status(byte: u8) -> Status {
    Status::from_raw(u32::from(byte % 5)).expect("0 to 4 are statuses")
}

fn attach(id: u32, page: u8) -> Attach {
    Attach {
        buffer: id,
        format: FORMAT,
        offset: u64::from(page % 60) * PAGE_SIZE,
        length: 4 * PAGE_SIZE,
        width: 64,
        height: 64,
        stride: 256,
    }
}

fuzz_target!(|bytes: &[u8]| {
    // 1.
    if let Ok(message) = Message::decode(bytes) {
        assert_eq!(message.encode().as_bytes(), bytes);
    }

    // 3.
    assert!(preferred_refresh_mhz(bytes) <= MAX_REFRESH_MHZ);

    // 2.
    let mut modes = [ScanoutMode::default(); MAX_SCANOUTS];
    modes[0] = ScanoutMode {
        width: 64,
        height: 64,
        enabled: true,
        refresh_mhz: 0,
    };
    let hello = Hello {
        version: VERSION,
        scanouts: 1,
        location: 0,
        modes,
        virgl: false,
        capsets: 0,
        capset: 0,
        capset_bytes: 0,
        cursor: true,
        timings: Timings::NONE,
        copies: false,
    };
    let mut session =
        Session::accept(&hello, &Hello::HANDLE_RIGHTS, CARD).expect("a good HELLO is accepted");
    let mut asked_attach = BTreeSet::new();
    let mut last_flipped = 0u64;
    let full = Rect {
        x: 0,
        y: 0,
        width: 64,
        height: 64,
    };

    for step in bytes.chunks_exact(3) {
        let (op, a, b) = (step[0], step[1], step[2]);
        let id = u32::from(a % 6);
        let was_broken = session.is_broken();
        match op % 11 {
            0 => {
                if session.attach(attach(id, b)).is_ok() {
                    let _ = asked_attach.insert(id);
                }
            }
            1 => {
                let _ = session.scanout(u32::from(b % 2), id, full);
            }
            2 => {
                let _ = session.flush(id, full);
            }
            3 => {
                let _ = session.detach(id);
            }
            4 => {
                let reply = Message::Attached {
                    buffer: id,
                    status: status(b),
                };
                if let Ok(Event::Attached { buffer, .. }) = session.receive(&reply) {
                    assert!(asked_attach.remove(&buffer), "attached without being asked");
                }
            }
            5 => {
                let reply = Message::Flipped {
                    sequence: u64::from(b % 12),
                    status: status(a),
                };
                if let Ok(Event::Flipped { sequence, .. }) = session.receive(&reply) {
                    assert_eq!(sequence, last_flipped + 1, "a flip out of order");
                    last_flipped = sequence;
                }
            }
            6 => {
                let _ = session.receive(&Message::Detached {
                    buffer: id,
                    status: status(b),
                });
            }
            7 => {
                let _ = session.stop();
            }
            // A cursor takes a sequence from the flushes' line, so the
            // order FLIPPED is accepted in above holds of it too.
            8 => {
                let buffer = if a % 7 == 6 { 0 } else { id };
                let _ = session.cursor(
                    u32::from(b % 2),
                    buffer,
                    (u32::from(b % 70), u32::from(a % 70)),
                    (i32::from(a) - 128, -i32::from(b)),
                );
            }
            9 => {
                let _ = session.move_cursor(u32::from(b % 2), (i32::from(a) - 128, i32::from(b)));
            }
            _ => {
                let _ = session.receive(&Message::Stopped);
            }
        }
        if was_broken {
            assert!(session.is_broken(), "a broken session recovered");
            assert!(session.attach(attach(1, 0)).is_err());
        }
    }
});
