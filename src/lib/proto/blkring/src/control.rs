//! The control plane: HELLO, READY, REFUSED, STOP and STOPPED between the
//! kernel and a driver, and START from `devmgr` to a driver it starts.
//!
//! Every message is a fixed little-endian structure whose first four bytes are
//! its type and next four its length; handles ride in the channel message's
//! handle array, and the glue passes the rights it read off them to
//! [`Hello::validate`] or [`Start::validate`].
//!
//! ```text
//! HELLO    driver -> kernel, 72 bytes, handles [ring VMO, data VMO, driver port]
//!   0 type 1   4 length   8 version u16   10 queues u16   12 block_size u32
//!   16 capacity u64   24 max_sectors u32   28 device_flags u32
//!   32 data_vmo_size u64   40 location u32   44 serial [u8; 20]
//!   64 name [u8; 8]
//! READY    kernel -> driver, 8 bytes, handles [kernel completion port]
//! REFUSED  kernel -> driver, 12 bytes, no handles
//!   8 reason u32, a Refusal
//! STOP     kernel -> driver, 8 bytes
//! STOPPED  driver -> kernel, 8 bytes
//! START    devmgr -> driver, 96 bytes, handles [device, control channel]
//!   8 common Block   24 notify Block   40 isr Block   56 device Block
//!     (a Block: 0 phys u64, 8 offset u32, 12 length u32)
//!   72 notify_off_multiplier u32   76 msix_table_size u16
//!   78 pci_device_id u16   80 location u32   84 name [u8; 8]
//!   92 pci_subsystem_vendor_id u16   94 pci_subsystem_id u16
//! ```
//!
//! START is the first and only message on a driver's bootstrap channel: the
//! device it is to drive, as `devmgr` read it off the kernel's enumeration,
//! and the driver's end of the ring's control channel `devmgr` made for it.
//! Ring 3 cannot walk configuration space, so START carries where each virtio
//! register block lies in physical memory — the page-aligned start of the
//! pages holding it, inside one of the device's apertures, the block's offset
//! within those pages and its length — which is what `io_mapping_create`
//! takes. A driver handed anything else first exits.

use core::fmt;

use ferrix_native_abi::rights::Rights;

use crate::geometry::{Device, DeviceFlags};
use crate::identity::{DiskName, Identity, Location, NAME_BYTES, SERIAL_BYTES};
use crate::layout::VERSION;

/// HELLO's message type.
pub const HELLO: u32 = 1;
/// READY's message type.
pub const READY: u32 = 2;
/// REFUSED's message type.
pub const REFUSED: u32 = 3;
/// STOP's message type.
pub const STOP: u32 = 4;
/// STOPPED's message type.
pub const STOPPED: u32 = 5;
/// START's message type.
pub const START: u32 = 6;

/// Bytes of the type and length every message starts with, and all of READY,
/// STOP and STOPPED.
pub const HEADER_BYTES: usize = 8;
/// Bytes of HELLO.
pub const HELLO_BYTES: usize = 72;
/// Bytes of REFUSED.
pub const REFUSED_BYTES: usize = 12;
/// Bytes of START.
pub const START_BYTES: usize = 96;
/// Bytes of the longest message.
pub const MAX_BYTES: usize = START_BYTES;

/// Ring pairs a v1 HELLO may announce.
pub const QUEUES: u16 = 1;

/// Exactly the rights the kernel holds a ring or data VMO with: no `DUPLICATE`,
/// no `TRANSFER`.
pub const VMO_RIGHTS: Rights =
    Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0);

/// Exactly the rights each side holds the other's port with.
pub const PORT_RIGHTS: Rights = Rights(Rights::WRITE.0 | Rights::TRANSFER.0);

/// HELLO's handles, in order, with exactly the rights each must carry.
pub const HELLO_RIGHTS: [Rights; 3] = [VMO_RIGHTS, VMO_RIGHTS, PORT_RIGHTS];

/// READY's handle, with exactly the rights it must carry.
pub const READY_RIGHTS: [Rights; 1] = [Rights::WRITE];

/// Exactly the rights a driver holds its end of the ring's control channel
/// with: it sends and receives on it, waits on it and was handed it, and
/// nobody copies it. The kernel inserts the end `block_ring_create` answers
/// with these, and `devmgr` passes it on unchanged.
pub const CONTROL_RIGHTS: Rights =
    Rights(Rights::TRANSFER.0 | Rights::READ.0 | Rights::WRITE.0 | Rights::WAIT.0);

