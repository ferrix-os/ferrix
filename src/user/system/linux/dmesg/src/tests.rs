//! Host tests: the records the kernel writes, read back and shown as
//! util-linux shows them.

use ferrix_linux_abi::kmsg;

use crate::args::{self, Action, Follow, Options, Time};
use crate::record::{self, Record};
use crate::show::{self, Clock, Human};

/// A machine that booted at 2026-10-23 12:00:00.250000 UTC.
struct Fixed;

impl Clock for Fixed {
    fn boot(&self) -> (i64, u32) {
        (1_792_756_800, 250_000)
    }
}

fn kmsg_record(priority: u8, seq: u64, usec: u64, text: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; kmsg::MESSAGE_MAX];
    let len = kmsg::devkmsg_record(priority, seq, usec, text, &mut buf).unwrap();
    buf[..len].to_vec()
}

fn syslog_lines(lines: &[(u8, u64, &[u8])]) -> Vec<u8> {
    let mut all = Vec::new();
    for &(priority, usec, text) in lines {
        let mut buf = [0u8; kmsg::MESSAGE_MAX];
        let len = kmsg::syslog_line(priority, usec, text, &mut buf).unwrap();
        all.extend_from_slice(&buf[..len]);
    }
    all
}

fn shown(records: &[Record], options: &Options) -> String {
    let mut human = Human::default();
    let mut out = Vec::new();
    for record in records
        .iter()
        .filter(|record| options.keeps(record.priority))
    {
        show::line(record, options, &Fixed, &mut human, &mut out);
    }
    String::from_utf8(out).unwrap()
}

fn parse(args: &[&str]) -> Result<Options, String> {
    args::parse(args.iter().map(|arg| (*arg).to_owned()))
}

#[test]
fn a_kmsg_record_reads_back_as_the_kernel_wrote_it() {
    let text = b"tab\there, a backslash \\ and \x1b[1mbold\x1b[0m";
    let read = record::parse_kmsg(&kmsg_record(14, 42, 12_345_678, text)).unwrap();
    assert_eq!(read.priority, Some(14));
    assert_eq!(read.seq, Some(42));
    assert_eq!(read.usec, Some(12_345_678));
    assert_eq!(read.text, text);
    assert_eq!((read.facility(), read.level()), (Some(1), Some(6)));
}

#[test]
fn a_kmsg_record_with_dictionary_lines_and_fields_after_the_flag_reads() {
    let read =
        record::parse_kmsg(b"6,7,100,-,caller=T1;hello\n SUBSYSTEM=pci\n DEVICE=+pci:0\n").unwrap();
    assert_eq!(read.text, b"hello");
    assert_eq!(record::parse_kmsg(b"no header at all\n"), None);
    assert_eq!(record::parse_kmsg(b"x,1,2,-;text\n"), None);
}

#[test]
fn syslog_lines_read_back_with_and_without_their_parts() {
    let buffer = syslog_lines(&[(6, 0, b"Booting"), (14, 1_500_000, b"login: ok")]);
    let read = record::parse_syslog(&buffer);
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].priority, Some(6));
    assert_eq!(read[0].usec, Some(0));
    assert_eq!(read[0].text, b"Booting");
    assert_eq!(read[1].usec, Some(1_500_000));
    assert_eq!(read[1].text, b"login: ok");
    // A buffer from a kernel without times or levels: text all the same.
    let bare = record::parse_syslog(b"plain line\n<3>no time\n");
    assert_eq!(bare[0].priority, None);
    assert_eq!(bare[0].text, b"plain line");
    assert_eq!(bare[1].priority, Some(3));
    assert_eq!(bare[1].usec, None);
    assert_eq!(bare[1].text, b"no time");
}

