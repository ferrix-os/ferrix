//! The rules against Linux's, as `kernel/cgroup/cgroup.c` applies them.

use crate::Refusal;
use crate::controllers::{self, Controller, Set, Standing};

#[test]
fn threaded_controllers_are_cpu_and_pids() {
    let all = controllers::ALL
        .iter()
        .fold(Set::EMPTY, |set, c| set.with(*c));
    assert_eq!(
        all.domain(),
        Set::EMPTY.with(Controller::Io).with(Controller::Memory)
    );
    assert!(Controller::Cpu.threaded() && Controller::Pids.threaded());
}

#[test]
fn no_internal_processes_when_enabling_a_domain_controller() {
    let memory = Set::EMPTY.with(Controller::Memory);
    let pids = Set::EMPTY.with(Controller::Pids);
    let busy = Standing {
        has_tasks: true,
        ..Standing::default()
    };
    // A cgroup with processes may not enable memory for its children...
    assert_eq!(controllers::vet_enable(memory, busy), Err(Refusal::Busy));
    // ...nor may one with processes and memory already on enable pids.
    let with_memory = Standing {
        subtree_control: memory,
        ..busy
    };
    assert_eq!(
        controllers::vet_enable(pids, with_memory),
        Err(Refusal::Busy)
    );
    // A threaded controller may be enabled beside processes while the cgroup
    // could still be a threaded root: no domain controller on, no populated
    // child.
    assert_eq!(controllers::vet_enable(pids, busy), Ok(()));
    let populated_child = Standing {
        populated_children: true,
        ..busy
    };
    assert_eq!(
        controllers::vet_enable(pids, populated_child),
        Err(Refusal::Busy)
    );
    // Nothing new to enable is never refused, and neither is the root.
    assert_eq!(controllers::vet_enable(Set::EMPTY, busy), Ok(()));
    let root = Standing { root: true, ..busy };
    assert_eq!(controllers::vet_enable(memory, root), Ok(()));
    // Without processes of its own, anything may be enabled.
    assert_eq!(controllers::vet_enable(memory, Standing::default()), Ok(()));
}

#[test]
fn no_internal_processes_when_moving_in() {
    let memory = Set::EMPTY.with(Controller::Memory);
    let pids = Set::EMPTY.with(Controller::Pids);
    let enabling = |subtree_control| Standing {
        subtree_control,
        ..Standing::default()
    };
    // Not into a cgroup that enables a domain controller for its children.
    assert_eq!(
        controllers::vet_destination(enabling(memory)),
        Err(Refusal::Busy)
    );
    // Into one enabling only threaded ones while it could be a threaded
    // root, but not once it has a populated child.
    assert_eq!(controllers::vet_destination(enabling(pids)), Ok(()));
    let populated = Standing {
        populated_children: true,
        ..enabling(pids)
    };
    assert_eq!(controllers::vet_destination(populated), Err(Refusal::Busy));
    // The root takes processes whatever it enables, and a cgroup enabling
    // nothing takes them whatever else holds.
    let root = Standing {
        root: true,
        ..enabling(memory)
    };
    assert_eq!(controllers::vet_destination(root), Ok(()));
    let plain = Standing {
        populated_children: true,
        has_tasks: true,
        ..Standing::default()
    };
    assert_eq!(controllers::vet_destination(plain), Ok(()));
}