/// Exactly the rights a driver holds its device with: `MANAGE`, for
/// `vmo_pin`, `interrupt_create` and `io_mapping_create`, and the `TRANSFER`
/// it arrived with.
pub const DEVICE_RIGHTS: Rights = Rights(Rights::TRANSFER.0 | Rights::MANAGE.0);

/// START's handles, in order, with exactly the rights each must carry.
pub const START_RIGHTS: [Rights; 2] = [DEVICE_RIGHTS, CONTROL_RIGHTS];

/// Field offsets of HELLO.
pub mod hello {
    /// `type`, `u32`.
    pub const TYPE: usize = 0;
    /// `length`, `u32`.
    pub const LENGTH: usize = 4;
    /// Ring `version`, `u16`.
    pub const VERSION: usize = 8;
    /// `queues`, `u16`.
    pub const QUEUES: usize = 10;
    /// `block_size`, `u32`.
    pub const BLOCK_SIZE: usize = 12;
    /// `capacity`, `u64`, in `block_size` sectors.
    pub const CAPACITY: usize = 16;
    /// `max_sectors`, `u32`.
    pub const MAX_SECTORS: usize = 24;
    /// `device_flags`, `u32`.
    pub const DEVICE_FLAGS: usize = 28;
    /// `data_vmo_size`, `u64`.
    pub const DATA_VMO_SIZE: usize = 32;
    /// `location`, `u32`: the PCI address.
    pub const LOCATION: usize = 40;
    /// `serial`, 20 bytes.
    pub const SERIAL: usize = 44;
    /// `name`, 8 bytes, NUL-padded.
    pub const NAME: usize = 64;
}

/// Field offsets of START, and of a [`Block`] within it.
pub mod start {
    /// `common`, a `Block`: virtio's common configuration.
    pub const COMMON: usize = 8;
    /// `notify`, a `Block`: the notification area.
    pub const NOTIFY: usize = 24;
    /// `isr`, a `Block`: the ISR status byte.
    pub const ISR: usize = 40;
    /// `device`, a `Block`: the device-specific configuration.
    pub const DEVICE: usize = 56;
    /// `notify_off_multiplier`, `u32`.
    pub const NOTIFY_OFF_MULTIPLIER: usize = 72;
    /// `msix_table_size`, `u16`: entries, 0 for a device with a line only.
    pub const MSIX_TABLE_SIZE: usize = 76;
    /// `pci_device_id`, `u16`.
    pub const PCI_DEVICE_ID: usize = 78;
    /// `location`, `u32`: the PCI address, as HELLO carries it.
    pub const LOCATION: usize = 80;
    /// `name`, 8 bytes, NUL-padded: the node name `devmgr` chose.
    pub const NAME: usize = 84;
    /// `pci_subsystem_vendor_id`, `u16`.
    pub const PCI_SUBSYSTEM_VENDOR_ID: usize = 92;
    /// `pci_subsystem_id`, `u16`: which machine made the device, where the
    /// device's own identifier cannot say (`DeviceInfo::subsystem_id`).
    pub const PCI_SUBSYSTEM_ID: usize = 94;

    /// A `Block`'s `phys`, `u64`.
    pub const BLOCK_PHYS: usize = 0;
    /// A `Block`'s `offset`, `u32`.
    pub const BLOCK_OFFSET: usize = 8;
    /// A `Block`'s `length`, `u32`.
    pub const BLOCK_LENGTH: usize = 12;
    /// Bytes of a `Block`.
    pub const BLOCK_BYTES: usize = 16;
}

/// Field offsets of REFUSED beyond the common header.
pub mod refused {
    /// `reason`, `u32`: a [`super::Refusal`].
    pub const REASON: usize = 8;
}

/// Why the kernel refused a HELLO, as REFUSED carries it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Refusal {
    /// The ring version is not 1.
    Version = 1,
    /// `queues` is not 1.
    Queues = 2,
    /// The ring header failed setup validation.
    Header = 3,
    /// A handle is missing, extra, or carries rights other than exactly the
    /// specified ones.
    Rights = 4,
    /// The message is not a well-formed HELLO.
    Malformed = 5,
    /// The device description is not one the kernel can use.
    Device = 6,
    /// `name` is not `vd` and one to three lowercase letters, NUL-padded.
    Name = 7,
    /// `name` names a node the kernel already published. Decided by the
    /// kernel glue, which knows every published node.
    NameInUse = 8,
    /// `location` names a device another accepted driver already serves.
    /// Decided by the kernel glue, which knows every accepted driver.
    LocationInUse = 9,
    /// `location` is not the PCI address of the device node the ring was
    /// created for. Decided by the kernel glue, which knows which node the ring
    /// is bound to; checked after the name and before the registry checks.
    WrongLocation = 10,
}

