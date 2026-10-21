//! The kernel's log kept across a reset: the region's format, which the
//! loader and the kernel both compile from here (`docs/POSTMORTEM.md` §3).
//!
//! The region is a header page and a ring. The loader finds the region where
//! [`choose`] says it is, judges the header the previous boot's loader wrote
//! ([`validate`]), reports the text the previous kernel left in the ring
//! ([`Text`]), and writes a fresh header ([`encode`]) before it hands the
//! region to the kernel. The kernel checks the header once more, as the loader
//! left it ([`fresh`]), and from then on writes ring bytes, the one count,
//! [`WRITTEN_OFFSET`], and the stop word, [`STOP_OFFSET`], and nothing else.
//!
//! ```text
//!   0  magic      "FXPSTORE"
//!   8  version    u32 = 2
//!  12  header     u32 = 4096       the ring starts here
//!  16  capacity   u64              bytes of ring, a power of two
//!  24  sequence   u64              which boot wrote it
//!  32  crc        u32              CRC-32C of bytes 0..32
//!  36  reserved   u32 = 0
//!  64  written    u64              bytes ever recorded
//!  72  stop       u64              how the boot stopped: a code and its complement, 0 = none
//! 4096 ring       byte n at 4096 + n % capacity
//! ```
//!
//! Version 1 had no stop word (`docs/POSTMORTEM.md` §5). [`validate`] still
//! reads it, so the loader reports and keeps what a kernel before version 2
//! left; the kernel attaches to version 2 alone.
//!
//! Nothing checks the ring's bytes: a firmware that used the region's RAM on
//! its way up leaves a valid header over damaged text. So the text is called
//! unchecked wherever it is reported, and [`shown`] keeps what is printed of
//! it printable.

use crate::PAGE_SIZE;

/// The region's first eight bytes.
pub const MAGIC: [u8; 8] = *b"FXPSTORE";
/// The header's layout version: the one [`encode`] writes and [`fresh`]
/// takes.
pub const VERSION: u32 = 2;
/// The version before the stop word, which [`validate`] still reads.
pub const VERSION_1: u32 = 1;
/// Bytes before the ring: one page.
pub const HEADER: u64 = PAGE_SIZE;
/// The ring's length the loader gives: 1 MiB.
pub const CAPACITY: u64 = 1 << 20;
/// The whole region the loader allocates.
pub const LEN: u64 = HEADER + CAPACITY;
/// The smallest ring a header may describe.
pub const MIN_CAPACITY: u64 = PAGE_SIZE;
/// The largest ring a header may describe.
pub const MAX_CAPACITY: u64 = 16 << 20;
/// Where the count of bytes ever recorded is, one of the two header fields
/// the kernel writes. Eight-byte aligned.
pub const WRITTEN_OFFSET: usize = 64;
/// A count at or above this is not one a kernel wrote: 256 TiB of log.
pub const WRITTEN_LIMIT: u64 = 1 << 48;
/// Where the stop word is ([`Stop::word`]), the other header field the
/// kernel writes. Eight-byte aligned.
pub const STOP_OFFSET: usize = WRITTEN_OFFSET + 8;
/// Bytes of the header [`validate`] reads and [`encode`] writes.
pub const HEADER_BYTES: usize = STOP_OFFSET + 8;
/// How far above the lowest conventional memory the region goes at least:
/// past where firmware does its own work before it hands over. OVMF
/// rewrites 8 to 27 MiB on every reset (its MEMFD and the firmware volumes
/// it decompresses, measured 2026-10-15), so 16 MiB, the first choice, lost
/// every record there.
pub const ABOVE_LOWEST: u64 = 64 << 20;
/// Where the region goes first, above the lowest conventional memory, when
/// the map has room there: past where another operating system booted on
/// the same machine puts its own image. Linux's EFI stub on a firmware with
/// no RNG protocol decompresses the kernel at 16 MiB (Ubuntu's 7.0 reaches
/// past 72 MiB), before its `memmap=` reservation takes effect, so a region
/// at 64 MiB is lost to the inspection Linux of the N150 box, and one at
/// 1 GiB is kept with `nokaslr memmap=2M$0x40000000`. A machine whose map has
/// no room there (a 512 MiB board) takes [`ABOVE_LOWEST`].
pub const PREFERRED_ABOVE_LOWEST: u64 = 1 << 30;
/// The alignment of the region's address.
pub const ALIGN: u64 = 2 << 20;

