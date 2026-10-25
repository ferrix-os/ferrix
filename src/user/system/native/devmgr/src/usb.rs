//! Class drivers for the USB interfaces a bus host's driver exports
//! (`ferrix-usbif`, `docs/USB-SERIAL.md` §6.1).
//!
//! The kernel knows nothing of USB: what it publishes is the host
//! controller, and its driver, `usbhid`, finds the devices. An interface
//! `usbhid` drives no class of comes to devmgr as ATTACH on that driver's
//! bootstrap channel, with the driver's end of the interface's channel.
//! [`USB_DRIVERS`] -- image-time data, like every other table here -- says
//! which class driver takes it; that driver is started in a sealed job of
//! its own, as any driver is, with BIND on its bootstrap channel: a number
//! of the class's own, and the interface channel.
//!
//! A class driver's death is heard twice: here, by its process's end, and
//! by `usbhid`, whose end of the interface channel closes, so it attaches
//! the interface again. That second ATTACH, under the same key, is where
//! the driver is started again, under the same number, as the device's
//! [`ferrix_restart::Policy`] decides -- the restart policy every driver
//! kind that is started again has, with the same budget. One whose budget
//! is spent keeps its channel here unanswered, so `usbhid` does not attach
//! it again and again.
//!
//! The kernel's protocol is not touched: no BOUND, UNBOUND or DIED goes to
//! it for a class driver, since it published no device for one. What
//! happens is said on the console.

use core::time::Duration;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_restart::{Decision, Ended, Policy, Restart};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::pending::{self, Process};
use ferrix_rt::native::port::Port;
use ferrix_usbif::{Identity, MAX_BYTES, Message};

use crate::{Drivers, MAX_RESTARTS, exit_of, image_index};

/// The port keys a bus host's bootstrap channel is watched under: this,
/// plus the host's slot among the started drivers.
pub(crate) const KEY_HOST: u64 = 1 << 62;
/// The port keys a class driver's end is watched under: this, plus how
/// many drivers were started for it shifted by 8, plus its place among the
/// children -- so the end of a driver already started again is told apart.
pub(crate) const KEY_CHILD: u64 = 1 << 61;

/// The most class drivers, live or kept, at once.
const MAX_CHILDREN: usize = 16;

/// How long a dead driver's process is given to be seen ended, when its
/// interface is attached again before its end reached the port.
const ENDED_PATIENCE: u64 = 1_000_000_000;

/// In [`USB_DRIVERS`]' product column: any product of the vendor.
const ANY_PRODUCT: u16 = 0xFFFF;

/// The class drivers: by vendor and product, the image that drives the
/// interface, and the class its numbers are counted in.
const USB_DRIVERS: [(u16, u16, &[u8], Class); 2] = [
    // FTDI's FT232H, FT232B and R, and QEMU's usb-serial.
    (0x0403, ANY_PRODUCT, b"usbserial", Class::Serial),
    // Altera's USB-Blaster.
    (0x09FB, 0x6001, b"usbserial", Class::Serial),
];

/// The class a driver's device numbers are counted in: `ttyUSB<number>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Serial,
}

/// One exported interface devmgr was told of.
struct Child {
    /// The bus host's slot and the generation of its driver that attached
    /// it, and that driver's key for it.
    host: usize,
    generation: u32,
    key: u64,
    identity: Identity,
    class: Class,
    number: u8,
    /// The image, by its place among the names.
    driver: Option<u16>,
    policy: Policy,
    job: Option<Job<Kernel>>,
    process: Option<Process<Kernel>>,
    /// Its BIND's channel, kept for as long as the driver runs.
    bootstrap: Option<Channel<Kernel>>,
    /// An interface channel kept unanswered: no driver, or none again.
    parked: Option<Channel<Kernel>>,
    dead: bool,
    /// How many drivers were started for it.
    starts: u32,
}

/// Every child.
pub(crate) struct Children {
    children: [Option<Child>; MAX_CHILDREN],
}

impl Children {
    pub(crate) const fn new() -> Self {
        Children {
            children: [const { None }; MAX_CHILDREN],
        }
    }

