//! The restart policy (§5.4), alone, as `devmgr` uses it.

use core::time::Duration;

use crate::{
    Backoff, Budget, Decision, Ended, Escalation, Exit, Instant, Next, Policy, Restart, Signal,
    wanted,
};

#[test]
fn which_ends_restart_is_systemds_table() {
    let ends = [
        Ended::Success,
        Ended::ExitCode,
        Ended::Signal,
        Ended::Timeout,
        Ended::Watchdog,
        Ended::CoreDump,
        Ended::OomKill,
    ];
    let row = |restart| ends.map(|ended| wanted(restart, ended));
    assert_eq!(row(Restart::No), [false; 7]);
    assert_eq!(row(Restart::Always), [true; 7]);
    assert_eq!(
        row(Restart::OnSuccess),
        [true, false, false, false, false, false, false]
    );
    assert_eq!(
        row(Restart::OnFailure),
        [false, true, true, true, true, true, true]
    );
    assert_eq!(
        row(Restart::OnAbnormal),
        [false, false, true, true, true, true, false]
    );
    assert_eq!(
        row(Restart::OnAbort),
        [false, false, true, false, false, true, false]
    );
    assert_eq!(
        row(Restart::OnWatchdog),
        [false, false, false, false, true, false, false]
    );
    assert!(!wanted(Restart::Always, Ended::StartLimitHit));
}

#[test]
fn clean_signals_are_success() {
    let signal = |number, core| {
        Ended::of(Exit::Signal {
            signal: Signal(number),
            core,
        })
    };
    assert_eq!(Ended::of(Exit::Code(0)), Ended::Success);
    assert_eq!(Ended::of(Exit::Code(2)), Ended::ExitCode);
    for clean in [1, 2, 13, 15] {
        assert_eq!(signal(clean, false), Ended::Success, "{clean}");
    }
    assert_eq!(signal(9, false), Ended::Signal);
    assert_eq!(signal(11, true), Ended::CoreDump);
}

#[test]
fn the_backoff_doubles_to_32_times() {
    let mut backoff = Backoff::new(Duration::from_millis(100));
    let delays: [u64; 8] =
        core::array::from_fn(|_| u64::try_from(backoff.next_delay().as_millis()).unwrap());
    assert_eq!(delays, [100, 200, 400, 800, 1600, 3200, 3200, 3200]);
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    let mut zero = Backoff::new(Duration::ZERO);
    assert_eq!(zero.next_delay(), Duration::ZERO);
    for _ in 0..100 {
        let _ = zero.next_delay();
    }
    assert_eq!(zero.steps(), 101, "counting past the cap does not overflow");
}

/// systemd's `ratelimit_below`: the window begins at the first start and
/// holds while no more than `interval` has passed since; the next start after
/// that begins a new one. Not a sliding window: at ten seconds exactly the
/// first start has not aged out, and just after it every start has.
#[test]
fn a_budget_with_a_clock_is_a_fixed_window() {
    let mut budget = Budget::new(5, Duration::from_secs(10));
    let at = |millis: u64| Some(Instant::from_millis(millis));
    for second in 0..5 {
        assert!(budget.start(at(second * 1000)), "start {second}");
    }
    assert!(!budget.start(at(5000)), "a sixth within ten seconds");
    assert!(!budget.start(at(9000)));
    assert!(!budget.start(at(10_000)), "ten seconds is still the window");
    assert!(budget.start(at(10_001)), "a new window begins");
    assert_eq!(budget.used(), 1, "and counts from one");
    for _ in 0..4 {
        assert!(budget.start(at(10_002)));
    }
    assert!(
        !budget.start(at(20_001)),
        "the new window holds for its interval"
    );
    budget.reset();
    assert_eq!(budget.used(), 0);
    assert!(budget.start(at(20_001)), "a reset begins again at once");
}

#[test]
fn a_budget_without_a_clock_is_a_count() {
    let mut budget = Budget::new(8, Duration::MAX);
    for start in 0..8 {
        assert!(budget.start(None), "start {start}");
    }
    assert!(!budget.start(None), "devmgr's eight restarts, counted");
    assert!(
        Budget::new(0, Duration::from_secs(10)).start(None),
        "a burst of 0 is no limit"
    );
    assert!(Budget::new(3, Duration::ZERO).unlimited());
}

#[test]
fn the_policy_decides_now_at_or_never() {
    let mut policy = Policy::new(
        Restart::OnFailure,
        Duration::from_secs(1),
        3,
        Duration::from_secs(60),
    );
    let t = Instant::from_millis(1000);
    assert_eq!(policy.decide(Ended::Success, Some(t)), Decision::Stay);
    assert_eq!(
        policy.decide(Ended::ExitCode, Some(t)),
        Decision::At(t + Duration::from_secs(1))
    );
    assert_eq!(
        policy.decide(Ended::ExitCode, Some(t)),
        Decision::At(t + Duration::from_secs(2))
    );
    assert_eq!(
        policy.decide(Ended::ExitCode, None),
        Decision::Now,
        "no clock, no delay"
    );
    assert_eq!(policy.decide(Ended::ExitCode, Some(t)), Decision::GiveUp);
    policy.reset();
    assert!(policy.start(Some(t)));
    assert_eq!(policy.backoff.steps(), 0);
}

