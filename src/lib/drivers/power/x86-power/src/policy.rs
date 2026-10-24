//! The driver's decisions each second: which step of the plan is due, and
//! backing off before the kernel's backstop has to (`docs/POWERCTL.md` §8).
//!
//! The policy climbs the plan one step every [`Plan::seconds`]. At or above
//! the soft limit it steps down one step, at most once every
//! [`BACKOFF_SPACING_SECONDS`], and holds there until the hottest reading
//! has been [`COOL_MARGIN`] C below the limit for [`COOL_SECONDS`]. A
//! third back-off without such a cool spell between, or a back-off asked of
//! the lowest step, gives up: firmware's value back, and nothing more is
//! asked until the driver starts again.
//!
//! When the kernel ends a claim on its lease and the driver claims again,
//! firmware's value is back in force: [`Policy::restart`] climbs the plan
//! again from its first step, one step a period as at the start, and keeps
//! what the back-offs learned (the cap, the count, a give-up).

use crate::decode::Reading;
use crate::plan::Plan;

/// The least time between two back-offs, in seconds: temperatures lag a
/// step down, and stepping again before they answer would overshoot.
pub const BACKOFF_SPACING_SECONDS: u64 = 5;
/// Degrees below the soft limit that count as cool.
pub const COOL_MARGIN: i64 = 5;
/// How long a cool spell lifts a back-off, in seconds.
pub const COOL_SECONDS: u64 = 30;
/// Back-offs without a cool spell between that give up.
pub const GIVE_UP_AFTER: u32 = 3;

/// What to ask the kernel this second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing changes.
    Hold,
    /// Set the VR current limit to this many amperes.
    Set {
        /// The step's amperes.
        amps: u32,
        /// Why: the plan, or a back-off.
        why: Why,
    },
    /// Put firmware's value back and ask nothing more.
    GiveUp {
        /// The temperature that made it.
        celsius: i64,
    },
}

/// Why a step changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// The plan's next step came due, or a back-off was lifted.
    Plan,
    /// The hottest reading was at or above the soft limit.
    BackOff {
        /// That reading.
        celsius: i64,
    },
}

/// The policy's state.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    plan: Plan,
    hot: i64,
    /// The step index in force, once the first is written.
    current: Option<usize>,
    /// The highest step index allowed after a back-off.
    cap: Option<usize>,
    /// When the last back-off was, in seconds since the start.
    last_backoff: Option<u64>,
    /// Back-offs since the last cool spell.
    backoffs: u32,
    /// When the hottest reading first went cool, since the last back-off.
    cool_since: Option<u64>,
    /// Whether the policy has given up.
    done: bool,
    /// The second the plan's clock counts from: 0, or when the claim was
    /// last regained.
    origin: u64,
}

impl Policy {
    /// A policy for `plan` with the soft limit `hot` degrees Celsius.
    #[must_use]
    pub const fn new(plan: Plan, hot: u32) -> Policy {
        Policy {
            plan,
            hot: hot as i64,
            current: None,
            cap: None,
            last_backoff: None,
            backoffs: 0,
            cool_since: None,
            done: false,
            origin: 0,
        }
    }

    /// The step index in force.
    #[must_use]
    pub const fn current(&self) -> Option<usize> {
        self.current
    }

    /// Whether the policy has given up.
    #[must_use]
    pub const fn done(&self) -> bool {
        self.done
    }

    /// The action for second `elapsed` since the start, given this second's
    /// reading. The caller reports a write the kernel refused with
    /// [`Policy::refused`].
    pub fn step(&mut self, elapsed: u64, reading: &Reading) -> Action {
        if self.done {
            return Action::Hold;
        }
        let hottest = reading.hottest();
        if let Some(celsius) = hottest.filter(|&celsius| celsius >= self.hot) {
            self.cool_since = None;
            let spaced = self
                .last_backoff
                .is_none_or(|last| elapsed.saturating_sub(last) >= BACKOFF_SPACING_SECONDS);
            if !spaced {
                return Action::Hold;
            }
            let Some(current) = self.current else {
                return Action::Hold;
            };
            self.backoffs += 1;
            if current == 0 || self.backoffs >= GIVE_UP_AFTER {
                self.done = true;
                return Action::GiveUp { celsius };
            }
            let lower = current - 1;
            self.cap = Some(lower);
            self.last_backoff = Some(elapsed);
            self.current = Some(lower);
            return Action::Set {
                amps: self.plan.step(lower),
                why: Why::BackOff { celsius },
            };
        }
        if self.cap.is_some() {
            let cool = hottest.is_some_and(|celsius| celsius <= self.hot - COOL_MARGIN);
            if cool {
                let since = *self.cool_since.get_or_insert(elapsed);
                if elapsed.saturating_sub(since) >= COOL_SECONDS {
                    self.cap = None;
                    self.backoffs = 0;
                    self.cool_since = None;
                }
            } else {
                self.cool_since = None;
            }
        }
        let planned = self.plan.step_at(elapsed.saturating_sub(self.origin));
        let target = self.cap.map_or(planned, |cap| planned.min(cap));
        if self.current == Some(target) {
            return Action::Hold;
        }
        self.current = Some(target);
        Action::Set {
            amps: self.plan.step(target),
            why: Why::Plan,
        }
    }

    /// The kernel refused the last [`Action::Set`]: forget it, so the next
    /// second asks again (a raise refused for its rate is asked once more a
    /// second later).
    pub fn refused(&mut self, was: Option<usize>) {
        self.current = was;
    }

    /// The claim was regained at second `elapsed`, with firmware's value in
    /// force: no step is, and the plan climbs again from its first, its
    /// clock counted from now. A back-off's cap, the back-off count and a
    /// give-up are kept: a lapse is no cool spell.
    pub fn restart(&mut self, elapsed: u64) {
        self.current = None;
        self.origin = elapsed;
        if self.last_backoff.is_some() {
            self.last_backoff = Some(elapsed);
        }
        self.cool_since = None;
    }
}
