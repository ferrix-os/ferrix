//! Mode lists (`docs/DISPLAY.md` §9): the pages and the session's checks of
//! them and of `SET_MODE`, the modes an EDID names, the timings made up for
//! them, and the scaling arithmetic.
//!
//! The QEMU EDIDs are what QEMU 10.2.1's `hw/display/edid-generate.c` makes
//! for a virtio-gpu's scanout, printed by that file compiled on its own
//! with the size and refresh given; what each lists follows from the
//! file's own table, which the tests name mode by mode.

extern crate std;

use std::vec::Vec;

use crate::message::*;
use crate::modes::*;
use crate::scale::Scaler;
use crate::session::*;
use crate::tests::page_of;

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(core::str::from_utf8(pair).expect("ascii"), 16).expect("hex")
        })
        .collect()
}

/// QEMU's EDID for a 1920x1080 scanout at 75 Hz, VNC's rate: a base block
/// and a CTA-861 extension.
const QEMU_1080P_75: &str = "
00ffffffffffff0049143412000000002a180104a5301b7806ee91a3544c9926
0f5054210800e1c0d1c0d100a940b300950081808140d25480a072382540e039
5540e71211000018000000f7000a004082002820000000000000000000fd0032
7d1ea0ff010a202020202020000000fc0051454d55204d6f6e69746f720a01b0
02030b00467d6560591f61000000100000000000000000000000000000000000
1000000000000000000000000000000000001000000000000000000000000000
0000000010000000000000000000000000000000000010000000000000000000
000000000000000010000000000000000000000000000000000000000000002f
";

fn edid_of(text: &str) -> Vec<u8> {
    hex(text)
}

fn sizes(list: &ModeList) -> Vec<(u16, u16, u32)> {
    list.as_slice()
        .iter()
        .map(|listed| {
            (
                listed.timing.hdisplay,
                listed.timing.vdisplay,
                listed.timing.refresh_hz(),
            )
        })
        .collect()
}

// -- Timings -----------------------------------------------------------------

#[test]
fn cvt_makes_vesas_own_timings() {
    // DMT gives these sizes CVT's timings (VESA DMT 1.13); Linux's
    // `drm_cvt_mode` leaves the clock unrounded, which is all that differs.
    let checks = [
        // width, height, Hz, reduced, the DMT entry's spans.
        // 1280x768 is 5:3, which `drm_cvt_mode`'s aspect test misses
        // (768 is no multiple of 9), so its vertical sync is CVT's custom
        // 10 lines where DMT's table has 7: Linux's own answer, kept.
        (
            1280,
            768,
            60,
            false,
            [1280, 1344, 1472, 1664],
            [768, 771, 781, 798],
        ),
        // `drm_cvt_mode` moves an hsync start already on the 8-pixel grid
        // on by 8 more; DMT's table has 1520. Linux's answer, kept.
        (
            1440,
            900,
            60,
            false,
            [1440, 1528, 1672, 1904],
            [900, 903, 909, 934],
        ),
        (
            1680,
            1050,
            60,
            false,
            [1680, 1784, 1960, 2240],
            [1050, 1053, 1059, 1089],
        ),
        (
            1920,
            1200,
            60,
            false,
            [1920, 2056, 2256, 2592],
            [1200, 1203, 1209, 1245],
        ),
        (
            1920,
            1200,
            60,
            true,
            [1920, 1968, 2000, 2080],
            [1200, 1203, 1209, 1235],
        ),
        (
            1280,
            768,
            60,
            true,
            [1280, 1328, 1360, 1440],
            [768, 771, 781, 790],
        ),
    ];
    for (width, height, hertz, reduced, h, v) in checks {
        let made = cvt(width, height, hertz, reduced).expect("a timing");
        assert_eq!(
            [made.hdisplay, made.hsync_start, made.hsync_end, made.htotal],
            h,
            "{width}x{height}@{hertz} reduced {reduced}"
        );
        assert_eq!(
            [made.vdisplay, made.vsync_start, made.vsync_end, made.vtotal],
            v,
            "{width}x{height}@{hertz} reduced {reduced}"
        );
        assert_eq!(made.refresh_hz(), hertz);
        assert_eq!(made.hsync_high, reduced, "CVT's polarities");
        assert_eq!(made.vsync_high, !reduced);
        assert_eq!(is_reduced(&made), reduced);
    }
    assert_eq!(cvt(0, 768, 60, false), None);
    assert_eq!(cvt(1024, 768, 0, false), None);
}