impl Refusal {
    /// The reason word this refusal is sent as.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self as u32
    }

    /// The refusal a reason word names, if it names one.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Option<Refusal> {
        match raw {
            1 => Some(Refusal::Version),
            2 => Some(Refusal::Queues),
            3 => Some(Refusal::Header),
            4 => Some(Refusal::Rights),
            5 => Some(Refusal::Malformed),
            6 => Some(Refusal::Device),
            7 => Some(Refusal::Name),
            8 => Some(Refusal::NameInUse),
            9 => Some(Refusal::LocationInUse),
            10 => Some(Refusal::WrongLocation),
            _ => None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Refusal::Version => "ring version mismatch",
            Refusal::Queues => "queues is not 1",
            Refusal::Header => "the ring header is invalid",
            Refusal::Rights => "a handle has the wrong rights",
            Refusal::Malformed => "malformed HELLO",
            Refusal::Device => "unusable device description",
            Refusal::Name => "malformed disk name",
            Refusal::NameInUse => "the disk name is already published",
            Refusal::LocationInUse => "another driver already serves this device",
            Refusal::WrongLocation => "the location is not the ring's own device",
        })
    }
}

/// HELLO's fields, as sent and as received.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hello {
    /// The ring version.
    pub version: u16,
    /// Ring pairs.
    pub queues: u16,
    /// Bytes in a logical sector.
    pub block_size: u32,
    /// Sectors on the device.
    pub capacity: u64,
    /// The most sectors one request may carry.
    pub max_sectors: u32,
    /// [`DeviceFlags`], as a word.
    pub device_flags: u32,
    /// Bytes in the data VMO.
    pub data_vmo_size: u64,
    /// [`Location`], as a word.
    pub location: u32,
    /// virtio-blk `GET_ID` bytes; all zero if the device did not answer.
    pub serial: [u8; SERIAL_BYTES],
    /// The node name, NUL-padded; [`DiskName`] checks it.
    pub name: [u8; NAME_BYTES],
}

/// What an accepted HELLO describes: the device and the disk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accepted {
    /// The device's geometry, for [`crate::KernelSide::attach`].
    pub device: Device,
    /// Which disk it is. The glue still refuses a name already published or a
    /// location already served ([`Refusal::NameInUse`],
    /// [`Refusal::LocationInUse`]), and numbers the node from
    /// [`DiskName::minor`].
    pub identity: Identity,
}

impl Hello {
    /// The v1 HELLO for `device`, the disk `identity` names.
    #[must_use]
    pub const fn new(device: &Device, identity: &Identity) -> Self {
        Self {
            version: VERSION,
            queues: QUEUES,
            block_size: device.block_size(),
            capacity: device.capacity(),
            max_sectors: device.max_sectors(),
            device_flags: device.flags().0,
            data_vmo_size: device.data_vmo_size(),
            location: identity.location.0,
            serial: identity.serial,
            name: *identity.name.as_bytes(),
        }
    }

    /// Every check the kernel makes on a HELLO on its own, given the rights of
    /// the handles that came with it, in the order §6.2 fixes: version, queues,
    /// rights, the device description, the name. The first failure is the
    /// refusal sent. The kernel glue's checks come after, on an accepted HELLO:
    /// first that `location` is the ring's own device
    /// ([`Refusal::WrongLocation`]), then the registry checks,
    /// [`Refusal::NameInUse`] and [`Refusal::LocationInUse`].
    ///
    /// Rights must match [`HELLO_RIGHTS`] exactly, and there must be exactly
    /// three: more rights than specified is refused as firmly as fewer. The
    /// glue must also check the handles' object types, which only it can see,
    /// and whether the name or location is already taken. The ring header is
    /// checked afterwards, by [`crate::KernelSide::attach`].
    ///
    /// # Errors
    ///
    /// The [`Refusal`] to send.
    pub fn validate(&self, handle_rights: &[Rights]) -> Result<Accepted, Refusal> {
        self.version_and_queues()?;
        if handle_rights != HELLO_RIGHTS {
            return Err(Refusal::Rights);
        }
        self.described()
    }

