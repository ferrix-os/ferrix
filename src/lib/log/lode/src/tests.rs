extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::console;
use crate::crc::crc32c;
use crate::cursor;
use crate::filter::{self, Expr, Op, Test, glob};
use crate::format::{Output, write};
use crate::ingest::{self, KernelSequence, Now, Taken};
use crate::kmsg;
use crate::record::{Record, level, valid_name};
use crate::segment::{self, Header};
use crate::time::{self, Civil, Zone};

fn header() -> Header {
    Header {
        boot_id: [0xab; 16],
        machine_id: [0; 16],
        first_seqnum: 1,
        created_us: 1_700_000_000_000_000,
        host: String::from("box"),
    }
}

fn record(seqnum: u64, message: &str) -> Record {
    let mut record = Record::new(6, 1_700_000_000_000_000 + seqnum, seqnum);
    record.seqnum = seqnum;
    record.push("MESSAGE", message.as_bytes());
    record.push("SYSLOG_IDENTIFIER", b"test");
    record
}

fn segment_with(records: &[Record]) -> Vec<u8> {
    let mut bytes = header().encode();
    for each in records {
        segment::encode_record(each, &mut bytes);
    }
    bytes
}

#[test]
fn crc32c_check_value() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    assert_eq!(crc32c(b""), 0);
}

#[test]
fn a_segment_reads_back_what_was_written() {
    let records = [
        record(1, "one"),
        record(2, "two\nlines"),
        record(3, "three"),
    ];
    let bytes = segment_with(&records);
    let scanned = segment::scan(&bytes, 0).unwrap();
    assert_eq!(scanned.header, header());
    assert_eq!(scanned.good, bytes.len());
    assert_eq!(scanned.records.len(), 3);
    for (found, wrote) in scanned.records.iter().zip(&records) {
        assert_eq!(found.record.fields, wrote.fields);
        assert_eq!(found.record.seqnum, wrote.seqnum);
        assert_eq!(found.record.boot_id, [0xab; 16]);
        assert_eq!(&*found.record.host, "box");
    }
}

#[test]
fn a_torn_tail_stops_the_scan_at_the_last_whole_entry() {
    let whole = segment_with(&[record(1, "one"), record(2, "two")]);
    let good = segment::scan(&whole, 0).unwrap().records[1].offset;
    for cut in good..whole.len() {
        let scanned = segment::scan(&whole[..cut], 0).unwrap();
        assert_eq!(scanned.records.len(), 1, "cut at {cut}");
        assert_eq!(scanned.good, good);
    }
}

#[test]
fn a_flipped_bit_ends_the_scan_there() {
    let mut bytes = segment_with(&[record(1, "one"), record(2, "two"), record(3, "three")]);
    let second = segment::scan(&bytes, 0).unwrap().records[1].offset;
    bytes[second + 30] ^= 1;
    let scanned = segment::scan(&bytes, 0).unwrap();
    assert_eq!(scanned.records.len(), 1);
    assert_eq!(scanned.good, second);
}

#[test]
fn a_sequence_that_goes_back_ends_the_scan() {
    let bytes = segment_with(&[record(5, "five"), record(4, "four")]);
    assert_eq!(segment::scan(&bytes, 0).unwrap().records.len(), 1);
}

#[test]
fn a_scan_goes_on_from_an_offset() {
    let bytes = segment_with(&[record(1, "one"), record(2, "two")]);
    let first = segment::scan(&bytes, 0).unwrap();
    let from = first.records[1].offset;
    let rest = segment::scan(&bytes, from).unwrap();
    assert_eq!(rest.records.len(), 1);
    assert_eq!(rest.records[0].record.seqnum, 2);
}

#[test]
fn garbage_is_no_segment() {
    assert!(segment::scan(b"not a segment at all, nothing here", 0).is_none());
    assert!(segment::scan(&[], 0).is_none());
}

#[test]
fn segment_names_sort_and_parse() {
    let name = segment::file_name("system", &[1; 16], 0x2a);
    assert!(segment::is_segment_name(&name));
    assert_eq!(segment::class_of_name(&name), Some("system"));
    assert!(!segment::is_segment_name(".lode-probe"));
    assert_eq!(segment::seqnum_of_name(&name), Some(0x2a));
    assert_eq!(
        segment::parse_id("01010101-0101-0101-0101-010101010101"),
        Some([1; 16])
    );
}

#[test]
fn field_names_are_journalds() {
    assert!(valid_name("MESSAGE"));
    assert!(valid_name("_PID"));
    assert!(!valid_name("message"));
    assert!(!valid_name("1ABC"));
    assert!(!valid_name("__CURSOR"));
    assert!(!valid_name(""));
}

#[test]
fn a_sender_cannot_write_a_trusted_field() {
    let mut record = Record::new(6, 0, 0);
    record.push_untrusted("_PID", b"1");
    record.push_untrusted("MESSAGE", b"hello");
    assert_eq!(record.get("_PID"), None);
    assert_eq!(record.message(), b"hello");
}

#[test]
fn a_long_message_is_cut_and_says_so() {
    let mut record = Record::new(6, 0, 0);
    record.push("MESSAGE", &vec![b'x'; crate::record::MESSAGE_MAX + 10]);
    assert_eq!(record.message().len(), crate::record::MESSAGE_MAX);
    assert_eq!(record.get("_TRUNCATED"), Some(&b"1"[..]));
}

#[test]
fn levels_by_name_and_number() {
    assert_eq!(level("err"), Some(3));
    assert_eq!(level("warn"), Some(4));
    assert_eq!(level("7"), Some(7));
    assert_eq!(level("8"), None);
    assert_eq!(level("loud"), None);
}

#[test]
fn kmsg_records_parse_as_the_kernel_writes_them() {
    let mut buf = [0_u8; 256];
    let length =
        ferrix_linux_abi_kmsg_record(14, 42, 12_345_678, b"init     hi\\there\x01", &mut buf);
    let parsed = kmsg::parse(&buf[..length]).unwrap();
    assert_eq!(parsed.facility, 1);
    assert_eq!(parsed.level, 6);
    assert_eq!(parsed.seq, 42);
    assert_eq!(parsed.usec, 12_345_678);
    assert_eq!(parsed.text, b"init     hi\\there\x01");
}