/// devmgr's policy for a driver, as `policy_for` makes it.
fn devmgr_policy() -> Policy {
    Policy::new(Restart::Always, Duration::from_secs(1), 8, Duration::MAX)
}

/// The box's crash of 2026-10-30, a driver that dies at every start: two
/// restarts with growing delays, then a start on a reinitialised device,
/// then quarantine.
#[test]
fn a_driver_that_dies_at_every_start_is_restarted_twice_reinitialised_then_quarantined() {
    let mut policy = devmgr_policy();
    let mut crashes = Escalation::new(Duration::from_secs(10));
    let at = |millis: u64| Instant::from_millis(millis);
    let died = Ended::Signal;
    // The first driver came up at 0 and died 100 ms later.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(100)), Some(at(0))),
        Next::Restart(Some(at(1100)))
    );
    // Restart 1 came up at 1100 and died at once.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(1200)), Some(at(1100))),
        Next::Restart(Some(at(3200)))
    );
    // Restart 2 never came up.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(3300)), None),
        Next::Reinit(Some(at(7300)))
    );
    assert_eq!(crashes.restarts(), 3);
    // The start on the reinitialised device died too.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(7400)), Some(at(7300))),
        Next::Quarantine
    );
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(8000)), None),
        Next::Quarantine,
        "and stays so"
    );
    assert_eq!(policy.budget.used(), 3, "quarantine starts nothing");
}

#[test]
fn a_driver_that_stayed_up_ends_the_streak() {
    let mut policy = devmgr_policy();
    let mut crashes = Escalation::new(Duration::from_secs(10));
    let at = |millis: u64| Instant::from_millis(millis);
    let died = Ended::Signal;
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(100)), Some(at(0))),
        Next::Restart(Some(at(1100)))
    );
    // Up for a millisecond under the window: still a failed start.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(11_099)), Some(at(1100))),
        Next::Restart(Some(at(13_099)))
    );
    // Up for the window exactly: its death is a first one, with the first
    // delay again.
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(23_099)), Some(at(13_099))),
        Next::Restart(Some(at(24_099)))
    );
    assert_eq!(crashes.restarts(), 1);
    // A reinitialised start that stays up ends the streak too.
    let _ = crashes.decide(&mut policy, died, Some(at(24_100)), Some(at(24_099)));
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(26_200)), None),
        Next::Reinit(Some(at(30_200)))
    );
    assert_eq!(
        crashes.decide(&mut policy, died, Some(at(40_200)), Some(at(30_200))),
        Next::Restart(Some(at(41_200)))
    );
}

#[test]
fn a_death_before_its_start_or_at_the_clock_end_saturates() {
    let mut policy = devmgr_policy();
    let mut crashes = Escalation::new(Duration::from_secs(10));
    // A clock that reads earlier than the start: no time up, a failed start.
    let _ = crashes.decide(
        &mut policy,
        Ended::Signal,
        Some(Instant::from_millis(5)),
        Some(Instant::from_millis(9)),
    );
    assert_eq!(crashes.restarts(), 1);
    assert_eq!(
        crashes.decide(
            &mut policy,
            Ended::Signal,
            Some(Instant::from_nanos(u64::MAX)),
            None
        ),
        Next::Restart(Some(Instant::from_nanos(u64::MAX))),
        "a deadline past the clock's end saturates"
    );
}

#[test]
fn the_start_limit_still_bounds_an_escalation() {
    let mut policy = devmgr_policy();
    let mut crashes = Escalation::new(Duration::from_secs(10));
    let mut now = 0;
    for start in 0..8 {
        now += 20_000;
        let next = crashes.decide(
            &mut policy,
            Ended::Signal,
            Some(Instant::from_millis(now)),
            Some(Instant::ZERO),
        );
        assert!(matches!(next, Next::Restart(_)), "start {start}: {next:?}");
    }
    now += 20_000;
    assert_eq!(
        crashes.decide(
            &mut policy,
            Ended::Signal,
            Some(Instant::from_millis(now)),
            Some(Instant::ZERO)
        ),
        Next::GiveUp
    );
}

#[test]
fn without_a_clock_every_death_is_a_failed_start_and_every_start_is_now() {
    let mut policy = devmgr_policy();
    let mut crashes = Escalation::new(Duration::from_secs(10));
    let mut next = || crashes.decide(&mut policy, Ended::Signal, None, None);
    assert_eq!(
        [next(), next(), next(), next()],
        [
            Next::Restart(None),
            Next::Restart(None),
            Next::Reinit(None),
            Next::Quarantine
        ]
    );
    crashes.reset();
    assert_eq!(
        crashes.decide(&mut policy, Ended::Signal, None, None),
        Next::Restart(None),
        "a BIND begins again"
    );
}

#[test]
fn an_end_restart_does_not_restart_stays() {
    let mut policy = Policy::new(Restart::No, Duration::ZERO, 8, Duration::MAX);
    let mut crashes = Escalation::new(Duration::from_secs(10));
    assert_eq!(
        crashes.decide(&mut policy, Ended::Signal, None, None),
        Next::Stay
    );
}
