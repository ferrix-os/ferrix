//! The manager, as unit sets and scripts of events asserting the actions
//! (§3): boot order and parallelism, cycles, the slice tree, restarts and
//! the start limit, stop escalation, shutdown order, and a service whose
//! main process exits while its cgroup stays populated.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use super::rig::{Paths, Rig};
use crate::Options;
use crate::event::{
    Action, ActiveState, Event, Name, OpResult, Pid, PowerAction, Reply, Request, Token, UnitId,
};
use crate::limits::{Memory, Tasks};
use crate::restart::Ended;
use crate::time::Instant;
use crate::value::Signal;

fn sorted(mut list: Vec<String>) -> Vec<String> {
    list.sort();
    list
}

fn owned(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| (String::from(*a), String::from(*b)))
        .collect()
}

#[test]
fn boot_starts_what_is_unordered_at_once_and_the_rest_in_order() {
    let mut rig = Rig::new(&[
        ("a.service", "[Service]\nExecStart=/bin/a\n"),
        ("b.service", "[Service]\nExecStart=/bin/b\n"),
        (
            "c.service",
            "[Unit]\nAfter=a.service\n[Service]\nExecStart=/bin/c\n",
        ),
        ("multi-user.target.wants/a.service", "->a.service"),
        ("multi-user.target.wants/b.service", "->b.service"),
        ("multi-user.target.wants/c.service", "->c.service"),
    ]);
    let boot = rig.boot();
    assert_eq!(
        Rig::made(&boot),
        [
            "system.slice",
            "system.slice/a.service",
            "system.slice/b.service"
        ],
        "the slice first, then both services' cgroups"
    );
    assert_eq!(
        sorted(rig.spawns(&boot)),
        ["a.service", "b.service"],
        "a and b at once"
    );
    assert_eq!(rig.state("sysinit.target"), ActiveState::Active);
    assert_eq!(rig.state("multi-user.target"), ActiveState::Inactive);

    let (_, after_a) = rig.spawned("a.service");
    assert_eq!(rig.spawns(&after_a), ["c.service"], "c is after a");
    assert_eq!(rig.state("a.service"), ActiveState::Active);
    let _ = rig.spawned("b.service");
    assert_eq!(
        rig.state("multi-user.target"),
        ActiveState::Inactive,
        "c is not up yet"
    );
    let (_, last) = rig.spawned("c.service");
    assert_eq!(rig.state("multi-user.target"), ActiveState::Active);
    assert!(Rig::lines(&last).contains(&String::from("multi-user.target: active")));
    assert_eq!(
        rig.manager.deadline(),
        None,
        "nothing is waiting on a clock"
    );
}

#[test]
fn an_ordering_cycle_of_wants_is_broken_with_a_warning() {
    let mut rig = Rig::new(&[
        (
            "x.service",
            "[Unit]\nAfter=y.service\n[Service]\nExecStart=/bin/x\n",
        ),
        (
            "y.service",
            "[Unit]\nAfter=x.service\n[Service]\nExecStart=/bin/y\n",
        ),
        ("multi-user.target.wants/x.service", "->x.service"),
        ("multi-user.target.wants/y.service", "->y.service"),
    ]);
    let boot = rig.boot();
    let lines = Rig::lines(&boot);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("found ordering cycle")),
        "{lines:?}"
    );
    assert_eq!(rig.spawns(&boot).len(), 1, "one of the two was dropped");
}

#[test]
fn an_ordering_cycle_of_requires_refuses_the_transaction() {
    let mut rig = Rig::new(&[
        (
            "p.service",
            "[Unit]\nRequires=q.service\nAfter=q.service\n[Service]\nExecStart=/bin/p\n",
        ),
        (
            "q.service",
            "[Unit]\nRequires=p.service\nAfter=p.service\n[Service]\nExecStart=/bin/q\n",
        ),
    ]);
    let actions = rig.request(Request::start("p.service"));
    let replies = Rig::replies(&actions);
    assert!(
        matches!(replies.as_slice(), [Reply::Refused(why)] if why.starts_with("ordering cycle")),
        "{replies:?}"
    );
    assert!(rig.spawns(&actions).is_empty());
}

#[test]
fn a_missing_requirement_refuses_and_a_missing_want_does_not() {
    let mut rig = Rig::new(&[
        (
            "r.service",
            "[Unit]\nRequires=gone.service\n[Service]\nExecStart=/bin/r\n",
        ),
        (
            "w.service",
            "[Unit]\nWants=gone.service\n[Service]\nExecStart=/bin/w\n",
        ),
    ]);
    let refused = rig.request(Request::start("r.service"));
    assert!(
        matches!(Rig::replies(&refused).as_slice(), [Reply::Refused(why)] if why.contains("gone.service"))
    );
    let started = rig.request(Request::start("w.service"));
    assert_eq!(rig.spawns(&started), ["w.service"]);
}

#[test]
fn the_slice_tree() {
    let mut rig = Rig::new(&[
        (
            "s.service",
            "[Service]\nExecStart=/bin/s\nSlice=user-1000.slice\nMemoryMax=64M\n",
        ),
        ("user-1000.slice", "[Slice]\nTasksMax=100\n"),
    ]);
    let boot = rig.boot();
    assert!(
        Rig::made(&boot).is_empty(),
        "nothing wants s, and the root is never made"
    );
    for perpetual in ["-.slice", "init.scope", "drivers.slice"] {
        assert_eq!(rig.state(perpetual), ActiveState::Active, "{perpetual}");
    }
    let actions = rig.request(Request::start("s.service"));
    assert_eq!(
        Rig::made(&actions),
        [
            "user.slice",
            "user.slice/user-1000.slice",
            "user.slice/user-1000.slice/s.service"
        ]
    );
    let limits: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::MakeGroup { limits, .. } => Some(*limits),
            _ => None,
        })
        .collect();
    assert!(limits[0].is_empty());
    assert_eq!(
        limits[1].tasks_max,
        Some(Tasks::Count(100)),
        "a slice's limits bound its branch"
    );
    assert_eq!(limits[2].memory_max, Some(Memory::Bytes(64 << 20)));
    assert_eq!(
        rig.manager
            .group(rig.id("init.scope"))
            .map(|p| String::from(p.as_str())),
        Some(String::from("init.scope"))
    );
    let stop = rig.request(Request::stop("drivers.slice"));
    assert!(
        matches!(Rig::replies(&stop).as_slice(), [Reply::Refused(_)]),
        "adopted, never stopped"
    );
}

/// Start `name`, answering its spawn; the pid.
fn start(rig: &mut Rig, name: &str) -> Pid {
    let actions = rig.request(Request::start(name));
    assert_eq!(rig.spawns(&actions), [name]);
    rig.spawned(name).0
}

#[test]
fn restarts_back_off_doubling_up_to_32_times() {
    let mut rig = Rig::new(&[(
        "r.service",
        "[Unit]\nStartLimitBurst=100\nStartLimitIntervalSec=1h\n[Service]\nExecStart=/bin/r\nRestart=always\nRestartSec=1s\n",
    )]);
    let mut pid = start(&mut rig, "r.service");
    let mut now = 0;
    for expected in [1, 2, 4, 8, 16, 32, 32, 32] {
        let exit = rig.exit(pid, 1);
        assert_eq!(rig.kills(&exit), owned(&[("r.service", "TERM>group")]));
        let _ = rig.emptied("r.service");
        assert_eq!(rig.sub("r.service"), "auto-restart");
        assert_eq!(rig.state("r.service"), ActiveState::Activating);
        let deadline = rig.manager.deadline().unwrap();
        assert_eq!(
            deadline,
            Instant::from_millis(now) + Duration::from_secs(expected)
        );
        now += expected * 1000;
        let restart = rig.at(now);
        assert_eq!(
            rig.spawns(&restart),
            ["r.service"],
            "restarted after {expected}s"
        );
        pid = rig.spawned("r.service").0;
    }
    assert_eq!(rig.state("r.service"), ActiveState::Active);
}

