//! The kernel log's boot check: what a reader of the ring is promised, on
//! small rings the check owns, and what the console records, on the log itself.
//!
//! On rings of the check's own, so that the properties can be driven to their
//! edges without writing 128 KiB into the boot's log:
//!
//! * **wrap**: a ring written past its length keeps its last `N - 1` bytes,
//!   in order;
//! * **overrun**: a reader that fell behind is moved to the oldest byte kept,
//!   and told exactly how many it lost;
//! * **partial read**: a reader with less room than there is to read takes
//!   what fits, and the next read goes on from there;
//! * **unread**: what `syslog(2)`'s `SIZE_UNREAD` answers, at each of those
//!   points;
//! * **two writers at once**, on two processors when there are two, while
//!   this task reads: the count is exact, every byte read is one the writers
//!   wrote, and every byte is either read or reported lost.
//!
//! On a ring and lines of the check's own, where the log's lines begin
//! ([`log::Lines`]): after a newline and where the priority changes, each
//! stamped once and with its priority; a line still being written handed to
//! no reader; a reader behind the lines kept, or behind the bytes kept, given
//! the first line kept whole.
//!
//! On the kernel log, that the console records what it sends and nothing
//! it was told not to: a task's write as a program's output is (without the
//! CR the port was sent), a line written the way a failure report is written
//! -- the path `console::write_panicking` takes once the panicking flag is
//! set, taken here without it -- and that a line sent with
//! `println_unlogged!` is on the port and not in the log.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};

use super::log::{self, Line, Lines, Read, Ring, Stamp};

/// The small ring wrap, overrun and partial reads are checked on.
const SMALL: usize = 64;

/// The ring two writers race on: small, so that they wrap it many times.
const RACE: usize = 256;

/// Bytes each racing writer records.
const RACE_BYTES: u64 = 20_000;

/// How long the racing writers may take, in nanoseconds.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// How far back in the kernel log the check looks for its own lines.
const WINDOW: usize = 4096;

static SMALL_RING: Ring<SMALL> = Ring::new();
static RACE_RING: Ring<RACE> = Ring::new();

/// Whether the racing writers may start: both wait for it, so that they
/// start together.
static GO: AtomicBool = AtomicBool::new(false);

/// What the check established, for the boot log.
#[derive(Debug)]
pub(crate) struct Report {
    /// Bytes the small ring kept after being written past its length.
    pub(crate) kept: usize,
    /// Bytes a reader that fell behind was told it lost.
    pub(crate) lost: u64,
    /// Bytes the two writers recorded between them.
    pub(crate) raced: u64,
    /// Bytes this task read while they did.
    pub(crate) read_racing: u64,
    /// Processors the writers ran on.
    pub(crate) processors: usize,
    /// Lines the check's own ring of lines began.
    pub(crate) lines: u64,
}

/// Run the check.
///
/// # Errors
///
/// The first property that did not hold, as a sentence.
///
/// Verifies: H.TRAP.13
pub(crate) fn run() -> Result<Report, &'static str> {
    let (kept, lost) = wrap_overrun_and_partial()?;
    let (read_racing, processors) = two_writers()?;
    console_records()?;
    let lines = lines_begin_and_are_found()?;
    grown_rings_keep_what_they_held()?;
    sizes_asked()?;
    Ok(Report {
        lines,
        kept,
        lost,
        raced: RACE_BYTES.saturating_mul(2),
        read_racing,
        processors,
    })
}

