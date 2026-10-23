//! The kernel log: every byte the console sends, kept for whoever may read it.
//!
//! # What it keeps
//!
//! The kernel's own lines ([`crate::console::println!`]) and a program's
//! output through the console (`write(2)` to a descriptor on it, which is
//! `console::write_bytes` and `console::write_raw`), in the order the port
//! sends them, as the program and the kernel wrote them: before a bare
//! newline becomes CRLF for the terminal. What it does not keep is the few
//! lines that carry the kernel's layout, which go to the port and the panic
//! screen and nowhere else: see `console::write_unlogged`.
//!
//! Two readers take it out. `syslog(2)` (`dmesg`), in the Linux personality,
//! and a ring-3 driver that holds a device whose binding may read the log --
//! the Pixel 7's USB serial port -- through a log control channel
//! (`src/kernel/src/interfaces/logctl`, `src/lib/proto/logctl`). Both are privileged: program output
//! is here, and a program's output is its own.
//!
//! # Why a ring of atomics
//!
//! As the console's recent-output ring, and for the same reasons: a byte is
//! recorded from wherever the console is written, which is interrupt handlers,
//! the idle task and a panic on a processor that may hold any lock. So a writer
//! takes no lock, allocates nothing, never waits for a reader and never wakes
//! one; readers look for new bytes on their own schedule. The storage is a
//! static array -- never the heap, which a panic cannot trust and which the
//! item's allocation argument (finding F-23) would have to count.
//!
//! One count, [`Ring::written`], says how many bytes have ever been recorded,
//! and never goes back: a byte is identified by its place in that sequence,
//! and a reader's cursor is such a place. The byte at `n` is in slot `n`
//! modulo the ring's length until byte `n + N` replaces it.
//!
//! A writer stores its byte into the slot and then claims the place with a
//! compare-and-exchange on the count, retrying at the next place if another
//! writer claimed it first. Nearly every writer holds the console's port lock,
//! so under it the ring is exact: a reader that sees the count at `n` sees
//! every byte below it. Writers that race without the lock -- a panic that
//! could not have it, or the boot check's two writers -- can leave one byte
//! stale for each such race, as the recent-output ring can. The count stays
//! exact, and nothing can leave the ring unreadable.
//!
//! # What a reader can lose
//!
//! A reader that falls more than the ring's length behind has lost what was
//! overwritten. [`Ring::read`] moves such a cursor to the oldest byte still
//! kept and says how many were skipped, and it checks again after copying:
//! a writer that went round while the copy ran turns the bytes it replaced
//! into lost ones rather than into a mix of old and new. Because a writer
//! stores before it claims, the slot of the place being claimed is always in
//! flight, so a ring of `N` slots keeps `N - 1` bytes a reader can trust.
//!
//! # Lines
//!
//! Beside the bytes, [`Lines`] keeps where each line starts, when, and how
//! urgent it is: what a reader needs to hand the log out as Linux's records,
//! `<6>[   12.345678] text` through `syslog(2)` and `6,42,12345678,-;text`
//! through `/dev/kmsg`. A line starts at the first byte after a newline, and
//! at a byte whose priority differs from the line it would continue -- the
//! kernel's line printed into a program's half-written one starts its own. Its
//! stamp is the free-running counter in microseconds, the clock
//! `CLOCK_MONOTONIC` reads, taken once per [`record`] that starts a line. The
//! ring of starts is recorded as the bytes are, store then claim, with the
//! same promise: under the port's lock it is exact, and a race without the
//! lock can leave one start stale, never the ring unreadable.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering, fence};

use ferrix_sync::Once;

/// The log's length: Linux's default, `CONFIG_LOG_BUF_SHIFT` of 17, which is
/// also what `syslog(2)` reports as the buffer's size.
pub(crate) const CAPACITY: usize = 1 << 17;