/// Where the checked part of the header ends, and the CRC covers.
const CHECKED: usize = 32;

/// CRC-32C (Castagnoli, reflected), bit by bit: it covers 32 bytes once a
/// boot, so a table would be all cost.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let low = crc & 1;
            crc >>= 1;
            if low != 0 {
                crc ^= 0x82F6_3B78;
            }
        }
    }
    !crc
}

/// A fresh header for a ring of `capacity` bytes written by boot `sequence`:
/// nothing recorded yet.
#[must_use]
pub fn encode(capacity: u64, sequence: u64) -> [u8; HEADER_BYTES] {
    let mut header = [0u8; HEADER_BYTES];
    let fields: [(usize, &[u8]); 5] = [
        (0, &MAGIC),
        (8, &VERSION.to_le_bytes()),
        (12, &(HEADER as u32).to_le_bytes()),
        (16, &capacity.to_le_bytes()),
        (24, &sequence.to_le_bytes()),
    ];
    for (at, bytes) in fields {
        put(&mut header, at, bytes);
    }
    let crc = crc32c(header.get(..CHECKED).unwrap_or_default());
    put(&mut header, CHECKED, &crc.to_le_bytes());
    header
}

/// Copy `bytes` into `header` at `at`, where they fit.
fn put(header: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(field) = header.get_mut(at..at.saturating_add(bytes.len())) {
        field.copy_from_slice(bytes);
    }
}

/// Why a header was not taken for one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The magic is not there: a cold boot, or RAM nobody wrote a region in.
    NothingThere,
    /// Another layout version.
    Version(u32),
    /// The header length, the capacity, or the capacity against the region's
    /// length is not one this format allows.
    Layout,
    /// The CRC of the checked part does not match.
    Crc,
    /// The count is beyond anything a kernel wrote.
    Written,
    /// Not a fresh header ([`fresh`]): something was recorded already.
    Recorded,
    /// Not a fresh header ([`fresh`]): a stop word is there, whole or not.
    Stopped,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::NothingThere => f.write_str("nothing there"),
            Refusal::Version(version) => write!(f, "damaged: version {version}"),
            Refusal::Layout => f.write_str("damaged: its layout is not this format's"),
            Refusal::Crc => f.write_str("damaged: its CRC does not match"),
            Refusal::Written => f.write_str("damaged: its count is out of bounds"),
            Refusal::Recorded => f.write_str("something was recorded already"),
            Refusal::Stopped => f.write_str("a stop was recorded already"),
        }
    }
}

/// A header [`validate`] took.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    /// Its layout version: [`VERSION`] or [`VERSION_1`].
    pub version: u32,
    /// The ring's length.
    pub capacity: u64,
    /// The boot that wrote it.
    pub sequence: u64,
    /// Bytes ever recorded.
    pub written: u64,
    /// The stop word as it is there, unjudged ([`ended`]); a version-1
    /// header has none, whatever its bytes are.
    pub stop: u64,
}

/// How a boot stopped when it stopped on purpose: the closed list of codes
/// the stop word holds (`docs/POSTMORTEM.md` §5.1). Codes 7 to 255 are kept
/// for the stops later slices add.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stop {
    /// The first failure report began.
    Panic = 1,
    /// `reboot(2)`'s RESTART or RESTART2.
    Restart = 2,
    /// `reboot(2)`'s `POWER_OFF`.
    PowerOff = 3,
    /// `reboot(2)`'s HALT.
    Halt = 4,
    /// Init exited, and `ferrix.onexit=reset` asks for a reset.
    ExitReset = 5,
    /// Init exited, and the default powers off.
    ExitPowerOff = 6,
}

impl Stop {
    /// Every stop, in code order.
    pub const ALL: [Stop; 6] = [
        Stop::Panic,
        Stop::Restart,
        Stop::PowerOff,
        Stop::Halt,
        Stop::ExitReset,
        Stop::ExitPowerOff,
    ];

    /// Its code.
    #[must_use]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The stop word that records it: the code in the low half, its
    /// complement in the high half, so that no word of zeros, and no word
    /// one changed bit away from another stop's, reads as a stop.
    #[must_use]
    pub const fn word(self) -> u64 {
        let code = self.code();
        (code as u64) | ((!code as u64) << 32)
    }

    /// The stop a code names, if it names one.
    #[must_use]
    pub const fn of(code: u32) -> Option<Stop> {
        match code {
            1 => Some(Stop::Panic),
            2 => Some(Stop::Restart),
            3 => Some(Stop::PowerOff),
            4 => Some(Stop::Halt),
            5 => Some(Stop::ExitReset),
            6 => Some(Stop::ExitPowerOff),
            _ => None,
        }
    }