/// Wrap, overrun, a partial read and the unread count, on [`SMALL_RING`].
/// Answers the bytes kept and the bytes reported lost.
///
/// Verifies: L.console.26, L.console.27, L.console.28
fn wrap_overrun_and_partial() -> Result<(usize, u64), &'static str> {
    let ring = &SMALL_RING;
    let pattern: Vec<u8> = (0..=255u8).cycle().take(100).collect();
    ring.record(&pattern);
    let keep = SMALL - 1;
    if ring.unread(0) != keep as u64 {
        return Err(
            "the unread count of a ring written past its length is not its length less one",
        );
    }

    // A partial read from the start: the cursor jumps to the oldest byte
    // kept, the lost count says by how much, and ten bytes come back.
    let mut cursor = 0u64;
    let mut out = [0u8; 10];
    let read = ring.read(&mut cursor, &mut out);
    let oldest = pattern.len() - keep;
    if read.lost != oldest as u64 {
        return Err("a reader behind a wrapped ring was not told how many bytes it lost");
    }
    if read.copied != out.len() || pattern.get(oldest..oldest + out.len()) != Some(&out[..]) {
        return Err("a partial read did not give the oldest bytes kept, in order");
    }
    if ring.unread(cursor) != (keep - out.len()) as u64 {
        return Err("the unread count after a partial read is wrong");
    }

    // The rest, in one read with room to spare: from where the last stopped.
    let mut rest = [0u8; SMALL];
    let read = ring.read(&mut cursor, &mut rest);
    let resumed = pattern.get(oldest + out.len()..);
    if read.lost != 0 || resumed != rest.get(..read.copied) || ring.unread(cursor) != 0 {
        return Err("a read after a partial one did not resume where it stopped");
    }

    // Caught up, then overrun: 200 more bytes, 137 of them gone by the time
    // the reader comes back.
    ring.record(&[0xA5; 200]);
    let read = ring.read(&mut cursor, &mut rest);
    let lost_expected = 200 - keep as u64;
    let copied = rest.get(..read.copied).unwrap_or_default();
    if read.lost != lost_expected || read.copied != keep || copied.iter().any(|&b| b != 0xA5) {
        return Err("an overrun reader was not told exactly what it lost");
    }
    Ok((keep, read.lost.saturating_add(oldest as u64)))
}

/// Two writers on [`RACE_RING`], on two processors when there are two, with
/// this task reading as they write. Answers the bytes read meanwhile and how
/// many processors the writers had.
///
/// Verifies: L.console.25, L.console.29
fn two_writers() -> Result<(u64, usize), &'static str> {
    let online = crate::smp::topology().map_or(1, crate::smp::Topology::online);
    let (first, second) = if online >= 2 { (0, 1) } else { (0, 0) };
    GO.store(false, Ordering::SeqCst);
    let a = crate::sched::spawn_on(
        "log-a",
        writer,
        b'A'.into(),
        NICE_0_WEIGHT,
        first,
        only(first)?,
    )?;
    let b = crate::sched::spawn_on(
        "log-b",
        writer,
        b'B'.into(),
        NICE_0_WEIGHT,
        second,
        only(second)?,
    )?;
    GO.store(true, Ordering::SeqCst);

    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let mut cursor = 0u64;
    let mut accounted = 0u64;
    let mut read_racing = 0u64;
    loop {
        let done = a.is_dead() && b.is_dead();
        let read = drain(&mut cursor)?;
        accounted = accounted.saturating_add(read.lost + read.copied as u64);
        if !done {
            read_racing = read_racing.saturating_add(read.copied as u64);
        }
        if done && read.copied == 0 {
            break;
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("the log check's racing writers never finished");
        }
    }
    if RACE_RING.written() != RACE_BYTES * 2 {
        return Err("two writers racing lost a byte of the count");
    }
    if accounted != RACE_BYTES * 2 {
        return Err("a reader of two racing writers neither read nor was told it lost some bytes");
    }
    Ok((read_racing, if first == second { 1 } else { 2 }))
}

/// Read what [`RACE_RING`] has for `cursor`, and require every byte to be one
/// a writer wrote.
fn drain(cursor: &mut u64) -> Result<Read, &'static str> {
    let mut out = [0u8; 97];
    let read = RACE_RING.read(cursor, &mut out);
    let copied = out.get(..read.copied).unwrap_or_default();
    if copied.iter().any(|&byte| byte != b'A' && byte != b'B') {
        return Err("a reader of two racing writers read a byte neither wrote");
    }
    Ok(read)
}

