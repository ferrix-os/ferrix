//! The frame: fixed windows per processor, one major frame for all, sealed once.

/// The most partitions a frame names, and the sets a class holds: partition 0 is the
/// kernel's own, every kernel thread and every task with no job.
pub const PARTITIONS: usize = 8;

/// The most windows on one processor: what bounds the search for the current one.
pub const WINDOWS_MOST: usize = 16;

/// The least partition-0 window a processor in service is sealed with: the reaper's and the
/// grace period's worker's one pass each, with room for the timer's work (`docs/microkernel/K9.md` §4).
pub const FLOOR_NS: u64 = 500_000;

/// One window: a partition's time on one processor, from `offset_ns` into the major frame
/// for `length_ns`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Window {
    /// Whose time it is.
    pub partition: u8,
    /// Where it starts in the major frame.
    pub offset_ns: u64,
    /// How long it lasts.
    pub length_ns: u64,
}

impl Window {
    /// Where it ends in the major frame, or `None` past `u64`.
    #[must_use]
    pub const fn end_ns(self) -> Option<u64> {
        self.offset_ns.checked_add(self.length_ns)
    }
}

/// Why a window or a seal was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// It overlaps a window already on its processor.
    Overlap,
    /// It ends past the major frame.
    PastMajor,
    /// Its processor is out of service, or there is no such processor.
    OutOfService,
    /// A processor in service has no partition-0 window of [`FLOOR_NS`].
    NoPartitionZero,
    /// Its processor already has [`WINDOWS_MOST`].
    Full,
    /// Its partition is not below [`PARTITIONS`].
    Partition,
    /// It, or the major frame, is empty.
    Empty,
}

impl Refusal {
    /// What the refusal says, for a boot check's line.
    #[must_use]
    pub const fn says(self) -> &'static str {
        match self {
            Refusal::Overlap => "a window overlaps another on its processor",
            Refusal::PastMajor => "a window ends past the major frame",
            Refusal::OutOfService => "a window is on a processor out of service",
            Refusal::NoPartitionZero => {
                "a processor in service has no partition-0 window of the floor"
            }
            Refusal::Full => "a processor has its most windows",
            Refusal::Partition => "a window names no partition",
            Refusal::Empty => "a window or the major frame is empty",
        }
    }
}

/// One processor's windows, in offset order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    windows: [Option<Window>; WINDOWS_MOST],
    count: usize,
}

impl Row {
    /// No windows.
    #[must_use]
    pub const fn empty() -> Row {
        Row {
            windows: [None; WINDOWS_MOST],
            count: 0,
        }
    }

    /// How many windows it has.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Its windows in offset order.
    pub fn windows(&self) -> impl Iterator<Item = Window> + '_ {
        self.windows.iter().flatten().copied()
    }

    fn add(&mut self, window: Window) -> Result<(), Refusal> {
        let end = window.end_ns().ok_or(Refusal::PastMajor)?;
        for other in self.windows() {
            let other_end = other.end_ns().unwrap_or(u64::MAX);
            if window.offset_ns < other_end && other.offset_ns < end {
                return Err(Refusal::Overlap);
            }
        }
        if self.count >= WINDOWS_MOST {
            return Err(Refusal::Full);
        }
        // Kept in offset order: insertion into a short array.
        let at = self
            .windows()
            .take_while(|other| other.offset_ns < window.offset_ns)
            .count();
        let mut moving = Some(window);
        for slot in self.windows.iter_mut().skip(at) {
            moving = core::mem::replace(slot, moving);
            if moving.is_none() {
                break;
            }
        }
        self.count += 1;
        Ok(())
    }

    /// The window at `phase` into the major frame: its partition and where it ends, or for a
    /// gap, `None` and where the next window starts (or the frame's end). One compare a window.
    #[must_use]
    pub fn at(&self, phase: u64, major_ns: u64) -> (Option<u8>, u64) {
        for window in self.windows() {
            let end = window.end_ns().unwrap_or(u64::MAX);
            if phase < window.offset_ns {
                return (None, window.offset_ns);
            }
            if phase < end {
                return (Some(window.partition), end);
            }
        }
        (None, major_ns)
    }
}