    /// [`Hello::validate`]'s checks of the message itself, for a core in user
    /// space (`docs/microkernel/BTRFSD.md` §2), which cannot read a handle's
    /// rights: its rule is `handle_replace` with exactly [`HELLO_RIGHTS`]'s, on
    /// each handle, which refuses fewer and leaves no more.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] to send.
    pub fn validate_message(&self) -> Result<Accepted, Refusal> {
        self.version_and_queues()?;
        self.described()
    }

    const fn version_and_queues(&self) -> Result<(), Refusal> {
        if self.version != VERSION {
            return Err(Refusal::Version);
        }
        if self.queues != QUEUES {
            return Err(Refusal::Queues);
        }
        Ok(())
    }

    fn described(&self) -> Result<Accepted, Refusal> {
        let device = Device::new(
            self.block_size,
            self.capacity,
            self.max_sectors,
            DeviceFlags(self.device_flags),
            self.data_vmo_size,
        )
        .map_err(|_| Refusal::Device)?;
        let name = DiskName::new(self.name).ok_or(Refusal::Name)?;
        Ok(Accepted {
            device,
            identity: Identity {
                location: Location(self.location),
                serial: self.serial,
                name,
            },
        })
    }
}

/// Where a virtio register block lies, as START carries it: the page-aligned
/// physical start of the pages holding it, inside one of the device's
/// apertures, the block's offset within those pages, and its length.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Block {
    /// Physical address of the first page, a multiple of the page size.
    pub phys: u64,
    /// The block's first byte, from `phys`.
    pub offset: u32,
    /// Bytes in the block.
    pub length: u32,
}

/// START's fields: what `devmgr` tells a driver about the device it starts
/// it on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Start {
    /// virtio's common configuration block.
    pub common: Block,
    /// The notification area.
    pub notify: Block,
    /// The ISR status block.
    pub isr: Block,
    /// The device-specific configuration block.
    pub device: Block,
    /// virtio's `notify_off_multiplier`.
    pub notify_off_multiplier: u32,
    /// MSI-X table entries; 0 when the device has a line only.
    pub msix_table_size: u16,
    /// The PCI device identifier, so a driver can refuse a device that is
    /// not its own before touching it.
    pub pci_device_id: u16,
    /// [`Location`], as a word: what HELLO must carry back.
    pub location: u32,
    /// The node name `devmgr` chose, NUL-padded: what HELLO must carry back.
    pub name: [u8; NAME_BYTES],
    /// The PCI subsystem vendor identifier, as `device_info` gave it.
    pub pci_subsystem_vendor_id: u16,
    /// The PCI subsystem identifier: which machine made a virtio device,
    /// where [`Start::pci_device_id`] cannot say. crosvm's GPU reads a 2D
    /// transfer's offset differently from QEMU's, and its driver has to know
    /// which it has.
    pub pci_subsystem_id: u16,
}

/// Why a START is not one a driver may act on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartError {
    /// A handle is missing, extra, or carries rights other than exactly the
    /// specified ones.
    Rights,
    /// `name` is not one HELLO could carry.
    Name,
}

impl Start {
    /// Every check a driver makes on a START, given the rights of the handles
    /// that came with it: exactly [`START_RIGHTS`], and a name HELLO can
    /// carry back. What the blocks describe is checked by mapping them.
    ///
    /// # Errors
    ///
    /// The [`StartError`], on which the driver exits.
    pub fn validate(&self, handle_rights: &[Rights]) -> Result<DiskName, StartError> {
        if handle_rights != START_RIGHTS {
            return Err(StartError::Rights);
        }
        DiskName::new(self.name).ok_or(StartError::Name)
    }
}

/// One control message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// Driver to kernel: here is the device and the ring.
    Hello(Hello),
    /// Kernel to driver: accepted; the completion port rides along.
    Ready,
    /// Kernel to driver: refused, with the reason word. Carried raw so that a
    /// driver can log a reason newer than itself; [`Refusal::from_raw`] names
    /// the known ones.
    Refused(u32),
    /// Kernel to driver: finish up and reset.
    Stop,
    /// Driver to kernel: finished, device reset.
    Stopped,
    /// `devmgr` to driver: this device, this ring.
    Start(Start),
}

/// Why bytes on the control channel are not a message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageError {
    /// Shorter than the type and length.
    Short,
    /// The type is not one v1 defines.
    UnknownType(u32),
    /// A HELLO for a ring version other than 1. Reported ahead of any length
    /// mismatch, since a newer version's HELLO may well be longer.
    Version(u16),
    /// The length field disagrees with the bytes received, or with the size
    /// this type has.
    Length,
}

