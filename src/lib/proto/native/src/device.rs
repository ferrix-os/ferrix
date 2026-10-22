//! A device, and the interrupts and register windows it hands a driver.

use ferrix_native_abi::nr;
use ferrix_native_abi::types::{
    APERTURE_INFO_BYTES, ApertureInfo, DEVICE_INFO_BYTES, DEVICE_LIMIT_ISOLATED_INTERRUPTS,
    DEVICE_LIMIT_PIN_CEILING, DEVICE_LIMIT_PIN_PAGES, DEVICE_LIMIT_PIN_ROOM, DeviceBlock,
    DeviceInfo, IoMappingSpec,
};

use crate::call::{Call, Syscall};
use crate::channel::Channel;
use crate::error::{Error, decode, decode_handle, decode_unit};
use crate::handle::{Object, OwnedHandle, object_handle, register};
use crate::port::Port;

object_handle!(
    /// A device node, as `devmgr` hands one to a driver.
    Device
);

/// A limit of a device's: `device_set_limit` and `device_get_limit`'s
/// (`docs/NVIDIA.md` §12.2 and §12.3). The pin limits are in pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// The device's pin budget, which only `devmgr` sets.
    PinPages,
    /// The machine's ceiling on twice every budget raised above the
    /// default: a quarter of RAM. Read only.
    PinCeiling,
    /// What of the ceiling other devices' raised budgets leave: the most
    /// twice this device's budget may be raised to. Read only.
    PinRoom,
    /// The isolated-interrupts mark, set-once by `devmgr` on a device that
    /// runs firmware of its own: 1 marked, 0 not.
    IsolatedInterrupts,
}

impl Limit {
    /// The number the calls take.
    const fn number(self) -> u64 {
        match self {
            Limit::PinPages => DEVICE_LIMIT_PIN_PAGES,
            Limit::PinCeiling => DEVICE_LIMIT_PIN_CEILING,
            Limit::PinRoom => DEVICE_LIMIT_PIN_ROOM,
            Limit::IsolatedInterrupts => DEVICE_LIMIT_ISOLATED_INTERRUPTS,
        }
    }
}

object_handle!(
    /// A hardware interrupt claimed from a device.
    Interrupt
);

object_handle!(
    /// An MMIO aperture claimed from a device.
    IoMapping
);

