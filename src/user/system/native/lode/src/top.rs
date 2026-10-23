//! `lode top` and `lode alerts` (`docs/LOGGING.md` §7.4).
//!
//! `lode top` lists who wrote the most -- per source, as the limiter counts
//! sources -- with records, bytes of message and a share, for a boot (this
//! one unless `-b` says), from the sealed segments' summaries without
//! reading their records, and from the records of the segments still being
//! written. `--live [SECONDS]` prints, every few seconds, who wrote what in
//! those seconds, a rate: where a flood is coming from now. `lode alerts`
//! follows the alerts lode says, a line each, for a box run or a script.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::format::bytes_text;
use ferrix_lode_core::limit;
use ferrix_lode_core::segment;

use crate::cli::{self, Known, Options, complain, out};
use crate::{sys, zone};

/// What `lode top` counts of a source.
#[derive(Debug, Clone, Default)]
struct Talker {
    /// The source.
    name: Vec<u8>,
    /// Its records.
    records: u64,
    /// The bytes of their messages.
    bytes: u64,
}

/// Add `records` and `bytes` to `name`'s count in `talkers`.
fn count(talkers: &mut Vec<Talker>, name: &[u8], records: u64, bytes: u64) {
    match talkers.iter_mut().find(|talker| talker.name == name) {
        Some(talker) => {
            talker.records += records;
            talker.bytes += bytes;
        }
        None => talkers.push(Talker {
            name: Vec::from(name),
            records,
            bytes,
        }),
    }
}

/// The table: the `rows` biggest, most records first, `rate` dividing each
/// by seconds.
fn table(mut talkers: Vec<Talker>, rows: usize, seconds: Option<u64>) -> Vec<u8> {
    talkers.sort_by(|a, b| {
        b.records
            .cmp(&a.records)
            .then(b.bytes.cmp(&a.bytes))
            .then(a.name.cmp(&b.name))
    });
    let total: u64 = talkers
        .iter()
        .map(|talker| talker.records)
        .sum::<u64>()
        .max(1);
    let mut text = match seconds {
        None => alloc::format!(
            "{:<32} {:>10} {:>9} {:>6}\n",
            "SOURCE",
            "RECORDS",
            "BYTES",
            "SHARE"
        ),
        Some(_) => alloc::format!(
            "{:<32} {:>10} {:>9} {:>6}\n",
            "SOURCE",
            "RECORDS/S",
            "BYTES/S",
            "SHARE"
        ),
    };
    for talker in talkers.iter().take(rows) {
        let per = seconds.unwrap_or(1).max(1);
        let share = talker.records * 1000 / total;
        text.push_str(&alloc::format!(
            "{:<32} {:>10} {:>9} {:>3}.{}%\n",
            String::from_utf8_lossy(&talker.name),
            talker.records / per,
            bytes_text(talker.bytes / per),
            share / 10,
            share % 10
        ));
    }
    text.into_bytes()
}