impl MessageError {
    /// What the kernel sends back for a HELLO that failed to decode.
    #[must_use]
    pub const fn refusal(self) -> Refusal {
        match self {
            MessageError::Version(_) => Refusal::Version,
            MessageError::Short | MessageError::UnknownType(_) | MessageError::Length => {
                Refusal::Malformed
            }
        }
    }
}

impl fmt::Display for MessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MessageError::Short => formatter.write_str("shorter than a message header"),
            MessageError::UnknownType(kind) => write!(formatter, "unknown message type {kind}"),
            MessageError::Version(version) => write!(formatter, "ring version {version}"),
            MessageError::Length => formatter.write_str("wrong length"),
        }
    }
}

/// An encoded message: at most [`MAX_BYTES`], no allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MAX_BYTES],
    len: usize,
}

impl Encoded {
    /// The bytes to send.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl Message {
    /// The handles this message carries.
    #[must_use]
    pub const fn handles(&self) -> usize {
        match self {
            Message::Hello(_) => HELLO_RIGHTS.len(),
            Message::Ready => READY_RIGHTS.len(),
            Message::Refused(_) | Message::Stop | Message::Stopped => 0,
            Message::Start(_) => START_RIGHTS.len(),
        }
    }

    /// Encode the message.
    #[must_use]
    pub fn encode(&self) -> Encoded {
        let mut bytes = [0; MAX_BYTES];
        let (kind, len) = match self {
            Message::Hello(hello) => {
                encode_hello(&mut bytes, hello);
                (HELLO, HELLO_BYTES)
            }
            Message::Ready => (READY, HEADER_BYTES),
            Message::Refused(reason) => {
                put(&mut bytes, refused::REASON, &reason.to_le_bytes());
                (REFUSED, REFUSED_BYTES)
            }
            Message::Stop => (STOP, HEADER_BYTES),
            Message::Stopped => (STOPPED, HEADER_BYTES),
            Message::Start(start) => {
                encode_start(&mut bytes, start);
                (START, START_BYTES)
            }
        };
        put(&mut bytes, hello::TYPE, &kind.to_le_bytes());
        // Every length is at most `MAX_BYTES`.
        put(&mut bytes, hello::LENGTH, &(len as u32).to_le_bytes());
        Encoded { bytes, len }
    }

    /// Decode a message received on the control channel.
    ///
    /// # Errors
    ///
    /// The [`MessageError`] saying why the bytes are not a message.
    /// [`MessageError::refusal`] names the reply to a bad HELLO.
    pub fn decode(bytes: &[u8]) -> Result<Message, MessageError> {
        let (Some(kind), Some(declared)) =
            (u32_at(bytes, hello::TYPE), u32_at(bytes, hello::LENGTH))
        else {
            return Err(MessageError::Short);
        };
        let size = match kind {
            HELLO => HELLO_BYTES,
            READY | STOP | STOPPED => HEADER_BYTES,
            REFUSED => REFUSED_BYTES,
            START => START_BYTES,
            other => return Err(MessageError::UnknownType(other)),
        };
        if kind == HELLO
            && let Some(version) = u16_at(bytes, hello::VERSION)
            && version != VERSION
        {
            return Err(MessageError::Version(version));
        }
        if usize::try_from(declared) != Ok(bytes.len()) || bytes.len() != size {
            return Err(MessageError::Length);
        }
        decode_body(kind, bytes).ok_or(MessageError::Length)
    }
}

/// HELLO's fields, after the type and length.
fn encode_hello(bytes: &mut [u8], hello: &Hello) {
    put(bytes, hello::VERSION, &hello.version.to_le_bytes());
    put(bytes, hello::QUEUES, &hello.queues.to_le_bytes());
    put(bytes, hello::BLOCK_SIZE, &hello.block_size.to_le_bytes());
    put(bytes, hello::CAPACITY, &hello.capacity.to_le_bytes());
    put(bytes, hello::MAX_SECTORS, &hello.max_sectors.to_le_bytes());
    put(
        bytes,
        hello::DEVICE_FLAGS,
        &hello.device_flags.to_le_bytes(),
    );
    put(
        bytes,
        hello::DATA_VMO_SIZE,
        &hello.data_vmo_size.to_le_bytes(),
    );
    put(bytes, hello::LOCATION, &hello.location.to_le_bytes());
    put(bytes, hello::SERIAL, &hello.serial);
    put(bytes, hello::NAME, &hello.name);
}

