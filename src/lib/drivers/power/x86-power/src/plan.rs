//! The VR current step plan: `A1,...,An@SECONDS`, the grammar
//! `ferrix.vrcurrent` had, now the driver's. The kernel's ceilings, not this
//! grammar, bound what is written: a step is a whole number of amperes of
//! one or two digits, and the driver drops a step above the ceiling
//! `HELLO` reports before it asks.

use crate::options::decimal;

/// The most steps a plan may name.
pub const MAX_STEPS: usize = 8;
/// The longest step, in seconds: an hour.
pub const MAX_SECONDS: u32 = 3600;

/// The steps a word asked for, and the seconds each lasts before the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The steps, in amperes; only the first [`Plan::count`] mean anything.
    pub amps: [u32; MAX_STEPS],
    /// How many steps, 1 to [`MAX_STEPS`].
    pub count: usize,
    /// Seconds between steps, 1 to [`MAX_SECONDS`]; unused when adaptive.
    pub seconds: u32,
    /// `auto:LO-HI`: the steps are a range the policy moves within by
    /// temperature, not a climb by the clock (`crate::policy`).
    pub adaptive: bool,
}

/// Why a plan word did not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Not `A1,...,An@SECONDS` of decimal digits, more than [`MAX_STEPS`]
    /// steps, a step of 0 A, or seconds outside 1 to [`MAX_SECONDS`].
    Malformed,
    /// A step lower than the one before it: a plan only climbs, and backing
    /// off is the policy's.
    Descending,
}

impl Plan {
    /// Step `index`'s amperes; the last step's past the end.
    #[must_use]
    pub fn step(&self, index: usize) -> u32 {
        let last = self.count.saturating_sub(1);
        self.amps.get(index.min(last)).copied().unwrap_or(0)
    }

    /// The step in force `elapsed` whole seconds after the first: one more
    /// every [`Plan::seconds`], the last kept.
    #[must_use]
    pub fn step_at(&self, elapsed: u64) -> usize {
        let step = elapsed / u64::from(self.seconds.max(1));
        let last = self.count.saturating_sub(1);
        usize::try_from(step).map_or(last, |step| step.min(last))
    }

    /// The plan without the steps above `ceiling` amperes, and how many were
    /// dropped. `None` when no step is left.
    #[must_use]
    pub fn below(&self, ceiling: u32) -> Option<(Plan, usize)> {
        let mut kept = Plan {
            amps: [0; MAX_STEPS],
            count: 0,
            seconds: self.seconds,
            adaptive: self.adaptive,
        };
        for &amps in self.amps.iter().take(self.count) {
            if amps <= ceiling
                && let Some(slot) = kept.amps.get_mut(kept.count)
            {
                *slot = amps;
                kept.count += 1;
            }
        }
        (kept.count > 0).then_some((kept, self.count - kept.count))
    }
}

/// The plan in a word's value.
///
/// # Errors
///
/// [`PlanError::Malformed`] for anything not of the form, and
/// [`PlanError::Descending`] for a step below the one before it.
pub fn parse(value: &[u8]) -> Result<Plan, PlanError> {
    if let Some(range) = value.strip_prefix(b"auto") {
        return adaptive(range);
    }
    let at = value
        .iter()
        .position(|&byte| byte == b'@')
        .ok_or(PlanError::Malformed)?;
    let seconds = value
        .get(at + 1..)
        .and_then(|text| decimal(text, 4))
        .ok_or(PlanError::Malformed)?;
    if seconds == 0 || seconds > MAX_SECONDS {
        return Err(PlanError::Malformed);
    }
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count: 0,
        seconds,
        adaptive: false,
    };
    for step in value
        .get(..at)
        .unwrap_or_default()
        .split(|&byte| byte == b',')
    {
        let amps = decimal(step, 2).ok_or(PlanError::Malformed)?;
        if amps == 0 {
            return Err(PlanError::Malformed);
        }
        if plan.count > 0 && amps < plan.step(plan.count - 1) {
            return Err(PlanError::Descending);
        }
        let slot = plan.amps.get_mut(plan.count).ok_or(PlanError::Malformed)?;
        *slot = amps;
        plan.count += 1;
    }
    Ok(plan)
}

/// The range an adaptive plan moves within without one named: the N150's
/// firmware value and a little above it, to the 44 A its box runs took.
pub const AUTO_RANGE: (u32, u32) = (30, 44);

/// `auto` or `auto:LO-HI`: [`MAX_STEPS`] steps spread evenly from LO to HI,
/// whole amperes, ascending; fewer when the range is narrower than that.
fn adaptive(range: &[u8]) -> Result<Plan, PlanError> {
    let (low, high) = if range.is_empty() {
        AUTO_RANGE
    } else {
        let range = range.strip_prefix(b":").ok_or(PlanError::Malformed)?;
        let dash = range
            .iter()
            .position(|&byte| byte == b'-')
            .ok_or(PlanError::Malformed)?;
        let low = decimal(range.get(..dash).unwrap_or_default(), 2).ok_or(PlanError::Malformed)?;
        let high =
            decimal(range.get(dash + 1..).unwrap_or_default(), 2).ok_or(PlanError::Malformed)?;
        (low, high)
    };
    if low == 0 {
        return Err(PlanError::Malformed);
    }
    if high < low {
        return Err(PlanError::Descending);
    }
    let mut plan = Plan {
        amps: [0; MAX_STEPS],
        count: 0,
        seconds: 1,
        adaptive: true,
    };
    let span = high - low;
    let last = u32::try_from(MAX_STEPS - 1).unwrap_or(1);
    for index in 0..=last {
        let amps = low + span * index / last;
        if plan.count > 0 && plan.step(plan.count - 1) == amps {
            continue;
        }
        if let Some(slot) = plan.amps.get_mut(plan.count) {
            *slot = amps;
            plan.count += 1;
        }
    }
    Ok(plan)
}
