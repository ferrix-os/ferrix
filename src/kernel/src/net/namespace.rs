//! A network namespace (`docs/NETNS.md`): one [`NetCore`] of its own, an owner,
//! and a charge.
//!
//! # What it is
//!
//! `src/lib/network/net`'s `Stack` already holds everything a namespace owns --
//! interfaces, addresses, routes, neighbours, ports, sockets -- so a namespace
//! is a `Stack` with the kernel's lock, wait queue and frame queues around it,
//! which is what [`NetCore`] is. The first namespace is the one every driver
//! and boot check has always used; [`create`] makes the others.
//!
//! # Who holds one
//!
//! A process holds the one it is in; a fork child copies it. A socket holds
//! the one it was made in, for life. A namespace ends with its last holder and
//! then returns what it holds to the outside: a physical interface to the
//! first namespace, and the ends of virtual pairs are destroyed ([`Drop`]).
//!
//! # What it costs
//!
//! Creating one is charged to the job of the caller. So is what its tables
//! grow to: [`NetNamespace::fit`] makes the `tables` charge what
//! [`Stack::footprint`] says, before a change that adds to them is applied and
//! after any change, so a program that owns a namespace cannot hold more
//! kernel heap in it than its job may (certification finding F-37).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_net::Stack;
use ferrix_net::iface::Backing;
use ferrix_net::stack::Config;
use ferrix_sync::Once;

use super::NetCore;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::userns::{self, UserNamespace};

/// `CLONE_NEWNET`.
pub(crate) const CLONE_NEWNET: u64 = 0x4000_0000;

/// What `/proc/<pid>/ns/net` names for the first namespace: the number a
/// Linux host usually shows for its own.
pub(crate) const FIRST_ID: u64 = 0xF000_0098;

/// The number of the next namespace. A counter of its own, so that no two
/// kinds of namespace share a number.
static NEXT_ID: AtomicU64 = AtomicU64::new(0xFA00_0000);

/// Heap a namespace takes that its `Arc` does not show: the loopback's
/// vectors, the maps' first nodes and the wait queue's state.
const BASE: usize = 512;

/// The most interfaces one namespace holds.
pub(crate) const MAX_INTERFACES: usize = 64;
/// The most addresses, over all its interfaces.
pub(crate) const MAX_ADDRESSES: usize = 256;
/// The most routes.
pub(crate) const MAX_ROUTES: usize = 1024;

/// What one change may add to the tables, which [`NetNamespace::fit`] makes
/// room for before the change is applied: two interfaces and their headroom.
const HEADROOM: usize = 1024;

/// A network namespace.
#[derive(Debug)]
pub(crate) struct NetNamespace {
    /// What `/proc/<pid>/ns/net` names.
    id: u64,
    /// The user namespace that was current when it was made: a process with a
    /// capability over it may change this one.
    owner: Arc<UserNamespace>,
    /// The stack and what the kernel keeps around it.
    core: NetCore,
    /// What its tables are charged, to the job that made it; `None` for the
    /// first namespace.
    tables: Option<SpinLock<Charge>>,
    /// The heap this is, charged to the job that made it.
    _charge: Option<Charge>,
}

/// The namespaces that exist, so that the driving task can tick each. Weak, so
/// that the list never keeps one alive. The first is not in it.
static LIVE: SpinLock<Vec<Weak<NetNamespace>>> = SpinLock::new(Vec::new());

/// The first namespace, made on first use.
pub(crate) fn first() -> &'static Arc<NetNamespace> {
    static FIRST: Once<Arc<NetNamespace>> = Once::new();
    FIRST.call_once(|| {
        Arc::new(NetNamespace {
            id: FIRST_ID,
            owner: Arc::clone(userns::first()),
            core: NetCore::new(Stack::new(Config::default()), Arc::new(WaitQueue::new())),
            tables: None,
            _charge: None,
        })
    })
}

/// The namespace of the process making the call: the running task's, or the
/// one a boot check is acting as, or the first for the kernel's own.
pub(crate) fn acting() -> Arc<NetNamespace> {
    userns::acting().map_or_else(|| Arc::clone(first()), |process| process.net_ns())
}

