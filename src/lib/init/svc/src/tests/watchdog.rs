//! The service watchdog (`docs/WATCHDOG-STAGES.md` §5): `WatchdogSec=`
//! armed when a service is up, moved by `WATCHDOG=1`, and at a miss
//! `WatchdogSignal=`, `TimeoutAbortSec=`, `cgroup.kill` and `Restart=`
//! with the result `watchdog`.

use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use super::rig::Rig;
use crate::event::{Action, ActiveState, Event, Pid, Ping, Request};
use crate::restart::Ended;
use crate::value::Signal;

const WATCHED: &str = "[Unit]\nStartLimitBurst=3\nStartLimitIntervalSec=1h\n[Service]\nType=notify\nExecStart=/bin/w\nWatchdogSec=2s\nRestart=on-watchdog\nRestartSec=0\nTimeoutStopSec=5s\n";

fn owned(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (String::from(*a), String::from(*b)))
        .collect()
}

/// Start `name` and make it ready; its main process.
fn up(rig: &mut Rig, name: &str) -> Pid {
    let actions = rig.request(Request::start(name));
    assert_eq!(rig.spawns(&actions), [name]);
    let (pid, _) = rig.spawned(name);
    let unit = rig.id(name);
    let _ = rig.step(Event::Ready { unit, status: None });
    assert_eq!(rig.state(name), ActiveState::Active);
    pid
}

fn ping(rig: &mut Rig, name: &str, ping: Ping) -> Vec<Action> {
    let unit = rig.id(name);
    rig.step(Event::Watchdog { unit, ping })
}

fn missed(actions: &[Action]) -> bool {
    actions
        .iter()
        .any(|action| matches!(action, Action::WatchdogMissed { .. }))
}

#[test]
fn the_main_process_is_told_its_interval() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let actions = rig.request(Request::start("w.service"));
    let spec = actions
        .iter()
        .find_map(|action| match action {
            Action::Spawn { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        spec.environment
            .contains(&(String::from("WATCHDOG_USEC"), String::from("2000000")))
    );
}

#[test]
fn pings_keep_it_up_and_a_miss_aborts_kills_and_restarts() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let pid = up(&mut rig, "w.service");
    for at in [1500, 3000, 4500] {
        rig.now = crate::time::Instant::from_millis(at);
        let _ = ping(&mut rig, "w.service", Ping::Alive);
    }
    assert!(!missed(&rig.at(6000)), "fed 1.5 s before");
    let miss = rig.at(6500);
    assert!(missed(&miss));
    assert!(
        Rig::lines(&miss).contains(&String::from("w.service: watchdog timeout (limit 2000 ms)"))
    );
    assert_eq!(rig.kills(&miss), owned(&[("w.service", "ABRT>group")]));
    assert_eq!(rig.sub("w.service"), "stop-watchdog");
    let _ = rig.killed(pid, Signal::ABRT);
    let restarted = rig.emptied("w.service");
    assert_eq!(rig.spawns(&restarted), ["w.service"]);
    assert_eq!(
        rig.manager.status(rig.id("w.service")).unwrap().result,
        None,
        "a new run"
    );
}

#[test]
fn a_service_that_ignores_the_abort_is_killed_after_the_abort_timeout() {
    let mut rig = Rig::new(&[(
        "w.service",
        "[Service]\nType=notify\nExecStart=/bin/w\nWatchdogSec=1s\nTimeoutAbortSec=3s\nTimeoutStopSec=30s\n",
    )]);
    let pid = up(&mut rig, "w.service");
    let miss = rig.at(1000);
    assert_eq!(rig.kills(&miss), owned(&[("w.service", "ABRT>group")]));
    let early = rig.at(3900);
    assert!(rig.kills(&early).is_empty());
    let kill = rig.at(4000);
    assert_eq!(rig.kills(&kill), owned(&[("w.service", "kill")]));
    let _ = rig.killed(pid, Signal::KILL);
    let _ = rig.emptied("w.service");
    assert_eq!(rig.state("w.service"), ActiveState::Failed);
    assert_eq!(
        rig.manager.status(rig.id("w.service")).unwrap().result,
        Some(Ended::Watchdog)
    );
}