/// One racing writer: [`RACE_BYTES`] of its letter, one record each.
fn writer(letter: usize) {
    let byte = u8::try_from(letter).unwrap_or(b'?');
    while !GO.load(Ordering::SeqCst) {
        core::hint::spin_loop();
    }
    for _ in 0..RACE_BYTES {
        RACE_RING.record(&[byte]);
    }
}

/// A set of the one processor `cpu`.
fn only(cpu: usize) -> Result<CpuSet, &'static str> {
    let mut set = CpuSet::empty();
    set.insert(cpu)
        .map_err(|_| "the log check names a processor out of range")?;
    Ok(set)
}

/// The ring the lines check records into: small, so that its bytes wrap.
const LINE_BYTES: usize = 64;

/// The lines it notes: fewer than the bytes hold, so that they wrap too.
const LINE_SLOTS: usize = 8;

static LINE_RING: Ring<LINE_BYTES> = Ring::new();
static LINE_STARTS: Lines<LINE_SLOTS> = Lines::new();

/// Record `bytes` at `priority` in the check's ring and lines, with `usec` as
/// the stamp any line they begin takes.
fn note(bytes: &[u8], priority: u8, usec: u64) {
    let now = move || usec;
    log::record_lines(
        &LINE_RING,
        &LINE_STARTS,
        bytes,
        priority,
        &mut Stamp::new(&now),
    );
}

/// Line `seq` of the check's lines, as a reader finds it now.
fn found(seq: u64) -> Option<Line> {
    LINE_STARTS.line(seq, LINE_RING.oldest(), LINE_RING.written())
}

/// Where lines begin, what they are stamped with, and what a reader is given,
/// on [`LINE_RING`] and [`LINE_STARTS`]. Answers how many lines began.
///
/// Verifies: L.console.50, L.console.51, L.console.52
fn lines_begin_and_are_found() -> Result<u64, &'static str> {
    // A kernel line, a program's line written in two pieces, and a kernel
    // line printed into a program's half-written one.
    note(b"one\n", log::KERNEL, 100);
    note(b"tw", log::PROGRAM, 200);
    note(b"o\n", log::PROGRAM, 300);
    note(b"pro", log::PROGRAM, 400);
    note(b"k\n", log::KERNEL, 500);
    let expected = [
        (0, 4, 100, log::KERNEL),
        (4, 8, 200, log::PROGRAM),
        (8, 11, 400, log::PROGRAM),
        (11, 13, 500, log::KERNEL),
    ];
    for (seq, (start, end, usec, priority)) in (0u64..).zip(expected) {
        let want = Line {
            seq,
            start,
            end,
            usec,
            priority,
        };
        if found(seq) != Some(want) {
            return Err(
                "a line of the log did not begin after a newline or a change of priority, \
                 with its first piece's stamp and its priority",
            );
        }
    }
    // A line still being written is no line yet; the next line makes it one.
    note(b"half", log::PROGRAM, 600);
    if found(4).is_some() {
        return Err("a line still being written was handed to a reader");
    }
    note(b"\n", log::PROGRAM, 700);
    if found(4).map(|line| (line.start, line.end, line.usec)) != Some((13, 18, 600)) {
        return Err("a line ended by its newline was not handed to a reader whole");
    }
    // A record of its own begins a line whatever it follows.
    note(b"pre", log::PROGRAM, 800);
    LINE_STARTS.break_line();
    note(b"rec\n", log::PROGRAM, 900);
    if found(6).map(|line| (line.start, line.usec)) != Some((21, 900)) {
        return Err("a line broken for a record of its own did not begin there");
    }
    // More lines than the slots keep, then more bytes than the ring keeps:
    // each time, the first line handed out is the first one kept whole.
    for _ in 0..LINE_SLOTS {
        note(b"ab\n", log::KERNEL, 1000);
    }
    let first = LINE_STARTS.first(LINE_RING.oldest());
    let begun = LINE_STARTS.begun();
    if found(0).is_some() || first != begun - (LINE_SLOTS as u64 - 1) || found(first).is_none() {
        return Err("a line whose slot was reused was handed out, or the first kept was not");
    }
    // A long line pushes the ring's oldest byte into the line two before
    // it: that one's start is gone, the one after it is the first kept.
    // The eight short lines begin at byte 25, three apart, and end at 49;
    // 59 more and a newline leave the ring's oldest at 46, the last's start.
    note(&[b'z'; LINE_BYTES - 5], log::KERNEL, 1100);
    note(b"\n", log::KERNEL, 1100);
    let first = LINE_STARTS.first(LINE_RING.oldest());
    let kept = found(first);
    if first != LINE_STARTS.begun() - 2
        || kept.is_none_or(|line| line.start < LINE_RING.oldest())
        || found(first - 1).is_some()
    {
        return Err("a line whose bytes were overwritten was handed out");
    }
    Ok(LINE_STARTS.begun())
}

