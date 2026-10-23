//! The reader: `lode`, or `journalctl` through a link, started from a shell
//! (`docs/LOGGING.md` §9).
//!
//! It reads the segment files itself, as journalctl does; nothing it does
//! waits on the service. Its arguments come from `/proc/self/cmdline`, since
//! a native program's entry is given its bootstrap register, not `argv`.

use alloc::string::String;
use alloc::vec::Vec;

use ferrix_lode_core::filter::{self, Expr, Op, Test};
use ferrix_lode_core::format::{self, Output};
use ferrix_lode_core::ingest;
use ferrix_lode_core::record::{Record, UNSPECIFIED, level};
use ferrix_lode_core::segment::{self, Header};
use ferrix_lode_core::summary::Summary;
use ferrix_lode_core::time::{self, Civil, Zone};
use ferrix_lode_core::{console, cursor};

use crate::store::{self, Dir, Segment};
use crate::{sys, zone};

/// What `lode --help` says.
const HELP: &str = "\
lode [OPTIONS...] [FIELD=VALUE...]   read the log (also as journalctl)
lode mark TEXT...                    write a mark into the log
lode verify [--kernel] [-b ...]      check the store
lode rotate                          seal every class's segment, begin new ones
lode session begin ID seat=S uid=U pid=P scope=SCOPE unit=UNIT token=HEX
lode session end ID                  what a seat's owner tells lode (root's)
COMMAND | lode cat [-t ID] [-p PRIO] [--journal]
                                     its lines into the log, as systemd-cat's
lode bench RECORDS [DIR]             time a unit's last 50 lines with and without summaries
lode upload [--config FILE]          send the log on to collectors (lode-upload.service)
lode upload-stub PORT FILE           a collector on 127.0.0.1:PORT that keeps what it is sent (a test peer)
lode provide-stub TOKEN [SEAT]       a screenshot provider that sends a 2x2 picture (a test peer)
lode delete --where EXPR [-b ...]    remove the matching records (root's)
lode levels [-b ...] [filters]       records with no level, and with a guessed one, per source

  -b, --boot[=ID|N]      a boot: 0 or none this one, -1 the one before, or its id
      --list-boots       the boots the store holds
  -k, --dmesg            the kernel's own records
  -u, --unit UNIT        a unit's records (globs; 'name' is name.service)
  -t, --identifier ID    a program's records;  -T ID leaves one out
  -s, --session ID       a session's records
  -p, --priority P[..Q]  at most P (a level's name or number), or P to Q; never a
                         record with no level unless asked: -p err,unspecified
  -g, --grep TEXT        records whose message has TEXT (globs; any case if lower case)
  -S, --since TIME       -U, --until TIME (now, today, -5min, 2026-10-23 14:01, @EPOCH)
      --since-mark TEXT  --until-mark TEXT: between the latest `lode mark`s that match
      --around TEXT DUR  every record within DUR (30s, 2min) of one whose message has TEXT
      --where EXPR       the filter language: unit = \"hyprix*\" and not priority > warning
  -n, --lines N          the last N;  -r, --reverse newest first
  -f, --follow           go on printing what is stored after
  -o, --output FORMAT    human short short-iso short-iso-precise short-precise
                         short-monotonic with-unit cat json export verbose
      --after-cursor C   after the record C names;  --show-cursor prints the last's
  -D, --directory DIR    read DIR instead of /var/log/journal and /run/log/journal
      --utc              times in UTC
";

/// The options of a read.
#[derive(Debug, Default)]
struct Options {
    /// `-b`: the boot asked for, as written; `Some("")` for this one.
    boot: Option<String>,
    /// `--list-boots`.
    list_boots: bool,
    /// What every record must match.
    expr: Option<Expr>,
    /// `FIELD=VALUE` arguments, grouped by `+`.
    matches: Vec<Vec<(String, Vec<u8>)>>,
    /// `-g`.
    grep: Option<Vec<u8>>,
    /// `--since`, `--until`, as written.
    since: Option<String>,
    /// `--until`.
    until: Option<String>,
    /// `--since-mark`, `--until-mark`.
    since_mark: Option<Vec<u8>>,
    /// `--until-mark`.
    until_mark: Option<Vec<u8>>,
    /// `--around TEXT DURATION`.
    around: Option<(Vec<u8>, u64)>,
    /// `-n`.
    lines: Option<usize>,
    /// `-r`.
    reverse: bool,
    /// `-f`.
    follow: bool,
    /// `-o`.
    output: Option<Output>,
    /// `--after-cursor`.
    after: Option<u64>,
    /// `--show-cursor`.
    show_cursor: bool,
    /// `-D`.
    directory: Option<Vec<u8>>,
    /// `--utc`.
    utc: bool,
    /// `verify --kernel`.
    kernel_gaps: bool,
    /// `lode levels`: the level table instead of the records.
    levels: bool,
}

/// Run the reader; the exit status.
pub(crate) fn run() -> i32 {
    let args = arguments();
    let mut rest: &[String] = args.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        Some("mark") => return mark(rest.get(1..).unwrap_or_default()),
        Some("rotate") => return rotate(),
        Some("session") => return announce(rest.get(1..).unwrap_or_default()),
        Some("cat") => return cat(rest.get(1..).unwrap_or_default()),
        Some("bench") => return bench(rest.get(1..).unwrap_or_default()),
        Some("provide-stub") => return provide_stub(rest.get(1..).unwrap_or_default()),
        Some("upload") => return crate::upload::run(rest.get(1..).unwrap_or_default()),
        Some("upload-stub") => return crate::upload::stub(rest.get(1..).unwrap_or_default()),
        Some("levels") => {
            return match parse(rest.get(1..).unwrap_or_default()) {
                Ok(mut options) => {
                    options.levels = true;
                    options.lines = None;
                    options.follow = false;
                    show(&options)
                }
                Err(why) => usage(&why),
            };
        }
        Some("delete") => {
            return match parse(rest.get(1..).unwrap_or_default()) {
                Ok(options) => delete(&options),
                Err(why) => usage(&why),
            };
        }
        Some("serve") => {
            complain("lode: `serve` is the service, which init starts (lode.service)");
            return 1;
        }
        Some("verify") => {
            rest = rest.get(1..).unwrap_or_default();
            return match parse(rest) {
                Ok(mut options) => {
                    let _ = options.output.get_or_insert(Output::Human);
                    verify(&options)
                }
                Err(why) => usage(&why),
            };
        }
        _ => {}
    }
    match parse(rest) {
        Ok(options) if options.list_boots => list_boots(&options),
        Ok(options) => show(&options),
        Err(why) if why.is_empty() => {
            out(HELP.as_bytes());
            0
        }
        Err(why) => usage(&why),
    }
}

/// The process's arguments, from `/proc/self/cmdline`.
fn arguments() -> Vec<String> {
    let bytes = sys::read_small(b"/proc/self/cmdline").unwrap_or_default();
    let mut args: Vec<String> = bytes
        .split(|&byte| byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    if args.last().is_some_and(String::is_empty) {
        let _ = args.pop();
    }
    args
}

/// Say `why` and how to ask for help; status 2.
pub(crate) fn usage(why: &str) -> i32 {
    complain(&alloc::format!("lode: {why}\nTry 'lode --help'."));
    2
}

/// A line on standard error.
pub(crate) fn complain(text: &str) {
    let _ = sys::write_all(2, text.as_bytes());
    let _ = sys::write_all(2, b"\n");
}

/// Bytes on standard output; a reader that went away ends lode quietly.
pub(crate) fn out(bytes: &[u8]) {
    if sys::write_all(1, bytes).is_err() {
        ferrix_rt::exit(0);
    }
}

/// The arguments, taken one at a time.
struct Words<'a> {
    /// All of them.
    args: &'a [String],
    /// The next.
    at: usize,
}

impl<'a> Words<'a> {
    /// The next argument.
    fn next(&mut self) -> Option<&'a String> {
        let word = self.args.get(self.at)?;
        self.at += 1;
        Some(word)
    }

    /// The next argument, if `take` wants it.
    fn next_if(&mut self, take: impl Fn(&str) -> bool) -> Option<&'a String> {
        let word = self.args.get(self.at).filter(|word| take(word))?;
        self.at += 1;
        Some(word)
    }
}

