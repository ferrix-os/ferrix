//! The driver model (`docs/DRIVER-MODEL.md`; the driver model's design
//! page): a driver is a [`Driver`] whose [`Driver::Resources`] say what it
//! needs from its node, [`run`] brings it up, and every resource comes in
//! typed.
//!
//! A driver never looks a resource up by string. It declares a struct of
//! its resources and derives [`Resources`] for it; the derive writes the
//! lookup by the tree's names or indices, and a requirement table embedded
//! in the image that devmgr checks against the node's references before it
//! starts the driver. So a node without a clock the driver needs is refused
//! at admission, with the node named, never in [`Driver::bind`].
//!
//! What this slice builds, step 1's (`docs/DRIVER-MODEL.md` §9): the
//! traits, [`run`], registers, interrupts and the requirement table, for a
//! generic device-tree node. What it stubs, saying so where it does: the
//! providers' clients (step 2, in `ferrix-platform`), pin states (step 2),
//! child devices and parent buses (step 4), and classes (`CHARDEV.md`'s
//! grant to drivers); each answers `ENOSYS` until its step.

use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
pub use ferrix_devmgr_proto::requirements::{Requirement, What};
use ferrix_fdt::identity::{RECORD_BYTES, Record};
pub use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::DEVICE_TREE_GENERIC;
use ferrix_rt::native::device::{Device, Interrupt as InterruptHandle};
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::{Bootstrap, Kernel};

use crate::class::{ClassPublisher, Request};

/// What every call of the driver model answers: a value, or the errno that
/// stopped it.
pub type Result<T> = core::result::Result<T, Errno>;

/// The device a driver is started on, as a handle.
pub type DeviceHandle = Device<Kernel>;

/// A driver of one device.
pub trait Driver: Sized {
    /// What it needs from its node, derived with `#[derive(Resources)]`.
    type Resources: Resources;

    /// Come up on the device, its resources already resolved.
    ///
    /// # Errors
    ///
    /// What stopped it; the process ends with it.
    fn bind(context: BindContext<Self::Resources>) -> Result<Self>;

    /// One event: a request on its class, a provider gone, the driver asked
    /// to stop.
    fn event(&mut self, event: Event) -> Flow;

    /// Leave the device quiet: no interrupt raised, no memory reached.
    fn quiesce(&mut self);
}

/// What a driver's resources are: the requirement table devmgr checks, and
/// the lookup that resolves them. Derived with `#[derive(Resources)]`.
pub trait Resources: Sized {
    /// Every resource, as devmgr checks it.
    const REQUIREMENTS: &'static [Requirement<'static>];

    /// The table as the image carries it, in `.ferrix.requirements`.
    fn requirement_table() -> &'static [u8];

    /// Resolve every field from `node`.
    ///
    /// # Errors
    ///
    /// The first field that could not be had.
    fn resolve(node: &mut Node) -> Result<Self>;
}

/// A resource that can be resolved from a node, for one requirement.
pub trait FromNode: Sized {
    /// Resolve it.
    ///
    /// # Errors
    ///
    /// `ENOENT` when the node does not give it; what else stopped it.
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self>;
}

impl<T: FromNode> FromNode for Option<T> {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        match T::from_node(node, requirement) {
            Ok(found) => Ok(Some(found)),
            // Absent, or naming a provider that is not there: an optional
            // resource the driver runs without.
            Err(Errno::ENOENT | Errno::ENXIO) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// The node a driver is started on: its handle, and its identity record.
#[derive(Debug)]
pub struct Node {
    device: DeviceHandle,
    record: [u8; RECORD_BYTES],
    length: usize,
    apertures: u32,
    vectors: u32,
}

impl Node {
    /// The node `device` is, its record read through `device_config_read`
    /// (`DEVICE_TREE_GENERIC`).
    ///
    /// # Errors
    ///
    /// `ENODEV` for a device that is not a generic tree node, or whose
    /// record does not read.
    pub fn read(device: DeviceHandle) -> Result<Self> {
        let info = device.info().map_err(|_| Errno::ENODEV)?;
        if info.virtio != DEVICE_TREE_GENERIC {
            return Err(Errno::ENODEV);
        }
        let length = usize::try_from(info.class)
            .map_err(|_| Errno::ENODEV)?
            .min(RECORD_BYTES);
        let mut record = [0u8; RECORD_BYTES];
        for at in (0..length).step_by(4) {
            let word = device
                .config_read(u16::try_from(at).map_err(|_| Errno::ENODEV)?, 4)
                .map_err(|_| Errno::ENODEV)?;
            for (slot, byte) in record.iter_mut().skip(at).zip(word.to_le_bytes()) {
                *slot = byte;
            }
        }
        let node = Node {
            device,
            record,
            length,
            apertures: info.apertures,
            vectors: info.vectors,
        };
        if node.record().is_none() {
            return Err(Errno::ENODEV);
        }
        Ok(node)
    }