#[test]
fn the_start_limit_fails_a_unit_until_reset() {
    let mut rig = Rig::new(&[(
        "b.service",
        "[Unit]\nStartLimitBurst=3\nStartLimitIntervalSec=10s\n[Service]\nExecStart=/bin/b\nRestart=on-failure\nRestartSec=0\n",
    )]);
    let mut pid = start(&mut rig, "b.service");
    for _ in 0..2 {
        let _ = rig.exit(pid, 1);
        let restarted = rig.emptied("b.service");
        assert_eq!(
            rig.spawns(&restarted),
            ["b.service"],
            "no delay, and budget left"
        );
        pid = rig.spawned("b.service").0;
    }
    let _ = rig.exit(pid, 1);
    let given_up = rig.emptied("b.service");
    assert!(
        rig.spawns(&given_up).is_empty(),
        "three starts in ten seconds are the budget"
    );
    assert_eq!(rig.state("b.service"), ActiveState::Failed);
    let status = rig.manager.status(rig.id("b.service")).unwrap();
    assert_eq!(status.result, Some(Ended::StartLimitHit));

    let refused = rig.request(Request::start("b.service"));
    assert!(rig.spawns(&refused).is_empty(), "a start is counted too");
    assert_eq!(Rig::replies(&refused), [Reply::Done(OpResult::Failed)]);

    let _ = rig.request(Request::ResetFailed(Some(String::from("b.service"))));
    assert_eq!(rig.state("b.service"), ActiveState::Inactive);
    let _ = start(&mut rig, "b.service");
    assert_eq!(rig.state("b.service"), ActiveState::Active);
}

#[test]
fn a_clean_exit_does_not_restart_on_failure() {
    let mut rig = Rig::new(&[(
        "c.service",
        "[Service]\nExecStart=/bin/c\nRestart=on-failure\n",
    )]);
    let pid = start(&mut rig, "c.service");
    let _ = rig.exit(pid, 0);
    let _ = rig.emptied("c.service");
    assert_eq!(rig.state("c.service"), ActiveState::Inactive);
    assert_eq!(rig.manager.deadline(), None);
}

#[test]
fn stop_escalates_from_the_signal_to_cgroup_kill() {
    let mut rig = Rig::new(&[(
        "e.service",
        "[Service]\nExecStart=/bin/e\nTimeoutStopSec=5s\n",
    )]);
    let pid = start(&mut rig, "e.service");
    rig.now = Instant::from_millis(1000);
    let stop = rig.request(Request::stop("e.service"));
    assert_eq!(rig.kills(&stop), owned(&[("e.service", "TERM>group")]));
    assert_eq!(rig.state("e.service"), ActiveState::Deactivating);
    assert_eq!(rig.at(5999), Vec::new(), "not yet");
    let escalate = rig.at(6000);
    assert_eq!(rig.kills(&escalate), owned(&[("e.service", "kill")]));
    let _ = rig.killed(pid, Signal::KILL);
    assert_eq!(
        rig.state("e.service"),
        ActiveState::Deactivating,
        "stopped means empty"
    );
    let done = rig.emptied("e.service");
    assert_eq!(rig.kills(&done), owned(&[("e.service", "remove")]));
    assert_eq!(
        Rig::replies(&done),
        [Reply::Done(OpResult::Done)],
        "the stop did stop it"
    );
    assert_eq!(
        rig.state("e.service"),
        ActiveState::Failed,
        "but by force, as systemd says"
    );
    assert_eq!(
        rig.manager.status(rig.id("e.service")).unwrap().result,
        Some(Ended::Timeout)
    );
}

#[test]
fn a_stop_that_is_obeyed_is_no_failure() {
    let mut rig = Rig::new(&[("e.service", "[Service]\nExecStart=/bin/e\n")]);
    let pid = start(&mut rig, "e.service");
    let _ = rig.request(Request::stop("e.service"));
    let _ = rig.killed(pid, Signal::TERM);
    let done = rig.emptied("e.service");
    assert_eq!(rig.state("e.service"), ActiveState::Inactive);
    assert_eq!(Rig::replies(&done), [Reply::Done(OpResult::Done)]);
}

#[test]
fn mixed_signals_the_main_process_then_kills_the_group() {
    let mut rig = Rig::new(&[(
        "m.service",
        "[Service]\nExecStart=/bin/m\nKillMode=mixed\nKillSignal=SIGINT\nTimeoutStopSec=2s\n",
    )]);
    let pid = start(&mut rig, "m.service");
    let stop = rig.request(Request::stop("m.service"));
    let signalled = alloc::format!("INT>{}", pid.0);
    assert_eq!(
        rig.kills(&stop),
        owned(&[("m.service", signalled.as_str())])
    );
    let _ = rig.exit(pid, 0);
    assert_eq!(
        rig.state("m.service"),
        ActiveState::Deactivating,
        "the rest is still there"
    );
    let escalate = rig.at(2000);
    assert_eq!(rig.kills(&escalate), owned(&[("m.service", "kill")]));
    let _ = rig.emptied("m.service");
    assert_eq!(
        rig.state("m.service"),
        ActiveState::Failed,
        "a stop that timed out"
    );
}

#[test]
fn kill_mode_process_leaves_the_rest_until_the_next_start() {
    let mut rig = Rig::new(&[(
        "p.service",
        "[Service]\nExecStart=/bin/p\nKillMode=process\nTimeoutStopSec=1s\n",
    )]);
    let pid = start(&mut rig, "p.service");
    let _ = rig.request(Request::stop("p.service"));
    let escalate = rig.at(1000);
    let killed = alloc::format!("KILL>{}", pid.0);
    assert_eq!(
        rig.kills(&escalate),
        owned(&[("p.service", killed.as_str())])
    );
    let stopped = rig.killed(pid, Signal::KILL);
    assert_eq!(
        rig.state("p.service"),
        ActiveState::Failed,
        "only the main process is waited for"
    );
    assert!(rig.kills(&stopped).is_empty(), "the populated cgroup stays");

    let again = rig.request(Request::start("p.service"));
    assert_eq!(
        rig.kills(&again),
        owned(&[("p.service", "kill")]),
        "leftovers go first"
    );
    assert!(rig.spawns(&again).is_empty());
    let clean = rig.emptied("p.service");
    assert_eq!(rig.spawns(&clean), ["p.service"]);
}

#[test]
fn a_main_process_that_exits_leaves_the_service_deactivating_until_its_cgroup_empties() {
    let mut rig = Rig::new(&[("d.service", "[Service]\nExecStart=/bin/d\n")]);
    let pid = start(&mut rig, "d.service");
    let exit = rig.exit(pid, 0);
    assert_eq!(rig.kills(&exit), owned(&[("d.service", "TERM>group")]));
    assert_eq!(rig.state("d.service"), ActiveState::Deactivating);
    assert_eq!(rig.sub("d.service"), "stop-sigterm");
    let _ = rig.emptied("d.service");
    assert_eq!(rig.state("d.service"), ActiveState::Inactive);
}

#[test]
fn a_forking_service_is_up_when_its_parent_exits_and_down_when_its_cgroup_empties() {
    let mut rig = Rig::new(&[("f.service", "[Service]\nType=forking\nExecStart=/bin/f\n")]);
    let pid = start(&mut rig, "f.service");
    assert_eq!(rig.state("f.service"), ActiveState::Activating);
    let _ = rig.exit(pid, 0);
    assert_eq!(
        rig.state("f.service"),
        ActiveState::Active,
        "the daemon it left is the service"
    );
    assert_eq!(rig.sub("f.service"), "running");

    let stop = rig.request(Request::stop("f.service"));
    assert_eq!(rig.kills(&stop), owned(&[("f.service", "TERM>group")]));
    assert_eq!(
        rig.state("f.service"),
        ActiveState::Deactivating,
        "the grandchild is still there"
    );
    let _ = rig.emptied("f.service");
    assert_eq!(rig.state("f.service"), ActiveState::Inactive);

    let pid = start(&mut rig, "f.service");
    let _ = rig.exit(pid, 0);
    let gone = rig.emptied("f.service");
    assert!(Rig::lines(&gone).contains(&String::from("f.service: stopped")));
    assert_eq!(
        rig.state("f.service"),
        ActiveState::Inactive,
        "its daemon ended on its own"
    );
}