/// An option's value: given with `=`, or the next argument.
fn value(
    flag: &str,
    inline: Option<&str>,
    words: &mut Words<'_>,
    what: &str,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(String::from(value)),
        None => words
            .next()
            .cloned()
            .ok_or_else(|| alloc::format!("{flag} needs {what}")),
    }
}

/// Read the arguments. `Err("")` asks for the help text.
fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut words = Words { args, at: 0 };
    let mut group: Vec<(String, Vec<u8>)> = Vec::new();
    while let Some(arg) = words.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        if selection(&mut options, flag, inline, &mut words)?
            || presentation(&mut options, flag, inline, &mut words)?
        {
            continue;
        }
        if flag == "+" {
            options.matches.push(core::mem::take(&mut group));
        } else if let Some((name, value)) = arg.split_once('=')
            && !arg.starts_with('-')
        {
            group.push((String::from(name), Vec::from(value.as_bytes())));
        } else {
            return Err(alloc::format!("{arg}: an option lode does not know"));
        }
    }
    if !group.is_empty() {
        options.matches.push(group);
    }
    Ok(options)
}

/// The options that choose records. Answers whether `flag` was one.
fn selection(
    options: &mut Options,
    flag: &str,
    inline: Option<&str>,
    words: &mut Words<'_>,
) -> Result<bool, String> {
    match flag {
        "-h" | "--help" => return Err(String::new()),
        "-b" | "--boot" => {
            let given = inline
                .map(String::from)
                .or_else(|| words.next_if(is_boot_word).cloned());
            options.boot = Some(given.unwrap_or_default());
        }
        "--list-boots" => options.list_boots = true,
        "-k" | "--dmesg" => {
            let _ = options.boot.get_or_insert_with(String::new);
            add(
                &mut options.expr,
                Expr::Test(Test::field("_TRANSPORT", Op::Eq, b"kernel")),
            );
        }
        "-u" | "--unit" => add(
            &mut options.expr,
            unit_expr(&value(flag, inline, words, "a unit")?),
        ),
        "-t" | "--identifier" => {
            let id = value(flag, inline, words, "an identifier")?;
            add(
                &mut options.expr,
                Expr::Test(Test::field("id", Op::Eq, id.as_bytes())),
            );
        }
        "-T" | "--exclude-identifier" => {
            let id = value(flag, inline, words, "an identifier")?;
            add(
                &mut options.expr,
                Expr::Test(Test::field("id", Op::Ne, id.as_bytes())),
            );
        }
        "-s" | "--session" => {
            let id = value(flag, inline, words, "a session id")?;
            add(
                &mut options.expr,
                Expr::Test(Test::field("session", Op::Eq, id.as_bytes())),
            );
        }
        "-p" | "--priority" => add(
            &mut options.expr,
            filter::priority_range(&value(flag, inline, words, "a priority")?)?,
        ),
        "-g" | "--grep" => {
            options.grep = Some(value(flag, inline, words, "a pattern")?.into_bytes());
        }
        "-S" | "--since" => options.since = Some(value(flag, inline, words, "a time")?),
        "-U" | "--until" => options.until = Some(value(flag, inline, words, "a time")?),
        "--since-mark" => {
            options.since_mark = Some(value(flag, inline, words, "a mark")?.into_bytes());
        }
        "--until-mark" => {
            options.until_mark = Some(value(flag, inline, words, "a mark")?.into_bytes());
        }
        "--around" => {
            let text = value(flag, inline, words, "a pattern and a duration")?;
            let span = value(flag, None, words, "a duration")?;
            let micros = duration(&span).ok_or_else(|| alloc::format!("{span}: not a duration"))?;
            options.around = Some((text.into_bytes(), micros));
        }
        "--where" => {
            let text = value(flag, inline, words, "an expression")?;
            let expr = filter::parse(&text)
                .map_err(|error| alloc::format!("--where: {} at byte {}", error.why, error.at))?;
            add(&mut options.expr, expr);
        }
        "--after-cursor" | "--cursor" => {
            let text = value(flag, inline, words, "a cursor")?;
            options.after =
                Some(cursor::seqnum(&text).ok_or_else(|| alloc::format!("{text}: not a cursor"))?);
        }
        "-D" | "--directory" => {
            options.directory = Some(value(flag, inline, words, "a directory")?.into_bytes());
        }
        "--kernel" => options.kernel_gaps = true,
        _ => return Ok(false),
    }
    Ok(true)
}