    /// Its identity record.
    #[must_use]
    pub fn record(&self) -> Option<Record<'_>> {
        Record::parse(self.record.get(..self.length)?).ok()
    }

    /// Its handle.
    #[must_use]
    pub const fn device(&self) -> &DeviceHandle {
        &self.device
    }

    /// The place `requirement` names among its kind: its index, or its
    /// tree name's place in the node's `<kind>-names`.
    ///
    /// # Errors
    ///
    /// `ENOENT` when the node has no such name.
    pub fn place(&self, requirement: &Requirement<'_>, names: &str) -> Result<u16> {
        if requirement.index != ferrix_devmgr_proto::requirements::BY_NAME {
            return Ok(requirement.index);
        }
        let record = self.record().ok_or(Errno::ENODEV)?;
        let found = record
            .names(names)
            .position(|name| name == requirement.name)
            .ok_or(Errno::ENOENT)?;
        u16::try_from(found).map_err(|_| Errno::ENOENT)
    }

    /// Whether the node's `property` has a resolved reference at `index`:
    /// `ENOENT` when it has none, `ENXIO` when it names a missing or
    /// disabled provider.
    ///
    /// # Errors
    ///
    /// As said.
    pub fn reference(&self, property: &str, index: u16) -> Result<Reference> {
        let record = self.record().ok_or(Errno::ENODEV)?;
        let found = record
            .references()
            .find(|entry| entry.property == property && entry.index == index)
            .ok_or(Errno::ENOENT)?;
        if !found.resolved() {
            return Err(Errno::ENXIO);
        }
        let mut cells = [0u32; ferrix_fdt::generic::MAX_SPECIFIER_CELLS];
        let mut count = 0;
        for (slot, cell) in cells.iter_mut().zip(found.cells()) {
            *slot = cell;
            count += 1;
        }
        Ok(Reference {
            provider: found.provider,
            cells,
            count,
        })
    }
}

/// A resolved reference: the provider node and the specifier cells, as the
/// tree gave them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The provider node's id.
    pub provider: u32,
    cells: [u32; ferrix_fdt::generic::MAX_SPECIFIER_CELLS],
    count: usize,
}

impl Reference {
    /// The specifier's cells, uninterpreted.
    #[must_use]
    pub fn cells(&self) -> &[u32] {
        self.cells.get(..self.count).unwrap_or(&[])
    }
}

/// What a node is, for the driver: its identity record, kept.
#[derive(Debug)]
pub struct DeviceInfo {
    record: [u8; RECORD_BYTES],
    length: usize,
}

impl DeviceInfo {
    /// The node's identity record: its compatible list, path, references,
    /// names and subtree.
    #[must_use]
    pub fn record(&self) -> Option<Record<'_>> {
        Record::parse(self.record.get(..self.length)?).ok()
    }

    /// The node's path.
    #[must_use]
    pub fn path(&self) -> &str {
        self.record().map_or("?", |record| record.path())
    }
}

/// Where a bus controller publishes the devices it finds (§5, step 4): a
/// stub until devmgr's protocol carries child devices.
#[derive(Debug, Default)]
pub struct ChildPublisher(());

impl ChildPublisher {
    /// Publish a child device. `ENOSYS` until step 4.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn publish(&self, _child: crate::bus::ChildDevice) -> Result<crate::bus::ChildId> {
        Err(Errno::ENOSYS)
    }

    /// Take a child device back. `ENOSYS` until step 4.
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn remove(&self, _child: crate::bus::ChildId) -> Result<()> {
        Err(Errno::ENOSYS)
    }
}

/// What [`Driver::bind`] is given.
#[derive(Debug)]
pub struct BindContext<R> {
    /// The driver's resources, resolved.
    pub resources: R,
    /// The device.
    pub device: DeviceHandle,
    /// What the node is.
    pub info: DeviceInfo,
    /// Where child devices are published (step 4).
    pub children: ChildPublisher,
    /// Where the device is published to programs.
    pub classes: ClassPublisher,
}

/// One event a driver is told of.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// A request on the driver's character device.
    CharacterDevice(Request),
    /// A provider the driver uses is gone, and is restarting (step 2).
    ProviderLost(ProviderLost),
    /// devmgr is ending the driver.
    Stopping,
}

/// Which provider went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLost {
    /// Its node's id.
    pub provider: u32,
}

/// What the driver does after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Go on.
    Continue,
    /// Quiesce the device, and wait to be bound again.
    Quiesce,
    /// Quiesce and end.
    Stop,
}

/// A device's interrupt, bound to a port of its own.
#[derive(Debug)]
pub struct Interrupt {
    handle: InterruptHandle<Kernel>,
    port: Port<Kernel>,
}

