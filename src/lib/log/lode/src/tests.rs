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
                line("noisy.service", 6, &alloc::format!("tick {n}"), n, n),
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