    /// What the loader says of it.
    #[must_use]
    pub const fn says(self) -> &'static str {
        match self {
            Stop::Panic => "a panic",
            Stop::Restart => "a restart a program asked for (reboot(2))",
            Stop::PowerOff => "a power-off a program asked for (reboot(2))",
            Stop::Halt => "a halt a program asked for (reboot(2))",
            Stop::ExitReset => "a reset after init exited (ferrix.onexit=reset)",
            Stop::ExitPowerOff => "a power-off after init exited",
        }
    }
}

/// How a found region's boot ended, as its stop word says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// A version-1 header, which has no stop word.
    NotKept,
    /// The word is 0: the kernel never said it stopped.
    NoneRecorded,
    /// A stop the list names.
    Stopped(Stop),
    /// A whole word whose code the list does not name: a newer kernel's.
    Unknown(u32),
    /// A word whose halves do not match.
    Damaged(u64),
}

/// Judge a stop word.
#[must_use]
pub const fn ended(word: u64) -> Ended {
    if word == 0 {
        return Ended::NoneRecorded;
    }
    let code = word as u32;
    if (word >> 32) as u32 != !code || code == 0 {
        return Ended::Damaged(word);
    }
    match Stop::of(code) {
        Some(stop) => Ended::Stopped(stop),
        None => Ended::Unknown(code),
    }
}

impl core::fmt::Display for Ended {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Ended::NotKept => f.write_str("unknown, an older kernel's header"),
            Ended::NoneRecorded => f.write_str(
                "no stop recorded: a hang, a watchdog or a reset from outside the kernel",
            ),
            Ended::Stopped(stop) => f.write_str(stop.says()),
            Ended::Unknown(code) => write!(f, "unknown code {code} (a newer kernel's)"),
            Ended::Damaged(word) => write!(f, "its stop word is damaged ({word:#x})"),
        }
    }
}

impl Found {
    /// How its boot ended.
    #[must_use]
    pub const fn ended(&self) -> Ended {
        if self.version == VERSION_1 {
            Ended::NotKept
        } else {
            ended(self.stop)
        }
    }

    /// How many bytes of the ring are text: the last `written`, or the last
    /// `capacity - 1` once it went round, the slot of the place being
    /// claimed being in flight (as in the kernel's own log ring).
    #[must_use]
    pub const fn kept(&self) -> u64 {
        let most = self.capacity.saturating_sub(1);
        if self.written < most {
            self.written
        } else {
            most
        }
    }
}

/// Read a little-endian `u32` at `at`.
fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    if let Some(field) = bytes.get(at..at.saturating_add(4)) {
        word.copy_from_slice(field);
    }
    u32::from_le_bytes(word)
}

/// Read a little-endian `u64` at `at`.
#[must_use]
pub fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    if let Some(field) = bytes.get(at..at.saturating_add(8)) {
        word.copy_from_slice(field);
    }
    u64::from_le_bytes(word)
}

/// Whether `capacity` is a ring length this format allows.
#[must_use]
pub const fn capacity_allowed(capacity: u64) -> bool {
    capacity.is_power_of_two() && capacity >= MIN_CAPACITY && capacity <= MAX_CAPACITY
}

/// Judge the first [`HEADER_BYTES`] of a region of `len` bytes.
pub fn validate(header: &[u8; HEADER_BYTES], len: u64) -> Result<Found, Refusal> {
    if header.get(..8) != Some(&MAGIC[..]) {
        return Err(Refusal::NothingThere);
    }
    let version = u32_at(header, 8);
    if version != VERSION && version != VERSION_1 {
        return Err(Refusal::Version(version));
    }
    let capacity = u64_at(header, 16);
    if u64::from(u32_at(header, 12)) != HEADER
        || !capacity_allowed(capacity)
        || HEADER.checked_add(capacity) != Some(len)
    {
        return Err(Refusal::Layout);
    }
    if crc32c(header.get(..CHECKED).unwrap_or_default()) != u32_at(header, CHECKED) {
        return Err(Refusal::Crc);
    }
    let written = u64_at(header, WRITTEN_OFFSET);
    if written >= WRITTEN_LIMIT {
        return Err(Refusal::Written);
    }
    Ok(Found {
        version,
        capacity,
        sequence: u64_at(header, 24),
        written,
        stop: u64_at(header, STOP_OFFSET),
    })
}