/// What the kernel's `devkmsg_record` writes, spelled out here so the test
/// does not depend on the kernel's crate: `prio,seq,usec,-;` and the text
/// with every byte outside printable ASCII, and the backslash, as `\xNN`.
fn ferrix_linux_abi_kmsg_record(
    priority: u8,
    seq: u64,
    usec: u64,
    text: &[u8],
    buf: &mut [u8],
) -> usize {
    let mut out: Vec<u8> = alloc::format!("{priority},{seq},{usec},-;").into_bytes();
    for &byte in text {
        if byte == b'\\' || !(0x20..0x7f).contains(&byte) {
            out.extend_from_slice(alloc::format!("\\x{byte:02x}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    out.push(b'\n');
    buf[..out.len()].copy_from_slice(&out);
    out.len()
}

#[test]
fn a_kmsg_record_without_its_prefix_is_none() {
    assert!(kmsg::parse(b"no prefix here").is_none());
    assert!(kmsg::parse(b"6,x,1,-;text").is_none());
}

#[test]
fn inits_unit_lines_name_the_unit() {
    let said = console::read(b"  init     hyprix.service[812]: frame took 9 ms");
    assert_eq!(said.identifier.as_deref(), Some("hyprix"));
    assert_eq!(said.unit.as_deref(), Some("hyprix.service"));
    assert_eq!(said.pid, Some(812));
    assert_eq!(said.message, b"frame took 9 ms");
}

#[test]
fn inits_own_lines_and_kernel_subsystems() {
    let said = console::read(b"  init     / is the root volume now");
    assert_eq!(said.identifier.as_deref(), Some("init"));
    assert_eq!(said.pid, Some(1));
    assert_eq!(said.unit, None);
    let said = console::read(b"  devmgr   started by pid 1: 3 drivers");
    assert_eq!(said.subsystem.as_deref(), Some("devmgr"));
    assert_eq!(said.message, b"devmgr   started by pid 1: 3 drivers");
}

#[test]
fn a_programs_name_colon_line() {
    let said = console::read(b"intel-gt: worker 1 is pid 6");
    assert_eq!(said.identifier.as_deref(), Some("intel-gt"));
    assert_eq!(said.message, b"worker 1 is pid 6");
    let said = console::read(b"just some words");
    assert_eq!(said.identifier, None);
}

#[test]
fn a_mark_is_a_field() {
    let said = console::read(b"lode-mark: box test: worker kill");
    assert_eq!(said.mark.as_deref(), Some(&b"box test: worker kill"[..]));
}

#[test]
fn kernel_records_become_stored_records() {
    let now = Now {
        realtime_us: 1_700_000_100_000_000,
        monotonic_us: 100_000_000,
    };
    let kernel = kmsg::KernelRecord {
        facility: 1,
        level: 6,
        seq: 7,
        usec: 40_000_000,
        text: Vec::from(&b"  init     sshd.service[90]: listening"[..]),
    };
    let record = ingest::from_kernel(&kernel, now);
    assert_eq!(record.realtime_us, 1_700_000_040_000_000);
    assert_eq!(record.get_str("UNIT"), Some("sshd.service"));
    assert_eq!(record.get_str("_KERNEL_SEQNUM"), Some("7"));
    assert_eq!(record.get_str("_TRANSPORT"), Some("console"));
    let kernel = kmsg::KernelRecord {
        facility: 0,
        ..kernel
    };
    let record = ingest::from_kernel(&kernel, now);
    assert_eq!(record.get_str("SYSLOG_IDENTIFIER"), Some("kernel"));
    assert_eq!(record.get_str("_TRANSPORT"), Some("kernel"));
}

#[test]
fn the_kernel_sequence_skips_what_was_stored_and_counts_what_was_lost() {
    let mut sequence = KernelSequence { last: Some(10) };
    assert_eq!(sequence.take(9), Taken::Seen);
    assert_eq!(sequence.take(10), Taken::Seen);
    assert_eq!(sequence.take(11), Taken::Next);
    assert_eq!(sequence.take(15), Taken::After { lost: 3 });
    let mut fresh = KernelSequence::default();
    assert_eq!(fresh.take(0), Taken::Next);
    let mut late = KernelSequence::default();
    assert_eq!(late.take(5), Taken::After { lost: 5 });
}

#[test]
fn kernel_gaps_tell_said_from_unsaid() {
    let mut records: Vec<Record> = Vec::new();
    for seq in [1_u64, 2, 3, 7, 8, 12] {
        let mut record = Record::new(6, 0, 0);
        record.push("_KERNEL_SEQNUM", alloc::format!("{seq}").as_bytes());
        records.push(record);
    }
    let lost = ingest::lost_record(3, 7, false, &records[3]);
    records.push(lost);
    let refs: Vec<&Record> = records.iter().collect();
    let gaps = ingest::kernel_gaps(&refs);
    assert_eq!(gaps.held, 6);
    assert_eq!(gaps.said_lost, 3);
    assert_eq!(gaps.unsaid, vec![(9, 12)]);
}

#[test]
fn globs() {
    assert!(glob(b"*", b""));
    assert!(glob(b"hyprix*", b"hyprix.service"));
    assert!(glob(b"*.service", b"a.service"));
    assert!(!glob(b"*.service", b"a.socket"));
    assert!(glob(b"a?c", b"abc"));
    assert!(glob(b"[a-c]x", b"bx"));
    assert!(!glob(b"[!a-c]x", b"bx"));
    assert!(glob(b"a*b*c", b"aXXbYYc"));
    assert!(!glob(b"a*b*c", b"aXXbYY"));
    assert!(filter::contains(b"worker", b"intel-gt: worker 1"));
}

fn unit_record(unit: &str, priority: u8, message: &str) -> Record {
    let mut record = Record::new(priority, 0, 0);
    record.push("UNIT", unit.as_bytes());
    record.push("MESSAGE", message.as_bytes());
    record
}

#[test]
fn the_filter_language_has_not_or_and_parentheses() {
    let expr = filter::parse(r#"unit = "hyprix*" and not priority > warning"#).unwrap();
    assert!(expr.matches(&unit_record("hyprix.service", 3, "x")));
    assert!(!expr.matches(&unit_record("hyprix.service", 6, "x")));
    assert!(!expr.matches(&unit_record("sshd.service", 3, "x")));
    let expr =
        filter::parse(r#"(unit = sshd.service or unit = "hyprix*") and message ~ took"#).unwrap();
    assert!(expr.matches(&unit_record("sshd.service", 6, "it took long")));
    assert!(!expr.matches(&unit_record("sshd.service", 6, "fast")));
    let expr = filter::parse(r#"! unit = "noisy*""#).unwrap();
    assert!(expr.matches(&unit_record("quiet.service", 6, "")));
    assert!(!expr.matches(&unit_record("noisy.service", 6, "")));
}

#[test]
fn a_missing_field_is_unequal() {
    let record = Record::new(6, 0, 0);
    assert!(Expr::Test(Test::field("UNIT", Op::Ne, b"x")).matches(&record));
    assert!(!Expr::Test(Test::field("UNIT", Op::Eq, b"*")).matches(&record));
}

#[test]
fn bad_expressions_say_where() {
    assert!(filter::parse("unit =").is_err());
    assert!(filter::parse("(unit = a").is_err());
    assert!(filter::parse(r#"unit = "open"#).is_err());
    assert!(filter::parse("unit = a b").is_err());
}

#[test]
fn civil_dates() {
    assert_eq!(time::civil(0), (1970, 1, 1));
    assert_eq!(time::civil(19_723), (2024, 1, 1));
    assert_eq!(time::days(2026, 10, 23), 20_749);
    assert_eq!(time::weekday(time::days(2026, 10, 23)), 5);
    let when = Civil::at(1_761_228_062_123_456, 7200);
    assert_eq!(
        (
            when.year,
            when.month,
            when.day,
            when.hour,
            when.minute,
            when.second
        ),
        (2025, 10, 23, 16, 1, 2)
    );
    assert_eq!(when.offset(), "+02:00");
}

#[test]
fn posix_zones_without_a_database() {
    let berlin = Zone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
    let winter = time::days(2026, 1, 15) as u64 * 86_400 * 1_000_000;
    let summer = time::days(2026, 7, 15) as u64 * 86_400 * 1_000_000;
    assert_eq!(berlin.offset_at(winter), 3600);
    assert_eq!(berlin.offset_at(summer), 7200);
    // The change on 2026-03-29 at 01:00 UTC.
    let change = (time::days(2026, 3, 29) as u64 * 86_400 + 3600) * 1_000_000;
    assert_eq!(berlin.offset_at(change - 1_000_000), 3600);
    assert_eq!(berlin.offset_at(change), 7200);
    assert_eq!(Zone::posix("UTC0").unwrap().offset_at(summer), 0);
    assert_eq!(Zone::posix("<+03>-3").unwrap().offset_at(summer), 10_800);
    let new_york = Zone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap();
    assert_eq!(new_york.offset_at(summer), -4 * 3600);
    assert!(Zone::posix("Europe/Berlin").is_none());
    let footer = b"TZif2.....binary....\nCET-1CEST,M3.5.0,M10.5.0/3\n";
    assert_eq!(
        time::tzif_footer(footer),
        Some("CET-1CEST,M3.5.0,M10.5.0/3")
    );
}

#[test]
fn since_takes_every_form_lode_prints() {
    let now = 1_761_228_062_000_000; // 2025-10-23 14:01:02 UTC
    let utc = Zone::utc();
    let at = |text: &str| time::parse(text, now, &utc);
    assert_eq!(at("now"), Some(now));
    assert_eq!(at("-30s"), Some(now - 30_000_000));
    assert_eq!(at("2 min ago"), Some(now - 120_000_000));
    assert_eq!(at("@1761228062"), Some(now));
    assert_eq!(at("2025-10-23 14:01:02"), Some(now));
    assert_eq!(at("2025-10-23T14:01:02Z"), Some(now));
    assert_eq!(at("2025-10-23 16:01:02.000 +02:00"), Some(now));
    assert_eq!(at("2025-10-23T16:01:02+0200"), Some(now));
    assert_eq!(at("2025-10-23"), Some(now - (14 * 3600 + 62) * 1_000_000));
    assert_eq!(at("today"), at("2025-10-23"));
    assert_eq!(at("14:01:02"), Some(now));
    assert_eq!(at("not a time"), None);
    let berlin = Zone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
    assert_eq!(time::parse("2025-10-23 16:01:02", now, &berlin), Some(now));
}

#[test]
fn formats() {
    let mut record = Record::new(3, 1_761_228_062_123_456, 5_000_001);
    record.seqnum = 9;
    record.host = alloc::rc::Rc::from("box");
    record.push("MESSAGE", b"it \"broke\"");
    record.push("SYSLOG_IDENTIFIER", b"hyprix");
    record.push("SYSLOG_PID", b"812");
    let zone = Zone::utc();
    let show = |output| {
        let mut out = Vec::new();
        write(&record, output, &zone, &mut out);
        String::from_utf8(out).unwrap()
    };
    assert_eq!(
        show(Output::Human),
        "2025-10-23 14:01:02.123 +00:00 E hyprix[812]: it \"broke\"\n"
    );
    assert_eq!(
        show(Output::Short),
        "Oct 23 14:01:02 box hyprix[812]: it \"broke\"\n"
    );
    assert_eq!(
        show(Output::ShortIso),
        "2025-10-23T14:01:02+00:00 box hyprix[812]: it \"broke\"\n"
    );
    assert_eq!(
        show(Output::ShortMonotonic),
        "[    5.000001] box hyprix[812]: it \"broke\"\n"
    );
    assert_eq!(show(Output::Cat), "it \"broke\"\n");
    let json = show(Output::Json);
    assert!(json.starts_with("{\"__CURSOR\":\"i=9;b=0000"), "{json}");
    assert!(json.contains("\"PRIORITY\":\"3\""));
    assert!(json.contains("\"MESSAGE\":\"it \\\"broke\\\"\""));
    assert!(json.ends_with("}\n"));
    let export = show(Output::Export);
    assert!(export.contains("\nMESSAGE=it \"broke\"\n"));
    assert!(export.ends_with("\n\n"));
    assert_eq!(
        show(Output::Verbose),
        "Thu 2025-10-23 14:01:02.123456 +00:00 [i=9;b=00000000000000000000000000000000]\n    \
         PRIORITY=3\n    FERRIX_LEVEL=3\n    _HOSTNAME=box\n    MESSAGE=it \"broke\"\n    \
         SYSLOG_IDENTIFIER=hyprix\n    SYSLOG_PID=812\n"
    );
    assert_eq!(Output::named("verbose"), Some(Output::Verbose));
}

#[test]
fn cursors_round_trip() {
    let mut record = Record::new(6, 0, 0);
    record.seqnum = 0x1234;
    let text = cursor::of(&record);
    assert_eq!(cursor::seqnum(&text), Some(0x1234));
    assert_eq!(cursor::seqnum("garbage"), None);
}

#[test]
fn and_with_all_is_the_other() {
    let test = Expr::Test(Test::field("UNIT", Op::Eq, b"a"));
    assert_eq!(Expr::All.and(test.clone()), test);
}

mod held_back {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::console;
    use crate::ingest::{self, MESSAGE_ID_CRASH};
    use crate::kmsg::KernelRecord;
    use crate::limit::{self, Action, Limiter, WINDOW_US};
    use crate::record::Record;
    use crate::retention::{self, DAY_US, Kept};
    use crate::segment;

    fn line(unit: &str, priority: u8, message: &str, seq: u64, at: u64) -> Record {
        let mut record = Record::new(priority, at, at);
        record.push("MESSAGE", message.as_bytes());
        record.push("UNIT", unit.as_bytes());
        record.push("_TRANSPORT", b"console");
        record.push("_KERNEL_SEQNUM", alloc::format!("{seq}").as_bytes());
        record
    }

    fn messages(records: &[Record]) -> Vec<String> {
        records
            .iter()
            .map(|record| String::from_utf8_lossy(record.message()).into_owned())
            .collect()
    }

    #[test]
    fn a_flood_of_chatter_is_held_and_said_and_errors_pass() {
        let mut limiter = Limiter::new(Vec::new());
        let mut out = Vec::new();
        for n in 0..1500_u64 {
            limiter.admit(
                // Alternating words, so the folder (which reads numbers
                // as one) does not take them for one line repeated.
                line(
                    "noisy.service",
                    6,
                    &alloc::format!("{} {n}", if n % 2 == 0 { "tick" } else { "tock" }),
                    n,
                    n,
                ),
                n,
                &mut out,
            );
        }
        limiter.admit(
            line("noisy.service", 3, "it broke", 1500, 1500),
            1500,
            &mut out,
        );
        limiter.admit(
            line("quiet.service", 6, "hello", 1501, 1501),
            1501,
            &mut out,
        );
        assert_eq!(out.len(), 1002);
        limiter.tick(WINDOW_US + 10, &mut out);
        let last = out.last().unwrap();
        assert_eq!(
            last.get_str("MESSAGE_ID"),
            Some(limit::MESSAGE_ID_SUPPRESSED)
        );
        assert_eq!(last.get_str("N_SUPPRESSED"), Some("500"));
        assert_eq!(last.get_str("HELD_FIRST_SEQNUM"), Some("1000"));
        assert_eq!(last.get_str("HELD_LAST_SEQNUM"), Some("1499"));
        let refs: Vec<&Record> = out.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
    }

    #[test]
    fn ranges_that_meet_cover_a_gap_together() {
        let mut records: Vec<Record> = Vec::new();
        for seq in [10_u64, 30] {
            records.push(line("a", 6, "x", seq, seq));
        }
        for (first, last) in [(11_u64, 19_u64), (20, 29)] {
            let mut said = Record::new(4, 0, 0);
            said.push("HELD_FIRST_SEQNUM", alloc::format!("{first}").as_bytes());
            said.push("HELD_LAST_SEQNUM", alloc::format!("{last}").as_bytes());
            records.push(said);
        }
        let refs: Vec<&Record> = records.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
        let _ = records.pop();
        let refs: Vec<&Record> = records.iter().collect();
        assert_eq!(ingest::kernel_gaps(&refs).unsaid, [(11, 30)]);
    }

    #[test]
    fn repeats_fold_into_one_record() {
        let mut limiter = Limiter::new(Vec::new());
        let mut out = Vec::new();
        for n in 0..5_u64 {
            limiter.admit(
                line("gpu", 6, "EAGAIN: a busy binding", n, n * 10),
                n * 10,
                &mut out,
            );
        }
        limiter.admit(line("gpu", 6, "something else", 5, 60), 60, &mut out);
        assert_eq!(
            messages(&out),
            [
                "EAGAIN: a busy binding",
                "message repeated 4 time(s): EAGAIN: a busy binding",
                "something else"
            ]
        );
        assert_eq!(out[1].get_str("REPEATED"), Some("4"));
        let refs: Vec<&Record> = out.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
    }

    #[test]
    fn a_flush_says_everything_held_at_once() {
        let mut limiter = Limiter::new(limit::parse_rules("drop where unit = gone\n").unwrap());
        let mut out = Vec::new();
        limiter.admit(line("gone", 6, "x", 1, 1), 1, &mut out);
        for n in 2..6_u64 {
            limiter.admit(line("gpu", 6, "same", n, n), n, &mut out);
        }
        limiter.flush(10, &mut out);
        let refs: Vec<&Record> = out.iter().collect();
        assert!(
            ingest::kernel_gaps(&refs).unsaid.is_empty(),
            "{:?}",
            messages(&out)
        );
        assert!(
            out.iter()
                .any(|record| record.get_str("REPEATED") == Some("3"))
        );
        assert!(
            out.iter()
                .any(|record| record.get_str("N_DROPPED") == Some("1"))
        );
    }

    #[test]
    fn the_kernels_own_lines_are_never_held() {
        let mut limiter = Limiter::new(Vec::new());
        let mut out = Vec::new();
        for n in 0..3000_u64 {
            let mut record = Record::new(6, n, n);
            record.push("MESSAGE", b"same");
            record.push("_TRANSPORT", b"kernel");
            limiter.admit(record, n, &mut out);
        }
        assert_eq!(out.len(), 3000);
    }

    #[test]
    fn rules_read_and_act_and_say_so() {
        let rules = limit::parse_rules(
            "# comment\n\
             drop where unit = \"spam*\"\n\
             sample 1/10 where id = sampled\n\
             level warning where unit = loud.service\n\
             fold 60s where id = chrome\n\
             limit 5/10s where subsystem = intelgpu\n",
        )
        .unwrap();
        assert_eq!(rules.len(), 5);
        assert_eq!(rules[1].action, Action::Sample(10));
        assert_eq!(rules[4].action, Action::Limit(5, 10_000_000));
        let mut limiter = Limiter::new(rules);
        let mut out = Vec::new();
        for n in 0..3_u64 {
            limiter.admit(
                line("spam.service", 6, &alloc::format!("x{n}"), n, n),
                n,
                &mut out,
            );
        }
        limiter.admit(line("loud.service", 6, "info", 3, 3), 3, &mut out);
        limiter.admit(line("loud.service", 4, "warn", 4, 4), 4, &mut out);
        assert_eq!(messages(&out), ["warn"]);
        limiter.tick(WINDOW_US + 10, &mut out);
        let said: Vec<String> = messages(&out[1..]);
        assert!(
            said.iter()
                .any(|m| m.contains("drop where unit = \"spam*\"") && m.contains("3 record")),
            "{said:?}"
        );
        assert!(limit::parse_rules("explode where unit = x").is_err());
        assert!(limit::parse_rules("drop").is_err());
        assert_eq!(
            limit::parse_rules("fold 0s where message ~ \"lode flood\"").unwrap()[0].action,
            Action::Fold(0)
        );
        assert!(limit::parse_rules("sample 1/0 where unit = x").is_err());
    }

    #[test]
    fn retention_classes_read_and_assign() {
        let classes = retention::parse(
            "class errors where priority <= err keep 1y max 256M\n\
             class debug where priority = debug ram-only\n\
             class system keep 90d max 1G\n",
        )
        .unwrap();
        assert_eq!(classes.len(), 3);
        assert_eq!(classes[0].keep_us, Some(365 * DAY_US));
        assert_eq!(classes[0].max_bytes, Some(256 << 20));
        assert!(classes[1].ram_only);
        assert_eq!(retention::assign(&classes, &line("a", 2, "", 0, 0)), 0);
        assert_eq!(retention::assign(&classes, &line("a", 7, "", 0, 0)), 1);
        assert_eq!(retention::assign(&classes, &line("a", 6, "", 0, 0)), 2);
        assert_eq!(retention::parse("").unwrap().len(), 2);
        assert!(retention::parse("class Bad").is_err());
        assert!(retention::parse("klass x").is_err());
    }

    fn kept(class: &str, name: &str, day: u64, size: u64, active: bool) -> Kept {
        Kept {
            class: String::from(class),
            name: String::from(name),
            created_us: day * DAY_US,
            size,
            active,
        }
    }

    #[test]
    fn vacuum_by_age_size_and_store() {
        let classes =
            retention::parse("class errors where priority <= err keep 10d\nclass system max 100\n")
                .unwrap();
        let segments = [
            kept("errors", "e1", 1, 10, false),
            kept("errors", "e2", 15, 10, false),
            kept("errors", "e3", 19, 10, true),
            kept("system", "s1", 1, 60, false),
            kept("system", "s2", 2, 60, false),
            kept("system", "s3", 3, 60, true),
        ];
        let removed = retention::vacuum(&classes, &segments, 20 * DAY_US, 1 << 30);
        assert_eq!(removed, ["e1", "s1", "s2"]);
        let removed = retention::vacuum(&[], &segments, 0, 100);
        assert_eq!(removed, ["e1", "s1", "s2"]);
        let removed = retention::vacuum(&[], &segments, 0, 50);
        assert_eq!(removed, ["e1", "s1", "s2", "e2"]);
    }

    #[test]
    fn crash_lines_become_crash_records() {
        let crash = console::crash(
            b"  signal   pid 27 ended by signal 7 at 0x7f00, pc 0x68f150: Trap { kind: Bus }",
        )
        .unwrap();
        assert_eq!(crash.pid, 27);
        assert_eq!(crash.signal, "SIGBUS");
        assert_eq!(crash.pc.as_deref(), Some("0x68f150"));
        assert!(console::crash(b"  signal   ARM and Thumb handlers returned").is_none());
        let kernel = KernelRecord {
            facility: 0,
            level: 6,
            seq: 3,
            usec: 1,
            text: Vec::from(
                &b"  signal   pid 9 has no room for signal 11's frame; ending it with SIGSEGV"[..],
            ),
        };
        let record = ingest::from_kernel(
            &kernel,
            ingest::Now {
                realtime_us: 10,
                monotonic_us: 10,
            },
        );
        assert_eq!(record.priority, 3);
        assert_eq!(record.get_str("MESSAGE_ID"), Some(MESSAGE_ID_CRASH));
        assert_eq!(record.get_str("CRASH_SIGNAL"), Some("SIGSEGV"));
    }

    #[test]
    fn a_rewrite_removes_only_what_matches() {
        let mut bytes = super::header().encode();
        for n in 1..=5_u64 {
            let mut record = Record::new(6, n, n);
            record.seqnum = n;
            record.push("MESSAGE", alloc::format!("m{n}").as_bytes());
            segment::encode_record(&record, &mut bytes);
        }
        bytes.extend_from_slice(&[1, 2, 3]);
        let (out, removed) = segment::rewrite(&bytes, |record| record.message() == b"m3").unwrap();
        assert_eq!(removed, 1);
        let scanned = segment::scan(&out, 0).unwrap();
        assert_eq!(scanned.good, out.len());
        let seqs: Vec<u64> = scanned
            .records
            .iter()
            .map(|found| found.record.seqnum)
            .collect();
        assert_eq!(seqs, [1, 2, 4, 5]);
    }
}

mod sessions {
    use crate::record::Record;
    use crate::session::{self, Said, Session, Sessions};

    fn session() -> Session {
        Session {
            id: 3,
            seat: alloc::string::String::from("seat0"),
            uid: 1000,
            pid: 812,
            scope: alloc::string::String::from("session-3.scope"),
            unit: alloc::string::String::from("hyprix.service"),
            token: [7; 16],
        }
    }

    #[test]
    fn a_session_line_round_trips() {
        let line = session::line(&session());
        assert!(line.starts_with("SESSION 3 seat=seat0 uid=1000 pid=812 "));
        assert_eq!(session::parse(&line), Some(Said::Began(session())));
        assert_eq!(session::parse("END 3"), Some(Said::Ended(3)));
        assert_eq!(session::parse("SESSION x"), None);
        assert_eq!(
            session::parse("SESSION 3 seat=seat0 uid=1000 scope=a;b unit=u"),
            None
        );
        assert_eq!(session::parse("HELLO"), None);
    }

    #[test]
    fn records_of_the_sessions_unit_are_stamped_while_it_lasts() {
        let mut sessions = Sessions::default();
        let mut said = Record::new(5, 0, 0);
        sessions.take(Said::Began(session()), &mut said);
        assert_eq!(said.get_str("SESSION_ID"), Some("3"));
        assert!(
            !said.message().windows(4).any(|w| w == b"0707"),
            "the token is never shown"
        );
        let mut line = Record::new(6, 0, 0);
        line.push("UNIT", b"hyprix.service");
        sessions.stamp(&mut line);
        assert_eq!(line.get_str("SESSION_ID"), Some("3"));
        let mut other = Record::new(6, 0, 0);
        other.push("UNIT", b"sshd.service");
        sessions.stamp(&mut other);
        assert_eq!(other.get("SESSION_ID"), None);
        assert!(sessions.on_seat("seat0").is_some());
        sessions.take(Said::Ended(3), &mut Record::new(5, 0, 0));
        let mut after = Record::new(6, 0, 0);
        after.push("UNIT", b"hyprix.service");
        sessions.stamp(&mut after);
        assert_eq!(after.get("SESSION_ID"), None);
        assert!(sessions.on_seat("seat0").is_none());
    }
}

mod protocols {
    use crate::protocols::{self, StreamHeader};

    #[test]
    fn rfc3164_as_musl_and_glibc_send_it() {
        let record = protocols::syslog(b"<30>Oct 23 14:01:02 sshd[812]: accepted key\n", 1, 2);
        assert_eq!(record.priority, 6);
        assert_eq!(record.get_str("SYSLOG_FACILITY"), Some("3"));
        assert_eq!(record.get_str("SYSLOG_IDENTIFIER"), Some("sshd"));
        assert_eq!(record.get_str("SYSLOG_PID"), Some("812"));
        assert_eq!(record.get_str("SYSLOG_TIMESTAMP"), Some("Oct 23 14:01:02"));
        assert_eq!(record.message(), b"accepted key");
        assert_eq!(record.get_str("_TRANSPORT"), Some("syslog"));
        let bare = protocols::syslog(b"<11>logger: no time", 1, 2);
        assert_eq!(bare.priority, 3);
        assert_eq!(bare.get_str("SYSLOG_IDENTIFIER"), Some("logger"));
        assert_eq!(bare.message(), b"no time");
        // No `<PRI>`: no level, and user's facility.
        let plain = protocols::syslog(b"just text, no tag", 1, 2);
        assert_eq!(plain.priority, crate::record::UNSPECIFIED);
        assert_eq!(plain.get_str("FERRIX_PRIORITY_SOURCE"), Some("unspecified"));
        assert_eq!(plain.get_str("SYSLOG_FACILITY"), Some("1"));
        assert_eq!(bare.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        assert_eq!(plain.get("SYSLOG_IDENTIFIER"), None);
        assert_eq!(plain.message(), b"just text, no tag");
    }

    #[test]
    fn rfc5424_with_structured_data() {
        let record = protocols::syslog(
            b"<165>1 2026-10-23T14:01:02.003Z box app 4242 ID47 [exampleSDID@32473 iut=\"3\" eventSource=\"App\"] \xEF\xBB\xBFAn application event",
            1,
            2,
        );
        assert_eq!(record.priority, 5);
        assert_eq!(record.get_str("SYSLOG_FACILITY"), Some("20"));
        assert_eq!(record.get_str("SYSLOG_IDENTIFIER"), Some("app"));
        assert_eq!(record.get_str("SYSLOG_PID"), Some("4242"));
        assert_eq!(record.get_str("SYSLOG_MSGID"), Some("ID47"));
        assert_eq!(
            record.get_str("SYSLOG_STRUCTURED_DATA"),
            Some("[exampleSDID@32473 iut=\"3\" eventSource=\"App\"]")
        );
        assert_eq!(record.message(), b"An application event");
        let nil = protocols::syslog(b"<14>1 - - - - - - hello", 1, 2);
        assert_eq!(nil.message(), b"hello");
        assert_eq!(nil.get("SYSLOG_IDENTIFIER"), None);
    }

    #[test]
    fn the_native_protocol_with_a_binary_value() {
        let mut datagram =
            alloc::vec::Vec::from(&b"MESSAGE=hello\nPRIORITY=3\n_PID=1\nCODE_LINE=12\nBLOB\n"[..]);
        datagram.extend_from_slice(&5_u64.to_le_bytes());
        datagram.extend_from_slice(b"a\nb\0c\n");
        let record = protocols::native(&datagram, 1, 2).unwrap();
        assert_eq!(record.priority, 3);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        assert_eq!(record.message(), b"hello");
        assert_eq!(record.get("_PID"), None);
        let unsaid =
            protocols::native(b"MESSAGE=hi\nFERRIX_PRIORITY_SOURCE=stated\n", 1, 2).unwrap();
        assert_eq!(unsaid.priority, crate::record::UNSPECIFIED);
        // The sender's own claim is dropped; lode says what it found.
        assert_eq!(
            unsaid.get_str("FERRIX_PRIORITY_SOURCE"),
            Some("unspecified")
        );
        assert_eq!(record.get_str("CODE_LINE"), Some("12"));
        assert_eq!(record.get("BLOB"), Some(&b"a\nb\0c"[..]));
        assert_eq!(record.get_str("_TRANSPORT"), Some("journal"));
        assert!(protocols::native(b"BLOB\n\x05\x00", 1, 2).is_none());
    }

    #[test]
    fn a_stream_header_and_its_lines() {
        let bytes = b"myapp\nmy.service\n6\n1\n0\n0\n0\n<3>broke\nfine\n";
        let (header, used) = protocols::stream_header(bytes).unwrap();
        assert_eq!(
            header,
            StreamHeader {
                identifier: "myapp".into(),
                unit: "my.service".into(),
                // Info is every stream's default: it says nothing.
                priority: crate::record::UNSPECIFIED,
                level_prefix: true,
                echoed: false,
            }
        );
        let broke = protocols::stream_line(&header, b"<3>broke", 1, 2);
        assert_eq!(broke.priority, 3);
        assert_eq!(broke.message(), b"broke");
        assert_eq!(broke.get_str("SYSLOG_IDENTIFIER"), Some("myapp"));
        assert_eq!(&bytes[used..used + 8], b"<3>broke");
        let fine = protocols::stream_line(&header, b"fine", 1, 2);
        assert_eq!(fine.priority, crate::record::UNSPECIFIED);
        assert_eq!(fine.get_str("FERRIX_PRIORITY_SOURCE"), Some("unspecified"));
        assert!(protocols::stream_header(b"half\nheader\n").is_none());
    }
}

mod summaries {
    use alloc::vec::Vec;

    use crate::filter;
    use crate::record::Record;
    use crate::summary::Summary;

    fn records() -> Vec<Record> {
        let mut out = Vec::new();
        for (seq, (unit, priority)) in [("a.service", 6_u8), ("b.service", 3), ("a.service", 7)]
            .into_iter()
            .enumerate()
        {
            let mut record = Record::new(priority, 1000 + seq as u64, seq as u64);
            record.seqnum = 10 + seq as u64;
            record.push("UNIT", unit.as_bytes());
            record.push("SYSLOG_IDENTIFIER", b"x");
            out.push(record);
        }
        out
    }

    #[test]
    fn a_summary_round_trips_and_says_what_cannot_match() {
        let summary = Summary::of(&records());
        assert_eq!(summary.seqs, (10, 12));
        assert_eq!(summary.times, (1000, 1002));
        assert_eq!(summary.levels, (3, 7));
        let bytes = summary.encode();
        assert_eq!(Summary::decode(&bytes), Some(summary.clone()));
        let mut broken = bytes;
        broken[20] ^= 1;
        assert_eq!(Summary::decode(&broken), None);
        let may = |text: &str| summary.may_match(&filter::parse(text).unwrap());
        assert!(may(r#"unit = "a*""#));
        assert!(!may(r#"unit = "c.service""#));
        assert!(may("priority <= err"));
        assert!(!may("priority <= crit"));
        assert!(!may(r#"unit = "c.service" and priority <= err"#));
        assert!(may(r#"unit = "c.service" or priority <= err"#));
        assert!(may(r#"MESSAGE = "anything""#));
        assert!(may(r#"not unit = "a.service""#));
        assert!(summary.overlaps(Some(1001), None));
        assert!(!summary.overlaps(Some(2000), None));
        assert!(!summary.overlaps(None, Some(999)));
    }
}

/// What the first records from a real machine (the N150, 2026-10-23)
/// showed lode reading wrong.
mod field_quality {
    use alloc::vec::Vec;

    use crate::console;
    use crate::ingest::{self, Now};
    use crate::kmsg::KernelRecord;
    use crate::limit::{self, Limiter, MESSAGE_ID_REPEATED, MESSAGE_ID_SUPPRESSED, WINDOW_US};
    use crate::record::{Record, UNSPECIFIED};

    fn kernel(facility: u8, text: &[u8]) -> KernelRecord {
        KernelRecord {
            facility,
            level: 6,
            seq: 1,
            usec: 1,
            text: Vec::from(text),
        }
    }

    const NOW: Now = Now {
        realtime_us: 1_700_000_000_000_000,
        monotonic_us: 1_000_000,
    };

    #[test]
    fn digits_glued_before_a_name_are_not_the_name() {
        for line in [
            &b"0usbhid: keyboard on port 3"[..],
            b"00usbhid: keyboard on port 3",
            b"0xusbhid: keyboard on port 3",
        ] {
            let said = console::read(line);
            assert_eq!(said.identifier.as_deref(), Some("usbhid"), "{line:?}");
            assert_eq!(said.message, b"keyboard on port 3");
        }
        assert_eq!(console::read(b"0x1f: a value").identifier, None);
        assert_eq!(console::read(b"9: a count").identifier, None);
    }

    #[test]
    fn a_tracing_line_gives_its_crate_and_level() {
        let said = console::read(
            b"\x1b[2m2026-10-23T17:21:51.786662Z\x1b[0m \x1b[33m WARN\x1b[0m \
              \x1b[2mrussh::server\x1b[0m\x1b[2m:\x1b[0m client went away",
        );
        assert_eq!(said.identifier.as_deref(), Some("russh"));
        assert_eq!(said.level, Some(4));
        assert_eq!(said.message, b"client went away");
    }

    #[test]
    fn a_programs_padded_name_without_the_kernels_indent() {
        let said = console::read(b"devmgr   intel-gt started as pid 41");
        assert_eq!(said.identifier.as_deref(), Some("devmgr"));
        assert_eq!(said.subsystem.as_deref(), Some("devmgr"));
        assert_eq!(console::read(b"a sentence with words").identifier, None);
    }

    #[test]
    fn levels_from_the_words_and_where_they_came_from() {
        let record = ingest::from_kernel(&kernel(1, b"ahci: port 2 failed to spin up"), NOW);
        assert_eq!(record.priority, 3);
        assert_eq!(record.get_str("FERRIX_PRIORITY_WORD"), Some("failed"));
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("derived"));
        let record = ingest::from_kernel(&kernel(1, b"sshd: connection refused"), NOW);
        assert_eq!(record.priority, 4);
        let record = ingest::from_kernel(&kernel(1, b"gpu: table written: 0 rows refused"), NOW);
        assert_eq!(record.priority, UNSPECIFIED);
        let record = ingest::from_kernel(&kernel(1, b"net: 0 dropped, 0 lost"), NOW);
        assert_eq!(record.priority, UNSPECIFIED);
        assert_eq!(
            record.get_str("FERRIX_PRIORITY_SOURCE"),
            Some("unspecified")
        );
        assert_eq!(record.get_str("FERRIX_PRIORITY_WORD"), None);
        // The kernel's boot checks say what they refused on purpose.
        let record = ingest::from_kernel(&kernel(0, b"check: a bad pointer was refused"), NOW);
        assert_eq!(record.priority, UNSPECIFIED);
        let record = ingest::from_kernel(&kernel(0, b"WARNING: a lock held too long"), NOW);
        assert_eq!(record.priority, 4);
        let record = ingest::from_kernel(&kernel(0, b"pci: could not map BAR 2"), NOW);
        assert_eq!(record.priority, 3);
        // A level a program wrote is its own, not a guess.
        let record = ingest::from_kernel(
            &kernel(1, b"2026-10-23T17:21:51.786662Z ERROR app::x: it failed"),
            NOW,
        );
        assert_eq!(record.priority, 3);
        assert_eq!(record.get_str("FERRIX_PRIORITY_WORD"), None);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        // A level a program wrote to `/dev/kmsg` is its own: `LOG_USER` at
        // another level than the console's info.
        let mut quiet = kernel(1, b"x: failed on purpose");
        quiet.level = 7;
        let record = ingest::from_kernel(&quiet, NOW);
        assert_eq!(record.priority, 7);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        assert_eq!(record.get_str("FERRIX_PRIORITY_WORD"), None);
    }

    #[test]
    fn a_level_a_program_wrote_to_kmsg_is_stated() {
        // A driver's `<30>`: LOG_DAEMON at info, a facility no console line
        // has, stated even at info; and at debug, words or none.
        let mut daemon = kernel(3, b"intel: render node published (1 node)");
        let record = ingest::from_kernel(&daemon, NOW);
        assert_eq!(record.priority, 6);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        assert_eq!(record.get_str("SYSLOG_IDENTIFIER"), Some("intel"));
        assert_eq!(record.get_str("SYSLOG_FACILITY"), Some("3"));
        daemon.text =
            Vec::from(&b"intel: uapi refused EXECBUFFER2 EIO - (0 not said before it)"[..]);
        daemon.level = 3;
        let record = ingest::from_kernel(&daemon, NOW);
        assert_eq!(record.priority, 3);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        assert_eq!(record.get_str("FERRIX_PRIORITY_WORD"), None);
        daemon.level = 7;
        daemon.text = Vec::from(&b"intel: freqlog: asked 300 MHz, failed nothing"[..]);
        let record = ingest::from_kernel(&daemon, NOW);
        assert_eq!(record.priority, 7);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        // LOG_USER at info is a console line or a `<6>` write: not stated.
        let record = ingest::from_kernel(&kernel(1, b"usbhid: keyboard on port 3"), NOW);
        assert_eq!(record.priority, UNSPECIFIED);
        // The kernel's own facility at another level is the kernel's.
        let mut own = kernel(0, b"pci: a bridge with no window");
        own.level = 4;
        let record = ingest::from_kernel(&own, NOW);
        assert_eq!(record.priority, 4);
        assert_eq!(record.get_str("FERRIX_PRIORITY_SOURCE"), Some("kernel"));
    }

    #[test]
    fn every_line_that_says_who_wrote_it_has_a_subsystem() {
        let subsystem = |facility, text: &[u8]| {
            String::from(
                ingest::from_kernel(&kernel(facility, text), NOW)
                    .get_str("FERRIX_SUBSYSTEM")
                    .unwrap_or("-"),
            )
        };
        assert_eq!(subsystem(1, b"usbhid: keyboard"), "usbhid");
        assert_eq!(subsystem(1, b"  init     sshd.service[9]: up"), "init");
        assert_eq!(subsystem(0, b"0x606 RAPL_POWER_UNIT 0xa0e03"), "kernel");
        assert_eq!(subsystem(1, b"words alone"), "-");
    }

    use alloc::string::String;

    fn line(message: &[u8], seq: u64) -> Record {
        let mut record = Record::new(6, seq, seq);
        record.push("MESSAGE", message);
        record.push("UNIT", b"hyprix.service");
        record.push("_TRANSPORT", b"console");
        record.push("_KERNEL_SEQNUM", alloc::format!("{seq}").as_bytes());
        record
    }

    #[test]
    fn lines_that_differ_only_in_numbers_fold() {
        assert!(limit::similar(b"839x497 at 181,99", b"12x4 at 0,1000"));
        assert!(!limit::similar(
            b"839x497 at 181,99",
            b"839x497 at 181,99 tiled"
        ));
        assert!(!limit::similar(b"port 1 up", b"port 1 down"));
        let mut limiter = Limiter::new(Vec::new());
        let mut out = Vec::new();
        for n in 0..50_u64 {
            let text = alloc::format!(
                "DVI-D-1: not scanning out directly: no window covers the screen \
                 (the top one is {}x497 at {},99, tiled)",
                800 + n,
                100 + n
            );
            limiter.admit(line(text.as_bytes(), n), n, &mut out);
        }
        limiter.admit(line(b"frame done", 50), 50, &mut out);
        assert_eq!(out.len(), 3);
        let folded = &out[1];
        assert_eq!(folded.get_str("MESSAGE_ID"), Some(MESSAGE_ID_REPEATED));
        assert_eq!(folded.get_str("REPEATED"), Some("49"));
        assert!(
            folded
                .get_str("MESSAGE")
                .unwrap()
                .starts_with("message repeated 49 time(s), numbers varying: DVI-D-1:")
        );
        assert!(
            folded
                .get_str("LAST_MESSAGE")
                .unwrap()
                .contains("849x497 at 149,99")
        );
        let refs: Vec<&Record> = out.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
        // Marks alike but for a number are each kept.
        let mut out = Vec::new();
        for n in 0..2_u64 {
            let mut mark = line(
                alloc::format!("lode-mark: capture me {n}").as_bytes(),
                60 + n,
            );
            mark.push("MARK", alloc::format!("capture me {n}").as_bytes());
            limiter.admit(mark, 60 + n, &mut out);
        }
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_blank_console_line_is_counted_not_stored() {
        let mut limiter = Limiter::new(Vec::new());
        let mut out = Vec::new();
        limiter.admit(line(b"", 1), 1, &mut out);
        limiter.admit(line(b"  ", 2), 2, &mut out);
        assert!(out.is_empty());
        limiter.flush(10, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].get_str("MESSAGE_ID"), Some(MESSAGE_ID_SUPPRESSED));
        assert_eq!(out[0].get_str("N_BLANK"), Some("2"));
        let refs: Vec<&Record> = out.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
    }

    #[test]
    fn each_kind_of_rule_is_read_on_its_own() {
        let rules = limit::parse_rules(
            "fold 0s where unit = \"hyprix*\"\n\
             level warning where unit = hyprix.service\n\
             drop where unit = hyprix.service\n",
        )
        .unwrap();
        let mut limiter = Limiter::new(rules);
        let mut out = Vec::new();
        // `level` holds the info lines, before `drop` is read.
        limiter.admit(line(b"tick 1", 1), 1, &mut out);
        let mut warn = line(b"tick 2", 2);
        warn.priority = 4;
        limiter.admit(warn.clone(), 2, &mut out);
        // `fold 0s` matched too: the same warning again is not folded.
        warn.seqnum = 3;
        limiter.admit(line_at(warn, 3), 3, &mut out);
        assert_eq!(
            out.len(),
            2,
            "{:?}",
            out.iter().map(Record::message).collect::<Vec<_>>()
        );
        limiter.flush(WINDOW_US + 10, &mut out);
        let refs: Vec<&Record> = out.iter().collect();
        assert!(ingest::kernel_gaps(&refs).unsaid.is_empty());
    }

    #[test]
    fn the_first_limit_that_matches_is_the_one() {
        let rules = limit::parse_rules(
            "limit 2/10s where unit = hyprix.service\n\
             limit 1000/30s where transport = console\n",
        )
        .unwrap();
        let mut limiter = Limiter::new(rules);
        let mut out = Vec::new();
        for (n, word) in ["one", "two", "three", "four"].iter().enumerate() {
            let n = n as u64;
            limiter.admit(line(word.as_bytes(), n), n, &mut out);
        }
        assert_eq!(out.len(), 2);
    }

    fn line_at(mut record: Record, seq: u64) -> Record {
        record.monotonic_us = seq;
        record.fields.retain(|field| field.name != "_KERNEL_SEQNUM");
        record.push("_KERNEL_SEQNUM", alloc::format!("{seq}").as_bytes());
        record
    }
}

/// Unspecified is a level of its own (`docs/LOGGING.md` §4.1).
mod unspecified {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::filter::{self, priority_range};
    use crate::format::{Output, level_table, write};
    use crate::record::{LevelSource, Record, UNSPECIFIED};
    use crate::time::Zone;

    fn at(level: u8, id: &str) -> Record {
        let mut record = Record::new(level, 1_761_228_062_000_000, 1);
        record.set_level(level, LevelSource::Stated);
        record.push("MESSAGE", b"m");
        record.push("SYSLOG_IDENTIFIER", id.as_bytes());
        record
    }

    #[test]
    fn ranges_leave_it_out_and_it_can_be_asked_for() {
        let none = at(UNSPECIFIED, "a");
        let err = at(3, "a");
        let info = at(6, "a");
        let err_only = priority_range("err").unwrap();
        assert!(err_only.matches(&err));
        assert!(!err_only.matches(&none));
        assert!(!priority_range("0..7").unwrap().matches(&none));
        let both = priority_range("err,unspecified").unwrap();
        assert!(both.matches(&err) && both.matches(&none) && !both.matches(&info));
        assert!(priority_range("unspecified").unwrap().matches(&none));
        assert!(priority_range("err..unspecified").is_err());
        assert!(priority_range("loud").is_err());
        let parsed = |text| filter::parse(text).unwrap();
        assert!(parsed("level = unspecified").matches(&none));
        assert!(!parsed("level = unspecified").matches(&info));
        assert!(!parsed("priority > warning").matches(&none));
        assert!(parsed("priority != err").matches(&none));
    }

    #[test]
    fn every_format_shows_it_and_none_calls_it_info() {
        let zone = Zone::utc();
        let show = |record: &Record, output| {
            let mut out = Vec::new();
            write(record, output, &zone, &mut out);
            String::from_utf8(out).unwrap()
        };
        let none = at(UNSPECIFIED, "a");
        assert!(show(&none, Output::Human).contains(" ? a: m"));
        let json = show(&none, Output::Json);
        assert!(!json.contains("\"PRIORITY\""), "{json}");
        assert!(json.contains("\"FERRIX_LEVEL\":\"unspecified\""));
        assert!(json.contains("\"FERRIX_PRIORITY_SOURCE\":\"unspecified\""));
        let export = show(&none, Output::Export);
        assert!(!export.contains("\nPRIORITY="));
        assert!(export.contains("\nFERRIX_LEVEL=unspecified\n"));
        let known = show(&at(3, "a"), Output::Json);
        assert!(known.contains("\"PRIORITY\":\"3\"") && known.contains("\"FERRIX_LEVEL\":\"3\""));
    }

    #[test]
    fn the_level_table_puts_the_most_unspecified_first() {
        let mut derived = at(3, "b");
        derived.set_level(3, LevelSource::Derived);
        let records = [
            at(6, "a"),
            at(UNSPECIFIED, "b"),
            at(UNSPECIFIED, "b"),
            derived,
            at(UNSPECIFIED, "c"),
        ];
        let table = String::from_utf8(level_table(&records)).unwrap();
        let rows: Vec<Vec<&str>> = table
            .lines()
            .map(|line| line.split_whitespace().collect())
            .collect();
        assert_eq!(rows[0], ["SOURCE", "RECORDS", "UNSPECIFIED", "DERIVED"]);
        assert_eq!(rows[1], ["b", "3", "2", "1"]);
        assert_eq!(rows[2], ["c", "1", "1", "0"]);
        assert_eq!(rows[3], ["a", "1", "0", "0"]);
    }

    #[test]
    fn a_stored_record_always_says_where_its_level_came_from() {
        let mut own = Record::new(5, 0, 0);
        own.stamp_level_source();
        assert_eq!(own.get_str("FERRIX_PRIORITY_SOURCE"), Some("stated"));
        let mut none = Record::new(UNSPECIFIED, 0, 0);
        none.stamp_level_source();
        assert_eq!(none.get_str("FERRIX_PRIORITY_SOURCE"), Some("unspecified"));
        none.set_level(UNSPECIFIED, LevelSource::Derived);
        assert_eq!(none.get_str("FERRIX_PRIORITY_SOURCE"), Some("unspecified"));
        assert_eq!(
            none.fields
                .iter()
                .filter(|field| field.name == "FERRIX_PRIORITY_SOURCE")
                .count(),
            1
        );
    }
}

/// Store-and-forward (`docs/LOGGING.md` §10.2).
mod uploads {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::record::Record;
    use crate::time::Zone;
    use crate::upload::{self, Backoff, Format, Url};

    #[test]
    fn a_configuration_reads_and_says_what_is_wrong() {
        let destinations = upload::parse_config(
            "# comment\n\
             collector http://127.0.0.1:19532/upload export\n\
             vector http://localhost:8080 json auth where priority <= warning\n\
             plain http://localhost export\n",
        )
        .unwrap();
        assert_eq!(destinations.len(), 3);
        assert_eq!(destinations[0].url.path, "/upload");
        assert_eq!(destinations[1].url.address, Some([127, 0, 0, 1]));
        assert_eq!(destinations[1].url.path, "/");
        assert!(destinations[1].auth && destinations[1].filter.is_some());
        assert_eq!(destinations[2].url.port, upload::DEFAULT_PORT);
        assert_eq!(destinations[2].url.path, "/upload");
        for bad in [
            "x http://10.0.2.2:19532 export",
            "x https://127.0.0.1 export",
            "x http://127.0.0.1 xml",
            "x/y http://127.0.0.1 export",
            "x http://127.0.0.1:0 export",
            "x http://127.0.0.1 export loud",
            "x http://127.0.0.1",
            "x http://127.0.0.1 export\nx http://127.0.0.1 json",
        ] {
            assert!(upload::parse_config(bad).is_err(), "{bad}");
        }
        assert!(
            upload::parse_config("x http://10.0.2.2 export")
                .unwrap_err()
                .contains("ssh tunnel")
        );
    }

    fn record(seq: u64, facility: &str) -> Record {
        let mut record = Record::new(6, 1_761_228_062_000_000, seq);
        record.seqnum = seq;
        record.push("MESSAGE", alloc::format!("line {seq}").as_bytes());
        record.push("SYSLOG_FACILITY", facility.as_bytes());
        record
    }

    #[test]
    fn auth_stays_unless_named_and_the_filter_chooses() {
        let all = &upload::parse_config("a http://localhost export").unwrap()[0];
        let auth = &upload::parse_config("a http://localhost export auth").unwrap()[0];
        let some = &upload::parse_config("a http://localhost export where message = \"line 1\"")
            .unwrap()[0];
        assert!(all.wants(&record(1, "1")));
        assert!(!all.wants(&record(1, "10")));
        assert!(!all.wants(&record(1, "4")));
        assert!(auth.wants(&record(1, "4")));
        assert!(some.wants(&record(1, "1")) && !some.wants(&record(2, "1")));
        let mut unit = record(1, "1");
        unit.push("_SYSTEMD_UNIT", b"auth.service");
        assert!(!all.wants(&unit));
    }

    #[test]
    fn a_request_and_its_answer() {
        let url = Url::parse("http://localhost:19532/upload", "/").unwrap();
        let records = [record(1, "1"), record(2, "1")];
        let refs: Vec<&Record> = records.iter().collect();
        let (body, taken) = upload::body(Format::Export, &refs, &Zone::utc());
        assert_eq!(taken, 2);
        assert_eq!(
            body.windows(13).filter(|w| *w == b"MESSAGE=line ").count(),
            2
        );
        let request = upload::request(&url, Format::Export, &body, "ab12", "box\r\nX: y");
        let text = String::from_utf8_lossy(&request);
        assert!(text.starts_with("POST /upload HTTP/1.1\r\nHost: localhost:19532\r\n"));
        assert!(text.contains("Content-Type: application/vnd.fdo.journal\r\n"));
        let (start, length) = upload::request_head(&request).unwrap();
        let head = &request[..start];
        assert_eq!(upload::header(head, "x-lode-machine-id"), Some("ab12"));
        // A host name cannot add a header of its own.
        assert_eq!(upload::header(head, upload::HOST_HEADER), Some("boxX:y"));
        assert_eq!(upload::header(head, "X"), None);
        assert_eq!(upload::request_line(head), Some(("POST", "/upload")));
        assert_eq!(length, body.len());
        assert_eq!(&request[start..], &body[..]);
        assert_eq!(upload::status(b"HTTP/1.1 202 Accepted\r\n"), Some(202));
        assert_eq!(upload::status(b"HTTP/1.0 503 Busy\r\nx"), Some(503));
        assert_eq!(upload::status(b"HTTP/1.1 200"), None);
        assert_eq!(upload::status(b"SSH-2.0 x\r\n"), None);
        let (json, _) = upload::body(Format::Json, &refs, &Zone::utc());
        assert_eq!(json.iter().filter(|&&byte| byte == b'\n').count(), 2);
    }

    #[test]
    fn a_batch_stops_at_its_bytes_but_holds_one() {
        let mut big = record(1, "1");
        // A value is cut at 64 KiB: seventeen of them pass a megabyte.
        for _ in 0..17 {
            big.push("BLOB", &alloc::vec![b'x'; 64 * 1024]);
        }
        let records = [big, record(2, "1")];
        let refs: Vec<&Record> = records.iter().collect();
        assert_eq!(upload::body(Format::Export, &refs, &Zone::utc()).1, 1);
    }

    #[test]
    fn backoff_doubles_to_five_minutes_and_resets() {
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..12).map(|_| backoff.fail()).collect();
        assert_eq!(&waits[..4], [1000, 2000, 4000, 8000]);
        assert_eq!(*waits.last().unwrap(), upload::BACKOFF_MAX_MS);
        backoff.succeed();
        assert_eq!(backoff.fail(), 1000);
    }

    #[test]
    fn records_gone_before_they_were_sent_are_counted() {
        assert_eq!(upload::unsent(10, [11, 12, 13]), 0);
        assert_eq!(upload::unsent(10, [15, 16, 20]), 7);
        assert_eq!(upload::unsent(10, [9, 10, 11]), 0);
        assert_eq!(upload::read_cursor(b"42\n"), Some(42));
        assert_eq!(upload::read_cursor(b"x"), None);
    }
}

/// Sealed segments, compressed (`docs/LOGGING.md` §8.2).
mod compressed {
    use alloc::vec::Vec;

    use crate::compress::{self, Compressor};
    use crate::record::Record;
    use crate::segment;

    fn segment_bytes(count: u64) -> Vec<u8> {
        let mut out = super::header().encode();
        for n in 0..count {
            let mut record = Record::new(6, 1_761_228_062_000_000 + n, n);
            record.seqnum = 100 + n;
            record.push(
                "MESSAGE",
                alloc::format!("frame {n} took {} ms on DVI-D-1", n % 17).as_bytes(),
            );
            record.push("SYSLOG_IDENTIFIER", b"hyprix");
            segment::encode_record(&record, &mut out);
        }
        out
    }

    #[test]
    fn a_segment_packs_smaller_and_unpacks_to_itself() {
        let raw = segment_bytes(5000);
        let packed = compress::compress(raw.clone()).unwrap();
        assert!(compress::is_compressed(&packed));
        assert!(
            packed.len() * 3 < raw.len(),
            "{} of {}",
            packed.len(),
            raw.len()
        );
        assert_eq!(compress::unpack(&packed).unwrap(), raw);
        assert_eq!(compress::expand(packed.clone()), raw);
        assert_eq!(compress::expand(raw.clone()), raw);
        assert_eq!(
            compress::header(&packed).unwrap(),
            compress::header(&raw).unwrap()
        );
        assert_eq!(
            segment::scan(&compress::expand(packed), 0)
                .unwrap()
                .records
                .len(),
            5000
        );
    }

    #[test]
    fn a_torn_tail_is_left_out_and_a_damaged_frame_ends_the_unpacking() {
        let mut raw = segment_bytes(3000);
        let good = raw.len();
        raw.extend_from_slice(&[7, 0, 0, 0, 1, 2]);
        let packed = compress::compress(raw.clone()).unwrap();
        assert_eq!(compress::unpack(&packed).unwrap(), &raw[..good]);
        // A bit flipped in the last frame: everything before it still reads.
        let mut damaged = packed.clone();
        let last = damaged.len() - 3;
        damaged[last] ^= 1;
        let partial = compress::unpack(&damaged).unwrap();
        assert!(partial.len() < good && raw.starts_with(&partial));
        let scanned = segment::scan(&partial, 0).unwrap();
        assert!(!scanned.records.is_empty());
        assert_eq!(scanned.good, partial.len());
        // A damaged prefix is no segment.
        let mut prefix = packed;
        prefix[20] ^= 1;
        assert!(compress::unpack(&prefix).is_none());
        assert!(compress::expand(prefix).is_empty());
    }

    #[test]
    fn a_frame_at_a_time_gives_the_same() {
        let raw = segment_bytes(4000);
        let mut compressor = Compressor::new(raw.clone()).unwrap();
        let mut steps = 1;
        while !compressor.step() {
            steps += 1;
        }
        assert!(steps > 2, "{steps}");
        assert_eq!(compressor.raw_len(), raw.len());
        assert_eq!(compress::unpack(&compressor.finish()).unwrap(), raw);
        assert!(Compressor::new(compress::compress(raw).unwrap()).is_none());
        assert!(Compressor::new(b"not a segment".to_vec()).is_none());
        let empty = super::header().encode();
        assert_eq!(
            compress::unpack(&compress::compress(empty.clone()).unwrap()).unwrap(),
            empty
        );
    }
}

/// Field names as indexes (`docs/LOGGING.md` §8.3's write budget).
mod field_names {
    use alloc::vec::Vec;

    use crate::crc::crc32c;
    use crate::record::Record;
    use crate::segment;

    #[test]
    fn known_names_cost_two_bytes_and_read_back_in_order() {
        let mut record = Record::new(6, 1, 2);
        record.seqnum = 7;
        record.push("MESSAGE", b"hi");
        record.push("MY_OWN", b"x");
        record.push("SYSLOG_IDENTIFIER", b"app");
        let mut out = Vec::new();
        segment::encode_record(&record, &mut out);
        // 48 fixed, 4 trailing; MESSAGE and SYSLOG_IDENTIFIER by index.
        assert_eq!(
            out.len(),
            48 + (2 + 4 + 2) + (2 + 6 + 4 + 1) + (2 + 4 + 3) + 4
        );
        let (back, length) = segment::entry_at(&out, 0).unwrap();
        assert_eq!(length, out.len());
        assert_eq!(back.fields, record.fields);
    }

    #[test]
    fn a_name_index_past_the_list_is_no_entry() {
        let mut record = Record::new(6, 1, 2);
        record.push("MESSAGE", b"hi");
        let mut out = Vec::new();
        segment::encode_record(&record, &mut out);
        // MESSAGE is index 0 at byte 48: make it one past the list, and
        // mend the checksum so only the index is wrong.
        let past = u16::try_from(segment::NAMES.len()).unwrap() | 0x8000;
        out[48..50].copy_from_slice(&past.to_le_bytes());
        let crc = crc32c(&out[8..]);
        out[4..8].copy_from_slice(&crc.to_le_bytes());
        assert!(segment::entry_at(&out, 0).is_none());
    }
}

/// TLS destinations, and what a receiver reads (`docs/LOGGING.md` §10.2,
/// §10.3).
mod receiving {
    use alloc::string::String;
    use alloc::vec::Vec;

    use crate::format::{Output, write};
    use crate::record::{LevelSource, Record, UNSPECIFIED};
    use crate::time::Zone;
    use crate::upload::{self, Format};

    #[test]
    fn https_goes_anywhere_with_a_ca_and_a_pair_of_cert_and_key() {
        let line = "nazuna https://192.168.9.10:19532/upload export ca=/etc/ca.pem \
                    cert=/etc/c.pem key=/etc/c.key where priority <= err";
        let destination = &upload::parse_config(line).unwrap()[0];
        assert!(destination.url.tls);
        assert_eq!(destination.url.address, Some([192, 168, 9, 10]));
        let tls = destination.tls.as_ref().unwrap();
        assert_eq!(tls.ca, "/etc/ca.pem");
        assert_eq!(tls.key.as_deref(), Some("/etc/c.key"));
        assert!(destination.filter.is_some());
        let named = &upload::parse_config("n https://nazuna.lan json ca=/ca").unwrap()[0];
        assert_eq!(named.url.address, None);
        assert_eq!(named.url.host, "nazuna.lan");
        assert!(named.tls.as_ref().unwrap().cert.is_none());
        for bad in [
            "n https://nazuna export",
            "n https://nazuna export ca=/ca cert=/c",
            "n http://127.0.0.1 export ca=/ca",
            "n https://naz_una export ca=/ca",
            "n https://nazuna export ca=/ca ca=/ca",
            "n https://nazuna export ca=",
        ] {
            assert!(upload::parse_config(bad).is_err(), "{bad}");
        }
    }

    fn records() -> Vec<Record> {
        let mut one = Record::new(3, 1_761_228_062_000_001, 5);
        one.seqnum = 41;
        one.boot_id = [7; 16];
        one.host = alloc::rc::Rc::from("box");
        one.set_level(3, LevelSource::Derived);
        one.push("MESSAGE", b"two\nlines");
        one.push("SYSLOG_IDENTIFIER", b"app");
        one.push("_TRANSPORT", b"kernel");
        one.push("BLOB", &[0, 255, 10]);
        let mut two = Record::new(UNSPECIFIED, 1_761_228_062_000_002, 6);
        two.seqnum = 42;
        two.boot_id = [7; 16];
        two.host = alloc::rc::Rc::from("box");
        two.set_level(UNSPECIFIED, LevelSource::Unspecified);
        two.push("MESSAGE", "said \"quoted\" \\ \u{e9} \x07".as_bytes());
        // A driver's stated level crosses to the collector as stated.
        let mut three = Record::new(7, 1_761_228_062_000_003, 7);
        three.seqnum = 43;
        three.boot_id = [7; 16];
        three.host = alloc::rc::Rc::from("box");
        three.set_level(7, LevelSource::Stated);
        three.push("MESSAGE", b"freqlog: asked 300 MHz");
        three.push("SYSLOG_IDENTIFIER", b"intel");
        alloc::vec![one, two, three]
    }

    fn written(output: Output) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records() {
            write(&record, output, &Zone::utc(), &mut out);
        }
        out
    }

    #[test]
    fn an_export_body_reads_back_as_the_records_it_was_made_of() {
        let back = upload::parse_export(&written(Output::Export)).unwrap();
        assert_eq!(back, records());
        assert!(upload::parse_export(b"MESSAGE\n\x09\x00").is_err());
        assert!(upload::parse_export(b"__SEQNUM=x\n").is_err());
        assert!(upload::parse_export(b"PRIORITY=9\n").is_err());
    }

    #[test]
    fn a_json_body_reads_back_as_the_records_it_was_made_of() {
        let back = upload::parse_json_lines(&written(Output::Json)).unwrap();
        assert_eq!(back, records());
        assert!(upload::parse_json_lines(b"{\"A\":1}").is_err());
        assert!(upload::parse_json_lines(b"{\"A\":\"x\"} trailing").is_err());
        assert!(upload::parse_json_lines(b"{\"A\":[1,300]}").is_err());
        assert_eq!(upload::parse_json_lines(b"\n{}\n").unwrap().len(), 1);
    }

    #[test]
    fn the_format_names_its_content_type() {
        assert_eq!(Format::Json.content_type(), "application/x-ndjson");
        let _ = String::new();
    }
}

fn unit_line(text: &str, stream: crate::units::Stream) -> crate::units::UnitLine {
    crate::units::UnitLine {
        unit: String::from("web.service"),
        invocation: Some([0x5a; 16]),
        pid: 812,
        uid: 1000,
        gid: 1000,
        stream,
        monotonic_us: 4_000_000,
        text: Vec::from(text.as_bytes()),
    }
}

#[test]
fn a_unit_line_crosses_inits_socket_whole() {
    use crate::units::{self, Stream};
    let line = unit_line("listening on :80 \u{1b}[1mnow\u{1b}[0m", Stream::Err);
    let mut bytes = Vec::new();
    units::encode(&line, &mut bytes);
    assert_eq!(
        bytes,
        b"L web.service 5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a 812 1000 1000 err 4000000 listening on :80 \x1b[1mnow\x1b[0m\n"
    );
    let read = units::decode(bytes.strip_suffix(b"\n").unwrap()).unwrap();
    assert_eq!(read, line);
    let no_run = units::decode(b"L x.service - 1 0 0 out 5 a b").unwrap();
    assert_eq!(no_run.invocation, None);
    assert_eq!(no_run.text, b"a b");
    for bad in [
        &b"L x.service - 1 0 0 sideways 5 text"[..],
        b"L x.service 12 1 0 0 out 5 text",
        b"L  - 1 0 0 out 5 text",
        b"X x.service - 1 0 0 out 5 text",
        b"L x.service - one 0 0 out 5 text",
    ] {
        assert_eq!(units::decode(bad), None, "{bad:?}");
    }
}

#[test]
fn a_unit_line_is_a_trusted_record() {
    use crate::units::{self, Stream};
    let now = Now {
        realtime_us: 1_700_000_010_000_000,
        monotonic_us: 10_000_000,
    };
    let record = units::record(&unit_line("port 2 failed to start", Stream::Err), now);
    assert_eq!(record.get("_SYSTEMD_UNIT"), Some(&b"web.service"[..]));
    assert_eq!(record.get("UNIT"), Some(&b"web.service"[..]));
    assert_eq!(record.get("SYSLOG_IDENTIFIER"), Some(&b"web"[..]));
    assert_eq!(record.get("_STREAM"), Some(&b"stderr"[..]));
    assert_eq!(record.get("_TRANSPORT"), Some(&b"stdout"[..]));
    assert_eq!(record.get("_PID"), Some(&b"812"[..]));
    assert_eq!(record.get("_UID"), Some(&b"1000"[..]));
    assert_eq!(
        record.get("_SYSTEMD_INVOCATION_ID"),
        Some(&b"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a"[..])
    );
    assert_eq!(record.get("_KERNEL_SEQNUM"), None);
    assert_eq!(record.message(), b"port 2 failed to start");
    // The level its words say, as the console copy's would be.
    assert_eq!(record.priority, 3);
    assert_eq!(crate::access::owner(&record), Some(1000));
}

#[test]
fn a_console_copy_pairs_with_its_trusted_line() {
    use crate::units::{self, Pairing, Stream};
    let now = Now {
        realtime_us: 1_700_000_010_000_000,
        monotonic_us: 10_000_000,
    };
    let line = unit_line("hello", Stream::Out);
    let copy_of = |line: &units::UnitLine, seq: u64| {
        let kernel = kmsg::KernelRecord {
            facility: 1,
            level: 6,
            seq,
            usec: 9_000_000,
            text: units::console_text(line),
        };
        ingest::from_kernel(&kernel, now)
    };
    let copy = copy_of(&line, 77);
    assert!(units::is_unit_copy(&copy));
    let mut pairing = Pairing::default();
    let mut out = Vec::new();
    // No trusted line: the copy comes back, to be stored alone.
    let copy = pairing.console(copy).unwrap_err();
    pairing.trusted(units::record(&line, now), 10_000_000, &mut out);
    assert!(out.is_empty());
    let paired = pairing.console(copy).unwrap();
    assert_eq!(paired.get("_KERNEL_SEQNUM"), Some(&b"77"[..]));
    assert_eq!(paired.get("_SYSTEMD_UNIT"), Some(&b"web.service"[..]));
    assert_eq!(paired.monotonic_us, 9_000_000);
    assert!(!pairing.is_waiting());
    // Another unit's copy, or another pid's, is not this line's.
    pairing.trusted(units::record(&line, now), 10_000_000, &mut out);
    let mut other = line.clone();
    other.pid = 813;
    assert!(pairing.console(copy_of(&other, 78)).is_err());
    // A trusted line whose copy never comes is stored alone, in time.
    pairing.expire(10_000_000 + units::PAIR_US - 1, &mut out);
    assert!(out.is_empty());
    pairing.expire(10_000_000 + units::PAIR_US, &mut out);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get("_KERNEL_SEQNUM"), None);
    // A copy stored alone first: the trusted line after it is not stored
    // again.
    let alone = copy_of(&line, 79);
    pairing.stored_alone(&alone, 20_000_000);
    out.clear();
    pairing.trusted(units::record(&line, now), 20_000_100, &mut out);
    assert!(out.is_empty());
    assert!(!pairing.is_waiting());
    assert_eq!(pairing.late, 1);
}

#[test]
fn records_go_to_their_owners_segments() {
    use crate::access::{self, Packet};
    let mut record = record(1, "x");
    assert_eq!(access::owner(&record), None);
    record.push("_UID", b"0");
    assert_eq!(access::owner(&record), None);
    let mut person = Record::new(6, 0, 0);
    person.push("_UID", b"1000");
    assert_eq!(access::owner(&person), Some(1000));
    let mut nobody = Record::new(6, 0, 0);
    nobody.push("_UID", b"65534");
    assert_eq!(access::owner(&nobody), None);
    // A sender's own `_UID` is never taken: only lode writes `_` fields.
    let mut forged = Record::new(6, 0, 0);
    forged.push_untrusted("_UID", b"1000");
    assert_eq!(access::owner(&forged), None);
    assert_eq!(access::user_dir(1000), "user-1000");
    assert_eq!(access::user_of_dir("user-1000"), Some(1000));
    assert_eq!(access::user_of_dir("user-"), None);
    assert_eq!(access::user_of_dir("user-10x"), None);
    let path = "user-1000/system@ab-0000000000000001.fxj";
    assert_eq!(access::owner_of_path(path), Some(1000));
    assert_eq!(access::owner_of_path("system@ab-1.fxj"), None);
    assert_eq!(access::base_name(path), "system@ab-0000000000000001.fxj");
    assert_eq!(
        segment::class_of_name(access::base_name(path)),
        Some("system")
    );
    assert_eq!(
        access::temporary(path, ".sum.new"),
        "user-1000/.system@ab-0000000000000001.fxj.sum.new"
    );
    assert_eq!(access::temporary("a.fxj", ".new"), ".a.fxj.new");
    let file = Packet::File(String::from(path));
    assert_eq!(Packet::decode(&file.encode()), Some(file));
    assert_eq!(Packet::decode(b"END 3\n"), Some(Packet::End(3)));
    for bad in [
        &b"F ../etc/shadow\n"[..],
        b"F user-1000//x\n",
        b"F \n",
        b"F x",
        b"END x\n",
    ] {
        assert_eq!(Packet::decode(bad), None, "{bad:?}");
    }
}

#[test]
fn a_summary_keeps_invocations_and_older_readers_ignore_them() {
    use crate::summary::Summary;
    let mut one = record(1, "a");
    one.push(
        "_SYSTEMD_INVOCATION_ID",
        b"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a",
    );
    let summary = Summary::decode(&Summary::of(&[one]).encode()).unwrap();
    let wanted = filter::parse("invocation = \"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\"").unwrap();
    let other = filter::parse("invocation = \"00000000000000000000000000000000\"").unwrap();
    assert!(summary.may_match(&wanted));
    assert!(!summary.may_match(&other));
    // A field number past what a reader knows is ignored, not misread.
    let mut values = summary.values.clone().unwrap();
    values.push((200, Vec::from(&b"future"[..]), 1));
    let later = Summary {
        values: Some(values),
        ..summary
    };
    let read = Summary::decode(&later.encode()).unwrap();
    assert!(read.may_match(&wanted));
}

#[test]
fn grep_finds_what_pcre2_finds_for_a_shells_patterns() {
    use crate::regex::Regex;
    let yes = |pattern: &str, text: &str| {
        Regex::new(pattern, false)
            .unwrap()
            .is_match(text.as_bytes())
    };
    assert!(yes("fail", "port 2 failed to start"));
    assert!(!yes("^fail", "port 2 failed to start"));
    assert!(yes("^port \\d+ fail(ed|ure)", "port 2 failed to start"));
    assert!(yes("start$", "port 2 failed to start"));
    assert!(!yes("port$", "port 2 failed to start"));
    assert!(yes("a.c", "xxabcxx"));
    assert!(!yes("a.c", "a\nc"));
    assert!(yes("colou?r", "color"));
    assert!(yes("colou?r", "colour"));
    assert!(yes("^a{2,3}$", "aaa"));
    assert!(!yes("^a{2,3}$", "aaaa"));
    assert!(yes("^a{2,}$", "aaaaa"));
    assert!(yes("x{y", "x{y"));
    assert!(yes("[[:digit:]]+ms", "took 120ms"));
    assert!(yes("[^a-z ]", "abc 1"));
    assert!(!yes("[^a-z ]", "abc def"));
    assert!(yes("\\bfps\\b", "trace:fps 60"));
    assert!(!yes("\\bfps\\b", "trace:fpsx 60"));
    assert!(yes("a.b", "aéb"));
    assert!(yes("\\x41", "A"));
    assert!(yes("(?:ab)+c", "ababc"));
    assert!(yes("1\\.5", "1.5"));
    assert!(!yes("1\\.5", "135"));
    assert!(yes("", "anything"));
    assert!(yes("a|", "b"));
    // Folded: journalctl's smart case is the caller's.
    let fold = Regex::new("error", true).unwrap();
    assert!(fold.is_match(b"An ERROR here"));
    assert!(Regex::new("(?i)ERROR", false).unwrap().is_match(b"error"));
    assert!(Regex::new("[A-Z]x", true).unwrap().is_match(b"ax"));
    assert!(crate::regex::has_upper("Error"));
    assert!(!crate::regex::has_upper("error 2"));
    // Linear: what makes a backtracking matcher hang is quick here.
    let nested = Regex::new("(a*)*b", false).unwrap();
    assert!(!nested.is_match("a".repeat(5000).as_bytes()));
    let many = Regex::new("(a|aa)+$", false).unwrap();
    assert!(many.is_match("a".repeat(5000).as_bytes()));
    // Not UTF-8: one character that only a negated class takes.
    assert!(yes("a[^b]c", "a\u{fffd}c"));
    assert!(Regex::new("a[^b]c", false).unwrap().is_match(b"a\xffc"));
    // What PCRE2 has and lode does not is refused, never matched wrongly.
    for pattern in [
        "(a)\\1", "(?=x)", "a++", "(ab", "ab)", "[ab", "*a", "a{3,2}", "\\q", "a{2000}",
    ] {
        let error = Regex::new(pattern, false).unwrap_err();
        assert!(error.at <= pattern.len(), "{pattern}: {error:?}");
    }
}

#[test]
fn a_zone_names_its_time() {
    let zone = Zone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
    // 2026-07-01 12:00 UTC: summer; 2026-01-15: winter.
    assert_eq!(zone.abbreviation_at(1_782_907_200_000_000), "CEST");
    assert_eq!(zone.abbreviation_at(1_768_478_400_000_000), "CET");
    assert_eq!(Zone::utc().abbreviation_at(0), "UTC");
    assert_eq!(Zone::posix("<+03>-3").unwrap().abbreviation_at(0), "+03");
}

#[test]
fn json_pretty_short_full_and_the_rest_print_as_journalctl_does() {
    let mut one = record(7, "hello \u{8}\u{c}\u{7f}");
    one.boot_id = [0x11; 16];
    one.host = alloc::rc::Rc::from("box");
    one.push("SYSLOG_PID", b"42");
    one.push("BLOB", &[0xff, 0x01]);
    let mut out = Vec::new();
    write(&one, Output::JsonPretty, &Zone::utc(), &mut out);
    let text = String::from_utf8(out).unwrap();
    let cursor = cursor::of(&one);
    assert_eq!(
        text,
        alloc::format!(
            "{{\n\t\"__CURSOR\" : \"{cursor}\",\n\t\"__REALTIME_TIMESTAMP\" : \"1700000000000007\",\n\t\"__MONOTONIC_TIMESTAMP\" : \"7\",\n\t\"__SEQNUM\" : \"7\",\n\t\"_BOOT_ID\" : \"11111111111111111111111111111111\",\n\t\"PRIORITY\" : \"6\",\n\t\"FERRIX_LEVEL\" : \"6\",\n\t\"_HOSTNAME\" : \"box\",\n\t\"MESSAGE\" : \"hello \\b\\f\u{7f}\",\n\t\"SYSLOG_IDENTIFIER\" : \"test\",\n\t\"SYSLOG_PID\" : \"42\",\n\t\"BLOB\" : [\n\t\t255,\n\t\t1\n\t]\n}}\n"
        )
    );
    // The one-line forms carry the same object.
    let mut compact = Vec::new();
    write(&one, Output::Json, &Zone::utc(), &mut compact);
    let mut seq = Vec::new();
    write(&one, Output::JsonSeq, &Zone::utc(), &mut seq);
    assert_eq!(seq.first(), Some(&0x1e));
    assert_eq!(&seq[1..], compact.as_slice());
    let mut sse = Vec::new();
    write(&one, Output::JsonSse, &Zone::utc(), &mut sse);
    assert!(sse.starts_with(b"data: {"));
    assert!(sse.ends_with(b"}\n\n"));
    // 2023-11-14 22:13:20 UTC was a Tuesday.
    let zone = Zone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
    let mut full = Vec::new();
    write(&one, Output::ShortFull, &zone, &mut full);
    assert_eq!(
        String::from_utf8(full).unwrap(),
        "Tue 2023-11-14 23:13:20 CET box test[42]: hello \\x08\\x0c\\x7f\n"
    );
    let mut unix = Vec::new();
    write(&one, Output::ShortUnix, &Zone::utc(), &mut unix);
    assert_eq!(
        String::from_utf8(unix).unwrap(),
        "1700000000.000007 box test[42]: hello \\x08\\x0c\\x7f\n"
    );
    for name in [
        "json-pretty",
        "json-seq",
        "json-sse",
        "short-full",
        "short-unix",
    ] {
        assert!(Output::named(name).is_some(), "{name}");
    }
}

#[test]
fn sizes_and_spans_read_and_print_as_systemd_does() {
    use crate::format::bytes_text;
    use crate::retention::{size_bytes, span_seconds};
    assert_eq!(size_bytes("500"), Some(500));
    assert_eq!(size_bytes("1K"), Some(1024));
    assert_eq!(size_bytes("100M"), Some(100 << 20));
    assert_eq!(size_bytes("1.5G"), Some(3 << 29));
    assert_eq!(size_bytes("2x"), None);
    assert_eq!(span_seconds("2weeks"), Some(1_209_600));
    assert_eq!(span_seconds("1d 12h"), Some(129_600));
    assert_eq!(span_seconds("90min"), Some(5_400));
    assert_eq!(span_seconds("30"), Some(30));
    assert_eq!(span_seconds("1fortnight"), None);
    assert_eq!(bytes_text(0), "0B");
    assert_eq!(bytes_text(1023), "1023B");
    assert_eq!(bytes_text(1024), "1.0K");
    assert_eq!(bytes_text(1536), "1.5K");
    assert_eq!(bytes_text(8 << 20), "8.0M");
    // The tenth is cut, not rounded: 1.99 MiB is 1.9M, as systemd says.
    assert_eq!(bytes_text((2 << 20) - 10_000), "1.9M");
    assert_eq!(bytes_text(3 << 30), "3.0G");
}

#[test]
fn alerts_read_and_fire_once_a_flood() {
    use crate::alert::{self, Action, Alerts};
    let text = "\
# a comment
limit 100/2s where id = noisy
alert flood rate > 3/s where id = \"chrome*\" notify \"%s is flooding the log (%c in a second)\"
alert where priority <= crit start crit-notify@%n.service
alert named-badly rate > x/s notify \"t\"
";
    // The limiter's parser leaves alert lines to the alerts', and the
    // alerts' leaves the limiter's.
    assert_eq!(
        crate::limit::parse_rules(&text.replace("alert named-badly rate > x/s notify \"t\"\n", ""))
            .unwrap()
            .len(),
        1
    );
    let error = alert::parse(text).unwrap_err();
    assert_eq!(error.line, 5);
    let alerts =
        alert::parse(&text.replace("alert named-badly rate > x/s notify \"t\"\n", "")).unwrap();
    assert_eq!(alerts.len(), 2);
    assert_eq!(alerts[0].name, "flood");
    assert_eq!(alerts[0].rate, Some((3, 1_000_000)));
    assert_eq!(alerts[1].name, "alert-4");
    assert_eq!(
        alerts[1].action,
        Action::Start(String::from("crit-notify@%n.service"))
    );
    let mut watching = Alerts::new(alerts);
    let mut fired = Vec::new();
    let chrome = |n: u64| {
        let mut record = record(n, "frame");
        record
            .fields
            .retain(|field| field.name != "SYSLOG_IDENTIFIER");
        record.push("SYSLOG_IDENTIFIER", b"chrome");
        record
    };
    // Three a second is not over three; the fourth is, once.
    for n in 0..3 {
        watching.observe(&chrome(n), 1_000_000 + n, &mut fired);
    }
    assert!(fired.is_empty());
    for n in 3..50 {
        watching.observe(&chrome(n), 1_000_000 + n, &mut fired);
    }
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].source, "chrome");
    assert_eq!(
        fired[0].action,
        Action::Notify(String::from("chrome is flooding the log (4 in a second)"))
    );
    assert_eq!(
        fired[0].line().unwrap(),
        "alert flood chrome chrome is flooding the log (4 in a second)\n"
    );
    let said = fired[0].record(1_700_000_000_000_000, 5);
    assert_eq!(said.get("ALERT"), Some(&b"flood"[..]));
    // Quiet for ten minutes for that source, then again.
    fired.clear();
    for n in 0..10 {
        watching.observe(&chrome(n), 300_000_000 + n, &mut fired);
    }
    assert!(fired.is_empty());
    for n in 0..10 {
        watching.observe(&chrome(n), 1_000_000 + alert::DEDUPE_US + n, &mut fired);
    }
    assert_eq!(fired.len(), 1);
    // A crit record starts the unit, `%n` expanded; the alert's own record
    // never feeds an alert.
    fired.clear();
    let mut crit = record(1, "disk on fire");
    crit.priority = 2;
    watching.observe(&crit, 5, &mut fired);
    assert_eq!(
        fired[0].action,
        Action::Start(String::from("crit-notify@alert-4.service"))
    );
    assert_eq!(fired[0].line(), None);
    let mut own = fired[0].record(0, 5);
    own.priority = 2;
    fired.clear();
    watching.observe(&own, 6, &mut fired);
    assert!(fired.is_empty());
}

#[test]
fn a_summary_counts_each_sources_lines_and_bytes() {
    use crate::summary::Summary;
    let mut records = Vec::new();
    for n in 1..=3 {
        records.push(record(n, "abcd"));
    }
    let mut other = record(4, "xy");
    other.push("UNIT", b"web.service");
    records.push(other);
    let summary = Summary::decode(&Summary::of(&records).encode()).unwrap();
    let sources = summary.sources.unwrap();
    assert!(sources.contains(&(Vec::from(&b"test"[..]), 3, 12)));
    assert!(sources.contains(&(Vec::from(&b"web.service"[..]), 1, 2)));
    // A summary written before the section reads, with no sources.
    let older = Summary {
        sources: None,
        ..Summary::of(&records)
    };
    assert_eq!(Summary::decode(&older.encode()).unwrap().sources, None);
}

#[test]
fn every_named_field_is_named_once_and_the_rest_are_other() {
    use crate::fields::{FieldGroup, NAMES, name_of, order_of};
    for (at, known) in NAMES.iter().enumerate() {
        assert!(
            valid_name(known.key) || known.key.starts_with("__"),
            "{}",
            known.key
        );
        assert_eq!(order_of(known.key), at, "{} once", known.key);
    }
    let named = |key: &str| name_of(key).map(|f| (f.name, f.group));
    assert_eq!(
        named("_SYSTEMD_UNIT"),
        Some(("Service", FieldGroup::Origin))
    );
    assert_eq!(named("CRASH_PC"), Some(("Code address", FieldGroup::Crash)));
    assert_eq!(named("__CURSOR"), Some(("Cursor", FieldGroup::Storage)));
    assert_eq!(
        named("__REALTIME_TIMESTAMP"),
        Some(("Time", FieldGroup::Time))
    );
    assert_eq!(
        named("MESSAGE_ID"),
        Some(("Message type", FieldGroup::Message))
    );
    assert_eq!(named("SOMETHING_ELSE"), None);
    assert_eq!(order_of("SOMETHING_ELSE"), NAMES.len());
}

#[test]
fn the_kernels_identity_line_names_a_crashed_process() {
    let who = console::identity(
        b"  signal   pid 4284 is steamwebhelper exe /home/ferrix/steam/steamwebhelper (deleted) at /home/ferrix/steam/libcef.so+0x1a01a1 cmdline ./steamwebhelper --type=gpu-process",
    )
    .unwrap();
    assert_eq!(who.pid, 4284);
    assert_eq!(who.comm, "steamwebhelper");
    assert_eq!(who.exe, "/home/ferrix/steam/steamwebhelper (deleted)");
    assert_eq!(
        who.module.as_deref(),
        Some("/home/ferrix/steam/libcef.so+0x1a01a1")
    );
    assert_eq!(who.cmdline, "./steamwebhelper --type=gpu-process");
    let anonymous = console::identity(b"  signal   pid 9 is a.out exe ? at ? cmdline ").unwrap();
    assert_eq!(anonymous.module, None);
    assert!(
        console::identity(b"  signal   pid 9 ended by signal 11 at 0x0, pc 0x1: Fault").is_none()
    );
    // Neither is it a crash line.
    assert!(console::crash(b"  signal   pid 9 is a.out exe ? at ? cmdline ").is_none());
}

#[test]
fn events_conf_switches_kinds_and_a_person_overrides_them() {
    use crate::events::Events;
    let mut events = Events::default();
    assert!(!events.on("window-opened"));
    assert!(events.on("app-error"));
    assert_eq!(events.level("app-error"), 4);
    events
        .apply(
            "# system\nwindow-opened on info\napp-error on err\n",
            "/etc/ferrix/lode.d/events.conf",
        )
        .unwrap();
    assert!(events.on("window-opened"));
    assert_eq!(events.level("app-error"), 3);
    events
        .apply("window-opened off\n", "~/.config/ferrix/lode/events.conf")
        .unwrap();
    assert!(!events.on("window-opened"));
    assert!(
        events
            .table()
            .contains("window-opened    off  info     hyprix   ~/.config")
    );
    // A file with one bad line changes nothing.
    let before = events.clone();
    assert_eq!(
        events
            .apply("app-error off\nwindows on\n", "x")
            .unwrap_err()
            .line,
        2
    );
    assert_eq!(events, before);
    assert!(events.apply("app-error maybe\n", "x").is_err());
    assert!(events.apply("app-error on loud\n", "x").is_err());
}

#[test]
fn an_echoed_stream_line_is_kept_once_as_the_streams() {
    use crate::units::{self, Echoes, Stream};
    let header = crate::protocols::stream_header(b"waybar\n\n6\n1\n0\n0\n1\n")
        .unwrap()
        .0;
    assert!(header.echoed);
    let echoed = crate::protocols::stream_line(&header, b"bar ready", 1, 2);
    assert_eq!(echoed.get("FERRIX_ECHOED"), Some(&b"1"[..]));
    let mut echoes = Echoes::default();
    let mut out = Vec::new();
    echoes.echoed(echoed, 10, &mut out);
    let now = Now {
        realtime_us: 1_700_000_010_000_000,
        monotonic_us: 10_000_000,
    };
    let mut line = units::record(&unit_line("bar ready", Stream::Out), now);
    line.push("_KERNEL_SEQNUM", b"40");
    let kept = echoes.unit_line(line, 10, true).unwrap();
    assert_eq!(kept.get("SYSLOG_IDENTIFIER"), Some(&b"waybar"[..]));
    assert_eq!(kept.get("_KERNEL_SEQNUM"), Some(&b"40"[..]));
    // Another unit line passes as it is with no echoing stream open; with
    // one open it is held for its echo, and kept when that comes later.
    let other = units::record(&unit_line("something else", Stream::Out), now);
    assert_eq!(
        echoes
            .unit_line(other.clone(), 10, false)
            .unwrap()
            .get("SYSLOG_IDENTIFIER"),
        Some(&b"web"[..])
    );
    let mut late = other;
    late.push("_KERNEL_SEQNUM", b"41");
    assert!(echoes.unit_line(late, 10, true).is_none());
    echoes.echoed(
        crate::protocols::stream_line(&header, b"something else", 1, 2),
        11,
        &mut out,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].get("_KERNEL_SEQNUM"), Some(&b"41"[..]));
    assert_eq!(out[0].get("SYSLOG_IDENTIFIER"), Some(&b"waybar"[..]));
    out.clear();
    echoes.echoed(
        crate::protocols::stream_line(&header, b"lonely", 1, 2),
        10,
        &mut out,
    );
    echoes.expire(10 + units::PAIR_US, &mut out);
    assert_eq!(out.len(), 1);
}

#[test]
fn blobs_pack_read_back_and_sweep() {
    use crate::blob::{self, Reference, Sweep};
    let big: Vec<u8> = (0..20_000_u32)
        .flat_map(|n| (n % 251).to_le_bytes())
        .collect();
    let small = b"maps: 3 lines".to_vec();
    // Small parts are fields; big ones blobs, named by their hash.
    let ((name, value), none) = blob::part("MAPS", "text", small.clone());
    assert_eq!(
        (name.as_str(), value.as_slice(), none),
        ("MAPS", &small[..], None)
    );
    let ((name, value), some) = blob::part("SCREENSHOT", "qoi", big.clone());
    assert_eq!(name, "BLOB_SCREENSHOT");
    let (id, bytes) = some.unwrap();
    assert_eq!(id, blob::id_of(&big));
    let reference = Reference::parse(&value).unwrap();
    assert_eq!(reference.size, big.len() as u64);
    assert_eq!(reference.kind, "qoi");
    // A pack of two, scanned and read back; a torn tail ends the scan.
    let other = vec![7_u8; 9000];
    let mut pack = blob::PACK_MAGIC.to_vec();
    pack.extend(blob::encode_entry(&id, "qoi", &bytes));
    pack.extend(blob::encode_entry(&blob::id_of(&other), "text", &other));
    let entries = blob::scan(&pack);
    assert_eq!(entries.len(), 2);
    assert!(entries[1].length < 9000, "packed");
    assert_eq!(blob::read(&pack, entries[0].offset).unwrap().1, big);
    assert_eq!(
        blob::read(&pack, entries[1].offset).unwrap(),
        (String::from("text"), other)
    );
    let mut torn = pack.clone();
    torn.truncate(torn.len() - 3);
    assert_eq!(blob::scan(&torn).len(), 1);
    // The index reads back; a damaged one does not.
    let index = blob::encode_index(&entries);
    assert_eq!(
        blob::decode_index(&index).unwrap()[1],
        (entries[1].id, entries[1].offset)
    );
    let mut bad = index;
    bad[20] ^= 1;
    assert_eq!(blob::decode_index(&bad), None);
    // A record names its blob; a summary lists it for the sweep's marks.
    let mut crash = record(1, "crashed");
    crash.push(&name, &value);
    assert_eq!(blob::references(&crash)[0].1.id, id);
    let summary =
        crate::summary::Summary::decode(&crate::summary::Summary::of(&[crash]).encode()).unwrap();
    assert_eq!(summary.blobs, vec![id]);
    // Mark and sweep.
    let mut live = alloc::collections::BTreeSet::new();
    assert_eq!(blob::sweep(&entries, &live, true), Sweep::Keep);
    assert_eq!(blob::sweep(&entries, &live, false), Sweep::Remove);
    let _ = live.insert(entries[1].id);
    assert!(matches!(blob::sweep(&entries, &live, false), Sweep::Rewrite(kept) if kept.len() == 1));
    let _ = live.insert(id);
    assert_eq!(blob::sweep(&entries, &live, false), Sweep::Keep);
}

#[test]
fn a_crash_storm_is_five_tombstones_and_a_few_counters() {
    use crate::tombstone::{self, Tombstones};
    let boot = [0x22; 16];
    let now = Now {
        realtime_us: 1_700_000_000_000_000,
        monotonic_us: 5,
    };
    let mut tombstones = Tombstones::new(5);
    let crash = |pid: u32| {
        let mut record = record(1, "signal pid ended");
        for (name, value) in [
            ("CRASH_PID", alloc::format!("{pid}")),
            ("CRASH_COMM", String::from("steamwebhelper")),
            ("CRASH_PROGRAM", String::from("/steam/steamwebhelper")),
            ("CRASH_MODULE", String::from("/steam/libcef.so+0x1a01a1")),
            ("CRASH_SIGNAL", String::from("SIGTRAP")),
        ] {
            record.push(name, value.as_bytes());
        }
        record
    };
    // The process's last lines are linked by cursor.
    for seq in 10..70 {
        tombstones.stored(4284, seq);
    }
    let first = tombstones.for_crash(&crash(4284), 70, &boot, now).unwrap();
    assert!(tombstone::is_tombstone(&first));
    assert_eq!(first.get_str("TOMBSTONE_COMM"), Some("steamwebhelper"));
    assert_eq!(
        first.get_str("TOMBSTONE_TOP_FRAME"),
        Some("/steam/libcef.so+0x1a01a1")
    );
    assert_eq!(
        first.get_str("TOMBSTONE_OF").map(String::from),
        Some(tombstone::cursor(70, &boot))
    );
    assert_eq!(
        first.get_str("TOMBSTONE_TAIL_FIRST").map(String::from),
        Some(tombstone::cursor(20, &boot))
    );
    assert_eq!(
        first.get_str("TOMBSTONE_TAIL_LAST").map(String::from),
        Some(tombstone::cursor(69, &boot))
    );
    // 146 crashes of one signature: five tombstones and four counters.
    let mut full = 1;
    let mut repeats = Vec::new();
    for n in 2..=146_u64 {
        if let Some(record) = tombstones.for_crash(&crash(5000 + n as u32), 100 + n, &boot, now) {
            if record.get("TOMBSTONE").is_some() {
                full += 1;
            } else {
                repeats.push(record.get_str("TOMBSTONE_REPEATS").map(String::from));
            }
        }
    }
    assert_eq!(full, 5);
    assert_eq!(
        repeats,
        ["10", "20", "40", "80"]
            .map(|n| Some(String::from(n)))
            .to_vec()
    );
    // Another signature has its own five.
    let mut other = crash(1);
    other.push("CRASH_SIGNAL", b"SIGSEGV");
    other
        .fields
        .retain(|field| !(field.name == "CRASH_SIGNAL" && field.value == b"SIGTRAP"));
    assert!(
        tombstones
            .for_crash(&other, 900, &boot, now)
            .unwrap()
            .get("TOMBSTONE")
            .is_some()
    );
}

#[test]
fn the_blob_quota_is_a_retention_line() {
    use crate::retention::{self, BLOB_QUOTA};
    assert_eq!(retention::blob_quota(""), BLOB_QUOTA);
    let text = "class errors where priority <= err keep 1y\nblobs max 64M\n";
    assert_eq!(retention::blob_quota(text), 64 << 20);
    assert_eq!(retention::parse(text).unwrap().len(), 1);
}
