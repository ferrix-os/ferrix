//! Where a frame's time went, by kind of work.
//!
//! A compositor's report of its slowest frame says *that* a frame was slow;
//! on a machine a hundred times slower than the one the renderer was written
//! on -- a 650 MHz Cortex-A7 drawing in software -- what is wanted next is
//! *which* part of it. So the renderer adds the time it spends on each kind
//! of work to one counter per kind, and the compositor takes the counters
//! after each frame and keeps those of the slowest.
//!
//! Wall time on the thread that asked, which for work spread over the cores
//! ([`crate::cores`]) is the time until the last band finished: what the
//! frame waited for, which is what a person waits for.
//!
//! Atomics rather than anything per thread, because a frame is drawn on one
//! thread and taken on the same one; relaxed, because a count that is a
//! microsecond late to be seen is still counted in the next frame's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A kind of work a frame is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The background and the layer surfaces under the windows.
    Behind,
    /// Keeping the copy of what is behind the windows for the blur.
    Backdrop,
    /// Windows' shadows.
    Shadow,
    /// Windows' borders.
    Border,
    /// The blur behind translucent windows and surfaces.
    Blur,
    /// Windows' own pixels.
    Surface,
    /// Layer surfaces over the windows, the drag icon and the pointer.
    Over,
    /// Copying the frame into the screen's buffer, turned if the monitor is.
    Present,
    /// Handing the buffer to the card: the kernel's cache clean and the flip.
    Flip,
}

impl Phase {
    /// Every kind, in the order a report lists them.
    pub const ALL: [Phase; 9] = [
        Phase::Behind,
        Phase::Backdrop,
        Phase::Shadow,
        Phase::Border,
        Phase::Blur,
        Phase::Surface,
        Phase::Over,
        Phase::Present,
        Phase::Flip,
    ];

    /// The word a report uses for it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Phase::Behind => "behind",
            Phase::Backdrop => "backdrop",
            Phase::Shadow => "shadow",
            Phase::Border => "border",
            Phase::Blur => "blur",
            Phase::Surface => "surface",
            Phase::Over => "over",
            Phase::Present => "present",
            Phase::Flip => "flip",
        }
    }
}

/// Microseconds spent on each kind since the last [`take`], by
/// [`Phase::ALL`]'s order.
static SPENT: [AtomicU64; Phase::ALL.len()] = [const { AtomicU64::new(0) }; Phase::ALL.len()];

/// Run `work`, counting its time to `phase`.
pub fn timed<T>(phase: Phase, work: impl FnOnce() -> T) -> T {
    let _timer = Timer::start(phase);
    work()
}

/// Counts the time from [`Timer::start`] until it is dropped to its phase:
/// for work that is the rest of a block rather than one call.
#[derive(Debug)]
pub struct Timer {
    /// What the time is counted to.
    phase: Phase,
    /// When it started.
    began: Instant,
}

impl Timer {
    /// Start counting to `phase`.
    #[must_use]
    pub fn start(phase: Phase) -> Self {
        Self {
            phase,
            began: Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let micros = u64::try_from(self.began.elapsed().as_micros()).unwrap_or(u64::MAX);
        if let Some(counter) = SPENT.get(self.phase as usize) {
            let _ = counter.fetch_add(micros, Ordering::Relaxed);
        }
    }
}

/// Microseconds spent on each kind since the last call, by
/// [`Phase::ALL`]'s order, and the counters back at zero.
#[must_use]
pub fn take() -> [u64; Phase::ALL.len()] {
    let mut spent = [0; Phase::ALL.len()];
    for (slot, counter) in spent.iter_mut().zip(&SPENT) {
        *slot = counter.swap(0, Ordering::Relaxed);
    }
    spent
}

/// How a window's pixels were put on the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// Copied, a row at a time: opaque and at its own size.
    Copy,
    /// Blended over what is under it, at its own size.
    Blend,
    /// Stretched, opaque.
    Stretch,
    /// Stretched and blended.
    StretchBlend,
    /// Copied straight into the screen's buffer, the frame drawn nowhere
    /// else: a window drawn alone (`crate::alone`).
    Direct,
    /// Not copied by the compositor at all: the card shown the client's own
    /// buffer (direct scanout).
    Scanout,
}

impl Path {
    /// The word a report uses for it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Path::Copy => "copy",
            Path::Blend => "blend",
            Path::Stretch => "stretch",
            Path::StretchBlend => "stretch-blend",
            Path::Direct => "direct",
            Path::Scanout => "scanout",
        }
    }
}

/// The largest window surface drawn since the last [`take_surface`]: what a
/// slow `surface` term is about. Its width, height, whether it has alpha and
/// its [`Path`], packed: zero is none.
static LARGEST: AtomicU64 = AtomicU64::new(0);

/// The largest window surface a frame drew, as [`drawn_surface`] heard it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drawn {
    /// Its buffer's width and height.
    pub size: (u32, u32),
    /// Whether its format has alpha.
    pub alpha: bool,
    /// How it was drawn.
    pub path: Path,
}

impl Drawn {
    /// As a report prints it: `1920x1200 xrgb copy`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{}x{} {} {}",
            self.size.0,
            self.size.1,
            if self.alpha { "argb" } else { "xrgb" },
            self.path.name()
        )
    }
}

/// Note that a window surface `size` large, with `alpha` or not, was drawn
/// by `path`; kept if it is the largest since the last [`take_surface`].
pub fn drawn_surface(size: (u32, u32), alpha: bool, path: Path) {
    let width = u64::from(size.0.min(0xFF_FFFF));
    let height = u64::from(size.1.min(0xFF_FFFF));
    let path = match path {
        Path::Copy => 0,
        Path::Blend => 1,
        Path::Stretch => 2,
        Path::StretchBlend => 3,
        Path::Direct => 4,
        Path::Scanout => 5,
    };
    let packed = (width << 40) | (height << 16) | (u64::from(alpha) << 8) | path | 0x80;
    let area = |packed: u64| (packed >> 40) * ((packed >> 16) & 0xFF_FFFF);
    let _ = LARGEST.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |was| {
        (area(packed) >= area(was)).then_some(packed)
    });
}

/// The largest window surface drawn since the last call, and none again.
#[must_use]
pub fn take_surface() -> Option<Drawn> {
    let packed = LARGEST.swap(0, Ordering::Relaxed);
    if packed == 0 {
        return None;
    }
    let size = (
        u32::try_from(packed >> 40).unwrap_or(0),
        u32::try_from((packed >> 16) & 0xFF_FFFF).unwrap_or(0),
    );
    let path = match packed & 0x7F {
        0 => Path::Copy,
        1 => Path::Blend,
        2 => Path::Stretch,
        3 => Path::StretchBlend,
        4 => Path::Direct,
        _ => Path::Scanout,
    };
    Some(Drawn {
        size,
        alpha: packed & 0x100 != 0,
        path,
    })
}

/// `spent`, as a report prints it: each kind that took any time, by name.
#[must_use]
pub fn describe(spent: &[u64; Phase::ALL.len()]) -> String {
    let mut out = String::new();
    for (phase, micros) in Phase::ALL.iter().zip(spent) {
        if *micros == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(phase.name());
        out.push(' ');
        out.push_str(&micros.to_string());
    }
    out
}