/// What util-linux 2.41's `dmesg` prints for these records, with and
/// without `-x`, `-r`, `-t`, `-T` and `-H`.
#[test]
fn the_output_is_util_linuxs() {
    let records = [
        record::parse_kmsg(&kmsg_record(6, 0, 0, b"Booting Ferrix")).unwrap(),
        record::parse_kmsg(&kmsg_record(14, 1, 12_345_678, b"hyprix: started")).unwrap(),
        record::parse_kmsg(&kmsg_record(3, 2, 12_400_000, b"disk: error")).unwrap(),
    ];
    let default = Options::default();
    assert_eq!(
        shown(&records, &default),
        "[    0.000000] Booting Ferrix\n\
         [   12.345678] hyprix: started\n\
         [   12.400000] disk: error\n"
    );
    let decode = parse(&["-x"]).unwrap();
    assert_eq!(
        shown(&records, &decode),
        "kern  :info  : [    0.000000] Booting Ferrix\n\
         user  :info  : [   12.345678] hyprix: started\n\
         kern  :err   : [   12.400000] disk: error\n"
    );
    let raw = parse(&["-r"]).unwrap();
    assert_eq!(
        shown(&records[..2], &raw),
        "<6>[    0.000000] Booting Ferrix\n<14>[   12.345678] hyprix: started\n"
    );
    let notime = parse(&["--notime"]).unwrap();
    assert_eq!(shown(&records[..1], &notime), "Booting Ferrix\n");
    let ctime = parse(&["-T"]).unwrap();
    assert_eq!(
        shown(&records, &ctime),
        "[Fri Oct 23 12:00:00 2026] Booting Ferrix\n\
         [Fri Oct 23 12:00:12 2026] hyprix: started\n\
         [Fri Oct 23 12:00:12 2026] disk: error\n"
    );
    let human = parse(&["-H"]).unwrap();
    assert_eq!(
        shown(&records, &human),
        "[Oct23 12:00] Booting Ferrix\n\
         [  +12.345678] hyprix: started\n\
         [   +0.054322] disk: error\n"
    );
}

#[test]
fn control_bytes_are_shown_escaped_and_utf8_as_it_is() {
    let record = Record {
        priority: Some(6),
        usec: Some(1),
        seq: None,
        text: "größe\u{7}\x1b[0m\t|"
            .as_bytes()
            .iter()
            .copied()
            .chain([0xff])
            .collect(),
    };
    assert_eq!(
        shown(&[record], &parse(&["-t"]).unwrap()),
        "größe\\x07\\x1b[0m\t|\\xff\n"
    );
}

#[test]
fn levels_and_facilities_filter_as_util_linux_names_them() {
    let records: Vec<Record> = [
        (6u32, "kern info"),
        (14, "user info"),
        (3, "kern err"),
        (30, "daemon info"),
    ]
    .iter()
    .map(|&(priority, text)| Record {
        priority: Some(priority),
        usec: Some(0),
        seq: None,
        text: text.as_bytes().to_vec(),
    })
    .collect();
    let texts = |args: &[&str]| -> Vec<String> {
        shown(&records, &parse(args).unwrap())
            .lines()
            .map(|line| line.trim_start_matches("[    0.000000] ").to_owned())
            .collect()
    };
    assert_eq!(texts(&["-k"]), ["kern info", "kern err"]);
    assert_eq!(texts(&["-u"]), ["user info", "daemon info"]);
    assert_eq!(texts(&["-l", "err"]), ["kern err"]);
    assert_eq!(texts(&["--level=err,info"]).len(), 4);
    assert_eq!(texts(&["-l", "warn+"]), ["kern err"]);
    assert_eq!(texts(&["-l", "+notice", "-f", "daemon"]), ["daemon info"]);
    assert_eq!(texts(&["-f", "kern,user", "-l3"]), ["kern err"]);
    assert!(parse(&["-l", "loud"]).is_err());
    assert!(parse(&["-f", "nowhere"]).is_err());
}

