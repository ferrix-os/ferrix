//! The driver's own share of the GGTT. i915 manages the whole GGTT with
//! `drm_mm` and reserves what firmware and the display hold
//! (`intel_ggtt` `ggtt_reserve_guc_top`, the BIOS framebuffer's preserved
//! range). Render-only keeps away from every entry the firmware left
//! present (M1 counts them) and allocates from a window above them, first
//! fit, page-aligned, with a fixed number of slots: the driver's rings,
//! context images and status pages are a few dozen objects.

/// How many allocations the space tracks at once.
pub const SLOTS: usize = 128;

/// One allocation: GGTT byte offset and length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    /// First byte.
    pub start: u64,
    /// Bytes, a multiple of 4096.
    pub len: u64,
}

/// Why an allocation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpaceError {
    /// No gap in the window is large enough.
    Full,
    /// All [`SLOTS`] are in use.
    Slots,
    /// A length of 0, or an alignment that is not a power of two of at
    /// least a page.
    Invalid,
}

/// The window `[start, end)` of the GGTT this driver owns, and what is in
/// it, sorted by start.
#[derive(Clone, Copy, Debug)]
pub struct GgttSpace {
    start: u64,
    end: u64,
    used: [Option<Range>; SLOTS],
    count: usize,
}

impl GgttSpace {
    /// A window from `start` to `end`, both page-aligned (rounded inwards).
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        GgttSpace {
            start: (start + 4095) & !4095,
            end: end & !4095,
            used: [None; SLOTS],
            count: 0,
        }
    }

    /// The window M1 would give: everything above the highest entry the
    /// firmware left present, up to the GGTT's size.
    #[must_use]
    pub const fn above_firmware(last_present_entry: Option<u64>, total: u64) -> Self {
        let start = match last_present_entry {
            Some(index) => (index + 1) * 4096,
            None => 0,
        };
        Self::new(start, total)
    }

    /// Allocate `len` bytes (rounded up to pages) at `align` (a power of two,
    /// at least 4096), first fit.
    pub fn alloc(&mut self, len: u64, align: u64) -> Result<Range, SpaceError> {
        if len == 0 || align < 4096 || !align.is_power_of_two() {
            return Err(SpaceError::Invalid);
        }
        if self.count >= SLOTS {
            return Err(SpaceError::Slots);
        }
        let len = (len + 4095) & !4095;
        let mut cursor = self.start;
        let mut at = 0;
        for used in self.used.iter().take(self.count).flatten() {
            let candidate = (cursor + align - 1) & !(align - 1);
            if candidate + len <= used.start {
                break;
            }
            cursor = used.start + used.len;
            at += 1;
        }
        let candidate = (cursor + align - 1) & !(align - 1);
        if candidate + len > self.end {
            return Err(SpaceError::Full);
        }
        let range = Range {
            start: candidate,
            len,
        };
        let tail = self.used.get_mut(at..=self.count).unwrap_or(&mut []);
        tail.rotate_right(1);
        if let Some(slot) = tail.first_mut() {
            *slot = Some(range);
        }
        self.count += 1;
        Ok(range)
    }

    /// Give `range` back; whether it was allocated.
    pub fn free(&mut self, range: Range) -> bool {
        let Some(at) = self
            .used
            .iter()
            .take(self.count)
            .position(|r| *r == Some(range))
        else {
            return false;
        };
        let tail = self.used.get_mut(at..self.count).unwrap_or(&mut []);
        tail.rotate_left(1);
        if let Some(last) = tail.last_mut() {
            *last = None;
        }
        self.count -= 1;
        true
    }

    /// Bytes allocated.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.used.iter().flatten().map(|r| r.len).sum()
    }
}
