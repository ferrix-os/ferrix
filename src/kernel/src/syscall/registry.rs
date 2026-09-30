//! Finding a POSIX process by its pid, and numbering its threads.
//!
//! Two questions only something outside a process can answer: which process
//! is pid 42, and which processes exist at all. `/proc` asks both on every
//! listing, `kill` and `wait4` ask the first, and a diagnostic dump asks the
//! second.
//!
//! # The table is the core's
//!
//! The numbers and the table that maps them to processes are in
//! [`crate::object::process`], because a pid is a core fact -- a job kill
//! walks the table, and the core cannot name the personality to do it. The
//! table holds each process as a [`Host`], weakly, so being listed never keeps
//! a process alive, and knows nothing of POSIX.
//!
//! What is here is the personality's typed view of it: what it registers, and
//! a lookup that answers with the POSIX [`Process`] rather than the core's
//! view of one. And the one Linux rule the table does not know, that thread
//! ids and pids are one space ([`allocate_thread`]).

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;

use crate::object::process::{self as table, Host};
use crate::syscall::pidns::{self, Numbers};
use crate::syscall::process::{self, Process};
use crate::syscall::thread::Thread;

pub(crate) use crate::object::process::{INIT_PID, PID_MAX, allocate_init, is_free, release};

/// Share a process and make it findable by its pid.
///
/// Every path that makes a process calls this where it would otherwise have
/// called `Arc::new`: a process that is shared but not registered is one
/// `kill` and `/proc` cannot see.
pub(crate) fn register(process: Process) -> Arc<Process> {
    let process = Arc::new(process);
    publish(&process);
    process
}

/// List a fork child's first thread on it, then make the child findable, in
/// that order: nothing can find the child without the mask its thread
/// inherited. See [`publish`].
pub(crate) fn publish_forked(child: &Arc<Process>, thread: &Arc<Thread>) {
    child.add_thread(thread);
    publish(child);
}

/// Choose a thread id for a new thread of `process`, which is already shared
/// and findable, and have it find `process` from the start: thread ids and
/// pids are one space, as on Linux, so `kill` or `prlimit` given a thread's
/// id reach its process. Also its numbers in every pid namespace `process` is
/// in below the first. `None` if every number is in use, or a namespace has
/// no room or no memory.
pub(crate) fn allocate_thread_in(process: &Arc<Process>) -> Option<(u32, Option<Arc<Numbers>>)> {
    // A thread is a task its job's `pids.max` counts, charged before its id
    // is chosen and given back with the id.
    process.charge_thread().ok()?;
    let Some(tid) = table::allocate() else {
        process.uncharge_thread();
        return None;
    };
    let numbers = match process.numbers() {
        None => None,
        Some(own) => match pidns::assign(own.namespace(), tid) {
            Ok(numbers) => Some(numbers),
            Err(_) => {
                release(tid);
                process.uncharge_thread();
                return None;
            }
        },
    };
    // Reserved just above, so naming it adds no entry and cannot fail.
    let _ = table::name(tid, weak(process));
    Some((tid, numbers))
}

/// [`allocate_thread_in`] for a process in the first namespace, which has no
/// numbers to keep: the boot checks'.
pub(crate) fn allocate_thread(process: &Arc<Process>) -> Option<u32> {
    allocate_thread_in(process).map(|(tid, _)| tid)
}

/// Give back thread id `tid` of `process`, if it still names that process.
/// Called by a thread other than its process's first as it is dropped.
pub(crate) fn release_thread(tid: u32, process: &Process) {
    table::release_naming(tid, process);
    process.uncharge_thread();
}

/// How many numbers name `process`: its pid, and one for each of its threads
/// that holds an id of its own. For the check that a thread which replaced
/// its process's program gave its own id back.
pub(crate) fn numbers_naming(process: &Process) -> usize {
    table::numbers_naming(process)
}

/// Make a process that is already shared findable by its pid.
///
/// For a process that must be complete before `kill`, a process group's
/// signal or `/proc` can reach it: a fork child, whose first thread -- with
/// the blocked mask a signal sent to it is judged against -- is listed first.
pub(crate) fn publish(process: &Arc<Process>) {
    let pid = process.pid();
    // The first process of a pid namespace is its init.
    if let Some(numbers) = process.numbers()
        && numbers.own() == 1
    {
        numbers.namespace().set_init(process);
    }
    if pid != 0 {
        // Reserved by `allocate` when the process was made, so naming it adds
        // no entry and cannot fail.
        let _ = table::name(pid, weak(process));
    }
}

/// The table's view of `process`: weak, and seen as the core sees it.
fn weak(process: &Arc<Process>) -> alloc::sync::Weak<dyn Host> {
    Arc::downgrade(process) as alloc::sync::Weak<dyn Host>
}

/// The live process with this pid.
pub(crate) fn find(pid: u32) -> Option<Arc<Process>> {
    table::downcast(table::find(pid)?)
}

/// Every live process, in ascending pid order.
///
/// Dropped, if it is dropped, with the table unlocked: see
/// [`table::live`].
///
/// # Errors
///
/// `ENOMEM` when there was no memory for the list.
pub(crate) fn live() -> Result<Vec<Arc<Process>>, Errno> {
    let hosts = table::live().map_err(|_| Errno::ENOMEM)?;
    Ok(hosts.into_iter().filter_map(table::downcast).collect())
}

/// The boot self-check's part: numbers are distinct, found, listed in order,
/// given back when a process goes, and not handed straight out again.
///
/// Returns how many processes it numbered.
pub(crate) fn check() -> Result<u32, &'static str> {
    let make = || process::new_for_check().map_err(|_| "no address space for the pid check");
    let first = make()?;
    let second = make()?;
    let (one, two) = (first.pid(), second.pid());
    if one == 0 || two == 0 || one == two {
        return Err("two processes were not given two distinct pids");
    }
    let found = |pid: u32, process: &Arc<Process>| {
        find(pid).is_some_and(|found| Arc::ptr_eq(&found, process))
    };
    if !found(one, &first) || !found(two, &second) {
        return Err("a process is not found by its own pid");
    }
    let listed: Vec<u32> = live()
        .map_err(|_| "no memory to list the live processes")?
        .iter()
        .map(|process| process.pid())
        .collect();
    let ascending = listed
        .windows(2)
        .all(|pair| matches!(pair, [low, high] if low < high));
    if !ascending || !listed.contains(&one) || !listed.contains(&two) {
        return Err("the live processes are not listed once each, in pid order");
    }

    drop(first);
    drop(second);
    if find(one).is_some() || find(two).is_some() {
        return Err("a dropped process is still found by its pid");
    }
    let third = make()?;
    if third.pid() == one || third.pid() == two {
        return Err("a pid was handed out again straight after it was given back");
    }
    if [one, two, third.pid()].contains(&INIT_PID) {
        return Err("an ordinary process was given init's pid");
    }

    // Init's pid, which nothing but init takes. Boot checks run before init
    // starts, so it is free here; taken and given straight back.
    if !is_free(INIT_PID) {
        return Err("init's pid was taken before init started");
    }
    if allocate_init() != Some(INIT_PID) {
        return Err("init was not given pid 1 while it was free");
    }
    if allocate_init().is_some() {
        return Err("init's pid was handed out twice");
    }
    release(INIT_PID);
    if !is_free(INIT_PID) {
        return Err("init's pid was not given back");
    }
    Ok(3)
}