#[test]
fn a_forking_parent_that_fails_fails_the_start() {
    let mut rig = Rig::new(&[("f.service", "[Service]\nType=forking\nExecStart=/bin/f\n")]);
    let pid = start(&mut rig, "f.service");
    let _ = rig.exit(pid, 3);
    let failed = rig.emptied("f.service");
    assert_eq!(rig.state("f.service"), ActiveState::Failed);
    assert_eq!(Rig::replies(&failed), [Reply::Done(OpResult::Failed)]);
}

#[test]
fn shutdown_stops_in_reverse_order_then_kills_what_is_left() {
    let mut rig = Rig::new(&[
        ("a.service", "[Service]\nExecStart=/bin/a\n"),
        (
            "b.service",
            "[Unit]\nAfter=a.service\n[Service]\nExecStart=/bin/b\n",
        ),
        ("keep.slice", "[Unit]\nDefaultDependencies=no\n"),
        (
            "k.service",
            "[Unit]\nDefaultDependencies=no\n[Service]\nExecStart=/bin/k\nSlice=keep.slice\n",
        ),
        ("multi-user.target.wants/a.service", "->a.service"),
        ("multi-user.target.wants/b.service", "->b.service"),
        ("multi-user.target.wants/k.service", "->k.service"),
    ]);
    let _ = rig.boot();
    let (a, _) = rig.spawned("a.service");
    let (k, _) = rig.spawned("k.service");
    let (b, _) = rig.spawned("b.service");
    assert_eq!(rig.state("multi-user.target"), ActiveState::Active);

    let down = rig.request(Request::Poweroff);
    assert_eq!(
        rig.kills(&down),
        owned(&[("b.service", "TERM>group")]),
        "b is after a, so it stops first"
    );
    let _ = rig.exit(b, 0);
    let next = rig.emptied("b.service");
    assert_eq!(
        rig.kills(&next),
        owned(&[("b.service", "remove"), ("a.service", "TERM>group")])
    );
    let _ = rig.exit(a, 0);
    let last = rig.emptied("a.service");
    assert_eq!(
        rig.kills(&last),
        owned(&[
            ("a.service", "remove"),
            ("system.slice", "remove"),
            ("k.service", "kill"),
            ("keep.slice", "kill"),
        ]),
        "the slice after its services; then every cgroup left, deepest first"
    );
    assert!(
        !last.iter().any(|action| matches!(action, Action::Power(_))),
        "k is still there"
    );
    let _ = rig.killed(k, Signal::KILL);
    let power = rig.emptied("k.service");
    assert!(power.contains(&Action::Power(PowerAction::Poweroff)));
    assert_eq!(rig.state("poweroff.target"), ActiveState::Active);

    let refused = rig.request(Request::start("a.service"));
    assert!(matches!(
        Rig::replies(&refused).as_slice(),
        [Reply::Refused(_)]
    ));
}

#[test]
fn shutdown_does_not_restart_what_it_stops() {
    let mut rig = Rig::new(&[
        (
            "r.service",
            "[Service]\nExecStart=/bin/r\nRestart=always\nRestartSec=0\n",
        ),
        ("multi-user.target.wants/r.service", "->r.service"),
    ]);
    let _ = rig.boot();
    let (pid, _) = rig.spawned("r.service");
    let _ = rig.request(Request::Reboot);
    let _ = rig.killed(pid, Signal::TERM);
    let last = rig.emptied("r.service");
    assert!(rig.spawns(&last).is_empty());
    assert_eq!(last.last(), Some(&Action::Power(PowerAction::Reboot)));
}

#[test]
fn a_boot_that_cannot_start_falls_back_to_rescue() {
    let mut rig = Rig::new(&[
        ("multi-user.target", "[Unit]\nRequires=missing.service\n"),
        ("sh.service", "[Service]\nExecStart=/bin/sh\n"),
        ("rescue.target.wants/sh.service", "->sh.service"),
    ]);
    let boot = rig.boot();
    let lines = Rig::lines(&boot);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("starting rescue.target")),
        "{lines:?}"
    );
    assert_eq!(rig.spawns(&boot), ["sh.service"]);
    let _ = rig.spawned("sh.service");
    assert_eq!(rig.state("rescue.target"), ActiveState::Active);
}

#[test]
fn a_boot_whose_requirement_fails_isolates_rescue() {
    let mut rig = Rig::new(&[
        (
            "multi-user.target",
            "[Unit]\nRequires=bad.service other.service\nAfter=bad.service\n",
        ),
        ("bad.service", "[Service]\nType=exec\nExecStart=/bin/bad\n"),
        ("other.service", "[Service]\nExecStart=/bin/other\n"),
    ]);
    let _ = rig.boot();
    let (bad, _) = rig.spawned("bad.service");
    let (other, _) = rig.spawned("other.service");
    let _ = rig.exit(bad, 127);
    let fallback = rig.emptied("bad.service");
    assert!(
        Rig::lines(&fallback)
            .iter()
            .any(|l| l.contains("starting rescue.target"))
    );
    assert_eq!(rig.state("bad.service"), ActiveState::Failed);
    assert_eq!(
        rig.kills(&fallback),
        owned(&[("bad.service", "remove"), ("other.service", "TERM>group")]),
        "isolate stops the rest"
    );
    let _ = rig.exit(other, 0);
    let _ = rig.emptied("other.service");
    assert_eq!(rig.state("rescue.target"), ActiveState::Active);
}

#[test]
fn exec_is_up_when_execve_succeeded_and_notify_when_it_says_so() {
    let mut rig = Rig::new(&[
        ("e.service", "[Service]\nType=exec\nExecStart=/bin/e\n"),
        ("n.service", "[Service]\nType=notify\nExecStart=/bin/n\n"),
    ]);
    let e = start(&mut rig, "e.service");
    assert_eq!(rig.state("e.service"), ActiveState::Activating);
    let unit = rig.id("e.service");
    let _ = rig.step(Event::Execed { unit, pid: e });
    assert_eq!(rig.state("e.service"), ActiveState::Active);

    let _ = start(&mut rig, "n.service");
    assert_eq!(rig.state("n.service"), ActiveState::Activating);
    let unit = rig.id("n.service");
    let _ = rig.step(Event::Ready {
        unit,
        status: Some(String::from("serving")),
    });
    assert_eq!(rig.state("n.service"), ActiveState::Active);
    let status = rig.manager.status(unit).unwrap();
    assert_eq!(status.status.as_deref(), Some("serving"));
}

#[test]
fn a_oneshot_runs_its_commands_in_turn_and_remains() {
    let mut rig = Rig::new(&[(
        "o.service",
        "[Service]\nType=oneshot\nExecStart=/bin/one\nExecStart=-/bin/two\nExecStart=/bin/three\nRemainAfterExit=yes\n",
    )]);
    let first = rig.request(Request::start("o.service"));
    assert_eq!(Rig::argvs(&first), [[String::from("/bin/one")]]);
    let (one, _) = rig.spawned("o.service");
    let second = rig.exit(one, 0);
    assert_eq!(Rig::argvs(&second), [[String::from("/bin/two")]]);
    let (two, _) = rig.spawned("o.service");
    let third = rig.exit(two, 1);
    assert_eq!(
        Rig::argvs(&third),
        [[String::from("/bin/three")]],
        "'-' ignores the failure"
    );
    let (three, _) = rig.spawned("o.service");
    let done = rig.exit(three, 0);
    assert_eq!(rig.state("o.service"), ActiveState::Active);
    assert_eq!(rig.sub("o.service"), "exited");
    assert_eq!(Rig::replies(&done), [Reply::Done(OpResult::Done)]);
}

