//! Kernel objects across to the load's local ones (docs/microkernel/M5-LIFT.md
//! §11). The load's device cores keep the objects they have in the hybrid kernel;
//! a driver holds kernel objects. At the edge a thread per direction carries what
//! crosses. A hop on the control path alone; a ring's data stays where it is.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_native::call::Syscall as _;
use ferrix_native::channel::{self, Channel};
use ferrix_native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_native::port::{self, Port as KernelPort};
use ferrix_native::vmo::Vmo as KernelVmo;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_rt::Kernel;

use crate::object::channel::{Endpoint, Sender};
use crate::object::port::Port;
use crate::object::{Object, Transfer};
use crate::sched;
use crate::user::vmo::Vmo;

const PAGE: usize = 4096;
/// How often an idle bridge asks whether its core has let go.
const LOOK_NANOS: u64 = 100_000_000;

fn owned<T>(make: impl FnOnce(OwnedHandle<Kernel>) -> T, handle: Handle) -> T {
    make(OwnedHandle::from_raw(Kernel, handle))
}

/// What a kernel handle is, as a local object. A kernel VMO comes to be the
/// frames of a local one above the arena; a port and a channel get a bridge.
pub fn import(handle: Handle) -> Option<Transfer> {
    use ferrix_native_abi::lift::{KIND_CHANNEL, KIND_PORT, KIND_VMO};
    let (kind, rights) = ferrix_native::lift_calls::object_kind(Kernel, handle).ok()?;
    let rights = Rights(rights);
    match kind {
        KIND_VMO => {
            let vmo = core::mem::ManuallyDrop::new(owned(KernelVmo::from_owned, handle));
            let bytes = vmo.size().ok()?;
            let pages = (bytes as usize).div_ceil(PAGE) as u64;
            let (first, at) = crate::mm::foreign_window(pages)?;
            let mapped = vmo.map(
                Some(at as usize),
                pages as usize * PAGE,
                ferrix_native::pending::Protection::ReadWrite,
                0,
            );
            if mapped.is_err() {
                crate::println!("linuxd: a driver's VMO could not be mapped");
                return None;
            }
            let local = Vmo::new_anonymous(pages).ok()?;
            for index in 0..pages {
                let _ = local.insert_absent(index, first + index);
            }
            crate::mm::foreign_bind(first, pages, handle.0);
            Some((Object::Vmo(local), rights))
        }
        KIND_PORT => {
            let local = Port::new().ok()?;
            spawn_out(Arc::clone(&local), handle);
            Some((Object::Port(local), rights))
        }
        KIND_CHANNEL => {
            let (local, ours) = Endpoint::pair().ok()?;
            bridge(ours, handle);
            Some((Object::Channel(local), rights))
        }
        _ => {
            crate::println!("linuxd: a driver handed a core an object it cannot take");
            None
        }
    }
}

/// A local object as a kernel handle, for a driver.
pub fn export(object: Object, rights: Rights) -> Option<Handle> {
    match object {
        Object::Port(local) => {
            let kernel = port::create(Kernel).ok()?;
            // A message moves the handle it carries: the driver gets its own,
            // with the rights the core gave, and this one stays to wait on.
            // TRANSFER is what lets this process's write carry it at all.
            let given = kernel
                .as_owned()
                .duplicate(Requested::Exactly(Rights(rights.0 | Rights::TRANSFER.0)))
                .ok()?;
            let handle = kernel.handle();
            core::mem::forget(kernel);
            spawn_in(local, handle);
            let given_handle = given.raw();
            core::mem::forget(given);
            Some(given_handle)
        }
        Object::Channel(local) => {
            let (near, far) = channel::create(Kernel).ok()?;
            let handle = far.handle();
            core::mem::forget(far);
            let near_handle = near.handle();
            core::mem::forget(near);
            bridge(local, near_handle);
            Some(handle)
        }
        Object::Vmo(local) => export_vmo(&local, rights),
        // A driver's kernel holds these itself; each core offers them and none needs
        // them here. A line would land in the middle of a program's output.
        Object::Audit | Object::MemoryScrub => None,
        other => {
            crate::println!("linuxd: a core handed a driver an object it cannot yet: {other:?}");
            None
        }
    }
}