/// One delivery of an interrupt, to acknowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptOccurrence {
    /// When it fired, in nanoseconds since boot.
    pub at: u64,
}

/// The key an interrupt's packets carry on its own port.
const INTERRUPT_KEY: u64 = 1;

impl Interrupt {
    /// Wait for it. Ends when the process is killed.
    ///
    /// # Errors
    ///
    /// `EINTR` when the wait was ended.
    pub fn wait(&self) -> Result<InterruptOccurrence> {
        let packet = self.port.wait(Deadline::Never).map_err(|_| Errno::EINTR)?;
        let [at, _] = packet.data;
        Ok(InterruptOccurrence { at })
    }

    /// Wait for it until `deadline`, `CLOCK_MONOTONIC` nanoseconds.
    ///
    /// # Errors
    ///
    /// `ETIMEDOUT` when it did not come.
    pub fn wait_until(&self, deadline: u64) -> Result<InterruptOccurrence> {
        let packet = self
            .port
            .wait(Deadline::At(deadline))
            .map_err(|_| Errno::ETIMEDOUT)?;
        let [at, _] = packet.data;
        Ok(InterruptOccurrence { at })
    }

    /// Let it be raised again, once its cause is cleared.
    pub fn acknowledge(&self, _occurrence: InterruptOccurrence) {
        let _ = self.handle.ack();
    }
}

impl FromNode for Interrupt {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        let place = node.place(requirement, "interrupt-names")?;
        if u32::from(place) >= node.vectors {
            return Err(Errno::ENOENT);
        }
        let handle = node
            .device
            .interrupt(usize::from(place))
            .map_err(|_| Errno::EBUSY)?;
        let port = port::create(Kernel).map_err(|_| Errno::ENOMEM)?;
        handle
            .bind(&port, INTERRUPT_KEY)
            .map_err(|_| Errno::EBUSY)?;
        Ok(Interrupt { handle, port })
    }
}

impl<M: crate::registers::RegisterMap> FromNode for crate::registers::Registers<M> {
    fn from_node(node: &mut Node, requirement: &Requirement<'_>) -> Result<Self> {
        let place = node.place(requirement, "reg-names")?;
        if u32::from(place) >= node.apertures {
            return Err(Errno::ENOENT);
        }
        crate::registers::Registers::map(&node.device, usize::from(place))
    }
}

/// A binding's pin states, derived with `#[derive(PinStates)]`.
pub trait PinStates: Copy {
    /// Every state, as the tree names it, and whether it may be absent.
    const STATES: &'static [PinStateName];

    /// This state's name in the tree's `pinctrl-names`.
    fn tree_name(&self) -> &'static str;
}

/// One pin state, as `#[derive(PinStates)]` writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinStateName {
    /// Its name in `pinctrl-names`.
    pub tree_name: &'static str,
    /// Whether the tree may leave it out.
    pub optional: bool,
}

/// A device's pin states, applied through its pin controller's provider.
#[derive(Debug)]
pub struct PinControl<S: PinStates> {
    /// Which of `S::STATES` the tree gives.
    given: [bool; 8],
    state: core::marker::PhantomData<S>,
}

impl<S: PinStates> PinControl<S> {
    /// Apply `state`. A state the tree does not give is nothing to apply.
    /// Otherwise `ENOSYS` until the pin controller's provider exists
    /// (step 2).
    ///
    /// # Errors
    ///
    /// `ENOSYS`.
    pub fn apply(&self, state: S) -> Result<()> {
        let given = S::STATES
            .iter()
            .zip(self.given)
            .any(|(name, given)| given && name.tree_name == state.tree_name());
        if given { Err(Errno::ENOSYS) } else { Ok(()) }
    }
}

impl<S: PinStates> FromNode for PinControl<S> {
    fn from_node(node: &mut Node, _requirement: &Requirement<'_>) -> Result<Self> {
        let mut given = [false; 8];
        for (slot, name) in given.iter_mut().zip(S::STATES) {
            let state = Requirement::named(What::PinControl, name.tree_name);
            let place = node.place(&state, "pinctrl-names");
            let place = match (place, name.tree_name) {
                (Err(Errno::ENOENT), "default") => Ok(0),
                (place, _) => place,
            };
            *slot = match place {
                Ok(place) => pin_state_given(node, place)?,
                Err(_) if name.optional => false,
                Err(error) => return Err(error),
            };
            if !*slot && !name.optional {
                return Err(Errno::ENOENT);
            }
        }
        Ok(PinControl {
            given,
            state: core::marker::PhantomData,
        })
    }
}