#[test]
fn a_start_timeout_fails_the_service() {
    let mut rig = Rig::new(&[(
        "slow.service",
        "[Service]\nType=notify\nExecStart=/bin/slow\nTimeoutStartSec=3s\nTimeoutStopSec=1s\n",
    )]);
    let pid = start(&mut rig, "slow.service");
    let timeout = rig.at(3000);
    assert_eq!(
        rig.kills(&timeout),
        owned(&[("slow.service", "TERM>group")])
    );
    let _ = rig.killed(pid, Signal::TERM);
    let failed = rig.emptied("slow.service");
    assert_eq!(rig.state("slow.service"), ActiveState::Failed);
    assert_eq!(
        rig.manager.status(rig.id("slow.service")).unwrap().result,
        Some(Ended::Timeout)
    );
    assert_eq!(Rig::replies(&failed), [Reply::Done(OpResult::Failed)]);
}

#[test]
fn start_pre_and_stop_commands_run_as_control_processes() {
    let mut rig = Rig::new(&[(
        "c.service",
        "[Service]\nExecStartPre=/bin/pre\nExecStart=/bin/main\nExecStop=/bin/stop $MAINPID\nExecStopPost=/bin/post\n",
    )]);
    let pre = rig.request(Request::start("c.service"));
    assert_eq!(Rig::argvs(&pre), [[String::from("/bin/pre")]]);
    let (pre, _) = rig.spawned("c.service");
    let main = rig.exit(pre, 0);
    assert_eq!(Rig::argvs(&main), [[String::from("/bin/main")]]);
    let (main_pid, _) = rig.spawned("c.service");
    let stop = rig.request(Request::stop("c.service"));
    let spec = stop.iter().find_map(|action| match action {
        Action::Spawn { spec, .. } => Some(spec.clone()),
        _ => None,
    });
    let spec = spec.unwrap();
    assert!(
        spec.environment
            .contains(&(String::from("MAINPID"), alloc::format!("{}", main_pid.0)))
    );
    let (stopper, _) = rig.spawned("c.service");
    let signal = rig.exit(stopper, 0);
    assert_eq!(rig.kills(&signal), owned(&[("c.service", "TERM>group")]));
    let _ = rig.exit(main_pid, 0);
    let post = rig.emptied("c.service");
    assert_eq!(Rig::argvs(&post), [[String::from("/bin/post")]]);
    let (post, _) = rig.spawned("c.service");
    let _ = rig.exit(post, 0);
    assert_eq!(rig.state("c.service"), ActiveState::Inactive);
}

#[test]
fn a_failed_condition_skips_the_unit_and_what_follows_it_starts() {
    let mut rig = Rig::with(
        &[
            (
                "gated.service",
                "[Unit]\nConditionPathExists=/etc/wanted\n[Service]\nExecStart=/bin/g\n",
            ),
            (
                "open.service",
                "[Unit]\nConditionPathExists=/etc/present\n[Service]\nExecStart=/bin/o\n",
            ),
            (
                "after.service",
                "[Unit]\nAfter=gated.service\n[Service]\nExecStart=/bin/a\n",
            ),
            ("multi-user.target.wants/gated.service", "->gated.service"),
            ("multi-user.target.wants/open.service", "->open.service"),
            ("multi-user.target.wants/after.service", "->after.service"),
        ],
        Box::new(Paths(alloc::vec!["/etc/present"])),
        Options::default(),
    );
    let boot = rig.boot();
    assert_eq!(sorted(rig.spawns(&boot)), ["after.service", "open.service"]);
    assert!(
        Rig::lines(&boot).contains(&String::from("gated.service: skipped, a condition failed"))
    );
    assert_eq!(rig.state("gated.service"), ActiveState::Inactive);
}

#[test]
fn a_target_given_on_the_command_line_boots_instead() {
    let mut rig = Rig::with(
        &[
            ("sh.service", "[Service]\nExecStart=/bin/sh\n"),
            ("rescue.target.wants/sh.service", "->sh.service"),
        ],
        Box::new(crate::NoProbe),
        Options {
            target: Some(String::from("rescue.target")),
        },
    );
    let boot = rig.boot();
    assert_eq!(rig.spawns(&boot), ["sh.service"]);
}

#[test]
fn conflicts_stop_the_other_unit_and_a_new_request_replaces_a_queued_one() {
    let mut rig = Rig::new(&[
        ("x.service", "[Service]\nExecStart=/bin/x\n"),
        (
            "y.service",
            "[Unit]\nConflicts=x.service\n[Service]\nExecStart=/bin/y\n",
        ),
    ]);
    let x = start(&mut rig, "x.service");
    let y = rig.request(Request::start("y.service"));
    assert_eq!(rig.kills(&y), owned(&[("x.service", "TERM>group")]));
    assert!(rig.spawns(&y).is_empty(), "y waits for x to stop");
    let _ = rig.exit(x, 0);
    let now_y = rig.emptied("x.service");
    assert_eq!(rig.spawns(&now_y), ["y.service"]);
}

#[test]
fn binds_to_stops_a_unit_whose_binding_goes_away() {
    let mut rig = Rig::new(&[
        ("dev.service", "[Service]\nExecStart=/bin/dev\n"),
        (
            "user.service",
            "[Unit]\nBindsTo=dev.service\nAfter=dev.service\n[Service]\nExecStart=/bin/user\n",
        ),
    ]);
    let started = rig.request(Request::start("user.service"));
    assert_eq!(rig.spawns(&started), ["dev.service"]);
    let (dev, after) = rig.spawned("dev.service");
    assert_eq!(rig.spawns(&after), ["user.service"]);
    let _ = rig.spawned("user.service");
    let crash = rig.killed(dev, Signal(11));
    assert_eq!(
        rig.kills(&crash),
        owned(&[
            ("dev.service", "TERM>group"),
            ("user.service", "TERM>group")
        ])
    );
}

#[test]
fn binds_to_stops_a_remaining_oneshot_whatever_its_kill_mode() {
    for mode in ["control-group", "process"] {
        let forker = alloc::format!(
            "[Unit]\nBindsTo=anchor.service\nAfter=anchor.service\n\
             [Service]\nType=oneshot\nRemainAfterExit=yes\nKillMode={mode}\n\
             ExecStart=/bin/forker\n"
        );
        let mut rig = Rig::new(&[
            ("anchor.service", "[Service]\nExecStart=/bin/anchor\n"),
            ("forker.service", forker.as_str()),
        ]);
        let _ = rig.request(Request::start("forker.service"));
        let (anchor, _) = rig.spawned("anchor.service");
        let (forker, _) = rig.spawned("forker.service");
        // The main process exits, leaving a grandchild in the cgroup.
        let _ = rig.exit(forker, 0);
        assert_eq!(rig.state("forker.service"), ActiveState::Active, "{mode}");
        let _ = rig.killed(anchor, Signal::TERM);
        let _ = rig.emptied("anchor.service");
        let stopping = rig.state("forker.service");
        if mode == "control-group" {
            assert_eq!(stopping, ActiveState::Deactivating, "{mode}");
            let _ = rig.emptied("forker.service");
        }
        assert_eq!(rig.state("forker.service"), ActiveState::Inactive, "{mode}");
    }
}