/// A ring of `N` bytes, `N` a power of two, which may be given more once.
pub(crate) struct Ring<const N: usize> {
    /// The bytes until [`Ring::grow`], the one at place `n` in slot `n % N`.
    early: [AtomicU8; N],
    /// The bytes after it: a longer power of two, the one at place `n` in
    /// slot `n` modulo its length.
    grown: Once<&'static [AtomicU8]>,
    /// The oldest place the grown slots hold: the oldest the early ones kept
    /// when it grew. Zero until then.
    floor: AtomicU64,
    /// How many bytes have ever been recorded.
    written: AtomicU64,
}

/// What one read took out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Read {
    /// Bytes copied to the front of the reader's buffer.
    pub(crate) copied: usize,
    /// Bytes between the cursor as it was given and the first byte copied
    /// that the ring no longer held: overwritten before this reader came for
    /// them.
    pub(crate) lost: u64,
}

impl<const N: usize> Ring<N> {
    /// That the early ring's length is a power of two, at compile time.
    const POWER_OF_TWO: () = assert!(
        N.is_power_of_two() && N >= 2,
        "a log ring is a power of two"
    );

    /// An empty ring.
    pub(crate) const fn new() -> Self {
        let () = Self::POWER_OF_TWO;
        Ring {
            early: [const { AtomicU8::new(0) }; N],
            grown: Once::new(),
            floor: AtomicU64::new(0),
            written: AtomicU64::new(0),
        }
    }

    /// The slots bytes are kept in now.
    fn slots(&self) -> &[AtomicU8] {
        self.grown.get().map_or(&self.early, |grown| grown)
    }

    /// How many bytes the ring holds now.
    pub(crate) fn capacity(&self) -> usize {
        self.slots().len()
    }