#[test]
fn the_command_line_takes_util_linuxs_flags() {
    assert_eq!(parse(&[]).unwrap(), Options::default());
    assert_eq!(
        parse(&["-c"]).unwrap().action,
        Action::Print { clear: true }
    );
    assert_eq!(parse(&["--clear"]).unwrap().action, Action::Clear);
    assert_eq!(parse(&["-n", "1"]).unwrap().action, Action::ConsoleLevel(2));
    assert_eq!(parse(&["-nerr"]).unwrap().action, Action::ConsoleLevel(4));
    assert_eq!(parse(&["-w"]).unwrap().follow, Follow::All);
    assert_eq!(parse(&["--follow-new"]).unwrap().follow, Follow::New);
    let many = parse(&["-xTr"]).unwrap();
    assert!(many.decode && many.raw);
    assert_eq!(many.time, Time::Ctime);
    assert!(parse(&["-S", "-w"]).is_err());
    assert!(parse(&["-s", "big"]).is_err());
    assert!(parse(&["-s", "65536"]).is_ok());
    assert!(parse(&["--no-such-flag"]).is_err());
    assert!(parse(&["stray"]).is_err());
    assert_eq!(parse(&["-h"]).unwrap().action, Action::Help);
    assert_eq!(parse(&["--version"]).unwrap().action, Action::Version);
}

#[test]
fn records_the_kernel_dropped_are_said_before_the_next() {
    let mut dropped = record::Dropped::default();
    let at = |seq: u64| record::parse_kmsg(&kmsg_record(6, seq, seq * 10, b"x")).unwrap();
    assert_eq!(dropped.before(&at(0)), None);
    assert_eq!(dropped.before(&at(1)), None);
    dropped.lost();
    let note = dropped.before(&at(636)).unwrap();
    assert_eq!(note.text, b"** 634 printk messages dropped **");
    assert_eq!((note.priority, note.usec), (Some(4), Some(6360)));
    assert_eq!(
        shown(&[note], &Options::default()),
        "[    0.006360] ** 634 printk messages dropped **\n"
    );
    // Told before the first record read: every number below it.
    let mut fresh = record::Dropped::default();
    fresh.lost();
    assert_eq!(
        fresh.before(&at(800)).unwrap().text,
        b"** 800 printk messages dropped **"
    );
    // Not told, no note, whatever the numbers.
    assert_eq!(fresh.before(&at(900)), None);
}

#[test]
fn a_dropped_note_from_syslog_reads_as_a_line() {
    let read = record::parse_syslog(b"<4>[   20.000001] ** 12 printk messages dropped **\n");
    assert_eq!(read[0].text, b"** 12 printk messages dropped **");
    assert_eq!(read[0].level(), Some(4));
}

#[test]
fn wall_clock_times_are_utc_dates_as_strftime_writes_them() {
    let at = |boot: (i64, u32)| {
        struct At((i64, u32));
        impl Clock for At {
            fn boot(&self) -> (i64, u32) {
                self.0
            }
        }
        let record = Record {
            priority: Some(6),
            usec: Some(0),
            seq: None,
            text: b"x".to_vec(),
        };
        let mut out = Vec::new();
        show::line(
            &record,
            &parse(&["-T"]).unwrap(),
            &At(boot),
            &mut Human::default(),
            &mut out,
        );
        String::from_utf8(out).unwrap()
    };
    assert_eq!(at((0, 0)), "[Thu Jan  1 00:00:00 1970] x\n");
    // 2024-02-29, a leap day, and the last second of a year.
    assert_eq!(at((1_709_164_800, 0)), "[Thu Feb 29 00:00:00 2024] x\n");
    assert_eq!(at((1_767_225_599, 0)), "[Wed Dec 31 23:59:59 2025] x\n");
}

#[test]
fn uptime_reads_as_microseconds() {
    assert_eq!(
        show::uptime_micros("12345.67 98765.43\n"),
        Some(12_345_670_000)
    );
    assert_eq!(show::uptime_micros("3\n"), Some(3_000_000));
    assert_eq!(show::uptime_micros("x"), None);
}