#[test]
fn a_standard_timing_is_vesas_where_vesa_has_one() {
    let found = standard(1280, 1024, 60, false).expect("DMT's");
    assert_eq!(
        (found.clock_khz, found.htotal, found.vtotal),
        (108_000, 1688, 1066)
    );
    // DMT's 2048x1152 has blanking of its own, which is not CVT's reduced
    // kind, so it is found as a standard timing names it, not reduced.
    let found = standard(2048, 1152, 60, false).expect("DMT's");
    assert_eq!((found.clock_khz, found.htotal), (162_000, 2250));
    // No DMT entry: CVT's.
    let made = standard(1792, 1000, 60, false).expect("CVT's");
    assert_eq!(made.refresh_hz(), 60);
}

#[test]
fn a_made_up_timing_is_the_one_the_kernel_listed() {
    // `drm.rs`'s `mode_at` for 1920x1080 at 75 Hz: totals 2080 x 1103.
    let made = synthetic(1920, 1080, 74_998);
    assert_eq!((made.htotal, made.vtotal), (2080, 1103));
    assert_eq!(
        u64::from(made.clock_khz),
        2080 * 1103 * 74_998_u64 / 1_000_000
    );
    assert_eq!(made.refresh_hz(), 75);
    assert!(made.is_mode());
}

// -- The list ------------------------------------------------------------------

#[test]
fn a_list_keeps_one_of_each_mode_and_one_preferred() {
    let mut list = ModeList::new();
    assert!(list.add(synthetic(1024, 768, 60_000), false));
    assert!(
        !list.add(synthetic(1024, 768, 60_400), false),
        "60 Hz again"
    );
    assert!(list.add(synthetic(1024, 768, 75_000), true));
    assert!(
        list.add(synthetic(800, 600, 60_000), true),
        "takes the mark"
    );
    let preferred: Vec<_> = list.as_slice().iter().filter(|l| l.preferred).collect();
    assert_eq!(preferred.len(), 1);
    assert_eq!(preferred[0].timing.hdisplay, 800);
    // Not a mode, or a refresh no display has.
    assert!(!list.add(Timing::default(), false));
    let mut fast = synthetic(640, 480, 60_000);
    fast.clock_khz = u32::MAX;
    assert!(!list.add(fast, false));
    // Full at 64.
    let mut full = ModeList::new();
    for height in 0..80 {
        let _ = full.add(synthetic(640, 400 + height, 60_000), false);
    }
    assert_eq!(full.len(), MAX_LISTED_MODES);
}

#[test]
fn a_list_sorts_as_linux_sorts_and_finds_the_nearest_refresh() {
    let mut list = ModeList::new();
    for (width, height, refresh, preferred) in [
        (800, 600, 60_000, false),
        (1920, 1080, 60_000, false),
        (1280, 720, 60_000, true),
        (1920, 1080, 144_000, false),
    ] {
        let _ = list.add(synthetic(width, height, refresh), preferred);
    }
    list.sort();
    assert_eq!(
        sizes(&list),
        [
            (1280, 720, 60),
            (1920, 1080, 144),
            (1920, 1080, 60),
            (800, 600, 60)
        ]
    );
    assert_eq!(
        list.find(1920, 1080, 120_000).map(|t| t.refresh_hz()),
        Some(144)
    );
    assert_eq!(
        list.find(1920, 1080, 50_000).map(|t| t.refresh_hz()),
        Some(60)
    );
    assert_eq!(list.find(1600, 900, 60_000), None);
}

#[test]
fn a_list_goes_as_pages_of_twenty_four() {
    let mut list = ModeList::new();
    for height in 0..50 {
        let _ = list.add(synthetic(640, 400 + height, 60_000), height == 0);
    }
    let pages: Vec<Message> = list.pages(3).collect();
    assert_eq!(pages.len(), 3);
    let mut seen = 0;
    for (index, message) in pages.iter().enumerate() {
        let Message::ModeList(page) = message else {
            panic!("a page")
        };
        assert_eq!((page.scanout, page.total), (3, 50));
        assert_eq!(page.first as usize, index * MODES_A_PAGE);
        seen += page.count;
        assert_eq!(page.finishes(), index == 2);
        assert_eq!(Message::decode(message.encode().as_bytes()), Ok(*message));
    }
    assert_eq!(seen, 50);
    assert_eq!(ModeList::new().pages(0).count(), 0);
}