/// Make a namespace owned by `owner`, charged to the running task's job.
///
/// # Errors
///
/// `ENOMEM` past the job's memory.
pub(crate) fn create(owner: Arc<UserNamespace>) -> Result<Arc<NetNamespace>, Errno> {
    let charge = Charge::bytes(arc_footprint::<NetNamespace>().saturating_add(BASE))
        .map_err(|_| Errno::ENOMEM)?;
    let tables = Charge::bytes(0).map_err(|_| Errno::ENOMEM)?;
    let progress = crate::fallible::try_arc(WaitQueue::new()).map_err(|_| Errno::ENOMEM)?;
    let core = NetCore::new(Stack::new_namespace(Config::default()), progress);
    let namespace = crate::fallible::try_arc(NetNamespace {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        owner,
        core,
        tables: Some(SpinLock::new(tables)),
        _charge: Some(charge),
    })
    .map_err(|_| Errno::ENOMEM)?;
    namespace.fit(0)?;
    let mut live = LIVE.lock();
    if live.len().is_power_of_two() {
        live.retain(|each| each.strong_count() > 0);
    }
    live.try_reserve(1).map_err(|_| Errno::ENOMEM)?;
    live.push(Arc::downgrade(&namespace));
    drop(live);
    Ok(namespace)
}

/// Collect every live namespace, the first included, into `out`.
pub(crate) fn live(out: &mut Vec<Arc<NetNamespace>>) {
    out.push(Arc::clone(first()));
    out.extend(LIVE.lock().iter().filter_map(Weak::upgrade));
}

impl NetNamespace {
    /// Its stack's kernel half.
    pub(crate) const fn core(&self) -> &NetCore {
        &self.core
    }

    /// What `/proc/<pid>/ns/net` names.
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// The user namespace that owns it.
    pub(crate) const fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// Whether `self` and `other` are one namespace.
    pub(crate) fn same(&self, other: &NetNamespace) -> bool {
        core::ptr::eq(self, other)
    }

    /// Make what the tables are charged for what they hold now, plus room for
    /// `extra` more.
    ///
    /// A growth past the job's memory first gives up what the namespace only
    /// learned (neighbours and fragments) and tries again; what is configured
    /// stays, and the refusal is `ENOMEM`. The first namespace is charged
    /// nothing, as before.
    ///
    /// # Errors
    ///
    /// `ENOMEM`.
    pub(crate) fn fit(&self, extra: usize) -> Result<(), Errno> {
        let Some(tables) = &self.tables else {
            return Ok(());
        };
        let wanted = self.core.look(Stack::footprint).saturating_add(extra);
        if tables.lock().resize(wanted).is_ok() {
            return Ok(());
        }
        self.core.with(|stack, _| stack.shed());
        let wanted = self.core.look(Stack::footprint).saturating_add(extra);
        tables.lock().resize(wanted).map_err(|_| Errno::ENOMEM)
    }

    /// Room for one change that adds to the tables, asked before it is
    /// applied. Follow it with `fit(0)` to settle on what is held.
    ///
    /// # Errors
    ///
    /// `ENOMEM`.
    pub(crate) fn admit(&self) -> Result<(), Errno> {
        self.fit(HEADROOM)
    }

    /// Bring the carrier of every virtual pair with an end here up to date:
    /// called after a change that may have brought one up or down.
    pub(crate) fn refresh_carriers(&self) {
        let pairs: Vec<u64> = self.core.look(|stack| {
            stack
                .interfaces()
                .iter()
                .filter_map(|each| super::veth::end_of(each.backing).map(|(pair, _)| pair))
                .collect()
        });
        for pair in pairs {
            super::veth::refresh(pair);
        }
    }

    /// Whether the tables have room for one more of a kind under the
    /// namespace's ceilings (`docs/NETNS.md` section 7).
    ///
    /// # Errors
    ///
    /// `ENOSPC`.
    pub(crate) fn roomy(&self, interfaces: usize) -> Result<(), Errno> {
        let full = self
            .core
            .look(|stack| stack.interfaces().len().saturating_add(interfaces) > MAX_INTERFACES);
        if full {
            return Err(Errno::ENOSPC);
        }
        Ok(())
    }
}

