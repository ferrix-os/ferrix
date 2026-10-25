//! Host tests: a debug flood sheds debug first and keeps the same source's
//! warnings and errors, another source keeps everything, and every shed is
//! said.

extern crate alloc;

use alloc::string::String;

use super::{Caps, Shed, WINDOW_US, describe, level_of};

#[test]
fn a_debug_flood_sheds_debug_and_keeps_warnings_and_errors() {
    let mut flood = Shed::new(Caps::CONSOLE, 0);
    let mut quiet = Shed::new(Caps::CONSOLE, 0);
    let (mut debug, mut warnings, mut errors, mut others) = (0_u32, 0_u32, 0_u32, 0_u32);
    // 10,000 debug lines a second for 3 s, a warning and an error every
    // 100 lines among them, and a quiet source's info line every 1000.
    for line in 0..30_000_u64 {
        let now = line * 100;
        debug += u32::from(flood.admit(7, now));
        if line % 100 == 0 {
            warnings += u32::from(flood.admit(4, now));
            errors += u32::from(flood.admit(3, now));
        }
        if line % 1000 == 0 {
            others += u32::from(quiet.admit(6, now));
        }
    }
    // The caps count every kept line: a window's first warnings and errors
    // come before its debug lines reach 100, so a few fewer debug lines.
    assert!(
        (270..=300).contains(&debug),
        "about 100 debug lines a second, the cap: {debug}"
    );
    assert_eq!(warnings, 300, "every warning kept");
    assert_eq!(errors, 300, "every error kept");
    assert_eq!(others, 30, "the quiet source keeps everything");
    assert_eq!(quiet.last(3 * WINDOW_US), None);
    let said = flood.last(3 * WINDOW_US).unwrap();
    assert_eq!(said.shed[7], 30_000 - u64::from(debug));
    assert_eq!(said.level(), 7);
    let mut text = String::new();
    describe(&said, "yserver", &mut text).unwrap();
    assert_eq!(
        text,
        alloc::format!(
            "{0} lines dropped (debug: {0}) from yserver in 3.0 s",
            30_000 - debug
        )
    );
}

#[test]
fn at_a_higher_rate_info_goes_next_then_warnings() {
    let mut shed = Shed::new(Caps::CONSOLE, 0);
    let kept: [u32; 8] = core::array::from_fn(|_| 0);
    let mut kept = kept;
    // 3,000 lines of each of info, notice and warning in one window.
    for _ in 0..3000 {
        for level in [6_u8, 5, 4] {
            if shed.admit(level, 10) {
                kept[usize::from(level)] += 1;
            }
        }
    }
    // Info and notice stop at 1,000 lines kept, warnings go on to 2,000.
    assert!(kept[4] > kept[5] + kept[6], "{kept:?}");
    assert_eq!(kept[4] + kept[5] + kept[6], 2000);
    let said = shed.last(10).unwrap();
    assert_eq!(said.level(), 4, "said at the most severe shed");
}

#[test]
fn a_line_says_its_level() {
    assert_eq!(level_of(b"<3>disk gone"), Some(3));
    assert_eq!(
        level_of(b"2026-10-24T14:19:01Z DEBUG present_pace: frame 9"),
        Some(7)
    );
    assert_eq!(level_of(b"[2026-10-24 INFO yserver] up"), Some(6));
    assert_eq!(level_of(b" WARN yserver: late"), Some(4));
    assert_eq!(level_of(b"ERROR: no card"), Some(3));
    assert_eq!(level_of(b"an ordinary line"), None);
    assert_eq!(level_of(b"DEBUGGER attached"), None);
}