#[test]
fn a_virtual_card_lists_what_the_kernel_listed_after_its_display() {
    let mut list = ModeList::new();
    add_virtual(&mut list, 1280, 800, 74_998);
    let listed = sizes(&list);
    assert_eq!(listed[0], (1280, 800, 75));
    assert!(list.as_slice()[0].preferred);
    assert_eq!(
        &listed[1..4],
        [(1280, 800, 60), (1280, 800, 120), (1280, 800, 144)]
    );
    assert_eq!(listed.len(), 4 + STANDARD_SIZES.len());
    // An EDID's preferred mode of that size keeps the mark.
    let mut list = ModeList::new();
    let _ = list.add(cvt(1280, 800, 75, false).expect("a timing"), true);
    add_virtual(&mut list, 1280, 800, 74_998);
    assert!(list.as_slice()[0].preferred);
    assert_eq!(list.as_slice().iter().filter(|l| l.preferred).count(), 1);
}

#[test]
fn a_scaling_card_lists_its_mode_and_the_sizes_inside_it() {
    let running = standard(1920, 1080, 60, false).expect("1080p");
    let mut list = ModeList::new();
    add_scaled(&mut list, running);
    let listed = sizes(&list);
    assert_eq!(listed[0], (1920, 1080, 60));
    assert!(list.as_slice()[0].preferred);
    assert_eq!(
        &listed[1..],
        [
            (1680, 1050, 60),
            (1600, 900, 60),
            (1280, 1024, 60),
            (1280, 720, 60),
            (1024, 768, 60),
            (800, 600, 60)
        ]
    );
}

// -- EDIDs -----------------------------------------------------------------------

#[test]
fn qemus_edid_names_every_mode_its_table_has() {
    let edid = edid_of(QEMU_1080P_75);
    assert_eq!(edid.len(), 256);
    let mut list = ModeList::new();
    crate::edid::modes(&edid, &mut list);
    let listed = sizes(&list);
    // The detailed timing first, preferred: the window's size at its rate.
    assert_eq!(listed[0], (1920, 1080, 75));
    assert!(list.as_slice()[0].preferred);
    assert_eq!(list.as_slice().iter().filter(|l| l.preferred).count(), 1);
    for wanted in [
        // The CTA-861 video codes, 125, 101, 96, 89, 31 and 97.
        (5120, 2160, 50),
        (4096, 2160, 50),
        (3840, 2160, 50),
        (2560, 1080, 50),
        (1920, 1080, 50),
        (3840, 2160, 60),
        // The established timings.
        (1024, 768, 60),
        (800, 600, 60),
        (640, 480, 60),
        // The eight standard timings.
        (2048, 1152, 60),
        (1920, 1080, 60),
        (1920, 1200, 60),
        (1600, 1200, 60),
        (1680, 1050, 60),
        (1440, 900, 60),
        (1280, 1024, 60),
        (1280, 960, 60),
        // Established Timings III, QEMU's 1440x1050 being the bit for
        // 1400x1050.
        (1280, 768, 60),
        (1920, 1440, 60),
        (1856, 1392, 60),
        (1792, 1344, 60),
        (1400, 1050, 60),
        (1360, 768, 60),
    ] {
        assert!(listed.contains(&wanted), "{wanted:?} in {listed:?}");
    }
    assert_eq!(listed.len(), 24, "{listed:?}");
    let dmt = list
        .as_slice()
        .iter()
        .find(|l| (l.timing.hdisplay, l.timing.vdisplay, l.timing.refresh_hz()) == (1920, 1080, 60))
        .expect("1080p60");
    assert_eq!((dmt.timing.clock_khz, dmt.timing.htotal), (148_500, 2200));
}

#[test]
fn an_edid_that_is_not_one_names_nothing() {
    let mut edid = edid_of(QEMU_1080P_75);
    for broken in [
        {
            let mut bytes = edid.clone();
            bytes[0] = 1;
            bytes
        },
        {
            let mut bytes = edid.clone();
            bytes[127] ^= 1;
            bytes
        },
        edid[..100].to_vec(),
        Vec::new(),
    ] {
        let mut list = ModeList::new();
        crate::edid::modes(&broken, &mut list);
        assert!(list.is_empty());
    }
    // A broken extension costs only its own modes.
    edid[255] ^= 1;
    let mut list = ModeList::new();
    crate::edid::modes(&edid, &mut list);
    assert_eq!(list.len(), 24 - 6);
}