    /// Keep the bytes in `bigger` from now on, a power of two longer than the
    /// ring is: every byte kept is copied to its place there first, so no
    /// reader can tell. Once only; `false`, and nothing changed, for a ring
    /// already grown or slots that are not longer or not a power of two.
    ///
    /// For a boot with one processor and interrupts masked: a writer racing
    /// the copy could leave its byte in the slots given up.
    pub(crate) fn grow(&self, bigger: &'static [AtomicU8]) -> bool {
        let old = self.slots();
        if self.grown.get().is_some()
            || !bigger.len().is_power_of_two()
            || bigger.len() <= old.len()
        {
            return false;
        }
        let written = self.written();
        let floor = self.oldest_at(old.len(), written);
        for at in floor..written {
            if let (Some(from), Some(to)) = (slot_of(old, at), slot_of(bigger, at)) {
                to.store(from.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        self.floor.store(floor, Ordering::Relaxed);
        let _ = self.grown.call_once(|| bigger);
        true
    }

    /// Record `bytes`, one place each.
    pub(crate) fn record(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.record_byte(byte);
        }
    }

    /// Record one byte: [`record_into`] over this ring's slots and count.
    fn record_byte(&self, byte: u8) {
        let _ = self.record_byte_at(byte);
    }

    /// [`Ring::record_byte`], answering the place the byte took.
    fn record_byte_at(&self, byte: u8) -> u64 {
        record_at(self.slots(), &self.written, byte)
    }

    /// The byte at place `at`, if the ring still holds it.
    pub(crate) fn byte_at(&self, at: u64) -> Option<u8> {
        let mut cursor = at;
        let mut byte = [0u8; 1];
        let read = self.read(&mut cursor, &mut byte);
        byte.first()
            .copied()
            .filter(|_| read.copied == 1 && read.lost == 0)
    }

    /// The oldest place a reader can trust now.
    pub(crate) fn oldest(&self) -> u64 {
        let written = self.written();
        self.oldest_at(self.capacity(), written)
    }

    /// How many bytes have ever been recorded: the place the next one takes.
    pub(crate) fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// The oldest place a reader can trust in a ring of `length` slots when
    /// `written` bytes have been recorded: `length - 1` back, the slot of the
    /// place being claimed being in flight, and never below what the ring
    /// held when it grew.
    fn oldest_at(&self, length: usize, written: u64) -> u64 {
        let back = written.saturating_add(1).saturating_sub(length as u64);
        back.max(self.floor.load(Ordering::Relaxed))
    }

    /// How many bytes a reader at `cursor` has still to read: from the cursor,
    /// or the oldest byte kept if that is later, to the newest.
    pub(crate) fn unread(&self, cursor: u64) -> u64 {
        let written = self.written();
        written.saturating_sub(cursor.max(self.oldest_at(self.capacity(), written)))
    }

    /// Copy what a reader at `cursor` has not read into `out`, as much as
    /// fits, and move the cursor past it and past anything lost.
    ///
    /// A cursor behind the oldest byte kept jumps forward to it, and the
    /// bytes it skipped are reported lost. So are any a writer replaced while
    /// they were being copied: they are dropped from the front of what was
    /// copied rather than handed on as a mix of old and new. A cursor ahead
    /// of the newest byte, which only a caller that made one up can have, is
    /// brought back to it.
    pub(crate) fn read(&self, cursor: &mut u64, out: &mut [u8]) -> Read {
        // The count before the slots: a ring grown after this has every
        // byte below the count at the same place in both.
        let end = self.written();
        let slots = self.slots();
        let mut lost = self.oldest_at(slots.len(), end).saturating_sub(*cursor);
        *cursor = cursor.saturating_add(lost).min(end);
        let wanted = end.saturating_sub(*cursor);
        let count = usize::try_from(wanted).map_or(out.len(), |wanted| wanted.min(out.len()));
        let taken = out.get_mut(..count).unwrap_or_default();
        copy_from(slots, *cursor, taken);

        // Every slot load above before the count below: a writer that went
        // round meanwhile is seen, and what it replaced is dropped.
        fence(Ordering::Acquire);
        let after = self.written.load(Ordering::Relaxed);
        let replaced = self.oldest_at(slots.len(), after).saturating_sub(*cursor);
        let stale = usize::try_from(replaced).map_or(count, |stale| stale.min(count));
        if stale != 0 {
            taken.copy_within(stale.., 0);
        }
        lost = lost.saturating_add(stale as u64);
        *cursor = cursor.saturating_add(count as u64);
        Read {
            copied: count.saturating_sub(stale),
            lost,
        }
    }
}

/// Load the bytes of `slots` from place `from` on into `out`.
fn copy_from(slots: &[AtomicU8], from: u64, out: &mut [u8]) {
    let mut at = from;
    for byte in out {
        *byte = slot_of(slots, at).map_or(b'?', |slot| slot.load(Ordering::Relaxed));
        at = at.wrapping_add(1);
    }
}

/// Record one byte into a ring of `slots`, whose length is a power of two,
/// counted by `written`: store it into the next place's slot, then claim the
/// place, and store again at the next if another writer claimed it first.
///
/// The one algorithm every log ring records with: the kernel log's static
/// [`Ring`], and the region the log is kept in across a reset
/// (`console::pstore`), whose slots are memory the loader gave. So the two
/// cannot record differently.
pub(crate) fn record_into(slots: &[AtomicU8], written: &AtomicU64, byte: u8) {
    let _ = record_at(slots, written, byte);
}

/// [`record_into`], answering the place the byte took.
fn record_at(slots: &[AtomicU8], written: &AtomicU64, byte: u8) -> u64 {
    let mut at = written.load(Ordering::Relaxed);
    loop {
        if let Some(slot) = slot_of(slots, at) {
            slot.store(byte, Ordering::Relaxed);
        }
        match written.compare_exchange_weak(
            at,
            at.wrapping_add(1),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return at,
            Err(now) => at = now,
        }
    }
}

/// The slot of `slots`, a power of two long, that place `at` is kept in.
fn slot_of(slots: &[AtomicU8], at: u64) -> Option<&AtomicU8> {
    let mask = (slots.len() as u64).wrapping_sub(1);
    let index = usize::try_from(at & mask).ok()?;
    slots.get(index)
}

/// The priority a kernel line is logged at: `KERN_INFO` of facility 0, the
/// kernel's. The kernel's lines carry no level of their own.
pub(crate) const KERNEL: u8 = 6;

/// The priority a program's console output is logged at: `LOG_INFO` of
/// `LOG_USER`, the facility Linux gives what a program writes to `/dev/kmsg`
/// without one, so that `dmesg -k` can leave it out.
pub(crate) const PROGRAM: u8 = (1 << 3) | 6;

/// Where each line of a log starts, when, and at what priority: a ring of
/// `M` lines, `M` a power of two, numbered from zero as they begin, which may
/// be given more once, as its log's bytes may.
pub(crate) struct Lines<const M: usize> {
    /// Each line's slots until [`Lines::grow`]: line `n` in slot `n % M`.
    early: [LineSlot; M],
    /// Its slots after it, a longer power of two.
    grown: Once<&'static [LineSlot]>,
    /// The first line the grown slots hold: the first the early ones kept
    /// when it grew. Zero until then.
    floor: AtomicU64,
    /// How many lines have ever begun: the number the next one takes.
    begun: AtomicU64,
    /// Zero when the next byte begins a line; otherwise one more than the
    /// priority of the line it continues.
    open: AtomicU32,
}

/// Where one line is kept.
pub(crate) struct LineSlot {
    /// The line's first byte's place in its log.
    start: AtomicU64,
    /// Its stamp: microseconds of the counter, shifted left eight, and its
    /// priority in the low eight bits.
    stamp: AtomicU64,
}

impl LineSlot {
    /// An empty slot.
    pub(crate) const fn new() -> Self {
        LineSlot {
            start: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }
    }
}

/// One line, as a reader finds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Line {
    /// Its number, Linux's record sequence number.
    pub(crate) seq: u64,
    /// Its first byte's place in the log.
    pub(crate) start: u64,
    /// The place past its last byte: where the next line starts, or the
    /// log's end for the last. Its newline, if it has one, is inside.
    pub(crate) end: u64,
    /// When it began, in microseconds of the counter.
    pub(crate) usec: u64,
    /// Its priority: facility shifted left three, and level.
    pub(crate) priority: u8,
}

impl<const M: usize> Lines<M> {
    /// That the early ring's length is a power of two, at compile time.
    const POWER_OF_TWO: () = assert!(
        M.is_power_of_two() && M >= 2,
        "a ring of lines is a power of two"
    );