/// The options that shape the output. Answers whether `flag` was one.
fn presentation(
    options: &mut Options,
    flag: &str,
    inline: Option<&str>,
    words: &mut Words<'_>,
) -> Result<bool, String> {
    match flag {
        "-n" | "--lines" => {
            let count = value(flag, inline, words, "a number")?;
            let count = count
                .trim_start_matches('+')
                .parse()
                .map_err(|_| alloc::format!("{count}: not a number"))?;
            options.lines = Some(count);
        }
        "-e" | "--pager-end" => {
            let _ = options.lines.get_or_insert(1000);
        }
        "-r" | "--reverse" => options.reverse = true,
        "-f" | "--follow" => options.follow = true,
        "-o" | "--output" => {
            let name = value(flag, inline, words, "a format")?;
            options.output =
                Some(Output::named(&name).ok_or_else(|| alloc::format!("{name}: no such format"))?);
        }
        "--show-cursor" => options.show_cursor = true,
        "--utc" => options.utc = true,
        "--no-pager" | "-a" | "--all" | "-q" | "--quiet" | "-l" | "--full" | "--no-hostname"
        | "-m" | "--merge" => {}
        _ => return Ok(false),
    }
    Ok(true)
}

/// Whether the word after `-b` is its value: a number or a boot id.
fn is_boot_word(word: &str) -> bool {
    let digits = word.trim_start_matches(['-', '+']);
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        || segment::parse_id(word).is_some()
}

/// `expr` and `more`.
fn add(expr: &mut Option<Expr>, more: Expr) {
    *expr = Some(match expr.take() {
        Some(existing) => existing.and(more),
        None => more,
    });
}

/// `-u UNIT`: the unit, and `UNIT.service` for a name with no type.
fn unit_expr(unit: &str) -> Expr {
    let exact = Expr::Test(Test::field("unit", Op::Eq, unit.as_bytes()));
    if unit.contains('.') || unit.contains('*') {
        return exact;
    }
    let service = alloc::format!("{unit}.service");
    exact.or(Expr::Test(Test::field("unit", Op::Eq, service.as_bytes())))
}

/// `30s`, `2min`, `1h`, `500ms`, a bare number of seconds: microseconds.
fn duration(text: &str) -> Option<u64> {
    if let Some(millis) = text.strip_suffix("ms") {
        return millis.parse::<u64>().ok()?.checked_mul(1000);
    }
    let micros = time::parse(&alloc::format!("+{text}"), 0, &Zone::utc())?;
    Some(micros)
}

/// The store directories a read looks in.
fn directories(options: &Options) -> Result<Vec<Dir>, String> {
    let paths: Vec<&[u8]> = match &options.directory {
        Some(path) => alloc::vec![path.as_slice()],
        None => alloc::vec![store::PERSISTENT, store::VOLATILE],
    };
    let mut dirs = Vec::new();
    let mut refused = None;
    for path in paths {
        match Dir::open(path) {
            Ok(dir) => dirs.push(dir),
            Err(error) if error.0 == sys::EACCES => refused = Some(path),
            Err(_) => {}
        }
    }
    if dirs.is_empty() {
        return Err(match refused {
            Some(path) => alloc::format!(
                "{}: permission denied (root and members of adm read the log)",
                String::from_utf8_lossy(path)
            ),
            None => String::from("no log here: neither /var/log/journal nor /run/log/journal"),
        });
    }
    Ok(dirs)
}

/// A segment with its header, and its summary when it has one.
pub(crate) struct Known {
    /// The segment.
    pub(crate) segment: Segment,
    /// Its header.
    pub(crate) header: Header,
    /// Its summary: a sealed segment's (step 5).
    pub(crate) summary: Option<Summary>,
}

impl Known {
    /// The last sequence number it can hold: its summary's, or, being
    /// written, any.
    pub(crate) fn last_seq(&self) -> u64 {
        self.summary
            .as_ref()
            .map_or(u64::MAX, |summary| summary.seqs.1)
    }
}

/// Every segment with a readable header, oldest first.
pub(crate) fn known(dirs: &[Dir]) -> Vec<Known> {
    store::all_segments(dirs)
        .into_iter()
        .filter_map(|segment| {
            let header = store::header(&segment)?;
            let summary = store::summary(&segment);
            Some(Known {
                segment,
                header,
                summary,
            })
        })
        .collect()
}

/// The segments a read may find a match in: `boot`'s, and of those the
/// ones whose summary does not rule the options out (step 5).
fn candidates<'a>(
    segments: &'a [Known],
    boot: Option<[u8; 16]>,
    options: &Options,
    (since, until): (Option<u64>, Option<u64>),
) -> Vec<&'a Known> {
    segments
        .iter()
        .filter(|each| boot.is_none_or(|boot| boot == each.header.boot_id))
        .filter(|each| {
            each.summary.as_ref().is_none_or(|summary| {
                summary.overlaps(since, until)
                    && options.after.is_none_or(|after| summary.seqs.1 > after)
                    && options
                        .expr
                        .as_ref()
                        .is_none_or(|expr| summary.may_match(expr))
            })
        })
        .collect()
}

/// The records of one segment.
pub(crate) fn records_of(each: &Known) -> Vec<Record> {
    store::bytes(&each.segment, 0)
        .and_then(|bytes| segment::scan(&bytes, 0))
        .map(|scanned| {
            scanned
                .records
                .into_iter()
                .map(|found| found.record)
                .collect()
        })
        .unwrap_or_default()
}

/// The newest `count` records `options` lets through, reading the
/// candidate segments newest first and stopping once no segment left can
/// hold a newer match than the `count`th found (step 5: a unit's last
/// lines cost the same on a store of any size).
fn newest(
    candidates: &[&Known],
    options: &Options,
    window: (Option<u64>, Option<u64>),
    count: usize,
) -> Vec<Record> {
    let mut order: Vec<&&Known> = candidates.iter().collect();
    order.sort_by_key(|each| core::cmp::Reverse(each.last_seq()));
    let mut found: Vec<Record> = Vec::new();
    for each in order {
        if found.len() >= count
            && let Some(threshold) = found.get(count - 1).map(|record| record.seqnum)
            && each.last_seq() < threshold
        {
            break;
        }
        found.extend(
            records_of(each)
                .into_iter()
                .filter(|record| passes(options, record, window.0, window.1)),
        );
        found.sort_by_key(|record| core::cmp::Reverse(record.seqnum));
        found.truncate(count);
    }
    found.reverse();
    found
}

/// The boots, oldest first: each boot's id once, in the order of its first
/// segment.
fn boots(segments: &[Known]) -> Vec<[u8; 16]> {
    let mut order: Vec<[u8; 16]> = Vec::new();
    for each in segments {
        if !order.contains(&each.header.boot_id) {
            order.push(each.header.boot_id);
        }
    }
    order
}

/// The boot `-b` names.
fn chosen_boot(word: &str, boots: &[[u8; 16]]) -> Result<[u8; 16], String> {
    if let Some(id) = segment::parse_id(word) {
        return Ok(id);
    }
    let offset: i64 = if word.is_empty() {
        0
    } else {
        word.parse()
            .map_err(|_| alloc::format!("{word}: not a boot"))?
    };
    let count = i64::try_from(boots.len()).unwrap_or(0);
    let index = if offset > 0 {
        offset - 1
    } else {
        count - 1 + offset
    };
    usize::try_from(index)
        .ok()
        .and_then(|index| boots.get(index))
        .copied()
        .ok_or_else(|| alloc::format!("no boot {word} in the log ({count} kept)"))
}