/// The ring and lines the grow check grows: small, and grown once.
static GROW_RING: Ring<16> = Ring::new();
static GROW_LINES: Lines<4> = Lines::new();

/// What [`Ring::grow`] and [`Lines::grow`] keep and refuse, on
/// [`GROW_RING`] and [`GROW_LINES`]: a ring written past its 16 bytes and
/// its 4 lines, grown to 64 and 16, still hands out what it kept, at the
/// same places and numbers, and keeps more after; a second grow, slots no
/// longer or not a power of two are refused.
///
/// Verifies: L.console.55, L.console.56
fn grown_rings_keep_what_they_held() -> Result<(), &'static str> {
    let record = |bytes: &[u8], usec: u64| {
        let now = move || usec;
        log::record_lines(
            &GROW_RING,
            &GROW_LINES,
            bytes,
            log::KERNEL,
            &mut Stamp::new(&now),
        );
    };
    for usec in 0..6 {
        record(b"line\n", usec);
    }
    let found = |seq| GROW_LINES.line(seq, GROW_RING.oldest(), GROW_RING.written());
    let before: Vec<Option<Line>> = (0..6).map(found).collect();
    let mut kept = [0u8; 15];
    let mut cursor = GROW_RING.oldest();
    let copied = GROW_RING.read(&mut cursor, &mut kept).copied;
    let five: &'static [log::LineSlot] = Vec::leak((0..5).map(|_| log::LineSlot::new()).collect());
    let odd: &'static [core::sync::atomic::AtomicU8] = Vec::leak(
        (0..48)
            .map(|_| core::sync::atomic::AtomicU8::new(0))
            .collect(),
    );
    let small: &'static [core::sync::atomic::AtomicU8] = Vec::leak(
        (0..16)
            .map(|_| core::sync::atomic::AtomicU8::new(0))
            .collect(),
    );
    if GROW_RING.grow(odd) || GROW_RING.grow(small) || GROW_LINES.grow(five) {
        return Err("a log ring grew into slots no longer or not a power of two");
    }
    let bytes: &'static [core::sync::atomic::AtomicU8] = Vec::leak(
        (0..64)
            .map(|_| core::sync::atomic::AtomicU8::new(0))
            .collect(),
    );
    let lines: &'static [log::LineSlot] =
        Vec::leak((0..16).map(|_| log::LineSlot::new()).collect());
    if !GROW_RING.grow(bytes) || !GROW_LINES.grow(lines) {
        return Err("a log ring did not grow into longer slots");
    }
    let mut again = [0u8; 15];
    let mut cursor = GROW_RING.written() - copied as u64;
    let read = GROW_RING.read(&mut cursor, &mut again);
    if read.copied != copied || again != kept {
        return Err("a grown ring did not hand out the bytes it kept, at their places");
    }
    if (0..6).map(found).collect::<Vec<_>>() != before {
        return Err("grown lines did not hand out the lines they kept, numbered as they were");
    }
    for usec in 6..12 {
        record(b"line\n", usec);
    }
    // 60 bytes and 12 lines now, more than 16 and 4: all kept from where
    // the early ring stopped holding them, byte 15 and line 3, and nothing
    // from before that, which was never copied.
    if GROW_RING.capacity() != 64
        || GROW_RING.oldest() != 15
        || found(3).is_none()
        || found(11).is_none()
        || found(2).is_some()
    {
        return Err(
            "a grown ring did not keep more than it could before, or handed out what it never held",
        );
    }
    let spare: &'static [core::sync::atomic::AtomicU8] = Vec::leak(
        (0..128)
            .map(|_| core::sync::atomic::AtomicU8::new(0))
            .collect(),
    );
    if GROW_RING.grow(spare) {
        return Err("a log ring grew a second time");
    }
    Ok(())
}