#[test]
fn a_large_screen_is_named_by_displayid() {
    // QEMU at 5120x2160@60 Hz: the size a base descriptor cannot hold goes
    // in a DisplayID extension's type I timing, marked preferred.
    const QEMU_5K: &str = "
00ffffffffffff0049143412000000002a180104a582367806ee91a3544c9926
0f5054210800e1c0d1c0d100a940b300950081808140000000f7000a00408200
2820000000000000000000fd00327d1ea0ff010a202020202020000000fc0051
454d55204d6f6e69746f720a0000001000000000000000000000000000000257
02030b00467d6560591f61000000100000000000000000000000000000000000
1000000000000000000000000000000000001000000000000000000000000000
0000000010000000000000000000000000000000000010000000000000000000
000000000000000010000000000000000000000000000000000000000000002f
7013170300030014116a0188ff13ff06ff0498006f084a000900090033000000
0000000000000000000000000000000000000000000000000000000000000000
0000000000000000000000000000000000000000000000000000000000000000
0000000000000000000000000000000000000000000000000000000000000090
";
    let edid = edid_of(QEMU_5K);
    let mut list = ModeList::new();
    crate::edid::modes(&edid, &mut list);
    list.sort();
    let first = list.as_slice()[0];
    assert!(first.preferred);
    assert_eq!(
        (
            first.timing.hdisplay,
            first.timing.vdisplay,
            first.timing.refresh_hz()
        ),
        (5120, 2160, 60)
    );
    assert_eq!(list.as_slice().iter().filter(|l| l.preferred).count(), 1);
}

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

// -- Scaling ------------------------------------------------------------------------

#[test]
fn a_smaller_mode_is_centred_with_its_shape_kept() {
    let scaler = Scaler::new((1280, 720), (1920, 1080)).expect("a scaler");
    assert_eq!(
        (
            scaler.area.x,
            scaler.area.y,
            scaler.area.width,
            scaler.area.height
        ),
        (0, 0, 1920, 1080)
    );
    let scaler = Scaler::new((1024, 768), (1920, 1080)).expect("a scaler");
    assert_eq!(
        (
            scaler.area.x,
            scaler.area.y,
            scaler.area.width,
            scaler.area.height
        ),
        (240, 0, 1440, 1080),
        "pillarboxed"
    );
    let scaler = Scaler::new((1280, 1024), (1920, 1080)).expect("a scaler");
    assert_eq!((scaler.area.width, scaler.area.height), (1350, 1080));
    let scaler = Scaler::new((1920, 1080), (1920, 1200)).expect("a scaler");
    assert_eq!(
        (
            scaler.area.x,
            scaler.area.y,
            scaler.area.width,
            scaler.area.height
        ),
        (0, 60, 1920, 1080),
        "letterboxed"
    );
    assert!(
        Scaler::new((1920, 1080), (1920, 1080))
            .expect("same")
            .is_identity()
    );
    assert_eq!(Scaler::new((0, 1), (1, 1)), None);
}

#[test]
fn an_integer_ratio_is_exact() {
    let scaler = Scaler::new((960, 540), (1920, 1080)).expect("a scaler");
    for x in 0..1920 {
        assert_eq!(scaler.source_x(x), x / 2);
    }
    for y in 0..1080 {
        assert_eq!(scaler.source_y(y), y / 2);
    }
}

#[test]
fn a_change_reaches_exactly_the_screen_pixels_that_read_it() {
    for (source, screen) in [
        ((1280, 720), (1920, 1080)),
        ((1024, 768), (1920, 1080)),
        ((800, 600), (1366, 768)),
        ((7, 5), (23, 11)),
    ] {
        let scaler = Scaler::new(source, screen).expect("a scaler");
        let width = scaler.area.width;
        for (from, to) in [
            (0, 1),
            (3, 4),
            (0, source.0),
            (source.0 - 1, source.0),
            (2, 5),
        ] {
            let changed = Rect {
                x: from,
                y: 0,
                width: to - from,
                height: 1,
            };
            let reading: Vec<u32> = (0..width)
                .filter(|&x| (from..to).contains(&scaler.source_x(x)))
                .collect();
            match scaler.damaged(changed) {
                Some(rect) => {
                    let got: Vec<u32> =
                        (rect.x - scaler.area.x..rect.x - scaler.area.x + rect.width).collect();
                    assert_eq!(
                        got, reading,
                        "{source:?} on {screen:?}, columns {from}..{to}"
                    );
                }
                None => assert!(reading.is_empty(), "{source:?} on {screen:?}, {from}..{to}"),
            }
        }
        // Every source column is read by some screen column when shown larger.
        let mut seen = std::vec![false; source.0 as usize];
        for x in 0..width {
            seen[scaler.source_x(x) as usize] = true;
        }
        assert!(seen.iter().all(|&read| read), "{source:?} on {screen:?}");
        assert_eq!(
            scaler.damaged(Rect {
                x: source.0,
                y: 0,
                width: 4,
                height: 1
            }),
            None
        );
    }
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