/// Judge a header as the loader leaves it for the kernel: one [`validate`]
/// takes, of version [`VERSION`] exactly, with nothing recorded and a stop
/// word of 0. The kernel's acceptance, before it writes a byte there.
pub fn fresh(header: &[u8; HEADER_BYTES], len: u64) -> Result<Found, Refusal> {
    let found = validate(header, len)?;
    if found.version != VERSION {
        return Err(Refusal::Version(found.version));
    }
    if found.written != 0 {
        return Err(Refusal::Recorded);
    }
    if found.stop != 0 {
        return Err(Refusal::Stopped);
    }
    Ok(found)
}

/// Where the region goes, given the conventional memory the firmware's map
/// describes as `(base, len)` ranges in any order, and the end of the direct
/// map: the lowest [`ALIGN`]ed address at least [`PREFERRED_ABOVE_LOWEST`]
/// above the lowest conventional byte rounded down to [`ALIGN`], whose `len`
/// bytes lie inside one range and end at or below `direct_end`; if there is
/// none, the same with [`ABOVE_LOWEST`]. The same map gives the same address,
/// which is how the next boot finds it again with nothing stored.
pub fn choose(
    conventional: impl IntoIterator<Item = (u64, u64)> + Clone,
    direct_end: u64,
    len: u64,
) -> Option<u64> {
    let lowest = conventional
        .clone()
        .into_iter()
        .filter(|&(_, size)| size != 0)
        .map(|(base, _)| base)
        .min()?;
    let lowest = lowest & !(ALIGN - 1);
    lowest_fit(
        conventional.clone(),
        lowest.checked_add(PREFERRED_ABOVE_LOWEST)?,
        direct_end,
        len,
    )
    .or_else(|| {
        lowest_fit(
            conventional,
            lowest.checked_add(ABOVE_LOWEST)?,
            direct_end,
            len,
        )
    })
}

/// The lowest [`ALIGN`]ed address at or above `floor` whose `len` bytes lie
/// inside one of the `conventional` ranges and end at or below `direct_end`.
fn lowest_fit(
    conventional: impl IntoIterator<Item = (u64, u64)>,
    floor: u64,
    direct_end: u64,
    len: u64,
) -> Option<u64> {
    conventional
        .into_iter()
        .filter_map(|(base, size)| {
            let end = base.checked_add(size)?.min(direct_end);
            let start = align_up(base.max(floor))?;
            (start.checked_add(len)? <= end).then_some(start)
        })
        .min()
}

/// `at` rounded up to [`ALIGN`].
const fn align_up(at: u64) -> Option<u64> {
    match at.checked_add(ALIGN - 1) {
        Some(up) => Some(up & !(ALIGN - 1)),
        None => None,
    }
}

/// The text a found region holds, in order: the bytes from the oldest kept
/// to the newest, read out of the ring.
#[derive(Clone, Copy, Debug)]
pub struct Text<'a> {
    /// The older part, from the oldest kept byte to the end of the ring.
    first: &'a [u8],
    /// The newer part, from the start of the ring.
    second: &'a [u8],
    /// Whether older bytes were overwritten, so the first line is only the
    /// end of one.
    cut: bool,
}

impl<'a> Text<'a> {
    /// The text of `ring`, `found.capacity` bytes, as `found` describes it.
    /// A ring of another length has none.
    #[must_use]
    pub fn of(ring: &'a [u8], found: &Found) -> Text<'a> {
        let none = Text {
            first: &[],
            second: &[],
            cut: false,
        };
        if ring.len() as u64 != found.capacity || found.capacity == 0 {
            return none;
        }
        let kept = found.kept();
        let oldest = found.written.saturating_sub(kept);
        let start = (oldest % found.capacity) as usize;
        let count = kept as usize;
        let tail = ring.len().saturating_sub(start);
        let cut = oldest != 0;
        if count <= tail {
            Text {
                first: ring
                    .get(start..start.saturating_add(count))
                    .unwrap_or_default(),
                second: &[],
                cut,
            }
        } else {
            Text {
                first: ring.get(start..).unwrap_or_default(),
                second: ring.get(..count.saturating_sub(tail)).unwrap_or_default(),
                cut,
            }
        }
    }

    /// Its length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.first.len().saturating_add(self.second.len())
    }