#[test]
fn the_directory_routes_an_open_and_starts_its_provider() {
    let mut rig = Rig::new(&[
        (
            "clip.service",
            "[Service]\nType=native\nExecStart=/sbin/vdagent\nOffers=ferrix.clipboard\n",
        ),
        (
            "hyprix.service",
            "[Service]\nExecStart=/bin/hyprix\nUses=ferrix.clipboard\n",
        ),
        ("rogue.service", "[Service]\nExecStart=/bin/rogue\n"),
    ]);
    let _ = rig.boot();
    let _ = start(&mut rig, "hyprix.service");
    let hyprix = rig.id("hyprix.service");
    let open = rig.step(Event::Open {
        from: hyprix,
        name: Name(String::from("ferrix.clipboard")),
        end: Token(7),
    });
    assert_eq!(
        rig.spawns(&open),
        ["clip.service"],
        "an OPEN is an activation"
    );
    let spec = open.iter().find_map(|a| match a {
        Action::Spawn { spec, .. } => Some(spec.bootstrap),
        _ => None,
    });
    assert_eq!(spec, Some(true));
    let _ = rig.spawned("clip.service");
    let clip = rig.id("clip.service");
    let ready = rig.step(Event::Ready {
        unit: clip,
        status: None,
    });
    assert!(ready.contains(&Action::Route {
        to: clip,
        name: Name(String::from("ferrix.clipboard")),
        end: Token(7),
    }));
    let _ = start(&mut rig, "rogue.service");
    let rogue = rig.id("rogue.service");
    let refused = rig.step(Event::Open {
        from: rogue,
        name: Name(String::from("ferrix.clipboard")),
        end: Token(8),
    });
    assert!(refused.contains(&Action::Refuse {
        to: rogue,
        name: Name(String::from("ferrix.clipboard")),
        end: Token(8),
    }));
}

#[test]
fn a_scope_groups_processes_init_did_not_start() {
    let mut rig = Rig::new(&[]);
    let _ = rig.boot();
    let made = rig.request(Request::Scope {
        unit: String::from("session-1.scope"),
        slice: Some(String::from("user-1000.slice")),
        pids: alloc::vec![Pid(4242)],
    });
    assert_eq!(
        Rig::made(&made),
        [
            "user.slice",
            "user.slice/user-1000.slice",
            "user.slice/user-1000.slice/session-1.scope"
        ]
    );
    let scope = rig.id("session-1.scope");
    assert!(made.contains(&Action::Move {
        unit: scope,
        pids: alloc::vec![Pid(4242)],
    }));
    assert_eq!(rig.state("session-1.scope"), ActiveState::Active);
    let gone = rig.emptied("session-1.scope");
    assert_eq!(rig.kills(&gone), owned(&[("session-1.scope", "remove")]));
    assert_eq!(rig.state("session-1.scope"), ActiveState::Inactive);
}

#[test]
fn oom_policy_stop_takes_the_whole_service_down() {
    let mut rig = Rig::new(&[(
        "big.service",
        "[Service]\nExecStart=/bin/big\nRestart=on-failure\nRestartSec=5s\n",
    )]);
    let pid = start(&mut rig, "big.service");
    let unit = rig.id("big.service");
    let oom = rig.step(Event::OomKilled { unit });
    assert_eq!(rig.kills(&oom), owned(&[("big.service", "TERM>group")]));
    let _ = rig.killed(pid, Signal::TERM);
    let _ = rig.emptied("big.service");
    assert_eq!(
        rig.manager.status(unit).unwrap().result,
        Some(Ended::OomKill)
    );
    assert_eq!(
        rig.sub("big.service"),
        "auto-restart",
        "and Restart= applies as to any failure"
    );
}

#[test]
fn status_reports_every_unit() {
    let mut rig = Rig::new(&[(
        "a.service",
        "[Unit]\nDescription=The A\n[Service]\nExecStart=/bin/a\n",
    )]);
    let _ = rig.boot();
    let actions = rig.request(Request::Status(Some(String::from("a.service"))));
    let replies = Rig::replies(&actions);
    let [Reply::Status(list)] = replies.as_slice() else {
        panic!("{replies:?}");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].description.as_deref(), Some("The A"));
    assert_eq!(
        (list[0].load, list[0].active, list[0].sub),
        ("loaded", ActiveState::Inactive, "dead")
    );
    let missing = rig.request(Request::Status(Some(String::from("nope.service"))));
    let replies = Rig::replies(&missing);
    assert!(matches!(replies.as_slice(), [Reply::Status(list)] if list[0].load == "not-found"));
}

#[test]
fn a_units_warnings_are_logged_as_it_loads() {
    let mut rig = Rig::new(&[
        (
            "a.service",
            "[Service]\nExecStart=/bin/a\nNoSuchKey=1\nEnvironment=\"PS1=%# \"\n",
        ),
        ("multi-user.target.wants/a.service", "->a.service"),
    ]);
    let lines = Rig::lines(&rig.boot());
    let warned: Vec<&String> = lines
        .iter()
        .filter(|line| line.starts_with("/lib/ferrix/units/a.service:"))
        .collect();
    assert_eq!(warned.len(), 2, "{lines:?}");
    assert!(
        warned
            .iter()
            .any(|line| line.contains(":3: Unknown key name 'NoSuchKey'")),
        "{lines:?}"
    );
    assert!(
        warned
            .iter()
            .any(|line| line.contains(":4: ") && line.contains("'%#'")),
        "{lines:?}"
    );
    let again = Rig::lines(&rig.request(Request::start("a.service")));
    assert!(
        !again.iter().any(|line| line.contains("NoSuchKey")),
        "a unit's warnings are said once, when it loads: {again:?}"
    );
}

#[test]
fn a_refused_unit_says_why_once() {
    let mut rig = Rig::new(&[(
        "n.service",
        "[Service]\nType=simple\nExecStart=/a\nExecStart=/b\n",
    )]);
    let mut lines = Rig::lines(&rig.boot());
    lines.extend(Rig::lines(
        &rig.request(Request::Status(Some(String::from("n.service")))),
    ));
    let said: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("n.service: ") && line.contains("more than one ExecStart="))
        .collect();
    assert_eq!(said.len(), 1, "{lines:?}");
    let again = Rig::lines(&rig.request(Request::start("n.service")));
    assert!(
        !again.iter().any(|line| line.contains("ExecStart=")),
        "said once, when it loads: {again:?}"
    );
}

#[test]
fn reload_runs_exec_reload_while_the_service_stays_up() {
    let mut rig = Rig::new(&[
        (
            "d.service",
            "[Service]\nExecStart=/bin/d\nExecReload=/bin/kick one\nExecReload=/bin/kick two\n",
        ),
        ("plain.service", "[Service]\nExecStart=/bin/plain\n"),
    ]);
    let _ = rig.request(Request::start("d.service"));
    let (main, _) = rig.spawned("d.service");
    let _ = rig.request(Request::start("plain.service"));
    let _ = rig.spawned("plain.service");

    let asked = rig.request(Request::Reload(String::from("d.service")));
    assert_eq!(Rig::argvs(&asked), [["/bin/kick", "one"]]);
    assert_eq!(
        (rig.state("d.service"), rig.sub("d.service")),
        (ActiveState::Active, "reload")
    );
    let (first, next) = rig.spawned("d.service");
    assert!(Rig::argvs(&next).is_empty());
    let second = rig.exit(first, 0);
    assert_eq!(Rig::argvs(&second), [["/bin/kick", "two"]]);
    let (last, _) = rig.spawned("d.service");
    let done = rig.exit(last, 0);
    assert_eq!(Rig::replies(&done), [Reply::Done(OpResult::Done)]);
    assert_eq!(rig.sub("d.service"), "running");

    let failing = rig.request(Request::Reload(String::from("d.service")));
    assert_eq!(Rig::argvs(&failing).len(), 1);
    let (kick, _) = rig.spawned("d.service");
    let failed = rig.exit(kick, 3);
    assert_eq!(Rig::replies(&failed), [Reply::Done(OpResult::Failed)]);
    assert_eq!(
        rig.sub("d.service"),
        "running",
        "a failed reload leaves it up"
    );
    assert_eq!(rig.state("d.service"), ActiveState::Active);

    let refused = rig.request(Request::Reload(String::from("plain.service")));
    assert!(matches!(
        Rig::replies(&refused).as_slice(),
        [Reply::Refused(_)]
    ));
    let _ = rig.request(Request::stop("d.service"));
    let _ = rig.exit(main, 0);
    let _ = rig.emptied("d.service");
    let stopped = rig.request(Request::Reload(String::from("d.service")));
    assert!(matches!(
        Rig::replies(&stopped).as_slice(),
        [Reply::Refused(_)]
    ));
}

