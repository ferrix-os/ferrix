//! When the next frame may be drawn: no sooner than one refresh of the
//! screen after the last.
//!
//! Hyprland draws a monitor when its frame scheduler says so, which is on
//! the screen's own clock (`CMonitorFrameScheduler::onFrame` in
//! `src/output/MonitorFrameScheduler.cpp`): a pointer reporting a thousand
//! times a second still draws sixty frames. This compositor's loop draws
//! when something changed, and until a frame that changes little cost
//! little that was pacing of a kind -- a frame took a tenth of a second, so
//! there were ten. Once a pointer motion's frame cost a fraction of a
//! millisecond there was one for every event the pointer sent.
//!
//! And a frame is not only drawing. On a card it ends in a page flip, and
//! Ferrix's virtio-gpu answers a flip by setting the scanout and sending the
//! whole framebuffer to the host, waited for (`show` in
//! `src/kernel/src/interfaces/display/drm.rs`). A guest with one processor spent it doing
//! that, and the pointer stuttered over everything -- worse than when every
//! frame was slow, which is how this was found.
//!
//! Input is read as soon as its device is ready and everything it does still
//! happens at once. Only the drawing waits, and what it draws is where the
//! pointer is by then.
//!
//! A card whose flip waits for the screen's vertical blank -- the Intel
//! driver's card0 copies a frame in the blank and answers then -- answers
//! the flip in that blank. Drawn on a grid of its own, the loop drew at any
//! point of the screen's refresh and then waited up to a whole one for the
//! answer: in the flip, until flips went to a thread of their own
//! (`crate::flush::FlipThread`), and since then for the frame to land
//! before the next is drawn. So a flip that waited ([`WAITED`]) sets the
//! grid by the blank it ended in, timed on the thread, as a KMS compositor
//! draws when the vblank event of its last flip comes (wlroots'
//! `wlr_output` frame event, Hyprland's
//! `CMonitorFrameScheduler::onFrame`): the next frame is drawn [`LEAD`]
//! before the next blank, and the clients are told that blank is when it
//! is shown (`wp_presentation_feedback.presented`).

use std::time::{Duration, Instant};

/// A flip that took this long waited for the screen.
pub(crate) const WAITED: Duration = Duration::from_millis(2);
/// How long before a blank the frame for it is drawn: the drawing of a
/// directly shown frame takes about a millisecond and its copy another.
pub(crate) const LEAD: Duration = Duration::from_millis(4);

/// The clock frames are drawn by.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pace {
    /// When the next frame may be drawn, or `None` before the first.
    next: Option<Instant>,
    /// When the frame last allowed was due: the refresh it stands for,
    /// which is what its presentation time says. On the grid of refreshes,
    /// where a real screen's vblanks are, rather than whenever the loop
    /// got round to it.
    drawn: Option<Instant>,
    /// The grid is the screen's: each frame is drawn [`LEAD`] before the
    /// blank it is shown in.
    aligned: bool,
}

impl Pace {
    /// How long remains before a paced frame may be drawn.
    ///
    /// There is no timer before the first frame: work which needs that frame
    /// draws it immediately.
    #[must_use]
    pub(crate) fn until(&self, now: Instant) -> Option<Duration> {
        self.next.map(|next| next.saturating_duration_since(now))
    }

    /// Whether a frame may be drawn at `now`, on a screen whose refresh is
    /// `period` nanoseconds; and if so, that one is about to be.
    ///
    /// The next is a period after this one was *due*, not after it was
    /// drawn, so a loop that wakes a little late each time still draws at
    /// the screen's rate rather than a little under it. A loop that was
    /// away for longer than a period starts again from now: the frames it
    /// missed are not owed.
    pub(crate) fn due(&mut self, now: Instant, period: u32) -> bool {
        let period = Duration::from_nanos(u64::from(period));
        match self.next {
            Some(next) if now < next => false,
            Some(next) if now.saturating_duration_since(next) < period => {
                self.drawn = Some(next);
                self.next = next.checked_add(period);
                true
            }
            _ => {
                self.drawn = Some(now);
                self.next = now.checked_add(period);
                true
            }
        }
    }