/// Every record of `segments` that `boot` (if any) allows.
fn load(segments: &[Known], boot: Option<[u8; 16]>) -> Vec<Record> {
    let mut records = Vec::new();
    for each in segments {
        if boot.is_some_and(|boot| boot != each.header.boot_id) {
            continue;
        }
        if let Some(bytes) = store::bytes(&each.segment, 0)
            && let Some(scanned) = segment::scan(&bytes, 0)
        {
            records.extend(scanned.records.into_iter().map(|found| found.record));
        }
    }
    records.sort_by_key(|record| record.seqnum);
    records
}

/// What `-g` matches: a lower-case pattern in any case.
fn grep_matches(pattern: &[u8], message: &[u8]) -> bool {
    if pattern.iter().any(u8::is_ascii_uppercase) {
        return filter::contains(pattern, message);
    }
    filter::contains(pattern, &message.to_ascii_lowercase())
}

/// Whether `record` passes every option but the marks, `--around` and `-n`.
fn passes(options: &Options, record: &Record, since: Option<u64>, until: Option<u64>) -> bool {
    if options.after.is_some_and(|after| record.seqnum <= after)
        || since.is_some_and(|since| record.realtime_us < since)
        || until.is_some_and(|until| record.realtime_us > until)
    {
        return false;
    }
    if let Some(expr) = &options.expr
        && !expr.matches(record)
    {
        return false;
    }
    if let Some(pattern) = &options.grep
        && !grep_matches(pattern, record.message())
    {
        return false;
    }
    options.matches.is_empty()
        || options
            .matches
            .iter()
            .any(|group| field_group(group, record))
}

/// journalctl's matches: the same field or'd, different fields and'd.
fn field_group(group: &[(String, Vec<u8>)], record: &Record) -> bool {
    let mut names: Vec<&str> = group.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.iter().all(|name| {
        group
            .iter()
            .filter(|(each, _)| each == name)
            .any(|(_, value)| match *name {
                // Not stored as fields: the level is the record's own.
                "PRIORITY" | "FERRIX_LEVEL" => {
                    let known = record.priority != UNSPECIFIED || *name == "FERRIX_LEVEL";
                    known && record.level_text().as_bytes() == value.as_slice()
                }
                _ => record.get(name) == Some(value.as_slice()),
            })
    })
}

/// The times `--since` and `--until` say.
fn window(options: &Options, zone: &Zone) -> Result<(Option<u64>, Option<u64>), String> {
    let now = sys::clock_us(sys::CLOCK_REALTIME);
    let parse = |text: &Option<String>| -> Result<Option<u64>, String> {
        text.as_ref()
            .map(|text| {
                time::parse(text, now, zone).ok_or_else(|| alloc::format!("{text}: not a time"))
            })
            .transpose()
    };
    Ok((parse(&options.since)?, parse(&options.until)?))
}

/// Cut `records` to the marks' window and `--around`'s.
fn narrow(options: &Options, all: &[Record], records: &mut Vec<Record>) {
    let mark_at = |text: &[u8], after: u64| {
        all.iter()
            .filter(|record| record.seqnum >= after)
            .filter(|record| {
                record
                    .get("MARK")
                    .is_some_and(|mark| filter::glob(text, mark))
            })
            .map(|record| record.seqnum)
            .collect::<Vec<u64>>()
    };
    let mut low = 0;
    if let Some(text) = &options.since_mark {
        low = mark_at(text, 0).last().copied().unwrap_or(u64::MAX);
    }
    let high = options
        .until_mark
        .as_ref()
        .and_then(|text| mark_at(text, low).first().copied())
        .unwrap_or(u64::MAX);
    records.retain(|record| record.seqnum >= low && record.seqnum <= high);
    if let Some((pattern, span)) = &options.around {
        let centres: Vec<u64> = all
            .iter()
            .filter(|record| grep_matches(pattern, record.message()))
            .map(|record| record.realtime_us)
            .collect();
        records.retain(|record| {
            centres
                .iter()
                .any(|&centre| record.realtime_us.abs_diff(centre) <= *span)
        });
    }
}

