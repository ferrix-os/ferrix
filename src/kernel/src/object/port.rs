//! Ports: an event queue a driver thread waits on.
//!
//! `docs/ARCHITECTURE.md` §3: how one driver thread services many sources. A
//! port holds packets. A program queues its own with `port_queue`; the kernel
//! queues one for each `object_wait_async` registration whose signals come
//! true; and `port_wait` takes the next, oldest first.
//!
//! # A registration is one-shot, and held by what it watches
//!
//! An [`Observer`] lives in the object it watches, under the same lock the
//! object takes to change the state it reports. Registering checks the state
//! under that lock and fires at once if a wanted signal is already asserted,
//! so a change can land neither between looking and registering nor between
//! registering and the change: whichever takes the lock second sees the other.
//! It fires once and is gone, which is what lets a driver loop — wait, handle,
//! register again — without packets piling up for a state it has not yet
//! dealt with.
//!
//! # User packets are bounded; signal packets are promised
//!
//! A program that queues without reading is told to wait at
//! [`PORT_CAPACITY`]. A signal packet is queued regardless: refusing it would
//! lose the event its registration was waiting for. So is an interrupt's.
//!
//! Neither may allocate when it is queued, though: a signal is queued by
//! whatever changed an object's state, which has no way to report that memory
//! ran out, and an interrupt's packet is queued by its interrupt handler. So
//! the room is taken earlier, where failing is still an answer (finding F-23):
//! every registration and every interrupt binding *promises* the port one
//! packet, and the promise reserves a slot in the queue when it is made. The
//! queue's capacity is always at least [`PORT_CAPACITY`] plus the promises
//! outstanding, and every packet it can hold is either a user packet, of
//! which there are at most [`PORT_CAPACITY`], or one a promise stands for.
//! A signal packet keeps its registration's promise until it is taken; an
//! interrupt binding keeps its promise for as long as it is bound, since its
//! line queues at most one packet at a time.
//!
//! # The queue's lock is interrupt-safe
//!
//! So that an interrupt handler can queue a packet for an interrupt bound to
//! the port. The handler wakes the port's waiters once it has let go of this
//! lock; see `object::interrupt`.

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{PACKET_INTERRUPT, PACKET_SIGNAL, PACKET_USER, PortPacket};
use ferrix_sync::IrqSpinLock;

use crate::arch;
use crate::fallible::{self, AllocError};
use crate::object::quota::{Charge, Resource};
use crate::sched::WaitQueue;

/// The most user packets a port holds unread.
pub(crate) const PORT_CAPACITY: usize = 1024;

/// The most registrations one object holds at once.
pub(crate) const MAX_OBSERVERS: usize = 64;

/// Why a port or an object refused a packet or a registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortError {
    /// The port is full of user packets, or the object of registrations.
    Full,
    /// The room a registration promises could not be reserved.
    NoMemory,
}

impl From<AllocError> for PortError {
    fn from(_: AllocError) -> PortError {
        PortError::NoMemory
    }
}

/// An event queue.
#[derive(Debug)]
pub(crate) struct Port {
    /// Packets, oldest first, and what they are owed: see the module
    /// documentation.
    queue: IrqSpinLock<Queue, arch::Irq>,
    /// Woken when a packet is queued. Shared, because a `port_fd`
    /// descriptor's `poll` and `epoll_wait` hold on to the queues they sleep
    /// on (`fs::wake`).
    waiters: Arc<WaitQueue>,
    /// The kernel object it is, charged to the job that made it
    /// (`object::quota`).
    #[expect(
        dead_code,
        reason = "AUDIT: held for its drop, which uncharges the job"
    )]
    charge: Charge,
}

/// A port's packets and the room reserved for them.
#[derive(Debug)]
struct Queue {
    /// The packets. Its capacity is at least [`PORT_CAPACITY`] plus
    /// `promised`, so a packet either kind of limit allows never allocates.
    packets: VecDeque<PortPacket>,
    /// User packets queued.
    users: usize,
    /// Registrations and bindings that may yet queue a packet, and signal
    /// packets queued and not yet taken.
    promised: usize,
    /// Packets that found no room, which the accounting says cannot happen.
    lost: u64,
}