impl<S: Syscall> Device<S> {
    /// `interrupt_create`: the device's interrupt number `index`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for an index the device does not have;
    /// [`Error::AlreadyBound`] if it is claimed; [`Error::AccessDenied`]
    /// without `MANAGE`.
    pub fn interrupt(&self, index: usize) -> Result<Interrupt<S>, Error> {
        let value = Call::new(nr::INTERRUPT_CREATE)
            .value(register(self.handle()))
            .value(index)
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(Interrupt::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }

    /// `io_mapping_create`: the aperture `spec` names.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] for a range outside the device's apertures or
    /// without `MANAGE`; [`Error::InvalidArgs`] for one that is not whole pages.
    pub fn io_mapping(&self, spec: IoMappingSpec) -> Result<IoMapping<S>, Error> {
        let mut bytes = [0_u8; size_of::<IoMappingSpec>()];
        for (slot, byte) in bytes.iter_mut().zip(
            spec.phys
                .to_ne_bytes()
                .into_iter()
                .chain(spec.len.to_ne_bytes()),
        ) {
            *slot = byte;
        }
        let value = Call::new(nr::IO_MAPPING_CREATE)
            .value(register(self.handle()))
            .input(&bytes)
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(IoMapping::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }
}

impl<S: Syscall> Device<S> {
    /// `device_info`: the device as enumeration found it, which is what
    /// whoever starts a driver on it puts in the driver's START. Any device
    /// handle will do.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device.
    pub fn info(&self) -> Result<DeviceInfo, Error> {
        let mut bytes = [0_u8; DEVICE_INFO_BYTES];
        let value = Call::new(nr::DEVICE_INFO)
            .value(register(self.handle()))
            .output(&mut bytes)
            .make(self.syscall());
        decode_unit(value)?;
        Ok(device_info(&bytes))
    }

    /// `device_aperture`: the device's aperture `index`, below
    /// [`DeviceInfo::apertures`], whole -- its 64-bit address and length, the
    /// BAR it came from and where in it, and its flags. Any device handle
    /// will do.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for an index the device does not have,
    /// [`Error::WrongType`] for a handle that is not a device.
    pub fn aperture(&self, index: usize) -> Result<ApertureInfo, Error> {
        let mut bytes = [0_u8; APERTURE_INFO_BYTES];
        let value = Call::new(nr::DEVICE_APERTURE)
            .value(register(self.handle()))
            .value(index)
            .output(&mut bytes)
            .make(self.syscall());
        decode_unit(value)?;
        Ok(aperture_info(&bytes))
    }

    /// `device_config_read`: `width` bytes, 1, 2 or 4, of the function's
    /// configuration space at `offset`, a multiple of `width` below 4096.
    /// Every byte is readable. Needs `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for another width or offset,
    /// [`Error::WrongType`] for a device that is not a PCI function with
    /// configuration space, [`Error::BadState`] for one the kernel refused,
    /// [`Error::AccessDenied`] without `MANAGE`.
    pub fn config_read(&self, offset: u16, width: u8) -> Result<u32, Error> {
        let mut bytes = [0_u8; 4];
        let value = Call::new(nr::DEVICE_CONFIG_READ)
            .value(register(self.handle()))
            .value(usize::from(offset))
            .value(usize::from(width))
            .output(&mut bytes)
            .make(self.syscall());
        decode_unit(value)?;
        Ok(u32::from_ne_bytes(bytes))
    }

    /// `device_config_write`: the low `width` bytes of `value` at `offset`,
    /// under the rules of [`Device::config_read`]. Only bytes inside the
    /// function's vendor-specific capabilities are the driver's; a write
    /// touching any other byte writes nothing. Needs `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] for a write any byte of which the driver may
    /// not make, or without `MANAGE`; otherwise as [`Device::config_read`].
    pub fn config_write(&self, offset: u16, width: u8, value: u32) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::DEVICE_CONFIG_WRITE)
                .value(register(self.handle()))
                .value(usize::from(offset))
                .value(usize::from(width))
                .value(value as usize)
                .make(self.syscall()),
        )
    }

    /// `device_quiesce`: the device's driver is gone, so turn its bus
    /// mastering off and release its block ring for the next driver. Needs
    /// `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::BadState`] while a driver still serves the device through a
    /// ring, or if the device's configuration space could not be reached;
    /// [`Error::TimedOut`] when the driver is gone but its ring has not ended
    /// within the kernel's patience, which is worth asking again;
    /// [`Error::AccessDenied`] without `MANAGE`.
    pub fn quiesce(&self) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::DEVICE_QUIESCE)
                .value(register(self.handle()))
                .make(self.syscall()),
        )
    }

    /// `device_clock`: the rate, in Hz, nearest `hz` that the kernel will run
    /// the device's pixel clock at, and with `set`, that rate set. Needs
    /// `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a device with no clock the kernel sets,
    /// [`Error::BadState`] when it cannot be set now (something else runs
    /// from it), [`Error::InvalidArgs`] for a rate of 0, and
    /// [`Error::AccessDenied`] without `MANAGE`.
    pub fn clock(&self, hz: u32, set: bool) -> Result<u32, Error> {
        let options = if set {
            ferrix_native_abi::types::CLOCK_SET
        } else {
            0
        };
        let value = Call::new(nr::DEVICE_CLOCK)
            .value(register(self.handle()))
            .value(hz as usize)
            .value(options as usize)
            .make(self.syscall());
        let rate = decode(value)?;
        u32::try_from(rate).map_err(|_| Error::InvalidArgs)
    }

    /// `device_set_limit`: set `which` of the device's limits to `value`.
    /// Needs `SET_LIMIT`, which `devmgr` keeps and no driver is given.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `SET_LIMIT`; [`Error::BadState`]
    /// while the device has live pins; [`Error::NoMemory`], with nothing
    /// changed, when the budget would take raised budgets past the kernel's
    /// ceiling; [`Error::InvalidArgs`] for a limit that cannot be set.
    pub fn set_limit(&self, which: Limit, value: usize) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::DEVICE_SET_LIMIT)
                .value(register(self.handle()))
                .value(which.number() as usize)
                .value(value)
                .make(self.syscall()),
        )
    }

    /// `device_get_limit`: `which` of the device's limits, in pages. Any
    /// device handle will do.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device.
    pub fn limit(&self, which: Limit) -> Result<usize, Error> {
        decode(
            Call::new(nr::DEVICE_GET_LIMIT)
                .value(register(self.handle()))
                .value(which.number() as usize)
                .make(self.syscall()),
        )
    }

    /// `device_isolation`: how the device is isolated, as
    /// `DEVICE_ISOLATION_*` bits. Any device handle will do.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device.
    pub fn isolation(&self) -> Result<u64, Error> {
        decode(
            Call::new(nr::DEVICE_ISOLATION)
                .value(register(self.handle()))
                .make(self.syscall()),
        )
        .map(|bits: usize| bits as u64)
    }

    /// Ask the kernel for a block ring on this device: the driver's end of
    /// the ring's control channel, over which HELLO goes next
    /// (`docs/BLOCK-RING.md` §6). Needs `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device,
    /// [`Error::AccessDenied`] without `MANAGE`, and the kernel's refusal
    /// for a device that already has a ring.
    pub fn block_ring(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::BLOCK_RING_CREATE)
    }

    /// Ask the kernel for a net ring on this device: the driver's end of the
    /// ring's control channel, over which HELLO goes next
    /// (`docs/NET-RING.md` §7). Needs `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device,
    /// [`Error::AccessDenied`] without `MANAGE`, and the kernel's refusal for
    /// a device that already has a ring.
    pub fn net_ring(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::NET_RING_CREATE)
    }

    /// Ask the kernel for the display control channel on this device: the
    /// driver's end, over which HELLO goes next (`docs/DISPLAY.md` §2.2).
    /// Needs `MANAGE`.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for a handle that is not a device,
    /// [`Error::AccessDenied`] without `MANAGE`, and the kernel's refusal for
    /// a device that already has one.
    pub fn display_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::DISPLAY_CONTROL_CREATE)
    }

    /// `render_control_create`: the *render* control channel for this
    /// device, over which `src/lib/proto/renderctl`'s HELLO goes next
    /// (`docs/GPU.md` §3.3).
    ///
    /// Separate from [`Device::display_control`], because a card has two
    /// conversations and a driver may serve one of them and not the other.
    ///
    /// # Errors
    ///
    /// `ALREADY_BOUND` when the device has one, and whatever the call said.
    pub fn render_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::RENDER_CONTROL_CREATE)
    }

    /// `input_control_create`: the input control channel for this device.
    ///
    /// # Errors
    ///
    /// `ALREADY_BOUND` when the device has one, and whatever the call said.
    pub fn input_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::INPUT_CONTROL_CREATE)
    }

    /// Make the sound control channel for this device and answer the
    /// driver's end of it (`docs/AUDIO.md` §3.2).
    ///
    /// # Errors
    ///
    /// `ALREADY_BOUND` when the device has one, and whatever the call said.
    pub fn sound_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::SOUND_CONTROL_CREATE)
    }

    /// Make the chardev control channel for this device and answer the
    /// driver's end of it, which carries no `TRANSFER` or `DUPLICATE`
    /// (`docs/NVIDIA.md` §4.4): HELLO on it lists the minors of major 195
    /// the driver serves, and REQUESTs come back ([`crate::chardev`]).
    ///
    /// # Errors
    ///
    /// `ALREADY_BOUND` when the device has one, `ACCESS_DENIED` for a device
    /// whose interrupts are not isolated, and whatever the call said.
    pub fn chardev_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::CHARDEV_CONTROL_CREATE)
    }

    /// Make a log control channel for this device and answer the driver's
    /// end of it: the kernel log, read with `ferrix_logctl`'s READ and
    /// answered with DATA from the oldest byte the log still keeps. The claim
    /// ends when the channel is closed.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] for a device whose binding may not read the
    /// log (only the Pixel 7's USB device controller's may),
    /// [`Error::AlreadyBound`] while another reader holds it, and whatever
    /// the call said.
    pub fn log_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::LOG_CONTROL_CREATE)
    }

    /// `power_control_create`: the x86-64 power core's claim, a channel end
    /// with `WAIT` alone, which sees `PEER_CLOSED` when the kernel ends the
    /// claim (`docs/POWERCTL.md`).
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] for a node that is not the power node,
    /// [`Error::AlreadyBound`] while another claim stands,
    /// [`Error::BadState`] once the kernel latched the core off.
    pub fn power_control(&self) -> Result<Channel<S>, Error> {
        self.ring(nr::POWER_CONTROL_CREATE)
    }

    /// The body every control channel shares: one call, one handle back.
    fn ring(&self, number: usize) -> Result<Channel<S>, Error> {
        let value = Call::new(number)
            .value(register(self.handle()))
            .make(self.syscall());
        let handle = decode_handle(value)?;
        Ok(Channel::from_owned(OwnedHandle::from_raw(
            self.syscall(),
            handle,
        )))
    }
}

