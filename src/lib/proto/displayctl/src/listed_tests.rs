//! The core's share of mode lists (`docs/DISPLAY.md` §9): the session's
//! checks of `MODE_LIST` pages and of `SET_MODE`, and `SETCRTC`'s match.

extern crate std;

use std::vec::Vec;

use crate::listed::listed_match;
use crate::message::*;
use crate::session::*;
use crate::tests::{page_of, synthetic};

// -- Pages and SET_MODE in the session ---------------------------------------------

fn session_of(scanouts: u16) -> Session {
    let mut hello = crate::tests::hello();
    hello.scanouts = scanouts;
    for mode in hello.modes.iter_mut().take(usize::from(scanouts)) {
        *mode = ScanoutMode {
            width: 1920,
            height: 1080,
            enabled: true,
            refresh_mhz: 60_000,
        };
    }
    Session::accept(&hello, &Hello::HANDLE_RIGHTS, 256 << 20).expect("a good HELLO")
}

fn listed(sizes: &[(u32, u32, bool)]) -> Vec<Listed> {
    sizes
        .iter()
        .map(|&(width, height, preferred)| Listed {
            timing: synthetic(width, height, 60_000),
            preferred,
        })
        .collect()
}

#[test]
fn a_list_in_one_page_is_taken_and_set_mode_names_its_modes() {
    let mut session = session_of(1);
    let sizes = [(1920, 1080, true), (1280, 720, false)];
    assert!(!session.has_list(0));
    let event = session.receive(&Message::ModeList(page_of(&sizes, 0, 2)));
    assert_eq!(
        event,
        Ok(Event::ModeList {
            scanout: 0,
            starts: true,
            finishes: true
        })
    );
    assert!(session.has_list(0));
    let list = listed(&sizes);
    let small = synthetic(1280, 720, 60_000);
    assert_eq!(
        session.set_mode(0, small, &list),
        Ok(Some(Message::SetMode {
            scanout: 0,
            timing: small
        }))
    );
    assert_eq!(session.set_mode(0, small, &list), Ok(None), "told already");
    // A refused flush after it: told again.
    session.mode_refused(0);
    assert!(matches!(session.set_mode(0, small, &list), Ok(Some(_))));
    // Not in the list, or not the list the driver sent.
    assert_eq!(
        session.set_mode(0, synthetic(800, 600, 60_000), &list),
        Err(RequestError::Mode)
    );
    assert_eq!(
        session.set_mode(0, small, &listed(&[(1280, 720, true)])),
        Err(RequestError::Mode)
    );
    assert_eq!(
        session.set_mode(1, small, &list),
        Err(RequestError::NoSuchScanout)
    );
}

#[test]
fn set_mode_needs_a_finished_list() {
    let mut session = session_of(1);
    let list = listed(&[(1920, 1080, true)]);
    assert_eq!(
        session.set_mode(0, synthetic(1920, 1080, 60_000), &list),
        Err(RequestError::Mode)
    );
    // Half a list is not one.
    let first: Vec<(u32, u32, bool)> = (0..24).map(|n| (800 + n, 600, n == 0)).collect();
    assert!(
        session
            .receive(&Message::ModeList(page_of(&first, 0, 30)))
            .is_ok()
    );
    assert!(!session.has_list(0));
}

#[test]
fn a_list_in_pages_is_taken_when_the_last_arrives_and_a_new_one_tells_again() {
    let mut session = session_of(2);
    let all: Vec<(u32, u32, bool)> = (0..30).map(|n| (800 + n, 600, n == 7)).collect();
    let mut first = page_of(&all[..24], 0, 30);
    first.scanout = 1;
    let mut second = page_of(&all[24..], 24, 30);
    second.scanout = 1;
    assert_eq!(
        session.receive(&Message::ModeList(first)),
        Ok(Event::ModeList {
            scanout: 1,
            starts: true,
            finishes: false
        })
    );
    assert_eq!(
        session.receive(&Message::ModeList(second)),
        Ok(Event::ModeList {
            scanout: 1,
            starts: false,
            finishes: true
        })
    );
    let list = listed(&all);
    let timing = list[29].timing;
    assert!(matches!(session.set_mode(1, timing, &list), Ok(Some(_))));
    // The same list again: the mode is to be told again.
    assert!(session.receive(&Message::ModeList(first)).is_ok());
    assert!(session.receive(&Message::ModeList(second)).is_ok());
    assert!(matches!(session.set_mode(1, timing, &list), Ok(Some(_))));
    // A new first page discards a list half sent.
    assert!(session.receive(&Message::ModeList(first)).is_ok());
    assert!(session.receive(&Message::ModeList(first)).is_ok());
    assert!(session.receive(&Message::ModeList(second)).is_ok());
}