    /// Whether it holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The byte at `at`, from the oldest.
    #[must_use]
    pub fn at(&self, at: usize) -> Option<u8> {
        match self.first.get(at) {
            Some(&byte) => Some(byte),
            None => self.second.get(at.checked_sub(self.first.len())?).copied(),
        }
    }

    /// Its two parts, oldest first: what a writer copies out.
    #[must_use]
    pub const fn parts(&self) -> (&'a [u8], &'a [u8]) {
        (self.first, self.second)
    }

    /// The lines of the text as `(start, end)` byte places, the newline
    /// excluded; a first line cut by the ring's wrap is skipped, as it is
    /// only part of one.
    fn lines(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let mut start = 0;
        let mut skip = self.cut;
        (0..self.len()).filter_map(move |at| {
            if self.at(at) != Some(b'\n') {
                return None;
            }
            let line = (start, at);
            start = at.saturating_add(1);
            (!core::mem::take(&mut skip)).then_some(line)
        })
    }

    /// The last whole line starting with `marker`, the marker left off.
    #[must_use]
    pub fn last_line_after(&self, marker: &[u8]) -> Option<(usize, usize)> {
        self.lines()
            .filter(|&(start, end)| {
                end.saturating_sub(start) >= marker.len()
                    && marker
                        .iter()
                        .enumerate()
                        .all(|(index, &byte)| self.at(start.saturating_add(index)) == Some(byte))
            })
            .last()
            .map(|(start, end)| (start.saturating_add(marker.len()), end))
    }

    /// The last line that is not empty or blanks only, the newline-less tail
    /// included.
    #[must_use]
    pub fn last_line(&self) -> Option<(usize, usize)> {
        let mut end = self.len();
        while end > 0 && matches!(self.at(end.saturating_sub(1)), Some(b'\n' | b'\r' | b' ')) {
            end -= 1;
        }
        if end == 0 {
            return None;
        }
        let mut start = end;
        while start > 0 && self.at(start.saturating_sub(1)) != Some(b'\n') {
            start -= 1;
        }
        Some((start, end))
    }

    /// The bytes from `start` to `end`, at most `most` of them, each as
    /// [`shown`] prints it, into `out`, with every `FERRIX-` made `FERRIX_`
    /// ([`defuse`]); how many were written.
    pub fn shown_into(&self, (start, end): (usize, usize), most: usize, out: &mut [u8]) -> usize {
        let mut count = 0;
        for (slot, at) in out
            .iter_mut()
            .zip(start..end.min(start.saturating_add(most)))
        {
            *slot = shown(self.at(at).unwrap_or(b'?'));
            count += 1;
        }
        defuse(out.get_mut(..count).unwrap_or_default());
        count
    }
}

/// Make every `FERRIX-` in `line` read `FERRIX_`: the previous kernel's
/// words quoted in this boot's log must never read as one of this boot's
/// markers (`FERRIX-PANIC`, `FERRIX-BOOT-OK`), which whatever reads the log
/// looks for anywhere in a line.
pub fn defuse(line: &mut [u8]) {
    const MARK: &[u8] = b"FERRIX-";
    let mut at: usize = 0;
    while let Some(window) = line.get(at..at.saturating_add(MARK.len())) {
        if window == MARK
            && let Some(dash) = line.get_mut(at.saturating_add(MARK.len() - 1))
        {
            *dash = b'_';
        }
        at += 1;
    }
}

/// What the command line asks of the region, `ferrix.pstore=`: on with no
/// option or exactly `on`, off with exactly `off`, and anything else is not
/// understood and taken as off, since a misspelt switch should not keep a
/// log nobody asked for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Switch<'a> {
    /// No option, or `on`.
    On,
    /// `off`.
    Off,
    /// Any other value, taken as off.
    NotUnderstood(&'a str),
}

/// The option [`switch`] reads.
pub const OPTION: &str = "ferrix.pstore";

/// Read [`OPTION`] from `cmdline`.
#[must_use]
pub fn switch(cmdline: &str) -> Switch<'_> {
    match crate::option_in(cmdline, OPTION) {
        None | Some("on") => Switch::On,
        Some("off") => Switch::Off,
        Some(other) => Switch::NotUnderstood(other),
    }
}

/// A byte of unchecked text as a report shows it: printable ASCII, tab and
/// newline as they are, anything else `?`.
#[must_use]
pub const fn shown(byte: u8) -> u8 {
    match byte {
        b'\t' | b'\n' | 0x20..=0x7E => byte,
        _ => b'?',
    }
}

#[cfg(test)]
mod tests;