#[test]
fn a_notify_status_alone_is_shown_and_is_not_readiness() {
    let mut rig = Rig::new(&[(
        "n.service",
        "[Service]\nType=notify\nNotifyFd=3\nExecStart=/bin/n\n",
    )]);
    let _ = rig.request(Request::start("n.service"));
    let _ = rig.spawned("n.service");
    let unit = rig.id("n.service");
    let _ = rig.step(Event::Status {
        unit,
        status: String::from("warming up"),
    });
    assert_eq!(rig.state("n.service"), ActiveState::Activating);
    let shown = rig.manager.status(unit).and_then(|s| s.status);
    assert_eq!(shown.as_deref(), Some("warming up"));
    let _ = rig.step(Event::Ready { unit, status: None });
    assert_eq!(rig.state("n.service"), ActiveState::Active);
}

/// The socket-unit actions in `actions`, as short words.
fn socket_actions(rig: &Rig, actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Listen { unit, .. } => Some(alloc::format!("listen {}", rig.name(*unit))),
            Action::Unlisten { unit } => Some(alloc::format!("unlisten {}", rig.name(*unit))),
            Action::Watch { unit } => Some(alloc::format!("watch {}", rig.name(*unit))),
            Action::Close { connection } => Some(alloc::format!("close {}", connection.0)),
            _ => None,
        })
        .collect()
}