impl Queue {
    /// Queue `packet` at the back or the front if the capacity holds it,
    /// which cannot allocate.
    fn push_fitting(&mut self, packet: PortPacket, front: bool) -> bool {
        if self.packets.len() >= self.packets.capacity() {
            self.lost = self.lost.saturating_add(1);
            return false;
        }
        if front {
            // NOALLOC: below the capacity, checked above.
            self.packets.push_front(packet);
        } else {
            // NOALLOC: below the capacity, checked above.
            self.packets.push_back(packet);
        }
        true
    }
}

impl Port {
    /// An empty port.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the port or its queue could not be allocated.
    pub(crate) fn new() -> Result<Arc<Port>, AllocError> {
        let charge = Charge::running(Resource::Objects, 1).map_err(|_| AllocError)?;
        let packets = fallible::try_deque_with_capacity(PORT_CAPACITY)?;
        let waiters = fallible::try_arc(WaitQueue::new())?;
        fallible::try_arc(Port {
            queue: IrqSpinLock::new(Queue {
                packets,
                users: 0,
                promised: 0,
                lost: 0,
            }),
            waiters,
            charge,
        })
    }

    /// Queue a program's own packet.
    ///
    /// # Errors
    ///
    /// [`PortError::Full`] at [`PORT_CAPACITY`].
    pub(crate) fn queue_user(&self, key: u64, data: [u64; 2]) -> Result<(), PortError> {
        {
            let mut queue = self.queue.lock();
            if queue.users >= PORT_CAPACITY {
                return Err(PortError::Full);
            }
            let packet = PortPacket {
                key,
                kind: PACKET_USER,
                signals: 0,
                data,
            };
            if !queue.push_fitting(packet, false) {
                return Err(PortError::Full);
            }
            queue.users += 1;
        }
        self.waiters.wake_all();
        Ok(())
    }

    /// Reserve room for one more packet a registration or a binding may
    /// queue, and count the promise.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the room could not be reserved.
    fn promise(&self) -> Result<(), AllocError> {
        let mut queue = self.queue.lock();
        let wanted = PORT_CAPACITY
            .saturating_add(queue.promised)
            .saturating_add(1);
        let additional = wanted.saturating_sub(queue.packets.len());
        fallible::try_reserve_deque(&mut queue.packets, additional)?;
        queue.promised += 1;
        Ok(())
    }

    /// A promise that will not be kept: its registration was dropped, or its
    /// binding undone.
    fn release_promise(&self) {
        let mut queue = self.queue.lock();
        queue.promised = queue.promised.saturating_sub(1);
    }

    /// Queue the packet a registration produces, on the room it promised.
    fn queue_signal(&self, key: u64, asserted: Signals) {
        let _ = self.queue.lock().push_fitting(
            PortPacket {
                key,
                kind: PACKET_SIGNAL,
                signals: asserted.0,
                data: [0; 2],
            },
            false,
        );
        self.waiters.wake_all();
    }

    /// Queue a bound interrupt's packet, from its interrupt handler.
    ///
    /// Touches only this queue's interrupt-safe lock and allocates nothing:
    /// its binding promised the room. Wakes no waiter: the handler does that
    /// once it has let go of this lock and the interrupt's own. Returns
    /// whether the packet was queued.
    pub(crate) fn queue_from_interrupt(&self, key: u64, fired_at: u64) -> bool {
        self.queue.lock().push_fitting(
            PortPacket {
                key,
                kind: PACKET_INTERRUPT,
                signals: 0,
                data: [fired_at, 0],
            },
            false,
        )
    }

    /// Queue a bound interrupt's packet from task context, for an interrupt
    /// that was already pending when it was bound.
    pub(crate) fn queue_interrupt(&self, key: u64, fired_at: u64) {
        let _ = self.queue_from_interrupt(key, fired_at);
        self.waiters.wake_all();
    }

