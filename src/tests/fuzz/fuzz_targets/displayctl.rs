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
//! 4. **An EDID's modes are bounded** (displayctl v11): whatever bytes, the
//!    modes read from them are at most 64, each a mode a display can have,
//!    at most one preferred, and none of them twice.
//! 5. **Mode lists keep their word**: driven by arbitrary `MODE_LIST` pages,
//!    the session takes a list only when its last page arrives, and
//!    `SET_MODE` goes only for a timing of the list it took, never for
//!    another list or another timing.

#![no_main]

use std::collections::BTreeSet;

use ferrix_displayctl::edid::preferred_refresh_mhz;
use ferrix_displayctl::message::{
    Attach, FORMAT, Hello, MAX_REFRESH_MHZ, MAX_SCANOUTS, Message, PAGE_SIZE, Rect, ScanoutMode,
    Status, Timings, VERSION,
};
use ferrix_displayctl::message::{Listed, MAX_LISTED_MODES, MODES_A_PAGE, ModePage};
use ferrix_display_modes::{ModeList, edid_modes, synthetic};
use ferrix_displayctl::session::{Event, Session, list_hash};
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

    // 4.
    let mut named = ModeList::new();
    edid_modes(bytes, &mut named);
    assert!(named.len() <= MAX_LISTED_MODES);
    assert!(named.as_slice().iter().all(|listed| listed.timing.is_mode()));
    assert!(named.as_slice().iter().filter(|listed| listed.preferred).count() <= 1);

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
        connectors: [0; MAX_SCANOUTS],
    };
    let mut session =
        Session::accept(&hello, &Hello::HANDLE_RIGHTS, CARD).expect("a good HELLO is accepted");
    let mut asked_attach = BTreeSet::new();
    let mut last_flipped = 0u64;
    // The pages of the list being sent, and the last list the session took.
    let mut pages: Vec<Listed> = Vec::new();
    let mut taken: Option<Vec<Listed>> = None;
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
        match op % 13 {
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
            10 => {
                let _ = session.receive(&Message::Stopped);
            }
            // 5: a page of up to `count` made-up modes, the first preferred
            // where `a` says, at an index and total the bytes choose.
            11 => {
                let count = u32::from(a % 26);
                let first = u32::from(b % 70);
                let total = u32::from(b / 3);
                let mut entries = [Listed::default(); MODES_A_PAGE];
                for (index, entry) in entries.iter_mut().take(count as usize).enumerate() {
                    *entry = Listed {
                        timing: synthetic(640 + index as u32, 480, 60_000),
                        preferred: index == usize::from(a % 5),
                    };
                }
                let page = ModePage {
                    scanout: u32::from(a / 128),
                    total,
                    first,
                    count,
                    entries,
                };
                let had = session.has_list(0);
                if let Ok(Event::ModeList { finishes, .. }) =
                    session.receive(&Message::ModeList(page))
                {
                    if finishes {
                        let built: Vec<Listed> = page.as_slice().to_vec();
                        if first == 0 {
                            pages = built;
                        } else {
                            pages.extend(built);
                        }
                        taken = Some(pages.clone());
                    } else if first == 0 {
                        pages = page.as_slice().to_vec();
                    } else {
                        pages.extend_from_slice(page.as_slice());
                    }
                    assert!(session.has_list(0) || !finishes || had);
                }
            }
            _ => {
                let mode = synthetic(640 + u32::from(a % 30), 480, 60_000);
                let list = taken.clone().unwrap_or_default();
                let held = list.iter().any(|listed| listed.timing == mode);
                if let Ok(Some(_)) = session.set_mode(0, mode, &list) {
                    assert!(held, "SET_MODE for a timing the list does not hold");
                    assert!(taken.is_some(), "SET_MODE with no list taken");
                }
                // Another list than the one taken is refused.
                let mut other = list.clone();
                other.push(Listed::default());
                assert!(session.set_mode(0, mode, &other).is_err());
                let _ = list_hash(&list);
            }
        }
        if was_broken {
            assert!(session.is_broken(), "a broken session recovered");
            assert!(session.attach(attach(1, 0)).is_err());
        }
    }
});