/// `lode top [-b ...] [--since ...] [-n ROWS] [filters]`, or with
/// `--live [SECONDS]` the rates now.
pub(crate) fn top(words: &[String]) -> i32 {
    let mut rest: Vec<String> = Vec::new();
    let mut live = None;
    let mut at = 0;
    while let Some(word) = words.get(at) {
        at += 1;
        if word == "--live" {
            let seconds = words
                .get(at)
                .and_then(|next| next.parse::<u64>().ok())
                .filter(|&seconds| seconds > 0);
            if seconds.is_some() {
                at += 1;
            }
            live = Some(seconds.unwrap_or(2));
        } else {
            rest.push(word.clone());
        }
    }
    let mut options = match cli::parse(&rest) {
        Ok(options) => options,
        Err(why) => return cli::usage(&why),
    };
    let _ = options.boot.get_or_insert_with(String::new);
    let rows = options.lines.unwrap_or(20);
    let source = match cli::directories(&options) {
        Ok(source) => source,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    match live {
        Some(seconds) => rates(&options, &source, rows, seconds),
        None => totals(&options, &source, rows),
    }
}

/// The boot `options` name among `segments`.
fn boot_of(options: &Options, segments: &[Known]) -> Result<Option<[u8; 16]>, String> {
    options
        .boot
        .as_ref()
        .map(|word| cli::chosen_boot(word, &cli::boots(segments)))
        .transpose()
}

/// `lode top`: a boot's sources.
fn totals(options: &Options, source: &cli::Source, rows: usize) -> i32 {
    let segments = cli::known(source);
    let boot = match boot_of(options, &segments) {
        Ok(boot) => boot,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let zone = zone::reader();
    let window = match cli::window(options, &zone) {
        Ok(window) => window,
        Err(why) => return cli::usage(&why),
    };
    let filtered = options.expr.is_some() || !options.matches.is_empty() || options.grep.is_some();
    let mut talkers = Vec::new();
    for each in cli::candidates(&segments, boot, options, window) {
        // A sealed segment wholly in the window, with nothing to filter,
        // is counted from its summary alone.
        let inside = each.summary.as_ref().is_some_and(|summary| {
            window.0.is_none_or(|since| summary.times.0 >= since)
                && window.1.is_none_or(|until| summary.times.1 <= until)
        });
        if !filtered
            && inside
            && let Some(sources) = each
                .summary
                .as_ref()
                .and_then(|summary| summary.sources.as_ref())
        {
            for (name, records, bytes) in sources {
                count(&mut talkers, name, *records, *bytes);
            }
            continue;
        }
        for record in cli::records_of(each) {
            if cli::passes(options, &record, window.0, window.1) {
                count(
                    &mut talkers,
                    &limit::source(&record),
                    1,
                    record.message().len() as u64,
                );
            }
        }
    }
    out(&table(talkers, rows, None));
    cli::release(&segments);
    0
}

/// `lode top --live SECONDS`: every `seconds`, who wrote what since the
/// last look, per second.
fn rates(options: &Options, source: &cli::Source, rows: usize, seconds: u64) -> i32 {
    let mut places: Vec<(String, usize)> = Vec::new();
    let mut first = true;
    loop {
        let segments = cli::known(source);
        let boot = boot_of(options, &segments).ok().flatten();
        let mut talkers = Vec::new();
        for each in segments
            .iter()
            .filter(|each| boot.is_none_or(|boot| boot == each.header.boot_id))
            .filter(|each| each.summary.is_none())
        {
            let start = places
                .iter()
                .find(|(name, _)| *name == each.segment.name)
                .map(|(_, offset)| *offset);
            let from = start.unwrap_or_else(|| each.header.encode().len());
            let Some(bytes) = crate::store::bytes(&each.segment, from) else {
                continue;
            };
            let (records, used) = segment::entries(&bytes);
            places.retain(|(name, _)| *name != each.segment.name);
            places.push((each.segment.name.clone(), from + used));
            // What was there before the first look is not this interval's.
            if first && start.is_none() {
                continue;
            }
            for record in records {
                if cli::passes(options, &record, None, None) {
                    count(
                        &mut talkers,
                        &limit::source(&record),
                        1,
                        record.message().len() as u64,
                    );
                }
            }
        }
        cli::release(&segments);
        if !first {
            let now = sys::clock_us(sys::CLOCK_REALTIME);
            let mut block = Vec::from(
                alloc::format!(
                    "-- {} records/s over the last {seconds} s, at {}\n",
                    talkers.iter().map(|talker| talker.records).sum::<u64>() / seconds,
                    stamp(now)
                )
                .as_bytes(),
            );
            block.extend_from_slice(&table(talkers, rows, Some(seconds)));
            out(&block);
        }
        first = false;
        sys::sleep_ms(seconds * 1000);
    }
}

/// `14:01:02`, the reader's local time.
fn stamp(realtime_us: u64) -> String {
    let zone = zone::reader();
    let when = ferrix_lode_core::time::Civil::at(realtime_us, zone.offset_at(realtime_us));
    alloc::format!("{:02}:{:02}:{:02}", when.hour, when.minute, when.second)
}

/// `lode alerts`: every alert lode says from now on, a line each, as long
/// as lode runs.
pub(crate) fn alerts() -> i32 {
    let fd = match cli::connect(crate::alerting::SOCKET) {
        Ok(fd) => fd,
        Err(error) => {
            complain(&alloc::format!(
                "lode: {}: errno {} (is lode running?)",
                String::from_utf8_lossy(crate::alerting::SOCKET),
                error.0
            ));
            return 1;
        }
    };
    let mut buf = [0_u8; 4096];
    loop {
        match sys::read(fd, &mut buf) {
            Ok(0) => return 0,
            Ok(got) => out(buf.get(..got).unwrap_or_default()),
            Err(error) if error.0 == sys::EINTR => {}
            Err(_) => return 1,
        }
    }
}