/// A buffer a core made, for a driver: the kernel VMO's pages become its frames,
/// so a program's mapping of it and the driver's are the same memory. The window
/// stays mapped for linuxd's life (M5-LIFT.md §11).
fn export_vmo(local: &Arc<Vmo>, rights: Rights) -> Option<Handle> {
    let pages = local.len_pages();
    let kernel = ferrix_native::vmo::create(Kernel, pages as usize * PAGE).ok()?;
    let (first, at) = crate::mm::foreign_window(pages)?;
    let _ = kernel
        .map(
            Some(at as usize),
            pages as usize * PAGE,
            ferrix_native::pending::Protection::ReadWrite,
            0,
        )
        .ok()?;
    for index in 0..pages {
        if !local.insert_absent(index, first + index) {
            crate::println!(
                "linuxd: a core's buffer already had pages: its driver sees none of them"
            );
            return None;
        }
    }
    let given = kernel
        .as_owned()
        .duplicate(Requested::Exactly(Rights(rights.0 | Rights::TRANSFER.0)))
        .ok()?;
    let near = kernel.handle();
    core::mem::forget(kernel);
    crate::mm::foreign_bind(first, pages, near.0);
    let handle = given.raw();
    core::mem::forget(given);
    Some(handle)
}

/// A local end and a kernel end, the same channel from here on.
pub fn bridge(local: Arc<Endpoint>, kernel: Handle) {
    let gone = Arc::new(AtomicBool::new(false));
    spawn_with(
        "bridge in",
        (Arc::clone(&local), kernel, Arc::clone(&gone)),
        |(local, kernel, gone)| {
            carry_in(&local, kernel, &gone);
            // Either end is gone: the carrier out ends too, and with both the local
            // end closes, which is what the core waits to see. This thread alone
            // closes the kernel end, since a wait on a handle keeps its channel open.
            gone.store(true, Ordering::SeqCst);
            local.waiters().wake_all();
            crate::sys::handle_close(kernel.0);
        },
    );
    spawn_with(
        "bridge out",
        (local, kernel, gone),
        |(local, kernel, gone)| carry_out(&local, kernel, &gone),
    );
}

fn spawn_with<T: Send + 'static>(name: &'static str, value: T, body: fn(T)) {
    struct Start<T> {
        value: T,
        body: fn(T),
    }
    fn entry<T>(argument: usize) {
        // SAFETY: `spawn_with` leaked it for this thread alone.
        let start = unsafe { Box::from_raw(argument as *mut Start<T>) };
        (start.body)(start.value);
    }
    let start = Box::into_raw(Box::new(Start { value, body }));
    if sched::spawn(name, entry::<T>, start as usize, 0).is_err() {
        crate::println!("linuxd: no thread for a bridge");
    }
}

/// From the driver to the core.
fn carry_in(local: &Endpoint, kernel: Handle, gone: &AtomicBool) {
    let channel = core::mem::ManuallyDrop::new(owned(Channel::from_owned, kernel));
    let mut bytes: Vec<u8> = alloc::vec![0; CHANNEL_MAX_BYTES];
    let mut handles = [Handle::INVALID; CHANNEL_MAX_HANDLES];
    loop {
        if gone.load(Ordering::SeqCst) {
            return;
        }
        // Bounded: the core's letting go has to be noticed here.
        let until = crate::timer::now_nanos().saturating_add(LOOK_NANOS);
        match channel.wait_one(
            Signals::READABLE | Signals::PEER_CLOSED,
            Deadline::At(until),
        ) {
            Ok(_) => {}
            Err(ferrix_native::error::Error::TimedOut) => continue,
            Err(_) => return,
        }
        let Ok(got) = channel.read(&mut bytes, &mut handles) else {
            return;
        };
        let mut objects: Vec<Transfer> = Vec::new();
        for handle in handles.iter().take(got.handles) {
            objects.extend(import(*handle));
        }
        let message = bytes.get(..got.bytes).unwrap_or(&[]).to_vec();
        let count = objects.len();
        if let Err(why) = local.write(Sender::KERNEL, message, count, || {
            Ok::<_, core::convert::Infallible>(objects)
        }) {
            crate::println!("  bridge   a write to the core failed: {why:?}");
            return;
        }
    }
}

