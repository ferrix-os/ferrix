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

use super::log::{self, Read, Ring};

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
    Ok(Report {
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
    crate::sched::wait_until_gone(&a, crate::sched::REAPER_PATIENCE_NANOS)?;
    crate::sched::wait_until_gone(&b, crate::sched::REAPER_PATIENCE_NANOS)?;
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
