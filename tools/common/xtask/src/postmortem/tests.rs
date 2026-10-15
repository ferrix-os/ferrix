//! The judgements `test-postmortem` makes, on transcripts made up here.

use super::*;

/// A region of boot `sequence` holding `text`.
fn region(sequence: u64, text: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; pstore::LEN as usize];
    bytes[..HEADER_BYTES].copy_from_slice(&pstore::encode(pstore::CAPACITY, sequence));
    bytes[pstore::WRITTEN_OFFSET..pstore::WRITTEN_OFFSET + 8]
        .copy_from_slice(&(text.len() as u64).to_le_bytes());
    let ring = pstore::HEADER as usize;
    bytes[ring..ring + text.len()].copy_from_slice(text);
    bytes
}

fn lines(text: &[&str]) -> Vec<String> {
    text.iter().map(|line| (*line).to_owned()).collect()
}

/// Boot 1 as the serial port shows it: the loader, then the kernel with two
/// unlogged lines.
fn serial() -> Vec<String> {
    lines(&[
        "Ferrix loader 0.1",
        "  pstore   at 0x1000000, 1 MiB: no previous kernel log (nothing there)",
        "",
        "Ferrix 0.1.0 on x86_64",
        "  kernel   0xffffffff80000000 -> 0x200000, 4096 KiB",
        "  pstore   mirroring the log to 0x1000000, 1024 KiB, boot 1; 40 bytes so far",
        "",
        "FERRIX-PANIC init exited",
        "  kaslr     slide 0x1000",
        "  see       src/kernel/src/power.rs",
        "  pstore    written back for the next boot",
    ])
}

/// What boot 1's kernel logged of that.
const LOGGED: &[u8] = b"\nFerrix 0.1.0 on x86_64\n  pstore   mirroring the log to 0x1000000, 1024 KiB, boot 1; 40 bytes so far\n\nFERRIX-PANIC init exited\n  see       src/kernel/src/power.rs\n";

#[test]
fn the_log_less_its_unlogged_lines_passes() {
    assert_eq!(
        region_problem(&serial(), &region(1, LOGGED)),
        None,
        "boot 1's log"
    );
}

#[test]
fn an_unlogged_line_in_the_region_fails() {
    let mirrored = b"\nFerrix 0.1.0 on x86_64\n  kaslr     slide 0x1000\n";
    let problem = region_problem(&serial(), &region(1, mirrored)).unwrap();
    assert!(problem.contains("unlogged line"), "{problem}");
}

#[test]
fn a_missing_logged_line_fails() {
    let short = b"\nFerrix 0.1.0 on x86_64\n\nFERRIX-PANIC init exited\n";
    let problem = region_problem(&serial(), &region(1, short)).unwrap();
    assert!(problem.contains("missing from the region"), "{problem}");
}

#[test]
fn a_line_the_serial_log_lacks_fails() {
    let mut extra = LOGGED.to_vec();
    extra.extend_from_slice(b"something else\n");
    let problem = region_problem(&serial(), &region(1, &extra)).unwrap();
    assert!(
        problem.contains("is not in boot 1's serial log"),
        "{problem}"
    );
}

#[test]
fn another_boot_or_a_bad_header_fails() {
    let problem = region_problem(&serial(), &region(2, LOGGED)).unwrap();
    assert!(problem.contains("boot 2's"), "{problem}");
    let mut damaged = region(1, LOGGED);
    damaged[24] ^= 1;
    let problem = region_problem(&serial(), &damaged).unwrap();
    assert!(problem.contains("refused"), "{problem}");
}

#[test]
fn the_address_comes_from_the_mirror_line() {
    assert_eq!(region_address(&serial()).unwrap(), 0x100_0000, "parsed");
    assert!(region_address(&lines(&["nothing"])).is_err(), "absent");
}

#[test]
fn a_quoted_panic_is_rendered_as_the_loader_renders_it() {
    assert_eq!(
        rendered("init exited, FERRIX-BOOT-OK"),
        "init exited, FERRIX_BOOT-OK",
        "defused"
    );
    assert_eq!(rendered(&"x".repeat(200)).len(), SHOWN, "cut");
    assert_eq!(rendered("a\u{e9}b"), "a??b", "two bytes, two marks");
}

#[test]
fn the_switch_must_take_no_region() {
    let good = lines(&[
        "  pstore   off (ferrix.pstore=off): no region this boot",
        "  pstore   none (the loader gave no region)",
    ]);
    assert_eq!(off_problem(&good), None, "off on both sides");
    let ignored = lines(&[
        "  pstore   boot 1's kernel log goes to 0x1000000",
        "  pstore   mirroring the log to 0x1000000, 1024 KiB, boot 1; 9 bytes so far",
    ]);
    assert!(off_problem(&ignored).is_some(), "the option ignored");
}

#[test]
fn the_write_back_comes_straight_after_the_report() {
    assert_eq!(write_back_problem(&serial()), None, "after the see line");
    let mut early = serial();
    let back = early.pop().unwrap();
    early.insert(7, back);
    assert!(
        write_back_problem(&early).is_some(),
        "before the explanation"
    );
    let mut never = serial();
    let _ = never.pop();
    assert!(write_back_problem(&never).is_some(), "never");
}

#[test]
fn a_region_that_starts_nowhere_in_the_serial_log_fails() {
    let elsewhere = b"not a kernel line\n";
    let problem = region_problem(&serial(), &region(1, elsewhere)).unwrap();
    assert!(problem.contains("does not start with"), "{problem}");
}

#[test]
fn a_region_that_stops_early_fails() {
    let early = b"\nFerrix 0.1.0 on x86_64\n";
    let problem = region_problem(&serial(), &region(1, early)).unwrap();
    assert!(
        problem.contains("missing from the region's end"),
        "{problem}"
    );
}

#[test]
fn a_region_without_the_log_so_far_fails() {
    let late = b"  pstore   mirroring the log to 0x1000000, 1024 KiB, boot 1; 40 bytes so far\n\nFERRIX-PANIC init exited\n  see       src/kernel/src/power.rs\n";
    let problem = region_problem(&serial(), &region(1, late)).unwrap();
    assert!(problem.contains("does not start with"), "{problem}");
}