impl<S: Syscall> Interrupt<S> {
    /// `interrupt_bind`: deliver this interrupt to `port` as packets carrying
    /// `key`.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyBound`]; [`Error::AccessDenied`] without `MANAGE`, or
    /// without `WRITE` on the port.
    pub fn bind(&self, port: &Port<S>, key: u64) -> Result<(), Error> {
        let key = key.to_ne_bytes();
        let value = Call::new(nr::INTERRUPT_BIND)
            .value(register(self.handle()))
            .value(register(port.handle()))
            .input(&key)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `interrupt_ack`: re-arm the interrupt after servicing it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] if it is not bound; [`Error::AccessDenied`]
    /// without `MANAGE`.
    pub fn ack(&self) -> Result<(), Error> {
        decode_unit(
            Call::new(nr::INTERRUPT_ACK)
                .value(register(self.handle()))
                .make(self.syscall()),
        )
    }
}

impl<S: Syscall> IoMapping<S> {
    /// `io_mapping_map`: map the aperture at `at`, or wherever it fits, and
    /// return the address.
    ///
    /// Safe at a fixed address because the kernel refuses one that overlaps
    /// any mapping (`AddressSpace::map_device`): it can add memory to this
    /// process, never replace memory it already has.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgs`] for an address that overlaps or is not the
    /// user's; [`Error::NoMemory`]; [`Error::AccessDenied`] without `MAP`;
    /// [`Error::AlreadyBound`] while a page of the aperture is mapped, by any
    /// process, with another memory type -- write-combining, or cached as a
    /// render node's window: one memory type per device page.
    pub fn map(&self, at: Option<usize>) -> Result<usize, Error> {
        let value = Call::new(nr::IO_MAPPING_MAP)
            .value(register(self.handle()))
            .value(at.unwrap_or(0))
            .make(self.syscall());
        decode(value)
    }

