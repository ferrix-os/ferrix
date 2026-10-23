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
                level_prefix: true
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
        // And a level the kernel was given stands.
        let mut quiet = kernel(1, b"x: failed on purpose");
        quiet.level = 7;
        let record = ingest::from_kernel(&quiet, NOW);
        assert_eq!(record.priority, 7);
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