/// START's fields, after the type and length.
fn encode_start(bytes: &mut [u8], message: &Start) {
    for (at, block) in [
        (start::COMMON, &message.common),
        (start::NOTIFY, &message.notify),
        (start::ISR, &message.isr),
        (start::DEVICE, &message.device),
    ] {
        put(bytes, at + start::BLOCK_PHYS, &block.phys.to_le_bytes());
        put(bytes, at + start::BLOCK_OFFSET, &block.offset.to_le_bytes());
        put(bytes, at + start::BLOCK_LENGTH, &block.length.to_le_bytes());
    }
    put(
        bytes,
        start::NOTIFY_OFF_MULTIPLIER,
        &message.notify_off_multiplier.to_le_bytes(),
    );
    put(
        bytes,
        start::MSIX_TABLE_SIZE,
        &message.msix_table_size.to_le_bytes(),
    );
    put(
        bytes,
        start::PCI_DEVICE_ID,
        &message.pci_device_id.to_le_bytes(),
    );
    put(bytes, start::LOCATION, &message.location.to_le_bytes());
    put(bytes, start::NAME, &message.name);
    put(
        bytes,
        start::PCI_SUBSYSTEM_VENDOR_ID,
        &message.pci_subsystem_vendor_id.to_le_bytes(),
    );
    put(
        bytes,
        start::PCI_SUBSYSTEM_ID,
        &message.pci_subsystem_id.to_le_bytes(),
    );
}

/// A [`Block`] at `at`.
fn block_at(bytes: &[u8], at: usize) -> Option<Block> {
    Some(Block {
        phys: u64_at(bytes, at.checked_add(start::BLOCK_PHYS)?)?,
        offset: u32_at(bytes, at.checked_add(start::BLOCK_OFFSET)?)?,
        length: u32_at(bytes, at.checked_add(start::BLOCK_LENGTH)?)?,
    })
}

/// The body of a message whose type and length have been checked.
fn decode_body(kind: u32, bytes: &[u8]) -> Option<Message> {
    Some(match kind {
        HELLO => Message::Hello(Hello {
            version: u16_at(bytes, hello::VERSION)?,
            queues: u16_at(bytes, hello::QUEUES)?,
            block_size: u32_at(bytes, hello::BLOCK_SIZE)?,
            capacity: u64_at(bytes, hello::CAPACITY)?,
            max_sectors: u32_at(bytes, hello::MAX_SECTORS)?,
            device_flags: u32_at(bytes, hello::DEVICE_FLAGS)?,
            data_vmo_size: u64_at(bytes, hello::DATA_VMO_SIZE)?,
            location: u32_at(bytes, hello::LOCATION)?,
            serial: array_at(bytes, hello::SERIAL)?,
            name: array_at(bytes, hello::NAME)?,
        }),
        READY => Message::Ready,
        REFUSED => Message::Refused(u32_at(bytes, refused::REASON)?),
        STOP => Message::Stop,
        STOPPED => Message::Stopped,
        START => Message::Start(Start {
            common: block_at(bytes, start::COMMON)?,
            notify: block_at(bytes, start::NOTIFY)?,
            isr: block_at(bytes, start::ISR)?,
            device: block_at(bytes, start::DEVICE)?,
            notify_off_multiplier: u32_at(bytes, start::NOTIFY_OFF_MULTIPLIER)?,
            msix_table_size: u16_at(bytes, start::MSIX_TABLE_SIZE)?,
            pci_device_id: u16_at(bytes, start::PCI_DEVICE_ID)?,
            location: u32_at(bytes, start::LOCATION)?,
            name: array_at(bytes, start::NAME)?,
            pci_subsystem_vendor_id: u16_at(bytes, start::PCI_SUBSYSTEM_VENDOR_ID)?,
            pci_subsystem_id: u16_at(bytes, start::PCI_SUBSYSTEM_ID)?,
        }),
        _ => return None,
    })
}

/// Copy `source` into `out` at `at`, as far as `out` reaches.
fn put(out: &mut [u8], at: usize, source: &[u8]) {
    if let Some(destination) = out.get_mut(at..) {
        for (to, from) in destination.iter_mut().zip(source) {
            *to = *from;
        }
    }
}

fn array_at<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    array_at(bytes, at).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    array_at(bytes, at).map(u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    array_at(bytes, at).map(u64::from_le_bytes)
}