    /// Take everything the bus host in `host`, started for the
    /// `generation`th time, wrote on its bootstrap channel. Whether the
    /// channel is still open, and so watched again.
    pub(crate) fn take(
        &mut self,
        host: usize,
        generation: u32,
        bootstrap: &Channel<Kernel>,
        drivers: &Drivers<'_>,
        port: &Port<Kernel>,
    ) -> bool {
        loop {
            let mut bytes = [0_u8; MAX_BYTES];
            let mut handles = [Handle::INVALID; 1];
            match bootstrap.read(&mut bytes, &mut handles) {
                Ok(received) => {
                    let handle = (received.handles == 1)
                        .then(|| {
                            handles
                                .first()
                                .map(|raw| OwnedHandle::from_raw(Kernel, *raw))
                        })
                        .flatten();
                    if let (Ok(Message::Attach { key, identity }), Some(handle)) = (
                        Message::decode(bytes.get(..received.bytes).unwrap_or_default()),
                        handle,
                    ) {
                        self.attach(
                            host,
                            generation,
                            key,
                            identity,
                            Channel::from_owned(handle),
                            drivers,
                            port,
                        );
                    }
                    // Anything else -- the host's "settled" -- is read and
                    // dropped, as it always was.
                }
                Err(ReadError::Failed(Error::ShouldWait)) => {
                    return bootstrap
                        .wait_async(
                            port,
                            Signals::READABLE | Signals::PEER_CLOSED,
                            KEY_HOST + host as u64,
                        )
                        .is_ok();
                }
                Err(_) => return false,
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "an ATTACH, its host, and what starting a driver needs"
    )]
    fn attach(
        &mut self,
        host: usize,
        generation: u32,
        key: u64,
        identity: Identity,
        interface: Channel<Kernel>,
        drivers: &Drivers<'_>,
        port: &Port<Kernel>,
    ) {
        let Some(at) = self.entry_for(host, generation, key, &identity, drivers) else {
            ferrix_rt::warn!(
                "devmgr   usb {:04x}:{:04x} interface {}: no slot left; not started",
                identity.vendor,
                identity.product,
                identity.interface
            );
            return;
        };
        let Some(Some(child)) = self.children.get_mut(at) else {
            return;
        };
        let Some(driver) = child.driver else {
            ferrix_rt::info!(
                "devmgr   usb {:04x}:{:04x} interface {} class {:02x}: no driver takes it",
                identity.vendor,
                identity.product,
                identity.interface,
                identity.class
            );
            child.parked = Some(interface);
            return;
        };
        let name = crate::driver_name(drivers.names, driver);
        // An ATTACH again is a driver gone: its end counts against the
        // budget, once it is seen.
        if let Some(process) = child.process.take() {
            if !child.dead {
                let until = ferrix_rt::linux::monotonic_nanos()
                    .unwrap_or(0)
                    .saturating_add(ENDED_PATIENCE);
                let _ = process.wait_one(Signals::TERMINATED, Deadline::At(until));
            }
            if let Some(job) = child.job.take() {
                let _ = job.kill();
            }
            let exit = exit_of(&process);
            if child.policy.decide(Ended::of(exit), None) != Decision::Now {
                ferrix_rt::warn!(
                    "devmgr   {name} for usb {:04x}:{:04x} interface {} (serial {}) died with status {}; its budget is spent and it stays unstarted",
                    identity.vendor,
                    identity.product,
                    identity.interface,
                    child.number,
                    crate::died_status(exit)
                );
                child.parked = Some(interface);
                return;
            }
            ferrix_rt::notice!(
                "devmgr   {name} for usb {:04x}:{:04x} interface {} (serial {}) died with status {}; started again ({} of {MAX_RESTARTS})",
                identity.vendor,
                identity.product,
                identity.interface,
                child.number,
                crate::died_status(exit),
                child.policy.budget.used()
            );
        }
        child.starts = child.starts.wrapping_add(1);
        let watch = KEY_CHILD | (u64::from(child.starts) << 8) | at as u64;
        match start(drivers, driver, name, child.number, interface, port, watch) {
            Ok((job, process, bootstrap)) => {
                ferrix_rt::info!(
                    "devmgr   {name} for usb {:04x}:{:04x} interface {}: started (serial {})",
                    identity.vendor,
                    identity.product,
                    identity.interface,
                    child.number
                );
                child.job = Some(job);
                child.process = Some(process);
                child.bootstrap = Some(bootstrap);
                child.dead = false;
            }
            Err(()) => ferrix_rt::warn!(
                "devmgr   {name} for usb {:04x}:{:04x} interface {}: could not be started",
                identity.vendor,
                identity.product,
                identity.interface
            ),
        }
    }

    /// The class driver whose end was watched under `key` ended: said, and
    /// kept, until its interface is attached again or the device goes.
    pub(crate) fn ended(&mut self, key: u64, drivers: &Drivers<'_>) {
        let at = (key & 0xFF) as usize;
        let starts = ((key >> 8) & 0xFFFF_FFFF) as u32;
        let Some(Some(child)) = self.children.get_mut(at) else {
            return;
        };
        // A driver started again already: this end was counted then.
        if child.dead || child.starts != starts {
            return;
        }
        child.dead = true;
        child.bootstrap = None;
        let Some(process) = &child.process else {
            return;
        };
        let exit = exit_of(process);
        let name = child
            .driver
            .map_or("?", |driver| crate::driver_name(drivers.names, driver));
        let status = crate::died_status(exit);
        if status == 0 {
            // Its interface went: the device, or the host. The number is
            // free for the next device.
            ferrix_rt::info!(
                "devmgr   {name} for usb {:04x}:{:04x} interface {} (serial {}) ended: its device went",
                child.identity.vendor,
                child.identity.product,
                child.identity.interface,
                child.number
            );
            if let Some(slot) = self.children.get_mut(at) {
                *slot = None;
            }
        } else {
            ferrix_rt::warn!(
                "devmgr   {name} for usb {:04x}:{:04x} interface {} (serial {}) died with status {status}",
                child.identity.vendor,
                child.identity.product,
                child.identity.interface,
                child.number
            );
        }
    }

    /// The bus host in `host` died: every interface it attached is gone,
    /// with its channel, and its drivers end themselves as they hear it.
    pub(crate) fn host_gone(&mut self, host: usize) {
        for slot in &mut self.children {
            if slot.as_ref().is_some_and(|child| child.host == host) {
                if let Some(job) = slot.as_ref().and_then(|child| child.job.as_ref()) {
                    let _ = job.kill();
                }
                *slot = None;
            }
        }
    }

    /// The child of host `host`'s `generation`th driver's `key`, made if it
    /// is new: its place.
    fn entry_for(
        &mut self,
        host: usize,
        generation: u32,
        key: u64,
        identity: &Identity,
        drivers: &Drivers<'_>,
    ) -> Option<usize> {
        let known = self.children.iter().position(|child| {
            child.as_ref().is_some_and(|child| {
                child.host == host && child.generation == generation && child.key == key
            })
        });
        if known.is_some() {
            return known;
        }
        let at = self.children.iter().position(Option::is_none)?;
        let (driver, class) = match driver_for(identity, drivers) {
            Some((driver, class)) => (Some(driver), class),
            None => (None, Class::Serial),
        };
        let number = self.free_number(class);
        let slot = self.children.get_mut(at)?;
        *slot = Some(Child {
            host,
            generation,
            key,
            identity: *identity,
            class,
            number,
            driver,
            policy: Policy::new(Restart::Always, Duration::ZERO, MAX_RESTARTS, Duration::MAX),
            job: None,
            process: None,
            bootstrap: None,
            parked: None,
            dead: false,
            starts: 0,
        });
        Some(at)
    }

    /// The lowest number no child of `class` holds.
    fn free_number(&self, class: Class) -> u8 {
        (0..=u8::MAX)
            .find(|&number| {
                !self
                    .children
                    .iter()
                    .flatten()
                    .any(|child| child.class == class && child.number == number)
            })
            .unwrap_or(u8::MAX)
    }
}

/// A class driver started: its job, its process, devmgr's end of its
/// bootstrap channel.
type Launched = (Job<Kernel>, Process<Kernel>, Channel<Kernel>);

/// The class driver [`USB_DRIVERS`] names for `identity`, if the initramfs
/// carries it.
fn driver_for(identity: &Identity, drivers: &Drivers<'_>) -> Option<(u16, Class)> {
    let (_, _, wanted, class) = USB_DRIVERS.iter().find(|(vendor, product, _, _)| {
        *vendor == identity.vendor && (*product == ANY_PRODUCT || *product == identity.product)
    })?;
    let driver = image_index(drivers.names, drivers.count, wanted)?;
    Some((driver, *class))
}

/// Start class driver `driver` for one interface: a sealed job, BIND with
/// the interface channel on its bootstrap, a watch on its end.
fn start(
    drivers: &Drivers<'_>,
    driver: u16,
    name: &str,
    number: u8,
    interface: Channel<Kernel>,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let image = drivers
        .images
        .get(usize::from(driver))
        .and_then(Option::as_ref)
        .ok_or(())?;
    let job = drivers.job.create_sealed_child().map_err(|_| ())?;
    let process = pending::create_process(&job, image, name).map_err(|_| ())?;
    let (near, far) = channel::create(Kernel).map_err(|_| ())?;
    near.write_with(
        Message::Bind { number }.encode().as_bytes(),
        [interface.into_owned()],
    )
    .map_err(|_| ())?;
    process.notify_on_exit(port, key).map_err(|_| ())?;
    process.start(far.into_owned()).map_err(|_| ())?;
    Ok((job, process, near))
}
