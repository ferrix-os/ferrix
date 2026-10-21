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

#[test]
fn a_reset_is_judged_by_its_wait_its_line_and_the_quote() {
    let boot_1: Vec<String> = [
        "FERRIX-PANIC init exited, and ferrix.onexit=panic asks for a panic",
        STOPPED_RESETS,
        RESET_LINE,
    ]
    .iter()
    .map(|line| (*line).to_owned())
    .collect();
    let after = vec![
        "  pstore   boot 1's last report: panicked: init exited, and ferrix.onexit=panic asks for a panic"
            .to_owned(),
    ];
    let enough = Some(Duration::from_secs(11));
    assert_eq!(reset_problem(&boot_1, &after, enough), None, "a good reset");
    assert!(
        reset_problem(&boot_1, &after, Some(Duration::from_secs(2)))
            .is_some_and(|why| why.starts_with("reset sooner than 10 s")),
        "too soon"
    );
    let halted: Vec<String> = boot_1
        .iter()
        .filter(|line| !line.contains(STOPPED_RESETS))
        .cloned()
        .collect();
    assert!(
        reset_problem(&halted, &after, enough).is_some(),
        "no stopped line"
    );
    assert!(reset_problem(&boot_1, &[], enough).is_some(), "no quote");
}

#[test]
fn a_misspelt_value_must_be_said_and_halt() {
    let good = lines(&[MISSPELT, STOPPED_HALTS]);
    assert_eq!(misspelt_problem(&good), None, "said and halted");
    assert!(
        misspelt_problem(&lines(&[STOPPED_HALTS])).is_some(),
        "not said"
    );
    assert!(
        misspelt_problem(&lines(&[MISSPELT, STOPPED_RESETS, RESET_LINE])).is_some(),
        "reset"
    );
}

/// Boot 1 of a stop case, and boot 2 as its loader and kernel say it.
fn stop_boots(stopping: &str, ended: &str) -> (Vec<String>, Vec<String>) {
    let mut boot_1 = serial();
    boot_1.push(stopping.to_owned());
    let after = lines(&[
        "Ferrix loader 0.1",
        "  pstore   at 0x1000000, 1 MiB: boot 1's kernel log, 900 bytes, 900 kept (unchecked)",
        ended,
        "  pstore   mirroring the log to 0x1000000, 1024 KiB, boot 2; 40 bytes so far",
    ]);
    (boot_1, after)
}

#[test]
fn a_stop_is_judged_by_its_line_and_the_loaders() {
    let restart = &CASES[0];
    let (boot_1, after) = stop_boots(
        "reboot: Restarting system",
        "  pstore   boot 1 ended: a restart a program asked for (reboot(2))",
    );
    assert_eq!(stop_problem(restart, &boot_1, &after), None, "a restart");
    let (_, none) = stop_boots(
        "reboot: Restarting system",
        "  pstore   boot 1 ended: no stop recorded: a hang, a watchdog or a reset from outside the kernel",
    );
    assert!(
        stop_problem(restart, &boot_1, &none).is_some_and(|why| why.contains("it said")),
        "no word where a restart was wanted"
    );
    let quiet: Vec<String> = boot_1
        .iter()
        .filter(|line| !line.starts_with("reboot:"))
        .cloned()
        .collect();
    assert!(
        stop_problem(restart, &quiet, &after).is_some(),
        "boot 1 never said it restarts"
    );
    let unattached: Vec<String> = after
        .iter()
        .filter(|line| !line.contains("boot 2;"))
        .cloned()
        .collect();
    assert!(
        stop_problem(restart, &boot_1, &unattached).is_some(),
        "boot 2 not mirrored"
    );
    assert!(
        stop_problem(restart, &boot_1, &[]).is_some(),
        "no second loader"
    );
}

#[test]
fn every_case_names_a_different_stop_and_the_hang_none() {
    for (index, case) in CASES.iter().enumerate() {
        for other in CASES.iter().skip(index + 1) {
            assert!(
                case.ended != other.ended || case.stopping != other.stopping,
                "{} and {} are the same case",
                case.name,
                other.name
            );
        }
    }
    for stop in Stop::ALL.into_iter().filter(|stop| *stop != Stop::Panic) {
        assert!(
            CASES.iter().any(|case| case.ended == Ended::Stopped(stop)),
            "{stop:?} has a case"
        );
    }
    assert!(
        CASES
            .iter()
            .any(|case| case.ended == Ended::NoneRecorded && case.how == How::Reset),
        "the reset from outside"
    );
}

#[test]
fn the_record_must_say_a_panic() {
    let mut panicked = region(1, LOGGED);
    panicked[pstore::STOP_OFFSET..pstore::STOP_OFFSET + 8]
        .copy_from_slice(&Stop::Panic.word().to_le_bytes());
    let said = lines(&["  pstore   boot 1 ended: a panic"]);
    assert_eq!(
        panic_word_problem(&panicked, &said),
        None,
        "the word and the line"
    );
    assert!(
        panic_word_problem(&region(1, LOGGED), &said).is_some(),
        "no word in the region"
    );
    assert!(panic_word_problem(&panicked, &[]).is_some(), "no line");
}