    /// Take the oldest packet.
    pub(crate) fn take(&self) -> Option<PortPacket> {
        let mut queue = self.queue.lock();
        let packet = queue.packets.pop_front()?;
        match packet.kind {
            PACKET_USER => queue.users = queue.users.saturating_sub(1),
            PACKET_SIGNAL => queue.promised = queue.promised.saturating_sub(1),
            _ => {}
        }
        Some(packet)
    }

    /// Put back a packet [`Port::take`] gave and the caller could not deliver,
    /// ahead of everything else.
    ///
    /// Its slot may have been taken meanwhile by a user packet, so this grows
    /// the queue if it has to. If that fails the packet is lost -- and the
    /// call putting it back was failing anyway, with the fault that stopped
    /// it being delivered.
    pub(crate) fn put_back(&self, packet: PortPacket) {
        {
            let mut queue = self.queue.lock();
            let kind = packet.kind;
            if queue.packets.len() >= queue.packets.capacity()
                && fallible::try_reserve_deque(&mut queue.packets, 1).is_err()
            {
                queue.lost = queue.lost.saturating_add(1);
                return;
            }
            if queue.push_fitting(packet, true) {
                match kind {
                    PACKET_USER => queue.users += 1,
                    PACKET_SIGNAL => queue.promised += 1,
                    _ => {}
                }
            }
        }
        self.waiters.wake_all();
    }

    /// Whether nothing is queued.
    pub(crate) fn is_empty(&self) -> bool {
        self.queue.lock().packets.is_empty()
    }

    /// Packets that found no room since the port was made: zero unless the
    /// promise accounting is wrong, which a boot check asks.
    pub(crate) fn lost(&self) -> u64 {
        self.queue.lock().lost
    }

    /// The queue woken when a packet arrives.
    pub(crate) fn waiters(&self) -> &WaitQueue {
        &self.waiters
    }

    /// [`Port::waiters`], as a reference a wait can keep: what a `port_fd`
    /// descriptor offers `poll` and `epoll_wait` to sleep on.
    pub(crate) fn shared_waiters(&self) -> &Arc<WaitQueue> {
        &self.waiters
    }
}

/// A port's promise of room for one packet, kept for as long as this is.
///
/// What an interrupt binding holds. Weak, so it does not keep a port nobody
/// holds alive; a port that has gone owes nothing.
#[derive(Debug)]
pub(crate) struct Promise {
    /// The port that promised.
    port: Weak<Port>,
}

impl Promise {
    /// Promise room on `port`.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the room could not be reserved.
    pub(crate) fn new(port: &Arc<Port>) -> Result<Promise, AllocError> {
        port.promise()?;
        Ok(Promise {
            port: Arc::downgrade(port),
        })
    }

    /// Whether anyone still holds the port.
    pub(crate) fn is_live(&self) -> bool {
        self.port.strong_count() > 0
    }

    /// The port, if anyone still holds it.
    pub(crate) fn port(&self) -> Option<Arc<Port>> {
        self.port.upgrade()
    }
}

impl Drop for Promise {
    fn drop(&mut self) {
        if let Some(port) = self.port.upgrade() {
            port.release_promise();
        }
    }
}

/// A one-shot request for a packet when signals come true.
///
/// It holds its port's promise of room for the packet, so firing it never
/// allocates; dropped unfired, it gives the promise back.
#[derive(Debug)]
pub(crate) struct Observer {
    /// Where the packet goes, and the room promised there.
    promise: Promise,
    /// The key the packet carries.
    key: u64,
    /// The signals it waits for; any of them fires it.
    signals: Signals,
}

impl Observer {
    /// A registration for a packet on `port` with `key` when any of `signals`
    /// is asserted.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when `port` could not reserve room for the packet.
    pub(crate) fn new(
        port: &Arc<Port>,
        key: u64,
        signals: Signals,
    ) -> Result<Observer, AllocError> {
        Ok(Observer {
            promise: Promise::new(port)?,
            key,
            signals,
        })
    }

    /// Whether `asserted` fires it.
    pub(crate) fn wants(&self, asserted: Signals) -> bool {
        self.signals.intersects(asserted)
    }

