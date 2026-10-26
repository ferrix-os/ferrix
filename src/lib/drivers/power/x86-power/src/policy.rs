//! The driver's decisions each second: which step of the plan is due, and
//! backing off before the kernel's backstop has to (`docs/POWERCTL.md` §8).
//!
//! The policy climbs the plan one step every [`Plan::seconds`]. At or above
//! the soft limit it steps down one step, at most once every
//! [`BACKOFF_SPACING_SECONDS`], and holds there until the hottest reading
//! has been [`COOL_MARGIN`] C below the limit for [`COOL_SECONDS`]; each
//! such cool spell lets it one step higher again, not straight back to the
//! plan's top. A
//! third back-off without such a cool spell between, or a back-off asked of
//! the lowest step, gives up: firmware's value back, and nothing more is
//! asked until the driver starts again.
//!
//! An adaptive plan (`auto:LO-HI`, [`Plan::adaptive`]) has no clock: it
//! starts at its lowest step and moves by temperature, reading the hottest
//! sensor's trend [`ADAPT_LOOKAHEAD_SECONDS`] ahead, so it finds the
//! highest step the cooling holds without the soft limit being reached
//! (`Policy::adapt`).
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

/// An adaptive plan's headroom: it climbs only while the hottest reading,
/// now and as its trend says it will be, is this far below the soft limit.
pub const ADAPT_HEADROOM: i64 = 6;
/// Within this many degrees of the soft limit, now or as the trend says,
/// an adaptive plan steps down one step.
pub const ADAPT_NEAR: i64 = 3;
/// How far ahead the trend is read, in seconds: about how long the
/// package takes to answer a step.
pub const ADAPT_LOOKAHEAD_SECONDS: i64 = 10;
/// The trend alone steps down only this close to the soft limit, or
/// nearer: a package warming fast from cold, as a game loading warms it,
/// is no reason to give back current (the N150 on 2026-10-26 backed off
/// from 42 A to 34 A at 68 C on its trend alone).
pub const ADAPT_TREND_FROM: i64 = 12;
/// The least time between two climbs, in seconds.
pub const ADAPT_CLIMB_SECONDS: u64 = 15;
/// The least time between two steps down, in seconds.
pub const ADAPT_DROP_SECONDS: u64 = 3;
/// How long the step that got too hot stays the ceiling before the policy
/// tries above it again, in seconds: the room warms, the load changes.
pub const ADAPT_PROBE_SECONDS: u64 = 180;

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
    /// An adaptive plan's state.
    adapt: Adapt,
}

/// What an adaptive plan keeps from one second to the next.
#[derive(Clone, Copy, Debug)]
struct Adapt {
    /// The last second's hottest reading and when it was.
    last: Option<(u64, i64)>,
    /// The trend, in thousandths of a degree a second, smoothed.
    slope: i64,
    /// When the step last changed.
    changed: Option<u64>,
    /// The highest step allowed, and since when: the one below the step
    /// that last got too hot.
    ceiling: Option<(usize, u64)>,
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
            adapt: Adapt {
                last: None,
                slope: 0,
                changed: None,
                ceiling: None,
            },
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
        if self.plan.adaptive {
            return self.adapt(elapsed, reading);
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
                // One step up a spell: on the N150 on 2026-10-25 a lifted
                // cap went from 37 A straight back to 44 A at 80 C, and the
                // package passed 90 C, where the kernel's backstop put
                // firmware's 25 A back until reboot.
                if elapsed.saturating_sub(since) >= COOL_SECONDS {
                    self.cap = self.cap.map(|cap| cap.saturating_add(1));
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

    /// An adaptive plan's second: from the lowest step, one step up at a
    /// time while the hottest reading and its trend leave
    /// [`ADAPT_HEADROOM`], one down as soon as the reading comes within
    /// [`ADAPT_NEAR`] of the soft limit, or the trend does once the reading
    /// is within [`ADAPT_TREND_FROM`], two at or above it. The step whose
    /// reading got that near caps the climb for [`ADAPT_PROBE_SECONDS`]; then one
    /// more is tried. It never gives up: the lowest step is the floor, and
    /// the kernel's backstop is still there above it.
    fn adapt(&mut self, elapsed: u64, reading: &Reading) -> Action {
        let Some(hottest) = reading.hottest() else {
            return Action::Hold;
        };
        if let Some((then, was)) = self.adapt.last
            && elapsed > then
        {
            let seconds = i64::try_from(elapsed - then).unwrap_or(i64::MAX);
            let now = (hottest - was).saturating_mul(1000) / seconds;
            self.adapt.slope = (self.adapt.slope * 2 + now) / 3;
        }
        self.adapt.last = Some((elapsed, hottest));
        let ahead = hottest + self.adapt.slope * ADAPT_LOOKAHEAD_SECONDS / 1000;
        let top = self.plan.count.saturating_sub(1);
        let Some(current) = self.current else {
            return self.adapt_to(0, elapsed, Why::Plan);
        };
        let since = |at: Option<u64>| at.map_or(u64::MAX, |at| elapsed.saturating_sub(at));
        let quiet = since(self.adapt.changed);
        if hottest >= self.hot {
            if current == 0 || quiet < ADAPT_DROP_SECONDS {
                return Action::Hold;
            }
            let lower = current.saturating_sub(2);
            self.adapt.ceiling = Some((lower, elapsed));
            return self.adapt_to(lower, elapsed, Why::BackOff { celsius: hottest });
        }
        let near = hottest >= self.hot - ADAPT_NEAR;
        let trend = ahead >= self.hot - ADAPT_NEAR && hottest >= self.hot - ADAPT_TREND_FROM;
        if near || trend {
            if current == 0 || quiet < ADAPT_DROP_SECONDS {
                return Action::Hold;
            }
            let lower = current - 1;
            // Only a reading near the limit says where the cooling ends;
            // the trend's guess caps nothing.
            if near {
                self.adapt.ceiling = Some((lower, elapsed));
            }
            return self.adapt_to(lower, elapsed, Why::BackOff { celsius: hottest });
        }
        let room = hottest.max(ahead) <= self.hot - ADAPT_HEADROOM;
        if !room || current >= top || quiet < ADAPT_CLIMB_SECONDS {
            return Action::Hold;
        }
        match self.adapt.ceiling {
            Some((ceiling, set)) if current >= ceiling => {
                if since(Some(set)) < ADAPT_PROBE_SECONDS {
                    return Action::Hold;
                }
                self.adapt.ceiling = Some((current + 1, elapsed));
            }
            _ => {}
        }
        self.adapt_to(current + 1, elapsed, Why::Plan)
    }

    /// Step `index` from second `elapsed`, as an adaptive plan moves.
    fn adapt_to(&mut self, index: usize, elapsed: u64, why: Why) -> Action {
        self.current = Some(index);
        self.adapt.changed = Some(elapsed);
        Action::Set {
            amps: self.plan.step(index),
            why,
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
