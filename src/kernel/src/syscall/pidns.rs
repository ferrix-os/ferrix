//! PID namespaces: which number a process has in which namespace, and what
//! a namespace's init is owed (`docs/PIDNS.md`).
//!
//! # The kernel number stays the key
//!
//! [`Process::pid`] is, as it was, the machine-wide number `object::process`
//! chose: the registry, the job tree, process groups, sessions and the
//! terminal all name a task by it, and two of them compare the same in every
//! namespace. A namespace below the first has numbers of its own that map to
//! it, held in a [`Numbers`] (Linux's `struct pid`) by a task that is in one.
//! A task in the first namespace holds none, and its number *is* the kernel
//! number: nothing here costs it anything.
//!
//! # The call speaks the caller's namespace
//!
//! [`to_user`] turns a task into the number its caller calls it by (0 when
//! the caller cannot see it), [`from_user`] and [`find_in`] the other way.
//! Where the viewer is not the caller of a system call -- a `siginfo` read
//! by a handler, a file in `/proc` -- it is the process doing the reading,
//! `userns::acting`.
//!
//! # Locks
//!
//! All leaves. A namespace's number map is taken alone, never two at once,
//! and never with the registry's table: [`assign`] takes the kernel number
//! first, releases the table, then the levels one after the other; a lookup
//! reads a map, releases it, then asks the table and checks what it found
//! against the number it was given.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{SIGKILL, SIGSTOP};

use crate::fallible;
use crate::object::process::{PID_MAX, RESERVED};
use crate::sync::SpinLock;
use crate::syscall::kill;
use crate::syscall::process::Process;
use crate::syscall::registry;
use crate::syscall::signal::Origin;
use crate::syscall::thread::Thread;
use crate::syscall::userns;

/// Give the child a pid namespace of its own.
pub(crate) const CLONE_NEWPID: u64 = 0x2000_0000;

/// Deepest nesting: Linux's `MAX_PID_NS_LEVEL`. `ENOSPC` past it.
pub(crate) const MAX_LEVEL: u32 = 32;

/// Levels a [`Numbers`] can hold: the first namespace's and up to
/// [`MAX_LEVEL`] below it.
const LEVELS: usize = MAX_LEVEL as usize + 1;

/// Bytes charged for each level's entry in a namespace's map, on top of the
/// record: a `BTreeMap` node holds a key, a value and its share of links.
const ENTRY_COST: usize = 64;

/// `/proc/<pid>/ns/pid`'s number for the first namespace: Linux's own.
const FIRST_ID: u64 = 0xEFFF_FFFC;
/// And for the ones made after it.
static NEXT_ID: AtomicU64 = AtomicU64::new(0xF100_0000);

/// A pid namespace below the first. (The first is `None` wherever a
/// namespace is optional: it has no record and no map.)
#[derive(Debug)]
pub(crate) struct PidNamespace {
    /// The namespace it was made in; `None` only when that is the first.
    parent: Option<Arc<PidNamespace>>,
    /// 1 for a child of the first.
    level: u32,
    /// What `/proc/<pid>/ns/pid` names.
    id: u64,
    /// Local number to kernel number.
    local: SpinLock<Local>,
    /// Its pid 1, while there is one.
    init: SpinLock<Weak<Process>>,
    /// Set as its init goes: no task joins a namespace that is ending.
    dying: AtomicBool,
    /// The kernel heap this is, charged to the job that made it (F-37).
    _charge: Option<Charge>,
}

/// What a namespace's numbers are kept in.
#[derive(Debug)]
struct Local {
    /// Every number in use, to the kernel number it names.
    map: BTreeMap<u32, u32>,
    /// The number handed out last.
    last: u32,
}

impl Local {
    /// Choose and record the next number for kernel number `kernel`:
    /// cyclic, as the registry's are, the first in a namespace being 1.
    fn allocate(&mut self, kernel: u32) -> Result<u32, Errno> {
        let mut candidate = self.last;
        for _ in 0..PID_MAX {
            candidate = if candidate + 1 >= PID_MAX {
                RESERVED
            } else {
                candidate + 1
            };
            if !self.map.contains_key(&candidate) {
                let _ = fallible::insert(&mut self.map, candidate, kernel)
                    .map_err(|_| Errno::ENOMEM)?;
                self.last = candidate;
                return Ok(candidate);
            }
        }
        Err(Errno::EAGAIN)
    }
}