    /// `io_mapping_map_combining`: map the aperture write-combining, at
    /// `at` or wherever it fits, and return the address.
    ///
    /// # Errors
    ///
    /// As [`IoMapping::map`] ([`Error::AlreadyBound`] while a page of it is
    /// mapped uncached or cached), and [`Error::InvalidArgs`] for an aperture
    /// the device does not say is prefetchable, or on x86-64 while a
    /// processor's PAT lacks its write-combining entry. The prefetchable bit
    /// is the device's own claim: the driver is trusted to combine only
    /// memory.
    pub fn map_combining(&self, at: Option<usize>) -> Result<usize, Error> {
        let value = Call::new(nr::IO_MAPPING_MAP_COMBINING)
            .value(register(self.handle()))
            .value(at.unwrap_or(0))
            .make(self.syscall());
        decode(value)
    }
}

/// An `ApertureInfo` as `device_aperture` wrote it: field by field, in the
/// order declared, native-endian.
fn aperture_info(bytes: &[u8; APERTURE_INFO_BYTES]) -> ApertureInfo {
    let long = |at: usize| {
        bytes
            .get(at..at + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map_or(0, u64::from_ne_bytes)
    };
    let byte = |at: usize| bytes.get(at).copied().unwrap_or(0);
    ApertureInfo {
        phys: long(0),
        len: long(8),
        bar: byte(16),
        flags: byte(17),
        reserved: [byte(18), byte(19), byte(20), byte(21), byte(22), byte(23)],
        offset: long(24),
    }
}

/// A `DeviceInfo` as `device_info` wrote it: field by field, in the order
/// declared, native-endian.
fn device_info(bytes: &[u8; DEVICE_INFO_BYTES]) -> DeviceInfo {
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|word| <[u8; 4]>::try_from(word).ok())
            .map_or(0, u32::from_ne_bytes)
    };
    let half = |at: usize| {
        bytes
            .get(at..at + 2)
            .and_then(|word| <[u8; 2]>::try_from(word).ok())
            .map_or(0, u16::from_ne_bytes)
    };
    let long = |at: usize| {
        bytes
            .get(at..at + 8)
            .and_then(|word| <[u8; 8]>::try_from(word).ok())
            .map_or(0, u64::from_ne_bytes)
    };
    let block = |at: usize| DeviceBlock {
        phys: long(at),
        offset: word(at + 8),
        length: word(at + 12),
    };
    DeviceInfo {
        common: block(0),
        notify: block(16),
        isr: block(32),
        device: block(48),
        location: word(64),
        class: word(68),
        apertures: word(72),
        vectors: word(76),
        notify_off_multiplier: word(80),
        vendor_id: half(84),
        device_id: half(86),
        msix_table_size: half(88),
        virtio: half(90),
        subsystem_vendor_id: half(92),
        subsystem_id: half(94),
    }
}