/// The sockets and connection each spawn in `actions` is given.
fn handed(actions: &[Action]) -> Vec<(Vec<UnitId>, Option<Token>)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Spawn { spec, .. } => Some((spec.sockets.clone(), spec.connection)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_connection_on_a_socket_starts_its_service_with_the_listening_socket() {
    let mut rig = Rig::new(&[
        ("web.socket", "[Socket]\nListenStream=8080\n"),
        ("web.service", "[Service]\nExecStart=/bin/web\n"),
        ("sockets.target", "[Unit]\n"),
    ]);
    let started = rig.request(Request::start("web.socket"));
    assert_eq!(socket_actions(&rig, &started), ["listen web.socket"]);
    assert_eq!(rig.state("web.socket"), ActiveState::Activating);
    let socket = rig.id("web.socket");
    let up = rig.step(Event::Listening {
        unit: socket,
        result: Ok(()),
    });
    assert_eq!(socket_actions(&rig, &up), ["watch web.socket"]);
    assert_eq!(
        (rig.state("web.socket"), rig.sub("web.socket")),
        (ActiveState::Active, "listening")
    );
    assert!(
        rig.spawns(&up).is_empty(),
        "nothing starts before a connection"
    );

    let incoming = rig.step(Event::Incoming { unit: socket });
    assert_eq!(rig.spawns(&incoming), ["web.service"]);
    assert_eq!(handed(&incoming), [(alloc::vec![socket], None)]);
    assert_eq!(rig.sub("web.socket"), "running");
    assert!(
        socket_actions(&rig, &incoming).is_empty(),
        "the service accepts now"
    );

    let (web, _) = rig.spawned("web.service");
    let _ = rig.exit(web, 0);
    let down = rig.emptied("web.service");
    assert_eq!(socket_actions(&rig, &down), ["watch web.socket"]);
    assert_eq!(rig.sub("web.socket"), "listening");

    let stopped = rig.request(Request::stop("web.socket"));
    assert_eq!(socket_actions(&rig, &stopped), ["unlisten web.socket"]);
    assert_eq!(rig.state("web.socket"), ActiveState::Inactive);
}

#[test]
fn a_socket_neither_listens_again_at_shutdown_nor_spins_on_a_refused_start() {
    let mut rig = Rig::new(&[
        ("web.socket", "[Socket]\nListenStream=8080\n"),
        ("web.service", "[Service]\nExecStart=/bin/web\n"),
        ("multi-user.target.wants/web.socket", "->web.socket"),
    ]);
    let _ = rig.boot();
    let socket = rig.id("web.socket");
    let _ = rig.step(Event::Listening {
        unit: socket,
        result: Ok(()),
    });
    let _ = rig.step(Event::Incoming { unit: socket });
    let (web, _) = rig.spawned("web.service");

    let down = rig.request(Request::Poweroff);
    let _ = rig.exit(web, 0);
    let stopped = rig.emptied("web.service");
    let watched = socket_actions(&rig, &down)
        .into_iter()
        .chain(socket_actions(&rig, &stopped))
        .filter(|action| action.starts_with("watch"))
        .count();
    assert_eq!(watched, 0, "nothing listens again once shutdown has begun");

    let late = rig.step(Event::Incoming { unit: socket });
    assert!(rig.spawns(&late).is_empty());
    assert!(
        !socket_actions(&rig, &late)
            .iter()
            .any(|a| a.starts_with("watch")),
        "a connection during shutdown does not re-arm the socket"
    );
}

#[test]
fn shutdown_stops_a_socket_its_service_and_what_is_after_them() {
    let mut rig = Rig::new(&[
        ("web.socket", "[Socket]\nListenStream=8080\n"),
        ("web.service", "[Service]\nExecStart=/bin/web\n"),
        ("echo.socket", "[Socket]\nListenStream=7\nAccept=yes\n"),
        (
            "echo@.service",
            "[Service]\nExecStart=/bin/echo\nStandardInput=socket\n",
        ),
        ("sockets.target", "[Unit]\n"),
        (
            "getty@.service",
            "[Unit]\nAfter=basic.target\n[Service]\nExecStart=/bin/getty %i\nRestart=always\n\
             RestartSec=0\nSendSIGHUP=yes\nTimeoutStopSec=5s\n",
        ),
        ("multi-user.target.wants/web.socket", "->web.socket"),
        ("multi-user.target.wants/echo.socket", "->echo.socket"),
        (
            "multi-user.target.wants/getty@console.service",
            "->getty@.service",
        ),
        ("test.slice", "[Slice]\nTasksMax=64\n"),
        (
            "hog.service",
            "[Service]\nSlice=test.slice\nExecStart=/bin/hog\n",
        ),
        ("multi-user.target.wants/hog.service", "->hog.service"),
    ]);
    let _ = rig.boot();
    let (hog, _) = rig.spawned("hog.service");
    let hog_unit = rig.id("hog.service");
    let _ = rig.step(Event::OomKilled { unit: hog_unit });
    let _ = rig.killed(hog, Signal::KILL);
    let _ = rig.emptied("hog.service");
    assert_eq!(rig.state("hog.service"), ActiveState::Failed);
    let _ = rig.request(Request::Scope {
        unit: String::from("probe.scope"),
        slice: None,
        pids: alloc::vec![Pid(4242)],
    });
    for name in ["web.socket", "echo.socket"] {
        let unit = rig.id(name);
        let _ = rig.step(Event::Listening {
            unit,
            result: Ok(()),
        });
    }
    let (getty, _) = rig.spawned("getty@console.service");
    let web_socket = rig.id("web.socket");
    let _ = rig.step(Event::Incoming { unit: web_socket });
    let (web, _) = rig.spawned("web.service");
    let echo_socket = rig.id("echo.socket");
    let _ = rig.step(Event::Accepted {
        unit: echo_socket,
        connection: Token(3),
    });
    let (echo, _) = rig.spawned("echo@1.service");
    let _ = rig.exit(echo, 0);
    let _ = rig.emptied("echo@1.service");
    assert_eq!(rig.state("multi-user.target"), ActiveState::Active);
    rig.reload();

    let _ = rig.request(Request::Poweroff);
    let _ = rig.exit(web, 0);
    let _ = rig.emptied("web.service");
    let _ = rig.exit(getty, 0);
    let _ = rig.emptied("getty@console.service");
    let _ = rig.emptied("probe.scope");
    for name in [
        "web.socket",
        "echo.socket",
        "getty@console.service",
        "probe.scope",
        "basic.target",
    ] {
        assert_eq!(
            rig.state(name),
            ActiveState::Inactive,
            "{name} was not stopped"
        );
    }
}

#[test]
fn shutdown_ends_a_unit_a_reload_masked_while_it_ran() {
    let units = [
        (
            "getty.service",
            "[Unit]\nAfter=basic.target\n[Service]\nExecStart=/bin/getty\n",
        ),
        ("multi-user.target.wants/getty.service", "->getty.service"),
    ];
    let mut rig = Rig::new(&units);
    let _ = rig.boot();
    let (getty, _) = rig.spawned("getty.service");
    assert_eq!(rig.state("multi-user.target"), ActiveState::Active);

    let mut masked = crate::source::Source::new();
    for (path, text) in super::rig::TARGETS {
        let entry = crate::source::Entry::File(text.as_bytes().to_vec());
        assert!(masked.add(crate::source::Layer::Image, path, entry).is_ok());
    }
    let alias = crate::source::Entry::Alias(String::from("multi-user.target"));
    assert!(
        masked
            .add(crate::source::Layer::Admin, "default.target", alias)
            .is_ok()
    );
    assert!(
        masked
            .add(
                crate::source::Layer::Admin,
                "getty.service",
                crate::source::Entry::Masked
            )
            .is_ok()
    );
    rig.manager.reload(masked);
    assert_eq!(
        rig.state("getty.service"),
        ActiveState::Active,
        "a reload stops nothing"
    );

    let down = rig.request(Request::Poweroff);
    let mut actions = down;
    actions.extend(rig.exit(getty, 0));
    actions.extend(rig.emptied("getty.service"));
    for name in ["getty.service", "basic.target"] {
        assert_eq!(
            rig.state(name),
            ActiveState::Inactive,
            "{name} was not stopped"
        );
    }
    assert!(
        actions
            .iter()
            .any(|action| matches!(action, Action::Power(_))),
        "the machine went down"
    );
}

#[test]
fn a_socket_whose_service_cannot_start_fails_rather_than_listening_on() {
    let mut rig = Rig::new(&[
        ("web.socket", "[Socket]\nListenStream=8080\n"),
        (
            "web.service",
            "[Unit]\nRequires=missing.service\n[Service]\nExecStart=/bin/web\n",
        ),
    ]);
    let _ = rig.request(Request::start("web.socket"));
    let socket = rig.id("web.socket");
    let _ = rig.step(Event::Listening {
        unit: socket,
        result: Ok(()),
    });
    let refused = rig.step(Event::Incoming { unit: socket });
    assert_eq!(socket_actions(&rig, &refused), ["unlisten web.socket"]);
    assert_eq!(rig.state("web.socket"), ActiveState::Failed);
}

#[test]
fn an_accepting_socket_starts_an_instance_per_connection() {
    let mut rig = Rig::new(&[
        ("echo.socket", "[Socket]\nListenStream=7\nAccept=yes\n"),
        (
            "echo@.service",
            "[Service]\nExecStart=/bin/echoer %i\nStandardInput=socket\n",
        ),
    ]);
    let _ = rig.request(Request::start("echo.socket"));
    let socket = rig.id("echo.socket");
    let _ = rig.step(Event::Listening {
        unit: socket,
        result: Ok(()),
    });

    let first = rig.step(Event::Accepted {
        unit: socket,
        connection: Token(7),
    });
    assert_eq!(rig.spawns(&first), ["echo@1.service"]);
    assert_eq!(handed(&first), [(Vec::new(), Some(Token(7)))]);
    assert_eq!(Rig::argvs(&first), [["/bin/echoer", "1"]]);
    assert_eq!(
        socket_actions(&rig, &first),
        ["watch echo.socket"],
        "it listens on at once"
    );

    let second = rig.step(Event::Accepted {
        unit: socket,
        connection: Token(8),
    });
    assert_eq!(rig.spawns(&second), ["echo@2.service"]);
    let (one, _) = rig.spawned("echo@1.service");
    let _ = rig.exit(one, 0);
    let gone = rig.emptied("echo@1.service");
    assert!(
        socket_actions(&rig, &gone).is_empty(),
        "the connection went with its process"
    );

    let _ = rig.request(Request::stop("echo.socket"));
    let late = rig.step(Event::Accepted {
        unit: socket,
        connection: Token(9),
    });
    assert_eq!(
        socket_actions(&rig, &late),
        ["close 9"],
        "a stopped socket takes nothing"
    );
}

#[test]
fn a_socket_that_cannot_listen_fails() {
    let mut rig = Rig::new(&[
        ("bad.socket", "[Socket]\nListenStream=/nope/bad.sock\n"),
        ("bad.service", "[Service]\nExecStart=/bin/bad\n"),
    ]);
    let _ = rig.request(Request::start("bad.socket"));
    let socket = rig.id("bad.socket");
    let failed = rig.step(Event::Listening {
        unit: socket,
        result: Err(crate::event::Errno(2)),
    });
    assert_eq!(socket_actions(&rig, &failed), ["unlisten bad.socket"]);
    assert_eq!(rig.state("bad.socket"), ActiveState::Failed);
    assert!(
        Rig::lines(&failed)
            .iter()
            .any(|line| line.contains("could not listen (errno 2)"))
    );
}

#[test]
fn a_socket_listens_before_its_service_starts() {
    let mut rig = Rig::new(&[
        ("web.socket", "[Socket]\nListenStream=8080\n"),
        ("web.service", "[Service]\nExecStart=/bin/web\n"),
        ("multi-user.target.wants/web.socket", "->web.socket"),
        ("multi-user.target.wants/web.service", "->web.service"),
    ]);
    let boot = rig.boot();
    assert_eq!(socket_actions(&rig, &boot), ["listen web.socket"]);
    assert!(
        rig.spawns(&boot).is_empty(),
        "web.service waits for its socket"
    );
    let socket = rig.id("web.socket");
    let up = rig.step(Event::Listening {
        unit: socket,
        result: Ok(()),
    });
    assert_eq!(rig.spawns(&up), ["web.service"]);
    assert_eq!(handed(&up), [(alloc::vec![socket], None)]);
}

#[test]
fn restart_stops_then_starts_and_answers_when_up() {
    let mut rig = Rig::new(&[("a.service", "[Service]\nExecStart=/bin/a\n")]);
    let pid = start(&mut rig, "a.service");
    let restart = rig.request(Request::restart("a.service"));
    assert_eq!(rig.kills(&restart), owned(&[("a.service", "TERM>group")]));
    let _ = rig.killed(pid, Signal::TERM);
    let again = rig.emptied("a.service");
    assert_eq!(rig.spawns(&again), ["a.service"]);
    assert!(Rig::replies(&again).is_empty(), "not up yet");
    let (_, up) = rig.spawned("a.service");
    assert_eq!(Rig::replies(&up), [Reply::Done(OpResult::Done)]);
}

#[test]
fn isolate_stops_what_the_target_does_not_need() {
    let mut rig = Rig::new(&[
        ("a.service", "[Service]\nExecStart=/bin/a\n"),
        ("multi-user.target.wants/a.service", "->a.service"),
    ]);
    let _ = rig.boot();
    let (pid, _) = rig.spawned("a.service");
    assert_eq!(rig.state("multi-user.target"), ActiveState::Active);
    let isolate = rig.request(Request::Isolate(String::from("rescue.target")));
    assert_eq!(rig.kills(&isolate), owned(&[("a.service", "TERM>group")]));
    let _ = rig.exit(pid, 0);
    let _ = rig.emptied("a.service");
    assert_eq!(rig.state("rescue.target"), ActiveState::Active);
    assert_eq!(rig.state("multi-user.target"), ActiveState::Inactive);
    assert_eq!(
        rig.state("sysinit.target"),
        ActiveState::Active,
        "rescue needs it"
    );
    let refused = rig.request(Request::Isolate(String::from("basic.target")));
    assert!(
        matches!(Rig::replies(&refused).as_slice(), [Reply::Refused(_)]),
        "no AllowIsolate="
    );
}

#[test]
fn fail_mode_refuses_to_replace_a_queued_operation_and_replace_mode_cancels_it() {
    let mut rig = Rig::new(&[
        (
            "a.service",
            "[Unit]\nAfter=b.service\nWants=b.service\n[Service]\nExecStart=/bin/a\n",
        ),
        ("b.service", "[Service]\nType=notify\nExecStart=/bin/b\n"),
    ]);
    let queued = rig.request(Request::start("a.service"));
    assert_eq!(rig.spawns(&queued), ["b.service"], "a waits for b");
    let _ = rig.spawned("b.service");
    let refused = rig.request(Request::Stop(String::from("a.service"), crate::Mode::Fail));
    assert!(
        matches!(Rig::replies(&refused).as_slice(), [Reply::Refused(why)] if why.contains("queued")),
        "{refused:?}"
    );
    let replaced = rig.request(Request::stop("a.service"));
    assert_eq!(
        Rig::replies(&replaced),
        [Reply::Done(OpResult::Canceled), Reply::Done(OpResult::Done)],
        "the start is canceled, and the stop has nothing to do"
    );
    let b = rig.id("b.service");
    let ready = rig.step(Event::Ready {
        unit: b,
        status: None,
    });
    assert!(rig.spawns(&ready).is_empty(), "a no longer starts");
}

/// What `actions` ask of device watches, by unit name: `watch /dev/…` or
/// `unwatch`.
fn device_actions(rig: &Rig, actions: &[Action]) -> Vec<(String, String)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::WatchDevice { unit, path } => {
                Some((rig.name(*unit), alloc::format!("watch {path}")))
            }
            Action::UnwatchDevice { unit } => Some((rig.name(*unit), String::from("unwatch"))),
            _ => None,
        })
        .collect()
}

