//! The controllers, their sets, and the no-internal-process rule that
//! decides whether enabling one beneath a cgroup, or a move into it, may
//! happen. How a set prints in `cgroup.controllers` and how a write to
//! `cgroup.subtree_control` parses are cgroupfs's (`ferrix_cgroupfs::controllers`).

use crate::Refusal;

/// A controller `docs/ARCHITECTURE.md` §6 names, in Linux's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Controller {
    /// `cpu`: weight and bandwidth handed to the scheduling classes.
    Cpu,
    /// `io`: per-cgroup bandwidth in the block layer's scheduler.
    Io,
    /// `memory`: where charges, reclaim and the OOM kill are scoped.
    Memory,
    /// `pids`: how many tasks a subtree may hold.
    Pids,
}

/// Every controller, in order.
pub const ALL: [Controller; 4] = [
    Controller::Cpu,
    Controller::Io,
    Controller::Memory,
    Controller::Pids,
];

impl Controller {
    /// The name the files use.
    pub fn name(self) -> &'static str {
        match self {
            Controller::Cpu => "cpu",
            Controller::Io => "io",
            Controller::Memory => "memory",
            Controller::Pids => "pids",
        }
    }

    /// Whether it is one of Linux's threaded controllers, which can share
    /// a cgroup with processes (`cpu` and `pids`; `cpuset` and `perf_event`
    /// are the others, and are not built here). `memory` and `io` are domain
    /// controllers, which the no-internal-process rule is about.
    pub fn threaded(self) -> bool {
        matches!(self, Controller::Cpu | Controller::Pids)
    }

    /// Its bit in a [`Set`].
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// A set of controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Set(u8);

impl Set {
    /// No controller.
    pub const EMPTY: Set = Set(0);

    /// Whether `controller` is in it.
    pub fn contains(self, controller: Controller) -> bool {
        self.0 & controller.bit() != 0
    }

    /// It, with `controller` added.
    #[must_use]
    pub const fn with(self, controller: Controller) -> Set {
        Set(self.0 | controller.bit())
    }

    /// It, with `controller` taken out.
    #[must_use]
    pub fn without(self, controller: Controller) -> Set {
        Set(self.0 & !controller.bit())
    }

    /// Whether it holds nothing.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every controller in it is also in `other`.
    pub fn is_subset(self, other: Set) -> bool {
        self.0 & !other.0 == 0
    }

    /// Its domain controllers: those not [`Controller::threaded`].
    #[must_use]
    pub fn domain(self) -> Set {
        self.iter()
            .filter(|controller| !controller.threaded())
            .fold(Set::EMPTY, Set::with)
    }

    /// It, with every controller in `other` added.
    #[must_use]
    pub fn union(self, other: Set) -> Set {
        Set(self.0 | other.0)
    }

    /// The controllers in both it and `other`.
    #[must_use]
    pub fn intersect(self, other: Set) -> Set {
        Set(self.0 & other.0)
    }

    /// It, with every controller in `other` taken out.
    #[must_use]
    pub fn minus(self, other: Set) -> Set {
        Set(self.0 & !other.0)
    }

    /// Its controllers, in order.
    pub fn iter(self) -> impl Iterator<Item = Controller> {
        ALL.into_iter()
            .filter(move |controller| self.contains(*controller))
    }
}

/// What a write to `cgroup.subtree_control` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Change {
    /// Controllers to enable for the children.
    pub enable: Set,
    /// Controllers to disable for them.
    pub disable: Set,
}

/// Where a cgroup stands, for the no-internal-process rule: the facts about
/// it Linux's rule reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent facts about one cgroup, each a condition of Linux's rule"
)]
pub struct Standing {
    /// Whether it is the root, which the rule exempts: `cgroup_is_mixable`.
    pub root: bool,
    /// Whether it has processes of its own: `cgroup_has_tasks`.
    pub has_tasks: bool,
    /// Whether a child of it is populated: `nr_populated_domain_children`.
    pub populated_children: bool,
    /// What its `cgroup.subtree_control` enables for its children now.
    pub subtree_control: Set,
}

impl Standing {
    /// Linux's `cgroup_can_be_thread_root`, for a cgroup that is not
    /// threaded (none is here): the root always, otherwise one with no
    /// populated child and no domain controller enabled. Such a cgroup may
    /// hold processes beside threaded controllers, since it could become the
    /// root of a threaded subtree.
    pub fn can_be_thread_root(self) -> bool {
        self.root || (!self.populated_children && self.subtree_control.domain().is_empty())
    }
}

/// Whether `enable`, the controllers a `cgroup.subtree_control` write turns
/// on that were not on already, may be turned on for a cgroup standing as
/// `standing`: Linux's `cgroup_vet_subtree_control_enable`.
///
/// Controllers may not be enabled for the children of a cgroup that has
/// processes of its own, because the children would then compete with those
/// processes for what the controller divides. The root is exempt, and so are
/// threaded controllers wherever the cgroup could be a threaded root.
///
/// # Errors
///
/// [`Refusal::Busy`] when the rule forbids it.
pub fn vet_enable(enable: Set, standing: Standing) -> Result<(), Refusal> {
    if enable.is_empty() || standing.root {
        return Ok(());
    }
    if enable.domain().is_empty() && standing.can_be_thread_root() {
        return Ok(());
    }
    if standing.has_tasks {
        return Err(Refusal::Busy);
    }
    Ok(())
}

/// Whether a process may be moved into a cgroup standing as `standing`, by
/// `cgroup.procs` or `clone3`'s `CLONE_INTO_CGROUP`: Linux's
/// `cgroup_migrate_vet_dst`. Not into a cgroup that enables a controller for
/// its children, unless it could be a threaded root.
///
/// # Errors
///
/// [`Refusal::Busy`] when the rule forbids it.
pub fn vet_destination(standing: Standing) -> Result<(), Refusal> {
    if standing.can_be_thread_root() || standing.subtree_control.is_empty() {
        return Ok(());
    }
    Err(Refusal::Busy)
}