/// `lode [options]`: print the records asked for, then follow if asked.
fn show(options: &Options) -> i32 {
    let zone = if options.utc {
        Zone::utc()
    } else {
        zone::reader()
    };
    let (since, until) = match window(options, &zone) {
        Ok(window) => window,
        Err(why) => return usage(&why),
    };
    let dirs = match directories(options) {
        Ok(dirs) => dirs,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = known(&dirs);
    let boot = match &options.boot {
        Some(word) => match chosen_boot(word, &boots(&segments)) {
            Ok(boot) => Some(boot),
            Err(why) => {
                complain(&alloc::format!("lode: {why}"));
                return 1;
            }
        },
        None => None,
    };
    let wanted = options.lines.or(options.follow.then_some(10));
    let whole =
        options.since_mark.is_some() || options.until_mark.is_some() || options.around.is_some();
    let (mut records, last) = match wanted {
        Some(count) if !whole => {
            let chosen = candidates(&segments, boot, options, (since, until));
            let records = newest(&chosen, options, (since, until), count);
            let last = segments
                .iter()
                .filter_map(|each| each.summary.as_ref().map(|summary| summary.seqs.1))
                .chain(records.last().map(|record| record.seqnum))
                .chain(
                    segments
                        .last()
                        .map(|each| records_of(each).last().map_or(0, |record| record.seqnum)),
                )
                .max()
                .unwrap_or(0);
            (records, last)
        }
        _ => {
            let all = load(&segments, boot);
            let mut records: Vec<Record> = all
                .iter()
                .filter(|record| passes(options, record, since, until))
                .cloned()
                .collect();
            narrow(options, &all, &mut records);
            if let Some(count) = wanted {
                let skip = records.len().saturating_sub(count);
                let _ = records.drain(..skip);
            }
            (records, all.last().map_or(0, |record| record.seqnum))
        }
    };
    if options.levels {
        out(&format::level_table(&records));
        return 0;
    }
    let mut last = last;
    if options.reverse {
        records.reverse();
    }
    let output = options.output.unwrap_or(Output::Human);
    let mut buffer = Vec::new();
    for record in &records {
        format::write(record, output, &zone, &mut buffer);
        flush_if_full(&mut buffer);
    }
    if options.show_cursor
        && let Some(record) = records.last()
    {
        buffer.extend_from_slice(alloc::format!("-- cursor: {}\n", cursor::of(record)).as_bytes());
    }
    out(&buffer);
    if options.follow {
        follow(options, &dirs, boot, &zone, output, &mut last);
    }
    0
}

/// Write `buffer` out once it holds 64 KiB.
fn flush_if_full(buffer: &mut Vec<u8>) {
    if buffer.len() >= 64 * 1024 {
        out(buffer);
        buffer.clear();
    }
}

/// `-f`: every quarter second, print what was stored after `last`, from
/// the newest two segments of each class, each read from where the last
/// look stopped.
fn follow(
    options: &Options,
    dirs: &[Dir],
    boot: Option<[u8; 16]>,
    zone: &Zone,
    output: Output,
    last: &mut u64,
) {
    let mut places: Vec<(String, usize)> = Vec::new();
    loop {
        let segments = known(dirs);
        let mut fresh: Vec<Record> = Vec::new();
        let mut seen: Vec<(&str, usize)> = Vec::new();
        for each in segments.iter().rev() {
            let class = segment::class_of_name(&each.segment.name).unwrap_or_default();
            let count = match seen.iter_mut().find(|(name, _)| *name == class) {
                Some((_, count)) => {
                    *count += 1;
                    *count
                }
                None => {
                    seen.push((class, 1));
                    1
                }
            };
            if count > 2 || boot.is_some_and(|boot| boot != each.header.boot_id) {
                continue;
            }
            let from = places
                .iter()
                .find(|(name, _)| *name == each.segment.name)
                .map_or_else(|| each.header.encode().len(), |(_, offset)| *offset);
            let Some(bytes) = store::bytes(&each.segment, from) else {
                continue;
            };
            let (records, used) = segment::entries(&bytes);
            let host: alloc::rc::Rc<str> = alloc::rc::Rc::from(each.header.host.as_str());
            fresh.extend(records.into_iter().map(|mut record| {
                record.boot_id = each.header.boot_id;
                record.host = alloc::rc::Rc::clone(&host);
                record
            }));
            places.retain(|(name, _)| *name != each.segment.name);
            places.push((each.segment.name.clone(), from + used));
        }
        fresh.sort_by_key(|record| record.seqnum);
        let mut buffer = Vec::new();
        for record in fresh {
            if record.seqnum <= *last {
                continue;
            }
            *last = record.seqnum;
            if passes(options, &record, None, None) {
                format::write(&record, output, zone, &mut buffer);
            }
        }
        out(&buffer);
        sys::sleep_ms(250);
    }
}

/// `--list-boots`.
fn list_boots(options: &Options) -> i32 {
    let zone = if options.utc {
        Zone::utc()
    } else {
        zone::reader()
    };
    let dirs = match directories(options) {
        Ok(dirs) => dirs,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = known(&dirs);
    let order = boots(&segments);
    let count = i64::try_from(order.len()).unwrap_or(0);
    let mut buffer = Vec::from(
        &b"IDX BOOT ID                           FIRST ENTRY                    LAST ENTRY\n"[..],
    );
    for (index, boot) in order.iter().enumerate() {
        let mine: Vec<&Known> = segments
            .iter()
            .filter(|each| each.header.boot_id == *boot)
            .collect();
        let first = mine.first().map_or(0, |each| each.header.created_us);
        let last = mine
            .iter()
            .filter_map(|each| each.summary.as_ref().map(|summary| summary.times.1))
            .max()
            .filter(|_| mine.last().is_some_and(|each| each.summary.is_some()));
        let last = last
            .or_else(|| {
                mine.last()
                    .and_then(|each| store::bytes(&each.segment, 0))
                    .and_then(|bytes| segment::scan(&bytes, 0))
                    .and_then(|scanned| {
                        scanned.records.last().map(|found| found.record.realtime_us)
                    })
            })
            .unwrap_or(first);
        let mut id = String::new();
        segment::push_hex(&mut id, boot);
        let offset = i64::try_from(index).unwrap_or(0) + 1 - count;
        buffer.extend_from_slice(
            alloc::format!(
                "{offset:>3} {id} {} {}\n",
                stamp(first, &zone),
                stamp(last, &zone)
            )
            .as_bytes(),
        );
    }
    out(&buffer);
    0
}

/// A time as `2026-10-23 14:01:02 +02:00`.
fn stamp(realtime_us: u64, zone: &Zone) -> String {
    let when = Civil::at(realtime_us, zone.offset_at(realtime_us));
    alloc::format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
        when.year,
        when.month,
        when.day,
        when.hour,
        when.minute,
        when.second,
        when.offset()
    )
}

/// `lode mark TEXT...`: a mark in the kernel log, which the service stores
/// with `MARK=TEXT`.
fn mark(words: &[String]) -> i32 {
    if words.is_empty() {
        return usage("mark needs a text");
    }
    let mut line = Vec::from(&b"<13>"[..]);
    line.extend_from_slice(console::MARK_PREFIX);
    line.extend_from_slice(words.join(" ").as_bytes());
    let written = sys::open(b"/dev/kmsg", sys::O_WRONLY).and_then(|fd| {
        let result = sys::write(fd, &line);
        sys::close(fd);
        result
    });
    match written {
        Ok(_) => 0,
        Err(error) => {
            complain(&alloc::format!(
                "lode: writing the mark to /dev/kmsg: errno {}",
                error.0
            ));
            1
        }
    }
}

/// `lode verify`: every segment's good length, and with `--kernel` the
/// chosen boot's kernel sequence, with any gap no record says (status 1).
fn verify(options: &Options) -> i32 {
    let dirs = match directories(options) {
        Ok(dirs) => dirs,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = known(&dirs);
    let mut buffer = Vec::new();
    let mut status = 0;
    for each in &segments {
        let Some(bytes) = store::bytes(&each.segment, 0) else {
            continue;
        };
        let Some(scanned) = segment::scan(&bytes, 0) else {
            buffer
                .extend_from_slice(alloc::format!("{}: no header\n", each.segment.name).as_bytes());
            status = 1;
            continue;
        };
        let torn = bytes.len() - scanned.good;
        buffer.extend_from_slice(
            alloc::format!(
                "{}: {} record(s), {} byte(s){}\n",
                each.segment.name,
                scanned.records.len(),
                bytes.len(),
                if torn > 0 {
                    alloc::format!(", {torn} byte(s) past the last good record")
                } else {
                    String::new()
                }
            )
            .as_bytes(),
        );
    }
    if options.kernel_gaps {
        let boot = chosen_boot(options.boot.as_deref().unwrap_or(""), &boots(&segments));
        let Ok(boot) = boot else {
            complain("lode: no boot to check");
            return 1;
        };
        let records = load(&segments, Some(boot));
        let refs: Vec<&Record> = records.iter().collect();
        let gaps = ingest::kernel_gaps(&refs);
        let restarts = records
            .iter()
            .filter(|record| record.get_str("MESSAGE_ID") == Some(ingest::MESSAGE_ID_RESTARTED))
            .count();
        buffer.extend_from_slice(
            alloc::format!(
                "kernel seq {}..{}: {} held, {} said lost, {} restart(s), unsaid gaps: {}\n",
                gaps.first.unwrap_or(0),
                gaps.last.unwrap_or(0),
                gaps.held,
                gaps.said_lost,
                restarts,
                if gaps.unsaid.is_empty() {
                    String::from("none")
                } else {
                    alloc::format!("{:?}", gaps.unsaid)
                }
            )
            .as_bytes(),
        );
        if !gaps.unsaid.is_empty() {
            status = 1;
        }
    }
    out(&buffer);
    status
}

/// Ask the service to seal every segment, through the kernel log it reads.
fn ask_rotation() -> Result<(), String> {
    let mut line = Vec::from(&b"<13>"[..]);
    line.extend_from_slice(crate::serve::ROTATE);
    line.extend_from_slice(b": requested");
    let fd = sys::open(b"/dev/kmsg", sys::O_WRONLY)
        .map_err(|error| alloc::format!("opening /dev/kmsg: errno {}", error.0))?;
    let written = sys::write(fd, &line);
    sys::close(fd);
    let _ = written.map_err(|error| alloc::format!("writing to /dev/kmsg: errno {}", error.0))?;
    // The service reads the request within a wakeup: a quarter second.
    sys::sleep_ms(600);
    Ok(())
}

/// `lode rotate`.
fn rotate() -> i32 {
    match ask_rotation() {
        Ok(()) => 0,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            1
        }
    }
}

/// `lode delete`: rotate, then rewrite every sealed segment holding a
/// record that matches, without it, and say in the log what was removed.
/// The newest segment of each class is left, since the service may be
/// writing it; the count of what it held that matched is said.
fn delete(options: &Options) -> i32 {
    if options.expr.is_none() && options.matches.is_empty() && options.grep.is_none() {
        return usage("delete needs --where, a match or -g: it never removes everything");
    }
    if let Err(why) = ask_rotation() {
        complain(&alloc::format!("lode: {why}"));
        return 1;
    }
    let zone = zone::reader();
    let (since, until) = match window(options, &zone) {
        Ok(window) => window,
        Err(why) => return usage(&why),
    };
    let dirs = match directories(options) {
        Ok(dirs) => dirs,
        Err(why) => {
            complain(&alloc::format!("lode: {why}"));
            return 1;
        }
    };
    let segments = known(&dirs);
    let boot = options
        .boot
        .as_ref()
        .and_then(|word| chosen_boot(word, &boots(&segments)).ok());
    let mut newest: Vec<&str> = Vec::new();
    let mut removed = 0;
    let mut rewritten = 0;
    let mut left = 0;
    for each in segments.iter().rev() {
        let class = segment::class_of_name(&each.segment.name).unwrap_or_default();
        let open = !newest.contains(&class);
        if open {
            newest.push(class);
        }
        if boot.is_some_and(|boot| boot != each.header.boot_id) {
            continue;
        }
        let Some(bytes) = store::bytes(&each.segment, 0) else {
            continue;
        };
        let matching = |record: &Record| passes(options, record, since, until);
        let Some((rest, count)) = segment::rewrite(&bytes, matching) else {
            continue;
        };
        if count == 0 {
            continue;
        }
        if open {
            left += count;
            continue;
        }
        // A compressed segment stays compressed.
        let rest = if store::is_compressed(each.segment.dir, &each.segment.name) {
            ferrix_lode_core::compress::compress(rest.clone()).unwrap_or(rest)
        } else {
            rest
        };
        match replace(&each.segment, &rest) {
            Ok(()) => {
                store::write_summary(each.segment.dir, &each.segment.name);
                removed += count;
                rewritten += 1;
            }
            Err(why) => {
                complain(&alloc::format!("lode: {}: {why}", each.segment.name));
                return 1;
            }
        }
    }
    let text = alloc::format!(
        "removed {removed} record(s) from {rewritten} segment(s) by uid {}; {left} in segments still being written left",
        sys::getuid()
    );
    let mut line = Vec::from(&b"<13>lode-delete: "[..]);
    line.extend_from_slice(text.as_bytes());
    if let Ok(fd) = sys::open(b"/dev/kmsg", sys::O_WRONLY) {
        let _ = sys::write(fd, &line);
        sys::close(fd);
    }
    out(alloc::format!("lode: {text}\n").as_bytes());
    0
}

/// Put `bytes` in place of `segment`: a new file beside it, synced, then
/// renamed over it, so a crash leaves the old one or the new one whole.
fn replace(segment: &Segment, bytes: &[u8]) -> Result<(), String> {
    let temporary = alloc::format!(".{}.new", segment.name);
    let _ = sys::unlinkat(segment.dir, temporary.as_bytes());
    let fd = sys::openat(
        segment.dir,
        temporary.as_bytes(),
        sys::O_WRONLY | sys::O_CREAT | sys::O_EXCL | sys::O_CLOEXEC,
        0o640,
    )
    .map_err(|error| alloc::format!("making its replacement: errno {}", error.0))?;
    let _ = sys::fchown(fd, crate::serve::UID, crate::serve::GID);
    let written = sys::write_all(fd, bytes).and_then(|()| sys::fdatasync(fd));
    sys::close(fd);
    written.map_err(|error| alloc::format!("writing its replacement: errno {}", error.0))?;
    sys::renameat(segment.dir, temporary.as_bytes(), segment.name.as_bytes())
        .map_err(|error| alloc::format!("putting its replacement in place: errno {}", error.0))
}

/// `lode session begin|end ...`: what a seat's owner other than `sessiond`
/// (a kiosk's starter, a test) tells lode, on its root-only socket.
fn announce(words: &[String]) -> i32 {
    let line = match words.split_first() {
        Some((verb, rest)) if verb == "begin" && !rest.is_empty() => {
            alloc::format!("SESSION {}\n", rest.join(" "))
        }
        Some((verb, [id])) if verb == "end" => alloc::format!("END {id}\n"),
        _ => {
            return usage(
                "session begin ID seat=S uid=U pid=P scope=SCOPE unit=UNIT token=HEX, or session end ID",
            );
        }
    };
    if ferrix_lode_core::session::parse(line.trim_end()).is_none() {
        return usage("that is not a session line lode takes");
    }
    let sent = connect(crate::seat::SOCKET).and_then(|fd| {
        let result = sys::write_all(fd, line.as_bytes());
        sys::close(fd);
        result
    });
    match sent {
        Ok(_) => 0,
        Err(error) => {
            complain(&alloc::format!(
                "lode: telling lode on {}: errno {} (root's, and lode must be running)",
                String::from_utf8_lossy(crate::seat::SOCKET),
                error.0
            ));
            1
        }
    }
}

/// Connect to a socket at `path`, blocking: the descriptor.
fn connect(path: &[u8]) -> Result<usize, sys::Errno> {
    use ferrix_rt::linux as rt;
    let fd = rt::socket(rt::AF_UNIX, rt::SOCK_STREAM, 0)?;
    let Some((address, length)) = rt::sockaddr_un(path) else {
        sys::close(fd);
        return Err(sys::Errno(22));
    };
    // SAFETY: `address` is a `sockaddr_un` of `length` bytes, read for the call.
    let connected = unsafe {
        rt::call(
            rt::numbers::CONNECT,
            [fd, address.as_ptr() as usize, length, 0, 0, 0],
        )
    };
    match connected {
        Ok(_) => Ok(fd),
        Err(error) => {
            sys::close(fd);
            Err(error)
        }
    }
}

/// Read frames from `fd` into `buf` until one is whole: it, or `None` at
/// the end or on a frame that does not read.
fn next_frame(fd: usize, buf: &mut Vec<u8>) -> Option<ferrix_lode_capture::Message> {
    use ferrix_lode_capture::Message;
    let mut chunk = [0_u8; 4096];
    loop {
        match Message::decode(buf) {
            Ok(Some((message, used))) => {
                let _ = buf.drain(..used);
                return Some(message);
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        match sys::read(fd, &mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(got) => buf.extend_from_slice(chunk.get(..got).unwrap_or_default()),
        }
    }
}

/// `lode provide-stub TOKEN [SEAT]`: the provider protocol's smallest peer,
/// for a test or a provider's author: registers with `TOKEN`, says whether
/// lode accepted it, then answers every request with a 2x2 picture.
fn provide_stub(words: &[String]) -> i32 {
    use ferrix_lode_capture::{FORMAT_QOI, Message, qoi};
    let Some(token) = words.first().and_then(|word| segment::parse_id(word)) else {
        return usage("provide-stub needs the session's token, 32 hex digits");
    };
    let seat = words
        .get(1)
        .cloned()
        .unwrap_or_else(|| String::from("seat0"));
    let mut fd = None;
    for _ in 0..100 {
        if let Ok(connected) = connect(b"/run/lode/capture") {
            fd = Some(connected);
            break;
        }
        sys::sleep_ms(100);
    }
    let Some(fd) = fd else {
        complain("lode: provide-stub: /run/lode/capture never answered (is Capture=yes?)");
        return 1;
    };
    let register = Message::Register {
        name: String::from("provide-stub"),
        seat,
        outputs: String::from("stub-1"),
        formats: FORMAT_QOI,
        token,
    };
    if sys::write_all(fd, &register.encode()).is_err() {
        return 1;
    }
    let mut buf = Vec::new();
    match next_frame(fd, &mut buf) {
        Some(Message::Accepted { .. }) => out(b"provide-stub: accepted\n"),
        _ => {
            out(b"provide-stub: refused\n");
            return 1;
        }
    }
    // Two rows of two: red, green; blue, white (XRGB in memory: B, G, R, X).
    let screen = [0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let picture = qoi::encode(2, 2, 8, &screen).unwrap_or_default();
    while let Some(message) = next_frame(fd, &mut buf) {
        if let Message::Capture {
            request, seqnum, ..
        } = message
        {
            let image = Message::Image {
                request,
                format: FORMAT_QOI,
                width: 2,
                height: 2,
                bytes: picture.clone(),
            };
            if sys::write_all(fd, &image.encode()).is_err() {
                break;
            }
            out(alloc::format!("provide-stub: sent record {seqnum}'s picture\n").as_bytes());
        }
    }
    0
}

/// `COMMAND | lode cat [-t ID] [-p PRIO] [--journal]`: standard input into
/// the log, a line a record -- through a stdout stream, as `systemd-cat`
/// sends it, or with `--journal` a native-protocol datagram a line.
fn cat(words: &[String]) -> i32 {
    let mut identifier = String::from("lode-cat");
    // No `-p`: no level, which the log shows as unspecified.
    let mut priority: Option<u8> = None;
    let mut journal = false;
    let mut at = 0;
    while let Some(word) = words.get(at) {
        at += 1;
        match word.as_str() {
            "-t" | "--identifier" => match words.get(at) {
                Some(value) => {
                    identifier.clone_from(value);
                    at += 1;
                }
                None => return usage("cat -t needs an identifier"),
            },
            "-p" | "--priority" => match words.get(at).and_then(|value| level(value)) {
                Some(value) if value != UNSPECIFIED => {
                    priority = Some(value);
                    at += 1;
                }
                _ => return usage("cat -p needs a priority"),
            },
            "--journal" => journal = true,
            other => {
                return usage(&alloc::format!(
                    "cat: {other}: pipe a command's output into `lode cat`"
                ));
            }
        }
    }
    let path: &[u8] = if journal {
        b"/run/systemd/journal/socket"
    } else {
        b"/run/systemd/journal/stdout"
    };
    let connected = if journal {
        connect_datagram(path)
    } else {
        connect(path)
    };
    let fd = match connected {
        Ok(fd) => fd,
        Err(error) => {
            complain(&alloc::format!(
                "lode: cat: {}: errno {}",
                String::from_utf8_lossy(path),
                error.0
            ));
            return 1;
        }
    };
    if !journal {
        // A stream's info is every unit's default and reads as no level,
        // so `-p info` is said on each line instead (`<6>`).
        let header = alloc::format!("{identifier}\n\n{}\n1\n0\n0\n0\n", priority.unwrap_or(6));
        if sys::write_all(fd, header.as_bytes()).is_err() {
            return 1;
        }
    }
    let mut held: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let got = match sys::read(0, &mut chunk) {
            Ok(0) | Err(_) => 0,
            Ok(got) => got,
        };
        held.extend_from_slice(chunk.get(..got).unwrap_or_default());
        let finished = got == 0;
        if !journal {
            let take = if finished {
                held.len()
            } else {
                held.iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |end| end + 1)
            };
            let send = info_said(priority, held.drain(..take).collect());
            if !send.is_empty() && sys::write_all(fd, &send).is_err() {
                return 1;
            }
        } else {
            while let Some(end) = held.iter().position(|&byte| byte == b'\n') {
                let line: Vec<u8> = held.drain(..=end).collect();
                send_native(
                    fd,
                    &identifier,
                    priority,
                    line.strip_suffix(b"\n").unwrap_or(&line),
                );
            }
            if finished && !held.is_empty() {
                let line = core::mem::take(&mut held);
                send_native(fd, &identifier, priority, &line);
            }
        }
        if finished {
            break;
        }
    }
    sys::close(fd);
    0
}

/// A stream's `chunk` of whole lines, each begun `<6>` when `-p info` was
/// asked for: a stream's own info is its default, and reads as no level.
fn info_said(priority: Option<u8>, chunk: Vec<u8>) -> Vec<u8> {
    if priority != Some(6) {
        return chunk;
    }
    let mut said = Vec::with_capacity(chunk.len() + 16);
    for line in chunk.split_inclusive(|&byte| byte == b'\n') {
        said.extend_from_slice(b"<6>");
        said.extend_from_slice(line);
    }
    said
}

/// One line as a native-protocol datagram on the connected `fd`.
fn send_native(fd: usize, identifier: &str, priority: Option<u8>, line: &[u8]) {
    let mut datagram = Vec::new();
    if let Some(priority) = priority {
        datagram.extend_from_slice(alloc::format!("PRIORITY={priority}\n").as_bytes());
    }
    datagram.extend_from_slice(alloc::format!("SYSLOG_IDENTIFIER={identifier}\n").as_bytes());
    if line.contains(&b'\n') {
        datagram.extend_from_slice(b"MESSAGE\n");
        datagram.extend_from_slice(&u64::try_from(line.len()).unwrap_or(0).to_le_bytes());
    } else {
        datagram.extend_from_slice(b"MESSAGE=");
    }
    datagram.extend_from_slice(line);
    datagram.push(b'\n');
    let _ = sys::write(fd, &datagram);
}

/// A datagram socket connected to `path`.
pub(crate) fn connect_datagram(path: &[u8]) -> Result<usize, sys::Errno> {
    use ferrix_rt::linux as rt;
    let fd = rt::socket(rt::AF_UNIX, 2, 0)?;
    let Some((address, length)) = rt::sockaddr_un(path) else {
        sys::close(fd);
        return Err(sys::Errno(22));
    };
    // SAFETY: `address` is a `sockaddr_un` of `length` bytes, read for the call.
    let connected = unsafe {
        rt::call(
            rt::numbers::CONNECT,
            [fd, address.as_ptr() as usize, length, 0, 0, 0],
        )
    };
    match connected {
        Ok(_) => Ok(fd),
        Err(error) => {
            sys::close(fd);
            Err(error)
        }
    }
}

/// `lode bench RECORDS [DIR]`: write a store of `RECORDS` records spread
/// over 200 units in 16 MiB segments, sealed and summarised, into `DIR`
/// (`/tmp/lode-bench`), then time `-u <a rare unit> -n 50` read through the
/// summaries and by reading every segment. Step 5's benchmark.
fn bench(words: &[String]) -> i32 {
    use ferrix_lode_core::record::Record;
    let Some(count) = words.first().and_then(|word| word.parse::<u64>().ok()) else {
        return usage("bench needs a number of records");
    };
    let path = words
        .get(1)
        .map_or(b"/tmp/lode-bench".to_vec(), |dir| dir.clone().into_bytes());
    let _ = sys::mkdirat(sys::AT_FDCWD, &path, 0o755);
    let Ok(dir) = Dir::open(&path) else {
        complain("lode: bench: no directory");
        return 1;
    };
    let header = |first: u64| Header {
        boot_id: [0xbe; 16],
        machine_id: [0; 16],
        first_seqnum: first,
        created_us: 1_700_000_000_000_000,
        host: String::from("bench"),
    };
    let mut bytes = header(1).encode();
    let mut first = 1;
    let write = |first: u64, bytes: &[u8]| {
        let name = segment::file_name("system", &[0xbe; 16], first);
        if let Ok(fd) = sys::openat(
            dir.fd,
            name.as_bytes(),
            sys::O_WRONLY | sys::O_CREAT | sys::O_TRUNC | sys::O_CLOEXEC,
            0o644,
        ) {
            let _ = sys::write_all(fd, bytes);
            sys::close(fd);
            store::write_summary(dir.fd, &name);
        }
    };
    for seq in 1..=count {
        // Unit 0 is rare: only in the first and the last thousand records.
        let unit = if seq <= 1000 || seq + 1000 > count {
            0
        } else {
            1 + seq % 199
        };
        let mut record = Record::new(6, 1_700_000_000_000_000 + seq, seq);
        record.seqnum = seq;
        record.push(
            "MESSAGE",
            alloc::format!("bench record {seq}, a line of about the length a driver writes")
                .as_bytes(),
        );
        record.push("UNIT", alloc::format!("unit{unit}.service").as_bytes());
        record.push("SYSLOG_IDENTIFIER", alloc::format!("unit{unit}").as_bytes());
        segment::encode_record(&record, &mut bytes);
        if bytes.len() >= 16 * 1024 * 1024 {
            write(first, &bytes);
            first = seq + 1;
            bytes = header(first).encode();
        }
    }
    write(first, &bytes);
    let dirs = [Dir { fd: dir.fd }];
    let options = Options {
        expr: Some(unit_expr("unit0.service")),
        ..Options::default()
    };
    let started = sys::clock_us(sys::CLOCK_MONOTONIC);
    let segments = known(&dirs);
    let chosen = candidates(&segments, None, &options, (None, None));
    let fast = newest(&chosen, &options, (None, None), 50);
    let summarised = sys::clock_us(sys::CLOCK_MONOTONIC) - started;
    let started = sys::clock_us(sys::CLOCK_MONOTONIC);
    let all = load(&segments, None);
    let slow: Vec<&Record> = all
        .iter()
        .filter(|record| passes(&options, record, None, None))
        .collect();
    let whole = sys::clock_us(sys::CLOCK_MONOTONIC) - started;
    let same = fast.len() == 50
        && slow.len() >= 50
        && slow
            .get(slow.len() - 50..)
            .is_some_and(|tail| tail.iter().zip(&fast).all(|(a, b)| a.seqnum == b.seqnum));
    out(alloc::format!(
        "bench: {count} records, {} segments; -u unit0 -n 50: {} ms through summaries, {} ms reading all; same answer: {}\n",
        segments.len(),
        summarised / 1000,
        whole / 1000,
        if same { "yes" } else { "no" }
    )
    .as_bytes());
    i32::from(!same)
}
