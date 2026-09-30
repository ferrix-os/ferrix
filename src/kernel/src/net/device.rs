//! Where a device a driver serves is (`docs/NETNS.md` section 3.2).
//!
//! A ring-3 driver serves a device, not an index: an interface's index is its
//! stack's to number, and changes when the interface moves to another network
//! namespace. The driver's ring holds a **key**, made here when the interface
//! is first added and kept for the life of the device, and every call the ring
//! makes into the net core comes through this module, which says which
//! namespace's stack the interface is in now and under which index.
//!
//! With the device in the first namespace, which is where every interface a
//! driver adds starts and where it returns to, each call is one look in a
//! vector of the devices there are (one or two).
//!
//! # Locking
//!
//! The registry is a leaf: a call looks the device up, releases the registry,
//! and only then enters the namespace's stack.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_net::iface::Interface;
use ferrix_net::{Stack, iface::Backing};

use super::NetNamespace;
use crate::object::port::Port;
use crate::sync::SpinLock;

/// One device.
#[derive(Debug)]
struct Device {
    /// What the driver's ring holds.
    key: u32,
    /// The namespace its interface is in.
    namespace: Weak<NetNamespace>,
    /// Its index there.
    index: u32,
}

/// Every device a driver serves or served.
static DEVICES: SpinLock<Vec<Device>> = SpinLock::new(Vec::new());

/// The next key. Never reused.
static NEXT_KEY: AtomicU32 = AtomicU32::new(1);

/// Register the interface `index` of `namespace` as a device, and answer its
/// key. The caller puts `Backing::Device(key)` on the interface before it
/// adds it, by asking [`new_key`] first.
pub(crate) fn new_key() -> u32 {
    NEXT_KEY.fetch_add(1, Ordering::Relaxed)
}

/// Record where the device `key` is.
pub(crate) fn place(key: u32, namespace: &Arc<NetNamespace>, index: u32) {
    let mut devices = DEVICES.lock();
    if let Some(device) = devices.iter_mut().find(|device| device.key == key) {
        device.namespace = Arc::downgrade(namespace);
        device.index = index;
        return;
    }
    devices.push(Device {
        key,
        namespace: Arc::downgrade(namespace),
        index,
    });
}

/// The namespace and index of the device `key`.
pub(crate) fn find(key: u32) -> Option<(Arc<NetNamespace>, u32)> {
    let devices = DEVICES.lock();
    let device = devices.iter().find(|device| device.key == key)?;
    Some((device.namespace.upgrade()?, device.index))
}

/// Forget the device `key`: its interface is gone for good.
pub(crate) fn forget(key: u32) {
    DEVICES.lock().retain(|device| device.key != key);
}

/// Run `body` on the stack the device `key` is in, with its index there;
/// `None` if the device is nowhere (its namespace is ending).
pub(crate) fn look<T>(key: u32, body: impl FnOnce(&Stack, u32) -> T) -> Option<T> {
    let (namespace, index) = find(key)?;
    Some(namespace.core().look(|stack| body(stack, index)))
}

/// Take the frames waiting for the device's driver.
pub(crate) fn take_outgoing(key: u32, want: usize) -> Vec<Vec<u8>> {
    find(key).map_or_else(Vec::new, |(namespace, index)| {
        namespace.core().take_outgoing(index, want)
    })
}

/// Hand the stack a frame the device received.
pub(crate) fn receive(key: u32, frame: &[u8]) {
    if let Some((namespace, index)) = find(key) {
        namespace.core().receive(index, frame);
    }
}

/// Say whether the device's link is up.
pub(crate) fn set_carrier(key: u32, up: bool) {
    if let Some((namespace, index)) = find(key) {
        namespace.core().set_carrier(index, up);
    }
}

/// Keep the device's interface for the next driver (`NetCore::park_interface`).
pub(crate) fn park(key: u32) {
    if let Some((namespace, index)) = find(key) {
        namespace.core().park_interface(index);
    }
}

/// Take the device's interface out of its stack and forget the device.
pub(crate) fn remove(key: u32) {
    if let Some((namespace, index)) = find(key) {
        namespace.core().forget_interface(index);
    }
    forget(key);
}

/// Have `port` told when frames are queued for the device.
pub(crate) fn wake_on_transmit(key: u32, port: &Arc<Port>) {
    if let Some((namespace, index)) = find(key) {
        namespace.core().wake_on_transmit(index, port);
    }
}

/// A device's interface comes home to the first namespace because the one it
/// was in has ended: flushed and down, as Linux's `default_device_exit`
/// leaves it, under its own name if the first namespace has none like it and
/// `devN` if it has. `waker` is the port its ring sleeps on, taken from the
/// namespace that ended.
pub(crate) fn come_home(key: u32, mut interface: Interface, waker: Option<Arc<Port>>) {
    let first = super::first();
    flush(&mut interface);
    let name = interface.name.as_bytes().to_vec();
    let free = first
        .core()
        .look(|stack| stack.interface_by_name(&name).is_none());
    if !free {
        let mut n = 0_u32;
        loop {
            let candidate = alloc::format!("dev{n}").into_bytes();
            if first
                .core()
                .look(|stack| stack.interface_by_name(&candidate).is_none())
            {
                interface.name = ferrix_net::iface::Name::new(&candidate);
                break;
            }
            n += 1;
        }
    }
    let Ok(index) = first
        .core()
        .with(|stack, _| stack.attach_interface(interface))
    else {
        forget(key);
        return;
    };
    place(key, first, index);
    if let Some(port) = waker {
        first.core().wake_on_transmit(index, &port);
    }
}

/// What a move does to an interface, as Linux's `dev_change_net_namespace`
/// does: it comes down and loses its addresses, and what the driver said of
/// the link stays.
pub(crate) fn flush(interface: &mut Interface) {
    use ferrix_net::iface::{IFF_LOWER_UP, IFF_RUNNING, IFF_UP};
    interface.addresses.clear();
    interface.flags &= !(IFF_UP | IFF_RUNNING);
    if !matches!(interface.backing, Backing::Device(_)) {
        interface.flags &= !IFF_LOWER_UP;
    }
}