#[test]
fn a_page_that_lies_breaks_the_session() {
    let good = page_of(&[(1920, 1080, true), (1280, 720, false)], 0, 2);
    let lies: Vec<(&str, ModePage)> = std::vec![
        (
            "a scanout the card has not",
            ModePage { scanout: 1, ..good }
        ),
        ("a total of 0", ModePage { total: 0, ..good }),
        ("a total past 64", ModePage { total: 65, ..good }),
        ("a count of 0", ModePage { count: 0, ..good }),
        ("a count past 24", ModePage { count: 25, ..good }),
        ("past its total", ModePage { total: 1, ..good }),
        (
            "an unexpected first",
            ModePage {
                first: 1,
                total: 3,
                ..good
            }
        ),
        ("no preferred", page_of(&[(1920, 1080, false)], 0, 1)),
        (
            "two preferred",
            page_of(&[(1920, 1080, true), (1280, 720, true)], 0, 2)
        ),
        ("a timing that is no mode", {
            let mut page = good;
            page.entries[1].timing.htotal = 0;
            page
        }),
    ];
    for (why, page) in lies {
        let mut session = session_of(1);
        assert_eq!(
            session.receive(&Message::ModeList(page)),
            Err(Refusal::Protocol),
            "{why}"
        );
        assert!(session.is_broken(), "{why}");
    }
    // A second page of another total, and a second page with no first.
    let all: Vec<(u32, u32, bool)> = (0..30).map(|n| (800 + n, 600, n == 0)).collect();
    let mut session = session_of(1);
    assert!(
        session
            .receive(&Message::ModeList(page_of(&all[..24], 0, 30)))
            .is_ok()
    );
    assert_eq!(
        session.receive(&Message::ModeList(page_of(&all[24..], 24, 31))),
        Err(Refusal::Protocol)
    );
    let mut session = session_of(1);
    assert_eq!(
        session.receive(&Message::ModeList(page_of(&all[24..], 24, 30))),
        Err(Refusal::Protocol)
    );
}

#[test]
fn a_page_decodes_strictly() {
    let good = Message::ModeList(page_of(&[(1920, 1080, true)], 0, 1));
    let bytes = good.encode().as_bytes().to_vec();
    assert_eq!(bytes.len(), MODE_LIST_BYTES);
    // A flag other than preferred.
    let mut flagged = bytes.clone();
    flagged[24 + TIMING_BYTES] = 2;
    assert_eq!(Message::decode(&flagged), Err(MessageError::Field));
    // Bytes past the count.
    let mut past = bytes;
    past[24 + LISTED_BYTES] = 1;
    assert_eq!(Message::decode(&past), Err(MessageError::Field));
    // SET_MODE's reserved word.
    let set = Message::SetMode {
        scanout: 0,
        timing: synthetic(1920, 1080, 60_000),
    };
    let mut bytes = set.encode().as_bytes().to_vec();
    assert_eq!(bytes.len(), SET_MODE_BYTES);
    bytes[12] = 1;
    assert_eq!(Message::decode(&bytes), Err(MessageError::Field));
}

#[test]
fn a_mode_list_in_hellos_place_is_no_hello() {
    // MODE_LIST before READY: the core reads only a HELLO there, and this
    // is not one.
    let page = Message::ModeList(page_of(&[(1920, 1080, true)], 0, 1));
    assert!(!matches!(
        Message::decode(page.encode().as_bytes()),
        Ok(Message::Hello(_))
    ));
}

// -- The core's match of a SETCRTC ---------------------------------------------------

fn info_of(timing: &Timing) -> ferrix_linux_abi::drm::ModeInfo {
    use ferrix_linux_abi::drm::Field as _;
    use ferrix_linux_abi::drm::{
        MODE_FLAG_NHSYNC, MODE_FLAG_NVSYNC, MODE_FLAG_PHSYNC, MODE_FLAG_PVSYNC,
    };
    let mut mode = ferrix_linux_abi::drm::ModeInfo::ZERO;
    mode.clock = timing.clock_khz;
    mode.hdisplay = timing.hdisplay;
    mode.hsync_start = timing.hsync_start;
    mode.hsync_end = timing.hsync_end;
    mode.htotal = timing.htotal;
    mode.vdisplay = timing.vdisplay;
    mode.vsync_start = timing.vsync_start;
    mode.vsync_end = timing.vsync_end;
    mode.vtotal = timing.vtotal;
    mode.vrefresh = timing.refresh_hz();
    mode.flags = if timing.hsync_high {
        MODE_FLAG_PHSYNC
    } else {
        MODE_FLAG_NHSYNC
    } | if timing.vsync_high {
        MODE_FLAG_PVSYNC
    } else {
        MODE_FLAG_NVSYNC
    };
    mode
}

#[test]
fn setcrtc_takes_only_a_listed_mode() {
    let list = listed(&[(1920, 1080, true), (1280, 720, false)]);
    let fast = Listed {
        timing: synthetic(1280, 720, 120_000),
        preferred: false,
    };
    let list = [list[0], list[1], fast];
    // Exactly as listed.
    assert_eq!(
        listed_match(&list, &info_of(&list[1].timing)),
        Some(list[1].timing)
    );
    assert_eq!(
        listed_match(&list, &info_of(&fast.timing)),
        Some(fast.timing)
    );
    // The same size and refresh, other numbers: the listed one.
    let mut near = info_of(&list[1].timing);
    near.htotal += 2;
    assert_eq!(listed_match(&list, &near), Some(list[1].timing));
    // The same size at a refresh nobody listed, and a size nobody listed.
    near.vrefresh = 75;
    assert_eq!(listed_match(&list, &near), None);
    assert_eq!(
        listed_match(&list, &info_of(&synthetic(800, 600, 60_000))),
        None
    );
    assert_eq!(listed_match(&[], &info_of(&list[0].timing)), None);
}