impl PidNamespace {
    /// 1 for a child of the first namespace.
    pub(crate) fn level(&self) -> u32 {
        self.level
    }

    /// Whether it is ending: its init has gone.
    pub(crate) fn is_dying(&self) -> bool {
        self.dying.load(Ordering::Acquire)
    }

    /// Its init, while one exists.
    pub(crate) fn init(&self) -> Option<Arc<Process>> {
        self.init.lock().upgrade()
    }

    /// Record `process` as its init. Once: the first task in a namespace.
    pub(crate) fn set_init(&self, process: &Arc<Process>) {
        *self.init.lock() = Arc::downgrade(process);
    }

    /// The kernel number it calls `number`, if it has one in use.
    pub(crate) fn kernel_of(&self, number: u32) -> Option<u32> {
        self.local.lock().map.get(&number).copied()
    }

    /// Whether `self` is `other` or below it.
    fn is_within(&self, other: &PidNamespace) -> bool {
        let mut at = self;
        loop {
            if core::ptr::eq(at, other) {
                return true;
            }
            match at.parent.as_deref() {
                Some(up) => at = up,
                None => return false,
            }
        }
    }
}

/// `/proc/<pid>/ns/pid`'s number for a namespace, `None` being the first.
pub(crate) fn id_of(namespace: Option<&Arc<PidNamespace>>) -> u64 {
    namespace.map_or(FIRST_ID, |namespace| namespace.id)
}

/// How deep a namespace is: 0 for the first.
pub(crate) fn level_of(namespace: Option<&Arc<PidNamespace>>) -> u32 {
    namespace.map_or(0, |namespace| namespace.level)
}

/// Make the namespace `clone` or `unshare` with `CLONE_NEWPID` asks for,
/// below `parent` (`None` for the first).
///
/// # Errors
///
/// `ENOSPC` past [`MAX_LEVEL`], Linux's answer; `ENOMEM` past the job's
/// memory (F-37).
#[inline(never)]
pub(crate) fn create(parent: Option<&Arc<PidNamespace>>) -> Result<Arc<PidNamespace>, Errno> {
    let level = level_of(parent) + 1;
    if level > MAX_LEVEL {
        return Err(Errno::ENOSPC);
    }
    let charge = Charge::arc::<PidNamespace>().map_err(|_| Errno::ENOMEM)?;
    let namespace = PidNamespace {
        parent: parent.cloned(),
        level,
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        local: SpinLock::new(Local {
            map: BTreeMap::new(),
            last: 0,
        }),
        init: SpinLock::new(Weak::new()),
        dying: AtomicBool::new(false),
        _charge: Some(charge),
    };
    fallible::try_arc(namespace).map_err(|_| Errno::ENOMEM)
}

/// One task's numbers in every namespace it is in below the first: Linux's
/// `struct pid`. A task in the first namespace has none.
#[derive(Debug)]
pub(crate) struct Numbers {
    /// The innermost namespace the task is in.
    ns: Arc<PidNamespace>,
    /// `nr[0]` is the kernel number; `nr[i]` the number in the level-`i`
    /// namespace of the chain, zero where none has been recorded yet.
    nr: [u32; LEVELS],
    /// This record and its map entries, charged to the job of the task that
    /// made the task (F-37).
    _charge: Option<Charge>,
}

impl Numbers {
    /// The innermost namespace.
    pub(crate) fn namespace(&self) -> &Arc<PidNamespace> {
        &self.ns
    }

    /// The number in `ns`: the task's own if `ns` is its innermost
    /// namespace, an outer one's if `ns` is above that, and zero for a
    /// namespace the task is not in.
    pub(crate) fn in_ns(&self, ns: &PidNamespace) -> u32 {
        if ns.level > self.ns.level {
            return 0;
        }
        let mut at: &PidNamespace = &self.ns;
        while at.level > ns.level {
            match at.parent.as_deref() {
                Some(up) => at = up,
                None => return 0,
            }
        }
        if core::ptr::eq(at, ns) {
            self.nr.get(ns.level as usize).copied().unwrap_or(0)
        } else {
            0
        }
    }