impl Drop for NetNamespace {
    /// The end of a namespace (`docs/NETNS.md` section 2.5): its physical
    /// interfaces go back to the first namespace and the ends of its virtual
    /// pairs are destroyed, each with its peer. Nothing else can reach this
    /// stack, so taking its lock contends with nobody.
    fn drop(&mut self) {
        let interfaces: Vec<_> = self.core.with(|stack, _| {
            let indexes: Vec<u32> = stack
                .interfaces()
                .iter()
                .filter(|each| each.backing != Backing::Software)
                .map(|each| each.index)
                .collect();
            indexes
                .into_iter()
                .filter_map(|index| stack.detach_interface(index))
                .collect()
        });
        for interface in interfaces {
            match interface.backing {
                Backing::Veth { pair, .. } => super::veth::destroy(pair),
                Backing::Device(key) => {
                    let waker = self.core.take_waker(interface.index);
                    super::device::come_home(key, interface, waker);
                }
                Backing::Software => {}
            }
        }
    }
}

/// Move the interface `index` of `from` into `to`, and answer its index there.
/// It comes down and loses its addresses, as Linux's does; its name must be
/// free in `to`. A driver's device keeps its key and its ring keeps serving it.
///
/// Both namespaces' locks are taken one after the other, never together; the
/// interface is in neither for the interval, and frames for it are dropped.
///
/// # Errors
///
/// `ENODEV` for an index that is not there, `EINVAL` for the loopback,
/// `EEXIST` for a name `to` has, `ENOSPC` and `ENOMEM` past `to`'s ceilings.
pub(crate) fn transfer(
    from: &Arc<NetNamespace>,
    index: u32,
    to: &Arc<NetNamespace>,
    rename: Option<&[u8]>,
) -> Result<u32, Errno> {
    if from.same(to) {
        return Ok(index);
    }
    let (name, loopback) = from
        .core
        .look(|stack| {
            stack
                .interface(index)
                .map(|each| (each.name, each.medium == ferrix_net::Medium::Loopback))
        })
        .ok_or(Errno::ENODEV)?;
    if loopback {
        return Err(Errno::EINVAL);
    }
    let name = rename.map_or(name, ferrix_net::iface::Name::new);
    to.roomy(1)?;
    to.admit()?;
    if to
        .core
        .look(|stack| stack.interface_by_name(name.as_bytes()).is_some())
    {
        return Err(Errno::EEXIST);
    }
    let mut interface = from
        .core
        .with(|stack, _| stack.detach_interface(index))
        .ok_or(Errno::ENODEV)?;
    let waker = from.core.take_waker(index);
    super::device::flush(&mut interface);
    let backing = interface.backing;
    let kept = interface.clone();
    interface.name = name;
    let placed = to.core.with(|stack, _| stack.attach_interface(interface));
    let Ok(new_index) = placed else {
        // Raced by a name taken meanwhile: put it back where it was.
        let back = from
            .core
            .with(|stack, _| stack.attach_interface(kept))
            .unwrap_or(index);
        settle_move(from, backing, back, waker);
        let _ = (from.fit(0), to.fit(0));
        return Err(Errno::EEXIST);
    };
    settle_move(to, backing, new_index, waker);
    let _ = (from.fit(0), to.fit(0));
    Ok(new_index)
}

/// Tell the registries and the ring where an interface went.
fn settle_move(
    now_in: &Arc<NetNamespace>,
    backing: Backing,
    index: u32,
    waker: Option<Arc<crate::object::port::Port>>,
) {
    match backing {
        Backing::Veth { pair, end } => {
            super::veth::moved(pair, end, now_in, index);
            super::veth::refresh(pair);
        }
        Backing::Device(key) => {
            super::device::place(key, now_in, index);
            if let Some(port) = waker {
                now_in.core.wake_on_transmit(index, &port);
            }
        }
        Backing::Software => {}
    }
}