/// The backend's answer about a device's node.
fn device(rig: &mut Rig, name: &str, present: bool) -> Vec<Action> {
    let unit = rig.id(name);
    rig.step(Event::Device { unit, present })
}

const CARD: &str = "dev-dri-card0.device";

const HYPRIX: (&str, &str) = (
    "hyprix.service",
    "[Unit]\nRequires=dev-dri-card0.device\nAfter=dev-dri-card0.device\n\
     [Service]\nExecStart=/bin/hyprix\n",
);

#[test]
fn a_service_after_a_device_waits_for_its_node_without_a_timeout() {
    let mut rig = Rig::new(&[HYPRIX]);
    let started = rig.request(Request::start("hyprix.service"));
    assert!(rig.spawns(&started).is_empty(), "not before the node");
    assert_eq!(
        device_actions(&rig, &started),
        owned(&[(CARD, "watch /dev/dri/card0")])
    );
    assert_eq!(rig.state(CARD), ActiveState::Activating);
    assert_eq!(rig.sub(CARD), "waiting");
    // The backend's first answer: not there yet.
    let absent = device(&mut rig, CARD, false);
    assert!(rig.spawns(&absent).is_empty());
    assert_eq!(rig.manager.deadline(), None, "a device waits for ever");
    let _ = rig.at(600_000);
    assert_eq!(rig.state(CARD), ActiveState::Activating);
    assert_eq!(rig.state("hyprix.service"), ActiveState::Inactive);
    // nvrm publishes the node.
    let plugged = device(&mut rig, CARD, true);
    assert_eq!(rig.spawns(&plugged), ["hyprix.service"]);
    assert_eq!(rig.state(CARD), ActiveState::Active);
    assert_eq!(rig.sub(CARD), "plugged");
    let _ = rig.spawned("hyprix.service");
    assert_eq!(rig.state("hyprix.service"), ActiveState::Active);
}

#[test]
fn a_device_already_there_starts_its_dependent_in_the_same_turn() {
    let mut rig = Rig::new(&[HYPRIX]);
    let started = rig.request(Request::start("hyprix.service"));
    assert_eq!(
        device_actions(&rig, &started),
        owned(&[(CARD, "watch /dev/dri/card0")])
    );
    // The backend stats the node as it arms the watch, and answers at once.
    let answered = device(&mut rig, CARD, true);
    assert_eq!(rig.spawns(&answered), ["hyprix.service"]);
    let (_, up) = rig.spawned("hyprix.service");
    assert!(
        matches!(Rig::replies(&up).as_slice(), [Reply::Done(OpResult::Done)]),
        "{up:?}"
    );
    // A second start finds the device up and watches nothing more.
    let again = rig.request(Request::start(CARD));
    assert!(device_actions(&rig, &again).is_empty());
}

#[test]
fn a_device_that_goes_away_stops_what_binds_to_it_and_not_what_requires_it() {
    let mut rig = Rig::new(&[
        HYPRIX,
        (
            "bound.service",
            "[Unit]\nBindsTo=dev-dri-card0.device\nAfter=dev-dri-card0.device\n\
             [Service]\nExecStart=/bin/bound\n",
        ),
    ]);
    let _ = rig.request(Request::start("hyprix.service"));
    let _ = rig.request(Request::start("bound.service"));
    let both = device(&mut rig, CARD, true);
    assert_eq!(
        sorted(rig.spawns(&both)),
        ["bound.service", "hyprix.service"]
    );
    let _ = rig.spawned("hyprix.service");
    let _ = rig.spawned("bound.service");
    let gone = device(&mut rig, CARD, false);
    assert_eq!(device_actions(&rig, &gone), owned(&[(CARD, "unwatch")]));
    assert_eq!(rig.state(CARD), ActiveState::Inactive);
    assert_eq!(rig.sub(CARD), "dead");
    assert_eq!(
        rig.kills(&gone),
        owned(&[("bound.service", "TERM>group")]),
        "BindsTo= stops; Requires= alone does not, as under systemd"
    );
    assert_eq!(rig.state("hyprix.service"), ActiveState::Active);
    // A late answer for a watch that is gone changes nothing.
    let late = device(&mut rig, CARD, true);
    assert!(late.is_empty(), "{late:?}");
    assert_eq!(rig.state(CARD), ActiveState::Inactive);
}

#[test]
fn stopping_a_device_unwatches_it_and_stops_what_requires_it() {
    let mut rig = Rig::new(&[HYPRIX]);
    let _ = rig.request(Request::start("hyprix.service"));
    let stopped = rig.request(Request::stop(CARD));
    assert_eq!(device_actions(&rig, &stopped), owned(&[(CARD, "unwatch")]));
    assert_eq!(rig.state(CARD), ActiveState::Inactive);
    assert!(rig.spawns(&stopped).is_empty());
    assert_eq!(rig.state("hyprix.service"), ActiveState::Inactive);
    let late = device(&mut rig, CARD, true);
    assert!(rig.spawns(&late).is_empty(), "the watch is gone");
    assert_eq!(rig.state(CARD), ActiveState::Inactive);
    // Up, then stopped: unwatched again; a stopped one has nothing to drop.
    let _ = rig.request(Request::start(CARD));
    let _ = device(&mut rig, CARD, true);
    let down = rig.request(Request::stop(CARD));
    assert_eq!(device_actions(&rig, &down), owned(&[(CARD, "unwatch")]));
    let again = rig.request(Request::stop(CARD));
    assert!(device_actions(&rig, &again).is_empty());
}

#[test]
fn a_device_name_that_is_no_node_refuses_what_requires_it() {
    let mut rig = Rig::new(&[(
        "odd.service",
        "[Unit]\nRequires=sys-fs-cgroup.device\n[Service]\nExecStart=/bin/odd\n",
    )]);
    let refused = rig.request(Request::start("odd.service"));
    assert!(
        matches!(Rig::replies(&refused).as_slice(), [Reply::Refused(why)] if why.contains("sys-fs-cgroup.device")),
        "{refused:?}"
    );
    assert!(device_actions(&rig, &refused).is_empty());
}