    /// No lines yet.
    pub(crate) const fn new() -> Self {
        let () = Self::POWER_OF_TWO;
        Lines {
            early: [const { LineSlot::new() }; M],
            grown: Once::new(),
            floor: AtomicU64::new(0),
            begun: AtomicU64::new(0),
            open: AtomicU32::new(0),
        }
    }

    /// The slots lines are kept in now.
    fn slots(&self) -> &[LineSlot] {
        self.grown.get().map_or(&self.early, |grown| grown)
    }

    /// How many lines the ring holds now, as a count.
    fn length(&self) -> u64 {
        self.slots().len() as u64
    }

    /// Keep the lines in `bigger` from now on, as [`Ring::grow`] keeps bytes:
    /// every line kept copied to its slot there first, once only.
    pub(crate) fn grow(&self, bigger: &'static [LineSlot]) -> bool {
        let old = self.slots();
        if self.grown.get().is_some()
            || !bigger.len().is_power_of_two()
            || bigger.len() <= old.len()
        {
            return false;
        }
        let begun = self.begun();
        let length = old.len() as u64;
        let floor = begun
            .saturating_sub(length - 1)
            .max(self.floor.load(Ordering::Relaxed));
        for seq in floor..begun {
            if let (Some(from), Some(to)) = (slot_in(old, seq), slot_in(bigger, seq)) {
                to.start
                    .store(from.start.load(Ordering::Relaxed), Ordering::Relaxed);
                to.stamp
                    .store(from.stamp.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        self.floor.store(floor, Ordering::Relaxed);
        let _ = self.grown.call_once(|| bigger);
        true
    }

    /// Note that a byte of `priority` is about to go to place `at`: a line
    /// begins there if the byte before ended one or was of another priority.
    /// The stamp is `now`'s answer, asked once however many lines begin.
    ///
    /// Before the byte is recorded, never after: a reader that sees the
    /// byte in its log's count sees the line it begins too, so it can never
    /// take the byte for part of the line before (`Lines::line`).
    pub(crate) fn note(&self, at: u64, byte: u8, priority: u8, stamp: &mut Stamp<'_>) {
        let open = u32::from(priority).saturating_add(1);
        if self.open.load(Ordering::Relaxed) != open {
            self.begin(at, stamp.usec(), priority);
        }
        let next = if byte == b'\n' { 0 } else { open };
        self.open.store(next, Ordering::Relaxed);
    }

    /// Make the next byte noted begin a line, whatever came before it, and
    /// say whether a line was left without its newline.
    pub(crate) fn break_line(&self) -> bool {
        self.open.swap(0, Ordering::Relaxed) != 0
    }

    /// Record that line `begun` starts at `at`: stored into its slots, then
    /// claimed, as [`record_into`] stores a byte.
    fn begin(&self, at: u64, usec: u64, priority: u8) {
        let stamp = usec.wrapping_shl(8) | u64::from(priority);
        let slots = self.slots();
        let mut seq = self.begun.load(Ordering::Relaxed);
        loop {
            if let Some(slot) = slot_in(slots, seq) {
                slot.start.store(at, Ordering::Relaxed);
                slot.stamp.store(stamp, Ordering::Relaxed);
            }
            match self.begun.compare_exchange_weak(
                seq,
                seq.wrapping_add(1),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => seq = now,
            }
        }
    }

    /// How many lines have ever begun: the number the next one takes.
    pub(crate) fn begun(&self) -> u64 {
        self.begun.load(Ordering::Acquire)
    }

    /// Whether line `seq` is still kept in a ring of `length` slots when
    /// `begun` have begun: the slot of line `begun` is in flight, so
    /// `length - 1` back at most, and none below what the ring held when it
    /// grew.
    fn kept(&self, seq: u64, begun: u64, length: u64) -> bool {
        seq < begun && begun - seq < length && seq >= self.floor.load(Ordering::Relaxed)
    }

    /// Line `seq` of `ring`'s lines, if it is kept and its first byte is
    /// still at or after the oldest byte `ring` keeps. A line is whole once
    /// another has begun after it, or, the last begun, once the log's last
    /// byte is a newline; `None` too for one still being written.
    ///
    /// The log's count is read first: every line whose first byte is below
    /// it has begun by then, since a line begins before its first byte is
    /// recorded ([`Lines::note`]), so no byte of a later line is taken for
    /// this one's. A line's end is where the next one begins, never the
    /// count read here, which a writer may have passed since.
    pub(crate) fn line<const N: usize>(&self, seq: u64, ring: &Ring<N>) -> Option<Line> {
        let written = ring.written();
        let oldest = ring.oldest_at(ring.capacity(), written);
        let begun = self.begun();
        let slots = self.slots();
        let length = slots.len() as u64;
        if !self.kept(seq, begun, length) {
            return None;
        }
        let start = slot_in(slots, seq)?.start.load(Ordering::Relaxed);
        let stamp = slot_in(slots, seq)?.stamp.load(Ordering::Relaxed);
        let next = seq.wrapping_add(1);
        let end = if next < begun {
            slot_in(slots, next)?.start.load(Ordering::Relaxed)
        } else if written > start && ring.byte_at(written - 1) == Some(b'\n') {
            written
        } else {
            return None;
        };
        // Every slot load above before the count below: a writer that went
        // round meanwhile is seen, and the line it replaced is not handed on.
        fence(Ordering::Acquire);
        let after = self.begun.load(Ordering::Relaxed);
        if !self.kept(seq, after, length) || start < oldest || end < start {
            return None;
        }
        Some(Line {
            seq,
            start,
            end,
            usec: stamp >> 8,
            priority: u8::try_from(stamp & 0xFF).unwrap_or(KERNEL),
        })
    }

    /// The first line still kept whose bytes are too, from `oldest` on: a
    /// search over the lines kept, whose starts only grow.
    pub(crate) fn first(&self, oldest: u64) -> u64 {
        let begun = self.begun();
        let slots = self.slots();
        let mut low = begun
            .saturating_sub(self.length() - 1)
            .max(self.floor.load(Ordering::Relaxed));
        let mut high = begun;
        while low < high {
            let middle = low + (high - low) / 2;
            let start =
                slot_in(slots, middle).map_or(u64::MAX, |slot| slot.start.load(Ordering::Relaxed));
            if start >= oldest {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }
}

/// The slot of `slots`, a power of two long, that line `seq` is kept in.
fn slot_in(slots: &[LineSlot], seq: u64) -> Option<&LineSlot> {
    let mask = (slots.len() as u64).wrapping_sub(1);
    slots.get(usize::try_from(seq & mask).ok()?)
}

/// A stamp taken at most once, when the first line of a record begins.
pub(crate) struct Stamp<'a> {
    /// What it was, once taken.
    taken: Option<u64>,
    /// Where it comes from.
    now: &'a dyn Fn() -> u64,
}

impl<'a> Stamp<'a> {
    /// A stamp `now` will give.
    pub(crate) fn new(now: &'a dyn Fn() -> u64) -> Self {
        Stamp { taken: None, now }
    }

    /// The stamp, taken now if it was not yet.
    fn usec(&mut self) -> u64 {
        *self.taken.get_or_insert_with(|| (self.now)())
    }
}

/// Bytes of log per line kept: a log of lines shorter than this on average
/// loses lines before its bytes go round.
const BYTES_PER_LINE: usize = 32;

/// How many lines the kernel log keeps until it is grown.
const LINES: usize = CAPACITY / BYTES_PER_LINE;

/// The boot option that asks for a larger log: Linux's, `log_buf_len=` a
/// size as `memparse` reads it, rounded up to a power of two.
pub(crate) const LOG_BUF_LEN: &str = "log_buf_len";

/// The largest log `log_buf_len=` gets: one allocation of the kernel heap's
/// largest order, 4 MiB. Linux allows more; a larger ask is given this.
const LARGEST: usize = 1 << 22;

/// The kernel log.
static LOG: Ring<CAPACITY> = Ring::new();

/// Where its lines start.
static LOG_LINES: Lines<LINES> = Lines::new();

/// The counter in microseconds: a line's stamp.
fn now_usec() -> u64 {
    crate::timer::now_nanos() / 1000
}

/// Record `bytes` in the kernel log at `priority`, [`KERNEL`] or
/// [`PROGRAM`] or a record's own. What the console calls for every byte it
/// sends, and all it asks of the log.
pub(crate) fn record(bytes: &[u8], priority: u8) {
    let now = now_usec;
    record_lines(&LOG, &LOG_LINES, bytes, priority, &mut Stamp::new(&now));
    super::pstore::record(bytes);
}

/// Record `bytes` in `ring` at `priority`, and in `lines` where each line
/// they hold begins: [`record`]'s whole work, over a ring and lines a check
/// may own.
pub(crate) fn record_lines<const N: usize, const M: usize>(
    ring: &Ring<N>,
    lines: &Lines<M>,
    bytes: &[u8],
    priority: u8,
    stamp: &mut Stamp<'_>,
) {
    for &byte in bytes {
        // Under the port's lock the place the byte takes is the count now.
        lines.note(ring.written(), byte, priority, stamp);
        let _ = ring.record_byte_at(byte);
    }
}

/// Make the next byte recorded begin a line of its own: for a record a
/// program sent through `/dev/kmsg`, which is a line whatever came before.
/// Says whether a line was left without its newline.
pub(crate) fn break_line() -> bool {
    LOG_LINES.break_line()
}

/// Grow the kernel log to what `log_buf_len=` asks, if it asks for more than
/// it has: the bytes and their lines, each copied over whole (`Ring::grow`,
/// `Lines::grow`). Answers the log's size when it grew, `None` when not
/// asked to.
///
/// Called once, at boot, with the heap up, on the one processor running and
/// with its interrupts masked, so that no writer races the copy.
///
/// # Errors
///
/// A size that does not read as one, or no memory for it; the log stays as
/// it was.
pub(crate) fn size_from(
    view: &ferrix_bootinfo::BootView<'_>,
) -> Result<Option<usize>, &'static str> {
    let Some(asked) = view.option(LOG_BUF_LEN) else {
        return Ok(None);
    };
    let Some(bytes) = size_asked(asked, LOG.capacity())? else {
        return Ok(None);
    };
    let lines = bytes / BYTES_PER_LINE;
    let mut slots = crate::fallible::try_with_capacity(bytes).map_err(|_| "no memory for it")?;
    crate::fallible::try_extend(&mut slots, (0..bytes).map(|_| AtomicU8::new(0)))
        .map_err(|_| "no memory for it")?;
    let mut starts = crate::fallible::try_with_capacity(lines).map_err(|_| "no memory for it")?;
    crate::fallible::try_extend(&mut starts, (0..lines).map(|_| LineSlot::new()))
        .map_err(|_| "no memory for it")?;
    // Kept for as long as the kernel runs: the log is never given back.
    let grew = LOG.grow(slots.leak()) && LOG_LINES.grow(starts.leak());
    Ok(grew.then_some(bytes))
}

/// The size `log_buf_len=asked` grows a log of `current` bytes to: the size
/// rounded up to a power of two, at most [`LARGEST`]; `None` when that is no
/// more than `current`, which Linux keeps too.
///
/// # Errors
///
/// A value that is not a size.
pub(crate) fn size_asked(asked: &str, current: usize) -> Result<Option<usize>, &'static str> {
    let bytes = ferrix_bootinfo::size_in(asked)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .and_then(usize::checked_next_power_of_two)
        .ok_or("log_buf_len is not a size")?
        .min(LARGEST);
    Ok((bytes > current).then_some(bytes))
}

/// How many bytes the kernel log holds: what `syslog(2)`'s `SIZE_BUFFER`
/// answers.
pub(crate) fn capacity() -> usize {
    LOG.capacity()
}

/// Read the kernel log from `cursor`: see [`Ring::read`].
pub(crate) fn read(cursor: &mut u64, out: &mut [u8]) -> Read {
    LOG.read(cursor, out)
}

/// How many bytes the kernel log has ever recorded.
pub(crate) fn written() -> u64 {
    LOG.written()
}

/// How many lines the kernel log has ever begun: the number the next takes.
pub(crate) fn lines_begun() -> u64 {
    LOG_LINES.begun()
}

/// The first line of the kernel log still kept, bytes and all.
pub(crate) fn first_line() -> u64 {
    LOG_LINES.first(LOG.oldest())
}

/// Line `seq` of the kernel log, whole; or, if it is gone, the first one
/// kept after it. `None` when there is no whole line from `seq` on yet.
pub(crate) fn line_from(seq: u64) -> Option<Line> {
    let from = seq.max(LOG_LINES.first(LOG.oldest()));
    LOG_LINES.line(from, &LOG)
}
