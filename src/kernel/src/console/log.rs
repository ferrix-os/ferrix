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

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering, fence};

/// The log's length: Linux's default, `CONFIG_LOG_BUF_SHIFT` of 17, which is
/// also what `syslog(2)` reports as the buffer's size.
pub const CAPACITY: usize = 1 << 17;

/// A ring of `N` bytes, `N` a power of two.
pub(crate) struct Ring<const N: usize> {
    /// The bytes, the one at place `n` in slot `n % N`.
    slots: [AtomicU8; N],
    /// How many bytes have ever been recorded.
    written: AtomicU64,
}

/// What one read took out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Read {
    /// Bytes copied to the front of the reader's buffer.
    pub copied: usize,
    /// Bytes between the cursor as it was given and the first byte copied
    /// that the ring no longer held: overwritten before this reader came for
    /// them.
    pub lost: u64,
}

impl<const N: usize> Ring<N> {
    /// The ring's length as a count.
    const LENGTH: u64 = {
        assert!(
            N.is_power_of_two() && N >= 2,
            "a log ring is a power of two"
        );
        N as u64
    };

    /// An empty ring.
    pub(crate) const fn new() -> Self {
        Ring {
            slots: [const { AtomicU8::new(0) }; N],
            written: AtomicU64::new(0),
        }
    }

    /// Record `bytes`, one place each.
    pub(crate) fn record(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.record_byte(byte);
        }
    }

    /// Record one byte: [`record_into`] over this ring's slots and count.
    fn record_byte(&self, byte: u8) {
        record_into(&self.slots, &self.written, byte);
    }

    /// The slot place `at` is kept in.
    fn slot(&self, at: u64) -> Option<&AtomicU8> {
        slot_of(&self.slots, at)
    }

    /// How many bytes have ever been recorded: the place the next one takes.
    pub(crate) fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    /// The oldest place a reader can trust when `written` bytes have been
    /// recorded: `N - 1` back, the slot of the place being claimed being in
    /// flight.
    const fn oldest_at(written: u64) -> u64 {
        written.saturating_add(1).saturating_sub(Self::LENGTH)
    }

    /// How many bytes a reader at `cursor` has still to read: from the cursor,
    /// or the oldest byte kept if that is later, to the newest.
    pub(crate) fn unread(&self, cursor: u64) -> u64 {
        let written = self.written();
        written.saturating_sub(cursor.max(Self::oldest_at(written)))
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
        let end = self.written();
        let mut lost = Self::oldest_at(end).saturating_sub(*cursor);
        *cursor = cursor.saturating_add(lost).min(end);
        let wanted = end.saturating_sub(*cursor);
        let count = usize::try_from(wanted).map_or(out.len(), |wanted| wanted.min(out.len()));
        let taken = out.get_mut(..count).unwrap_or_default();
        self.copy_from(*cursor, taken);

        // Every slot load above before the count below: a writer that went
        // round meanwhile is seen, and what it replaced is dropped.
        fence(Ordering::Acquire);
        let after = self.written.load(Ordering::Relaxed);
        let replaced = Self::oldest_at(after).saturating_sub(*cursor);
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

    /// Load the bytes from place `from` on into `out`.
    fn copy_from(&self, from: u64, out: &mut [u8]) {
        let mut at = from;
        for byte in out {
            *byte = self
                .slot(at)
                .map_or(b'?', |slot| slot.load(Ordering::Relaxed));
            at = at.wrapping_add(1);
        }
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
            Ok(_) => return,
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

/// The kernel log.
static LOG: Ring<CAPACITY> = Ring::new();

/// Record `bytes` in the kernel log. What the console calls for every byte it
/// sends, and all it asks of the log.
pub(crate) fn record(bytes: &[u8]) {
    LOG.record(bytes);
    super::pstore::record(bytes);
}

/// Read the kernel log from `cursor`: see [`Ring::read`].
pub fn read(cursor: &mut u64, out: &mut [u8]) -> Read {
    LOG.read(cursor, out)
}

/// How many bytes the kernel log has ever recorded.
pub fn written() -> u64 {
    LOG.written()
}

/// How many bytes a reader of the kernel log at `cursor` has still to read.
pub fn unread(cursor: u64) -> u64 {
    LOG.unread(cursor)
}