/// From the core to the driver.
fn carry_out(local: &Endpoint, kernel: Handle, gone: &AtomicBool) {
    let channel = core::mem::ManuallyDrop::new(owned(Channel::from_owned, kernel));
    loop {
        let _ = local
            .waiters()
            .wait_trusting(|| gone.load(Ordering::SeqCst) || local.readable_or_closed());
        if gone.load(Ordering::SeqCst) {
            return;
        }
        let message = match local.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => message,
            Err(crate::object::channel::ReadError::Empty) => continue,
            Err(_) => {
                // The core let go of it: the driver sees PEER_CLOSED.
                gone.store(true, Ordering::SeqCst);
                return;
            }
        };
        let mut handles: Vec<Option<OwnedHandle<Kernel>>> = message
            .handles
            .into_iter()
            .filter_map(|(object, rights)| export(object, rights))
            .map(|handle| Some(OwnedHandle::from_raw(Kernel, handle)))
            .collect();
        if channel.write_slots(&message.bytes, &mut handles).is_err() {
            return;
        }
    }
}

fn spawn_out(local: Arc<Port>, kernel: Handle) {
    spawn_with("port out", (local, kernel), |(local, kernel)| {
        let port = core::mem::ManuallyDrop::new(owned(KernelPort::from_owned, kernel));
        loop {
            let _ = local.waiters().wait_trusting(|| !local.is_empty());
            while let Some(packet) = local.take() {
                if let Err(why) = port.queue(packet.key, packet.data) {
                    crate::println!("  bridge   a bell refused: {why:?}");
                    return;
                }
            }
        }
    });
}

fn spawn_in(local: Arc<Port>, kernel: Handle) {
    spawn_with("port in", (local, kernel), |(local, kernel)| {
        let port = core::mem::ManuallyDrop::new(owned(KernelPort::from_owned, kernel));
        while let Ok(packet) = port.wait(Deadline::Never) {
            if local.queue_user(packet.key, packet.data).is_err() {
                return;
            }
        }
    });
}

/// `PORT_FD`'s local port for a program's kernel port: a thread of its own, charged
/// to the partition, holds a wait-only duplicate and keeps one packet in the local
/// port while the kernel's has any. The program takes packets from the kernel's by
/// its own call, so readable here lags a drain by at most a look.
pub fn port_mirror(kept: Handle) -> Option<Arc<Port>> {
    let local = Port::new().ok()?;
    spawn_with("port fd", (Arc::clone(&local), kept), |(local, kept)| {
        let port = core::mem::ManuallyDrop::new(owned(KernelPort::from_owned, kept));
        let readable = || {
            let now = crate::timer::now_nanos();
            port.wait_one(Signals::READABLE, Deadline::At(now)).is_ok()
        };
        while Arc::strong_count(&local) > 1 {
            let until = crate::timer::now_nanos().saturating_add(LOOK_NANOS);
            if port
                .wait_one(Signals::READABLE, Deadline::At(until))
                .is_err()
            {
                continue;
            }
            if local.is_empty() && local.queue_user(0, [0; 2]).is_err() {
                break;
            }
            while readable() && Arc::strong_count(&local) > 1 {
                crate::sched::sleep_for(1_000_000);
            }
            let _ = local.take();
        }
        crate::sys::handle_close(kept.0);
    });
    Some(local)
}