/// What `log_buf_len=` grows the kernel log to (`log::size_asked`).
///
/// Verifies: L.console.57
fn sizes_asked() -> Result<(), &'static str> {
    let current = 1 << 17;
    let cases: [(&str, Result<Option<usize>, &str>); 6] = [
        ("1M", Ok(Some(1 << 20))),
        ("300k", Ok(Some(1 << 19))),
        ("0x100000", Ok(Some(1 << 20))),
        ("64K", Ok(None)),
        ("1G", Ok(Some(1 << 22))),
        ("lots", Err("log_buf_len is not a size")),
    ];
    for (asked, want) in cases {
        if log::size_asked(asked, current) != want {
            return Err(
                "log_buf_len did not size the log as a power of two between the log's size and 4 MiB",
            );
        }
    }
    Ok(())
}

/// A task's write, as a program's output.
const PROGRAM: &[u8] = b"  log      a task's write, as a program's output is: in the kernel log\n";
/// A line written as a failure report is.
const REPORT: &str = "  log      a line written as a failure report is: in the kernel log";
/// A line that says where the kernel is.
const UNLOGGED: &str = "  log      a line sent unlogged, as one that says where the kernel is: \
                        on the port, not in the log";

/// What the console records in the kernel log, and what it keeps out.
///
/// Verifies: L.console.31, L.console.32, L.console.34
fn console_records() -> Result<(), &'static str> {
    if !crate::sched::may_block() {
        return Err("the log check ran where a task's write could not wait");
    }
    super::write_bytes(PROGRAM);
    super::write_panicking(format_args!("{REPORT}\n"), true);
    crate::console::println_unlogged!("{UNLOGGED}");
    let recent = recent_log();
    if !contains(&recent, PROGRAM) {
        return Err("a task's write is not in the kernel log as it was written, without its CR");
    }
    if !contains(&recent, REPORT.as_bytes()) {
        return Err("a line written as a failure report is not in the kernel log");
    }
    if contains(&recent, UNLOGGED.as_bytes()) {
        return Err("a line sent unlogged is in the kernel log");
    }
    priorities_logged()
}

/// The priorities [`console_records`]' lines were logged at: a program's for
/// the task's write, the kernel's for the line written as a failure report.
/// Found by reading the kernel log's lines back from those begun just before.
///
/// Verifies: L.console.54
fn priorities_logged() -> Result<(), &'static str> {
    let program = PROGRAM.strip_suffix(b"\n").unwrap_or(PROGRAM);
    let (mut task, mut report) = (None, None);
    let mut seq = log::lines_begun().saturating_sub(64);
    let mut room = vec![0u8; WINDOW];
    while let Some(line) = log::line_from(seq) {
        let mut cursor = line.start;
        let want = usize::try_from(line.end - line.start)
            .unwrap_or(WINDOW)
            .min(WINDOW);
        let read = log::read(&mut cursor, room.get_mut(..want).unwrap_or_default());
        let text = room.get(..read.copied).unwrap_or_default();
        if text.starts_with(program) {
            task = Some(line.priority);
        } else if text.starts_with(REPORT.as_bytes()) {
            report = Some(line.priority);
        }
        seq = line.seq.wrapping_add(1);
    }
    if task != Some(log::PROGRAM) {
        return Err("a task's write was not logged as a line of its own at a program's priority");
    }
    if report != Some(log::KERNEL) {
        return Err("a kernel line was not logged as a line of its own at the kernel's priority");
    }
    a_record_is_a_line_of_its_own()
}