/// Whether the node's `pinctrl-<place>` names resolved configurations.
fn pin_state_given(node: &Node, place: u16) -> Result<bool> {
    const PREFIX: &[u8; 8] = b"pinctrl-";
    let mut property = [0u8; 16];
    let (head, tail) = property.split_at_mut(PREFIX.len());
    head.copy_from_slice(PREFIX);
    let length = PREFIX.len() + place.to_string_in(tail)?;
    let name =
        core::str::from_utf8(property.get(..length).unwrap_or(&[])).map_err(|_| Errno::EINVAL)?;
    match node.reference(name, 0) {
        Ok(_) => Ok(true),
        Err(Errno::ENOENT) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Writing a number's decimal digits without allocating.
trait Digits {
    fn to_string_in(self, out: &mut [u8]) -> Result<usize>;
}

impl Digits for u16 {
    fn to_string_in(self, out: &mut [u8]) -> Result<usize> {
        let mut digits = [0u8; 5];
        let mut value = self;
        let mut count = 0;
        loop {
            let slot = digits.get_mut(count).ok_or(Errno::EINVAL)?;
            *slot = b'0' + (value % 10) as u8;
            count += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        for (index, slot) in out.iter_mut().take(count).enumerate() {
            *slot = *digits.get(count - 1 - index).ok_or(Errno::EINVAL)?;
        }
        Ok(count)
    }
}

/// The bootstrap channel [`entry`](crate::entry) keeps for [`run`], as its
/// raw handle; 0 when there is none.
static BOOTSTRAP: AtomicU32 = AtomicU32::new(0);

/// Keep the bootstrap channel for [`run`]: what `ferrix_driver::entry!()`
/// does before it calls `main`.
#[doc(hidden)]
pub fn keep_bootstrap(bootstrap: Bootstrap) {
    if let Some(channel) = bootstrap {
        BOOTSTRAP.store(channel.into_owned().into_raw().0, Ordering::Release);
    }
}

/// Where [`run`] stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Stopped {
    /// No bootstrap channel, or no START with its device.
    Start = 1,
    /// The device is not a generic tree node.
    Node = 2,
    /// A resource could not be resolved.
    Resources = 3,
    /// `bind` failed.
    Bind = 4,
}

/// Bring `D` up on the device devmgr started it on, then serve its events
/// until devmgr ends it: START, the node's record, every resource resolved
/// (the requirement table kept in the image), [`Driver::bind`], and the
/// loop. Ends the process.
pub fn run<D: Driver>() -> ! {
    // Keep the table in the image: devmgr reads it there.
    let _ = core::hint::black_box(D::Resources::requirement_table());
    let status = match serve::<D>() {
        Ok(()) => 0,
        Err(stopped) => stopped as i32,
    };
    ferrix_rt::exit(status)
}

fn serve<D: Driver>() -> core::result::Result<(), Stopped> {
    let raw = BOOTSTRAP.swap(0, Ordering::AcqRel);
    if raw == 0 {
        return Err(Stopped::Start);
    }
    let boot =
        ferrix_rt::native::channel::Channel::from_owned(OwnedHandle::from_raw(Kernel, Handle(raw)));
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Stopped::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Stopped::Start)?;
    if received.handles != 1
        || !matches!(
            StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()),
            Ok(StartMessage::Start(_))
        )
    {
        return Err(Stopped::Start);
    }
    let [handle] = handles;
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handle));
    let mut node = Node::read(device).map_err(|_| Stopped::Node)?;
    let resources = D::Resources::resolve(&mut node).map_err(|error| {
        ferrix_rt::err!(
            "driver: {}: a resource could not be had ({error:?})",
            node.record().map_or("?", |record| record.path())
        );
        Stopped::Resources
    })?;
    let Node {
        device,
        record,
        length,
        ..
    } = node;
    let context = BindContext {
        resources,
        device,
        info: DeviceInfo { record, length },
        children: ChildPublisher::default(),
        classes: ClassPublisher::default(),
    };
    let path = context.info.path();
    let mut copy = [0u8; 256];
    let shown = copy_str(path, &mut copy);
    let mut driver = D::bind(context).map_err(|error| {
        ferrix_rt::err!("driver: {shown}: bind failed ({error:?})");
        Stopped::Bind
    })?;
    // No class or provider events reach a driver yet: it serves until devmgr
    // closes its bootstrap channel, ending it.
    let _ = boot.wait_one(Signals::PEER_CLOSED, Deadline::Never);
    if driver.event(Event::Stopping) != Flow::Continue {
        driver.quiesce();
    }
    Ok(())
}

/// `text`, copied into `out` and cut at a character, as a `&str` of `out`.
fn copy_str<'o>(text: &str, out: &'o mut [u8; 256]) -> &'o str {
    let mut length = text.len().min(out.len());
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    for (slot, byte) in out.iter_mut().zip(text.bytes().take(length)) {
        *slot = byte;
    }
    core::str::from_utf8(out.get(..length).unwrap_or(&[])).unwrap_or("?")
}