    /// Whether anyone still holds its port.
    fn is_live(&self) -> bool {
        self.promise.is_live()
    }

    /// Queue its packet, reporting the wanted signals among `asserted`. Call
    /// with no lock held that the port's wake-up might need.
    ///
    /// The packet is queued on the room the registration promised, and the
    /// promise goes with it: it is released when the packet is taken, not
    /// when this is dropped.
    pub(crate) fn fire(self, asserted: Signals) {
        let Observer {
            mut promise,
            key,
            signals,
        } = self;
        let port = core::mem::take(&mut promise.port);
        drop(promise);
        if let Some(port) = port.upgrade() {
            port.queue_signal(key, asserted.intersection(signals));
        }
    }
}

/// The registrations waiting on one object, and those it has fired and not
/// yet delivered.
///
/// Firing is two steps, because a packet must be queued with no lock held
/// that the port's wake-up might need: under the object's lock, [`trigger`]
/// moves every registration the change fires onto `firing`; with the lock let
/// go, the caller takes them off one at a time ([`Observers::next_fired`])
/// and fires each. `firing` has room for every registration the object
/// holds, reserved as each is made, so the first step never allocates.
#[derive(Debug, Default)]
pub(crate) struct Observers {
    /// Waiting.
    listed: Vec<Observer>,
    /// Fired by a change, waiting to be delivered, with what the change
    /// asserted.
    firing: Vec<(Observer, Signals)>,
}

impl Observers {
    /// No registrations.
    pub(crate) const fn new() -> Observers {
        Observers {
            listed: Vec::new(),
            firing: Vec::new(),
        }
    }

    /// Take one fired registration to deliver.
    pub(crate) fn next_fired(&mut self) -> Option<(Observer, Signals)> {
        self.firing.pop()
    }

    /// Every registration still waiting, taken out: what a closing object
    /// fires at once.
    pub(crate) fn take_listed(&mut self) -> Vec<Observer> {
        core::mem::take(&mut self.listed)
    }
}

/// Add `observer` to an object's registrations.
///
/// # Errors
///
/// [`PortError::Full`] at [`MAX_OBSERVERS`], counted after dropping any whose
/// port has gone; [`PortError::NoMemory`] when there is no room to list it or
/// to fire it.
pub(crate) fn register(list: &mut Observers, observer: Observer) -> Result<(), PortError> {
    list.listed.retain(Observer::is_live);
    if list.listed.len() >= MAX_OBSERVERS {
        return Err(PortError::Full);
    }
    fallible::try_reserve(&mut list.listed, 1)?;
    // Room for every listed registration to fire at once, on top of those
    // already fired and not yet delivered.
    fallible::try_reserve(&mut list.firing, list.listed.len().saturating_add(1))?;
    fallible::push_within(&mut list.listed, observer).map_err(|_| PortError::NoMemory)
}

/// Move every registration in `list` that `asserted` fires onto its `firing`
/// list, for the caller to deliver with [`Observers::next_fired`] once it has
/// let go of the object's lock. Returns whether any fired.
pub(crate) fn trigger(list: &mut Observers, asserted: Signals) -> bool {
    let mut any = false;
    let mut index = 0;
    while let Some(observer) = list.listed.get(index) {
        if !observer.wants(asserted) {
            index += 1;
            continue;
        }
        let observer = list.listed.remove(index);
        match fallible::push_within(&mut list.firing, (observer, asserted)) {
            Ok(()) => any = true,
            // Room was reserved for it as it was listed, so this is not
            // reached. If it were, the registration stays and fires on a
            // later change rather than the machine allocating here.
            Err((observer, _)) => {
                // NOALLOC: one was removed from this index just above.
                list.listed.insert(index, observer);
                index += 1;
            }
        }
    }
    any
}

/// Deliver what `take` hands out, one registration at a time, each with no
/// lock held: `take` is expected to lock the object, call
/// [`Observers::next_fired`], and let go.
pub(crate) fn deliver(mut take: impl FnMut() -> Option<(Observer, Signals)>) {
    while let Some((observer, asserted)) = take() {
        observer.fire(asserted);
    }
}