    /// When the frame [`Pace::due`] last allowed is shown: the blank after
    /// it on a grid set by the screen, else when it was due.
    #[must_use]
    pub(crate) fn shown(&self) -> Option<Instant> {
        let drawn = self.drawn?;
        Some(if self.aligned {
            drawn.checked_add(LEAD).unwrap_or(drawn)
        } else {
            drawn
        })
    }

    /// A flip that began at `began` came back at `ended`. One that waited
    /// for the screen ended in its blank, and the next frame is drawn
    /// [`LEAD`] before the one a `period` on.
    pub(crate) fn flipped(&mut self, began: Instant, ended: Instant, period: u32) {
        if ended.saturating_duration_since(began) < WAITED {
            return;
        }
        let period = Duration::from_nanos(u64::from(period));
        let Some(next) = ended
            .checked_add(period)
            .and_then(|at| at.checked_sub(LEAD))
        else {
            return;
        };
        self.next = Some(next);
        self.aligned = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sixty frames a second, as `wl_output` says it.
    const PERIOD: u32 = 16_666_666;

    /// Even a loop that asks more often than a refresh draws a second's worth
    /// of frames in a second and no more.
    #[test]
    fn a_second_of_asking_is_a_second_of_frames() {
        let start = Instant::now();
        let mut pace = Pace::default();
        let drawn = (0..500u64)
            .filter(|pass| pace.due(start + Duration::from_millis(pass * 2), PERIOD))
            .count();
        assert!(
            (59..=61).contains(&drawn),
            "{drawn} frames in a second at sixty a second"
        );
    }

    /// The first frame is drawn when it is asked for, and a frame asked for
    /// after a long quiet is drawn at once rather than owed the ones the
    /// quiet skipped.
    #[test]
    fn nothing_waits_that_does_not_have_to() {
        let start = Instant::now();
        let mut pace = Pace::default();
        assert!(pace.due(start, PERIOD), "the first frame waited");
        assert!(
            !pace.due(start + Duration::from_millis(5), PERIOD),
            "a second frame inside the same refresh"
        );
        let later = start + Duration::from_secs(3);
        assert!(pace.due(later, PERIOD), "a frame after a quiet waited");
        assert!(
            !pace.due(later + Duration::from_millis(1), PERIOD),
            "the quiet was paid back in frames"
        );
    }

    /// A flip that waited for the blank sets the grid: the next frame is
    /// drawn the lead before the next blank, and is said to be shown at it.
    #[test]
    fn a_flip_that_waited_sets_the_grid_by_its_blank() {
        let start = Instant::now();
        let mut pace = Pace::default();
        assert!(pace.due(start, PERIOD));
        // The flip waited 9 ms for the blank.
        let blank = start + Duration::from_millis(10);
        pace.flipped(start + Duration::from_millis(1), blank, PERIOD);
        let next = blank + Duration::from_nanos(u64::from(PERIOD)) - LEAD;
        assert!(!pace.due(next - Duration::from_micros(10), PERIOD));
        assert!(pace.due(next, PERIOD));
        assert_eq!(pace.shown(), Some(next + LEAD));
    }

    /// A flip that did not wait leaves the grid where it was.
    #[test]
    fn a_flip_that_did_not_wait_changes_nothing() {
        let start = Instant::now();
        let mut pace = Pace::default();
        assert!(pace.due(start, PERIOD));
        let before = pace.until(start);
        pace.flipped(start, start + Duration::from_millis(1), PERIOD);
        assert_eq!(pace.until(start), before);
        assert_eq!(pace.shown(), Some(start));
    }

    #[test]
    fn the_next_frame_has_a_wait_until_its_clock() {
        let start = Instant::now();
        let mut pace = Pace::default();
        assert_eq!(pace.until(start), None, "the first frame is immediate");
        assert!(pace.due(start, PERIOD));
        assert_eq!(
            pace.until(start),
            Some(Duration::from_nanos(u64::from(PERIOD)))
        );
        assert_eq!(
            pace.until(start + Duration::from_nanos(u64::from(PERIOD))),
            Some(Duration::ZERO)
        );
    }
}