/// The most processors a frame covers: the kernel's processor masks are one word.
pub const FRAME_CPUS: usize = 64;

/// Give `cpu` a window in `rows`, a frame being set: the caller keeps the rows (a kernel
/// static, so nothing this large is ever on a stack) and seals them with [`seal`].
///
/// # Errors
///
/// [`Refusal::Partition`], [`Refusal::Empty`], [`Refusal::OutOfService`] for no such
/// processor, [`Refusal::Overlap`], [`Refusal::Full`] or [`Refusal::PastMajor`] when it
/// cannot end.
pub fn set_window(rows: &mut [Row], cpu: usize, window: Window) -> Result<(), Refusal> {
    if usize::from(window.partition) >= PARTITIONS {
        return Err(Refusal::Partition);
    }
    if window.length_ns == 0 {
        return Err(Refusal::Empty);
    }
    if cpu >= FRAME_CPUS {
        return Err(Refusal::OutOfService);
    }
    rows.get_mut(cpu).ok_or(Refusal::OutOfService)?.add(window)
}

/// Whether `rows` seal as a frame `major_ns` long for the processors in `in_service` (bit `n`
/// for processor `n`). Nothing is changed: the caller records the seal once.
///
/// # Errors
///
/// [`Refusal::Empty`] for no major frame, [`Refusal::PastMajor`],
/// [`Refusal::OutOfService`] for a window on a processor not in service, and
/// [`Refusal::NoPartitionZero`].
pub fn seal(rows: &[Row], major_ns: u64, in_service: u64) -> Result<(), Refusal> {
    if major_ns == 0 {
        return Err(Refusal::Empty);
    }
    for cpu in 0..FRAME_CPUS {
        let row = rows.get(cpu).copied().unwrap_or(Row::empty());
        let serving = u32::try_from(cpu)
            .ok()
            .and_then(|bit| 1_u64.checked_shl(bit))
            .is_some_and(|bit| in_service & bit != 0);
        if row.count != 0 && !serving {
            return Err(Refusal::OutOfService);
        }
        if row
            .windows()
            .any(|window| window.end_ns().is_none_or(|end| end > major_ns))
        {
            return Err(Refusal::PastMajor);
        }
        if serving
            && !row
                .windows()
                .any(|window| window.partition == 0 && window.length_ns >= FLOOR_NS)
        {
            return Err(Refusal::NoPartitionZero);
        }
    }
    if rows.iter().skip(FRAME_CPUS).any(|row| row.count != 0) {
        return Err(Refusal::OutOfService);
    }
    Ok(())
}

/// What one processor's class holds of the frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Part {
    /// Its windows.
    pub row: Row,
    /// The major frame.
    pub major_ns: u64,
    /// The first frame's start.
    pub epoch: u64,
}

impl Part {
    /// `row` of a sealed frame, `major_ns` long from `epoch`; `None` for a row with no window.
    #[must_use]
    pub const fn new(row: Row, major_ns: u64, epoch: u64) -> Option<Part> {
        if row.count == 0 || major_ns == 0 {
            return None;
        }
        Some(Part {
            row,
            major_ns,
            epoch,
        })
    }

    /// At `now`: the partition whose window it is (`None` in a gap, or before the epoch),
    /// and the absolute time that answer ends.
    #[must_use]
    pub fn at(&self, now: u64) -> (Option<u8>, u64) {
        if now < self.epoch {
            return (None, self.epoch);
        }
        let since = now - self.epoch;
        let phase = since % self.major_ns;
        let start = now - phase;
        let (partition, until) = self.row.at(phase, self.major_ns);
        (partition, start.saturating_add(until))
    }
}