#[test]
fn a_trigger_fires_it_at_once() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let _ = up(&mut rig, "w.service");
    let fired = ping(&mut rig, "w.service", Ping::Trigger);
    assert!(missed(&fired));
    assert!(Rig::lines(&fired).contains(&String::from(
        "w.service: watchdog requested by the service"
    )));
}

#[test]
fn watchdog_usec_changes_the_interval_and_zero_is_refused() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let _ = up(&mut rig, "w.service");
    let refused = ping(&mut rig, "w.service", Ping::Interval(Duration::ZERO));
    assert_eq!(Rig::lines(&refused).len(), 1);
    let _ = ping(
        &mut rig,
        "w.service",
        Ping::Interval(Duration::from_secs(10)),
    );
    assert!(!missed(&rig.at(9000)));
    assert!(missed(&rig.at(10_000)));
}

#[test]
fn the_start_limit_ends_the_restarts() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let mut pid = up(&mut rig, "w.service");
    let mut now = 0;
    for run in 0..3 {
        now += 2000;
        assert!(missed(&rig.at(now)), "run {run}");
        let _ = rig.killed(pid, Signal::ABRT);
        let after = rig.emptied("w.service");
        if run == 2 {
            assert!(rig.spawns(&after).is_empty());
            break;
        }
        assert_eq!(rig.spawns(&after), ["w.service"]);
        pid = rig.spawned("w.service").0;
        let unit = rig.id("w.service");
        let _ = rig.step(Event::Ready { unit, status: None });
    }
    assert_eq!(rig.state("w.service"), ActiveState::Failed);
    assert_eq!(
        rig.manager.status(rig.id("w.service")).unwrap().result,
        Some(Ended::StartLimitHit)
    );
}

#[test]
fn a_ping_before_it_is_up_arms_nothing_and_a_slow_start_is_the_start_timeout() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let _ = rig.request(Request::start("w.service"));
    let _ = rig.spawned("w.service");
    let _ = ping(&mut rig, "w.service", Ping::Alive);
    assert!(!missed(&rig.at(5000)));
    assert_eq!(rig.sub("w.service"), "start");
}

#[test]
fn a_stop_disarms_it() {
    let mut rig = Rig::new(&[("w.service", WATCHED)]);
    let pid = up(&mut rig, "w.service");
    let stop = rig.request(Request::stop("w.service"));
    assert_eq!(rig.kills(&stop), owned(&[("w.service", "TERM>group")]));
    assert!(!missed(&rig.at(3000)), "no watchdog during stop-sigterm");
    let _ = rig.killed(pid, Signal::TERM);
    let _ = rig.emptied("w.service");
    assert_eq!(rig.state("w.service"), ActiveState::Inactive);
}

#[test]
fn a_type_that_cannot_ping_gets_no_watchdog() {
    let mut rig = Rig::new(&[("s.service", "[Service]\nExecStart=/bin/s\nWatchdogSec=1s\n")]);
    let actions = rig.request(Request::start("s.service"));
    let spec = actions
        .iter()
        .find_map(|action| match action {
            Action::Spawn { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        !spec
            .environment
            .iter()
            .any(|(name, _)| name == "WATCHDOG_USEC")
    );
    let _ = rig.spawned("s.service");
    assert!(!missed(&rig.at(10_000)));
    assert_eq!(rig.state("s.service"), ActiveState::Active);
}

#[test]
fn a_native_service_is_watched_too() {
    let mut rig = Rig::new(&[(
        "n.service",
        "[Service]\nType=native\nExecStart=/sbin/n\nWatchdogSec=1s\n",
    )]);
    let _ = up(&mut rig, "n.service");
    assert!(missed(&rig.at(1000)));
}