    /// The number in its own innermost namespace: 1 for a namespace's init.
    pub(crate) fn own(&self) -> u32 {
        self.nr.get(self.ns.level as usize).copied().unwrap_or(0)
    }

    /// Every number from the first namespace's down to the innermost, as
    /// `NSpid` lists them.
    pub(crate) fn chain(&self) -> &[u32] {
        self.nr.get(..=self.ns.level as usize).unwrap_or_default()
    }
}

impl Drop for Numbers {
    /// Give every local number back: the map entries go, each under its own
    /// namespace's lock and no other.
    fn drop(&mut self) {
        let mut at: Option<&PidNamespace> = Some(&self.ns);
        while let Some(namespace) = at {
            let number = self.nr.get(namespace.level as usize).copied().unwrap_or(0);
            if number != 0 {
                let mut local = namespace.local.lock();
                if local.map.get(&number) == self.nr.first() {
                    let _ = local.map.remove(&number);
                }
            }
            at = namespace.parent.as_deref();
        }
    }
}

/// Number kernel number `kernel` in `ns` and every namespace above it, for a
/// task about to be made (or a thread about to start) in `ns`.
///
/// # Errors
///
/// `ENOMEM` for a namespace that is ending, for the job's memory, and for the
/// maps' nodes; `EAGAIN` when a namespace has no number left. Nothing is
/// recorded on an error.
#[inline(never)]
pub(crate) fn assign(ns: &Arc<PidNamespace>, kernel: u32) -> Result<Arc<Numbers>, Errno> {
    if ns.is_dying() {
        return Err(Errno::ENOMEM);
    }
    let cost = arc_footprint::<Numbers>() + ENTRY_COST * ns.level as usize;
    let charge = Charge::bytes(cost).map_err(|_| Errno::ENOMEM)?;
    let mut numbers = Numbers {
        ns: Arc::clone(ns),
        nr: [0; LEVELS],
        _charge: Some(charge),
    };
    if let Some(slot) = numbers.nr.first_mut() {
        *slot = kernel;
    }
    // Outermost first, so the first number of a fresh namespace is taken by
    // its first member whatever else has happened above. Each under its own
    // lock; a failure leaves `numbers` holding what was recorded, and its
    // drop gives that back.
    let mut chain: Vec<&PidNamespace> = Vec::new();
    let mut at: Option<&PidNamespace> = Some(ns);
    while let Some(namespace) = at {
        chain.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
        chain.push(namespace);
        at = namespace.parent.as_deref();
    }
    for namespace in chain.iter().rev() {
        let number = namespace.local.lock().allocate(kernel)?;
        if let Some(slot) = numbers.nr.get_mut(namespace.level as usize) {
            *slot = number;
        }
    }
    fallible::try_arc(numbers).map_err(|_| Errno::ENOMEM)
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// The number `viewer` calls `target` by: its kernel number for a viewer in
/// the first namespace, which sees everything; its number in the viewer's
/// namespace for a viewer below it; zero when the viewer cannot see it.
pub(crate) fn to_user(viewer: &Process, target: &Process) -> u32 {
    number_for(viewer.numbers(), target.numbers(), target.pid())
}

/// [`to_user`] for a task named by its numbers and kernel number.
pub(crate) fn number_for(
    viewer: Option<&Arc<Numbers>>,
    target: Option<&Arc<Numbers>>,
    kernel: u32,
) -> u32 {
    match (viewer, target) {
        (None, _) => kernel,
        (Some(viewer), Some(target)) => target.in_ns(&viewer.ns),
        (Some(_), None) => 0,
    }
}

/// The number `viewer` calls thread `tid` of `process` by.
pub(crate) fn tid_to_user(viewer: &Process, thread: &Thread) -> u32 {
    let tid = thread.tid();
    if viewer.numbers().is_none() {
        return tid;
    }
    if tid == thread.process().pid() {
        return to_user(viewer, thread.process());
    }
    number_for(viewer.numbers(), thread.numbers().as_ref(), tid)
}

/// The kernel number behind `number`, as `viewer` counts.
pub(crate) fn from_user(viewer: &Process, number: u32) -> Option<u32> {
    match viewer.numbers() {
        None => Some(number),
        Some(viewer) => viewer.ns.kernel_of(number),
    }
}

/// The live process `viewer` calls `number`.
pub(crate) fn find_in(viewer: &Process, number: u32) -> Option<Arc<Process>> {
    let kernel = from_user(viewer, number)?;
    let found = registry::find(kernel)?;
    // A stale entry in the map names a kernel number reused since, by a task
    // the viewer cannot see. A thread's number finds its process, as always.
    (to_user(viewer, &found) != 0).then_some(found)
}

/// Every live process `viewer` can see, in ascending kernel order.
///
/// # Errors
///
/// `ENOMEM` when there was no memory for the list.
#[inline(never)]
pub(crate) fn live_in(viewer: &Process) -> Result<Vec<Arc<Process>>, Errno> {
    let mut all = registry::live()?;
    if viewer.numbers().is_some() {
        all.retain(|process| to_user(viewer, process) != 0);
    }
    Ok(all)
}

/// The number the reading process calls kernel number `kernel`: what a
/// `siginfo` or a socket's credentials tell, read by the process that reads
/// them. The kernel number when nothing is reading, or the reader is in the
/// first namespace; zero when it cannot be seen.
#[inline(never)]
pub(crate) fn show_pid(kernel: u32) -> u32 {
    let Some(reader) = userns::acting() else {
        return kernel;
    };
    if reader.numbers().is_none() {
        return kernel;
    }
    registry::find(kernel).map_or(0, |sender| to_user(&reader, &sender))
}

/// The number of the group or session `record` names, as `viewer` counts:
/// `kernel` for a viewer in the first namespace.
pub(crate) fn group_to_user(viewer: &Process, record: Option<&Arc<Numbers>>, kernel: u32) -> u32 {
    number_for(viewer.numbers(), record, kernel)
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

/// Whether `process` is the init of a namespace below the first.
pub(crate) fn is_init(process: &Process) -> bool {
    process.numbers().is_some_and(|numbers| numbers.own() == 1)
}

/// Whether a signal to a namespace's init is thrown away: Linux's
/// `SIGNAL_UNKILLABLE` with `force`. The init of a namespace below the first
/// ignores a signal it has no handler for -- from anyone, the kernel
/// included -- except `SIGKILL` and `SIGSTOP` from an ancestor namespace
/// (or the kernel's own kill), which are how a namespace is ended from
/// outside.
#[inline(never)]
pub(crate) fn discards(target: &Process, signal: u32, origin: Origin) -> bool {
    if !is_init(target) || !target.with_signals(|signals| signals.is_default(signal)) {
        return false;
    }
    let forced = (signal == SIGKILL || signal == SIGSTOP) && from_ancestor(target, origin);
    !forced
}

/// Whether the sender of `origin` is outside `target`'s namespace.
fn from_ancestor(target: &Process, origin: Origin) -> bool {
    let sender = match origin {
        Origin::Kernel => return true,
        Origin::User { pid, .. } | Origin::Thread { pid, .. } | Origin::Child { pid, .. } => pid,
        Origin::Fault { .. } => return false,
    };
    let Some(ns) = target.numbers().map(|numbers| &numbers.ns) else {
        return true;
    };
    registry::find(sender).is_some_and(|sender| {
        sender
            .numbers()
            .is_none_or(|numbers| numbers.in_ns(ns) == 0)
    })
}

/// A namespace's init is going: no task may join it, and every other task in
/// it and below it is sent `SIGKILL` (Linux's `zap_pid_ns_processes`).
#[inline(never)]
pub(crate) fn init_gone(init: &Process) {
    let Some(numbers) = init.numbers() else {
        return;
    };
    if numbers.own() != 1 {
        return;
    }
    numbers.ns.dying.store(true, Ordering::Release);
    for process in registry::live().unwrap_or_default() {
        let inside = process
            .numbers()
            .is_some_and(|other| other.ns.is_within(&numbers.ns));
        if inside && !core::ptr::eq(Arc::as_ptr(&process), init) {
            kill::send(&process, SIGKILL, Origin::Kernel);
        }
    }
}

/// Where the children of `process` go when it ends, past any subreaper: the
/// init of its own namespace when that is below the first, `None` when the
/// first namespace's rule (pid 1 of the machine) applies.
pub(crate) fn local_reaper(process: &Process) -> Option<Option<Arc<Process>>> {
    let numbers = process.numbers()?;
    Some(
        numbers
            .ns
            .init()
            .filter(|init| !init.is_terminated() && !core::ptr::eq(Arc::as_ptr(init), process)),
    )
}

/// Whether `a` and `b` are in the same innermost namespace: the bound a
/// subreaper search stays inside.
pub(crate) fn same_namespace(a: &Process, b: &Process) -> bool {
    match (a.numbers(), b.numbers()) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(&a.ns, &b.ns),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// For a procfs instance, which names processes in the namespace that mounted it
// ---------------------------------------------------------------------------

/// The number `ns` (`None` for the first) calls `process`; zero if it cannot
/// see it.
pub(crate) fn name_in(ns: Option<&Arc<PidNamespace>>, process: &Process) -> u32 {
    match ns {
        None => process.pid(),
        Some(ns) => process.numbers().map_or(0, |numbers| numbers.in_ns(ns)),
    }
}

/// The number `ns` calls thread `tid` of `process`; zero if it cannot see it.
pub(crate) fn thread_name_in(ns: Option<&Arc<PidNamespace>>, process: &Process, tid: u32) -> u32 {
    match ns {
        None => tid,
        Some(_) if tid == process.pid() => name_in(ns, process),
        Some(ns) => process
            .thread_by_tid(tid)
            .and_then(|thread| thread.numbers())
            .map_or(0, |numbers| numbers.in_ns(ns)),
    }
}

/// The kernel number `ns` (`None` for the first) calls `number`.
pub(crate) fn kernel_in(ns: Option<&Arc<PidNamespace>>, number: u32) -> Option<u32> {
    match ns {
        None => Some(number),
        Some(ns) => ns.kernel_of(number),
    }
}

/// The numbers to print on an `NS*` line of `/proc/<pid>/status`, for a task
/// with `numbers`, read from the namespace at `level`: its number at that
/// level and every one below. Empty when the task is in the first namespace
/// and the reader is too, which Linux prints as the one number alone.
#[inline(never)]
pub(crate) fn status_chain(numbers: Option<&Arc<Numbers>>, level: u32) -> Vec<u32> {
    numbers
        .and_then(|numbers| numbers.chain().get(level as usize..))
        .map(<[u32]>::to_vec)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Groups and sessions named by kernel number, for the terminal
// ---------------------------------------------------------------------------

/// The number `viewer` calls process group `kernel`: zero when it has no
/// member the viewer can see a number for. The terminal keeps kernel
/// numbers; a program reads them through this.
#[inline(never)]
pub(crate) fn pgrp_to_user(viewer: &Process, kernel: u32) -> u32 {
    if viewer.numbers().is_none() {
        return kernel;
    }
    registry::live()
        .unwrap_or_default()
        .iter()
        .find(|member| member.pgid() == kernel)
        .map_or(0, |member| member.pgid_in(viewer))
}

/// The kernel number of the process group `viewer` calls `number`, if a
/// process in it is there to be found.
#[inline(never)]
pub(crate) fn pgrp_from_user(viewer: &Process, number: u32) -> Option<u32> {
    if viewer.numbers().is_none() {
        return Some(number);
    }
    registry::live()
        .unwrap_or_default()
        .iter()
        .find(|member| member.pgid_in(viewer) == number && number != 0)
        .map(|member| member.pgid())
}

/// [`pgrp_to_user`] for a session.
#[inline(never)]
pub(crate) fn sid_to_user(viewer: &Process, kernel: u32) -> u32 {
    if viewer.numbers().is_none() {
        return kernel;
    }
    registry::live()
        .unwrap_or_default()
        .iter()
        .find(|member| member.sid() == kernel)
        .map_or(0, |member| member.sid_in(viewer))
}