/// A task's write with no newline, then.
const HALF: &[u8] = b"  log      a task's write with no newline, then a record: ";
/// A record, as `/dev/kmsg` writes one, at `user.notice`.
const RECORD: &[u8] = b"  log      a record written to the log is a line of its own";
/// The record's priority.
const RECORD_PRIORITY: u8 = (1 << 3) | 5;

/// `console::write_record` after a task's half-written line: the record is a
/// line of the kernel log of its own, at the priority it was given, its
/// newline inside it.
///
/// Verifies: L.console.53
fn a_record_is_a_line_of_its_own() -> Result<(), &'static str> {
    super::write_bytes(HALF);
    super::write_record(RECORD_PRIORITY, RECORD);
    let mut seq = log::lines_begun().saturating_sub(8);
    let mut room = vec![0u8; WINDOW];
    let mut found = None;
    while let Some(line) = log::line_from(seq) {
        let mut cursor = line.start;
        let want = usize::try_from(line.end - line.start)
            .unwrap_or(WINDOW)
            .min(WINDOW);
        let read = log::read(&mut cursor, room.get_mut(..want).unwrap_or_default());
        let text = room.get(..read.copied).unwrap_or_default();
        if text.starts_with(RECORD) {
            found = Some((line.priority, text.last() == Some(&b'\n')));
        }
        seq = line.seq.wrapping_add(1);
    }
    if found != Some((RECORD_PRIORITY, true)) {
        return Err(
            "a record written after a half-written line was not a line of its own, \
                    at its priority, ending in its newline",
        );
    }
    Ok(())
}

/// The last [`WINDOW`] bytes of the kernel log.
fn recent_log() -> Vec<u8> {
    let mut out = vec![0u8; WINDOW];
    let mut cursor = log::written().saturating_sub(WINDOW as u64);
    let read = log::read(&mut cursor, &mut out);
    out.truncate(read.copied);
    out
}

/// Whether `needle` is somewhere in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Where the region kept across a reset may be (`console::pstore::placement`),
/// held to regions made up from the memory map: one inside the kernel image's
/// entry, and around the loader's region, one not on a page, one shorter than
/// its header, one whose ring is not a power of two and one running past its
/// entry. Each is refused, and the loader's own region, when there is one, is
/// taken. Answers how many were refused.
///
/// Verifies: `L.console.43`
pub(crate) fn check_pstore_placement(
    view: &ferrix_bootinfo::BootView<'_>,
) -> Result<usize, &'static str> {
    use ferrix_bootinfo::PAGE_SIZE;
    use ferrix_bootinfo::pstore::{HEADER, MIN_CAPACITY};

    use super::pstore::placement;

    let small = HEADER + MIN_CAPACITY;
    let raw = view.raw();
    if placement(view, raw.kernel_phys, small).is_ok() {
        return Err("pstore placement took a region inside the kernel image's entry");
    }
    let mut refused = 1;
    let Some((phys, len)) = super::pstore::attached() else {
        return Ok(refused);
    };
    if placement(view, phys, len).is_err() {
        return Err("pstore placement refused the loader's region, which the boot attached");
    }
    let made_up = [
        (phys + 1, len, "a region not on a page"),
        (phys, HEADER, "a region no longer than its header"),
        (
            phys,
            HEADER + 3 * PAGE_SIZE,
            "a ring that is not a power of two",
        ),
        (
            phys + len - PAGE_SIZE,
            small,
            "a region running past its entry",
        ),
    ];
    for (at, length, what) in made_up {
        if placement(view, at, length).is_ok() {
            return Err(what);
        }
        refused += 1;
    }
    Ok(refused)
}
