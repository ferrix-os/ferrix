//! The USB interface protocol: how the process that owns a USB host
//! controller (`usbhid`) hands one interface of one device to a class
//! driver in a process of its own, and carries that driver's transfers
//! (`docs/USB-SERIAL.md` §6.1).
//!
//! Three parties, two channels:
//!
//! * **host → devmgr**, on the host's bootstrap channel: [`Message::Attach`]
//!   for each interface the host drives no class of itself, with the
//!   driver's end of a new *interface channel*. devmgr matches the
//!   identity against its table ([`Identity`]) and starts the class driver
//!   with that end as its bootstrap channel, or keeps it unanswered.
//! * **host ↔ class driver**, on the interface channel. The host's first
//!   message is [`Message::Hello`], with a VMO of [`VMO_BYTES`] both map:
//!   the data of control transfers and bulk transfers goes through it, in
//!   the regions below, and the messages carry only where and how much.
//!   The host owns the controller's DMA memory and copies between it and
//!   the VMO; no page of the class driver's is ever pinned for a device.
//!
//! A class driver's end closing -- it exited or died -- is how the host
//! hears it went: it closes the streams the driver opened and attaches the
//! interface again, on a new channel, which is how devmgr's restart of a
//! dead driver happens. The host's end closing is the device going, or the
//! host dying: the class driver ends.
//!
//! # What a class driver may do
//!
//! [`Message::Control`] carries any control request [`allowed`] lets
//! through, the rules usbfs applies to a claimed interface: none that
//! changes the device's address or configuration, and none aimed at
//! another interface or another interface's endpoint. [`Message::Open`]
//! opens one of the interface's own bulk endpoints as a stream: IN
//! chunks arrive as [`Message::In`] in slots of the VMO, each held until
//! [`Message::Free`] gives it back, so a driver that reads slowly holds
//! the device back rather than losing bytes; OUT goes as
//! [`Message::Send`] from one of two slots, answered [`Message::Sent`]
//! once every byte is queued to the controller, [`Message::Empty`] once
//! the device has taken them all.
//!
//! Every message is a fixed little-endian structure whose first byte is
//! its kind, decoded strictly. Nothing here sends, waits or allocates.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// The version a HELLO names.
pub const VERSION: u8 = 1;

/// The shared VMO's size.
pub const VMO_BYTES: usize = 32 * 1024;
/// Where a control transfer's data stage is, and its most bytes.
pub const CONTROL_AT: usize = 0;
/// The longest control data stage.
pub const CONTROL_BYTES: usize = 1024;
/// Where the OUT slots start; [`OUT_SLOTS`] of [`OUT_SLOT_BYTES`].
pub const OUT_AT: usize = 4096;
/// How many OUT slots: one being sent while the next is filled.
pub const OUT_SLOTS: usize = 2;
/// An OUT slot's size.
pub const OUT_SLOT_BYTES: usize = 4096;
/// Where the IN slots start; [`IN_SLOTS`] of [`IN_SLOT_BYTES`].
pub const IN_AT: usize = 16 * 1024;
/// How many IN slots.
pub const IN_SLOTS: usize = 16;
/// An IN slot's size: one request of a stream, a high-speed packet.
pub const IN_SLOT_BYTES: usize = 512;

const _: () = assert!(
    OUT_AT + OUT_SLOTS * OUT_SLOT_BYTES <= IN_AT
        && IN_AT + IN_SLOTS * IN_SLOT_BYTES <= VMO_BYTES
        && CONTROL_AT + CONTROL_BYTES <= OUT_AT,
    "the regions fit the VMO and do not overlap"
);

/// The most endpoints a HELLO lists.
pub const MAX_ENDPOINTS: usize = 4;
/// The longest string kept of a device's.
pub const TEXT_BYTES: usize = 32;
/// The longest message.
pub const MAX_BYTES: usize = 256;

const KIND_ATTACH: u8 = 1;
const KIND_HELLO: u8 = 2;
const KIND_DONE: u8 = 3;
const KIND_IN: u8 = 4;
const KIND_SENT: u8 = 5;
const KIND_EMPTY: u8 = 6;
const KIND_LOST: u8 = 7;
const KIND_BIND: u8 = 8;
const KIND_CONTROL: u8 = 16;
const KIND_OPEN: u8 = 17;
const KIND_SEND: u8 = 18;
const KIND_FREE: u8 = 19;

/// A device's string, as ASCII, cut to [`TEXT_BYTES`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Text {
    bytes: [u8; TEXT_BYTES],
    len: usize,
}

impl Text {
    /// `bytes`, cut to [`TEXT_BYTES`]; anything but printable ASCII becomes
    /// `_`, so a name made of it is a path segment.
    #[must_use]
    pub fn new(bytes: &[u8]) -> Text {
        let mut text = Text::default();
        for (slot, &byte) in text.bytes.iter_mut().zip(bytes) {
            *slot = if byte.is_ascii_graphic() || byte == b' ' {
                byte
            } else {
                b'_'
            };
            text.len += 1;
        }
        text
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// What an exported interface is.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Identity {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// `bInterfaceClass`, `bInterfaceSubClass`, `bInterfaceProtocol`.
    pub class: u8,
    /// See `class`.
    pub subclass: u8,
    /// See `class`.
    pub protocol: u8,
    /// `bInterfaceNumber`.
    pub interface: u8,
    /// 0 low, 1 full, 2 high, 3 super speed.
    pub speed: u8,
    /// Where the device is: the root port from one in the top byte, then
    /// the hub ports below it, four bits each, nearest the root lowest.
    pub path: u32,
    /// The manufacturer's string.
    pub manufacturer: Text,
    /// The product's string.
    pub product_name: Text,
    /// The serial number's string.
    pub serial: Text,
}

/// An endpoint of the interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Endpoint {
    /// `bEndpointAddress`: the number, and bit 7 for IN.
    pub address: u8,
    /// `bmAttributes`: the transfer type in bits 1..0, 2 for bulk.
    pub attributes: u8,
    /// `wMaxPacketSize`.
    pub max_packet: u16,
    /// `bInterval`.
    pub interval: u8,
}

impl Endpoint {
    /// Whether it is a bulk endpoint.
    #[must_use]
    pub const fn is_bulk(&self) -> bool {
        self.attributes & 3 == 2
    }

    /// Whether it is IN.
    #[must_use]
    pub const fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
}

/// How a request ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// It did.
    Ok,
    /// [`allowed`] refused it, or it named an endpoint not the interface's.
    Refused,
    /// The device is gone.
    Gone,
    /// The device stalled it.
    Stall,
    /// It did not finish in time.
    Timeout,
    /// It failed otherwise.
    Failed,
    /// A slot or a stream is in use.
    Busy,
    /// Its fields made no sense: a length past a region, a slot that is
    /// not one.
    Invalid,
}

impl Status {
    const fn code(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::Refused => 1,
            Status::Gone => 2,
            Status::Stall => 3,
            Status::Timeout => 4,
            Status::Failed => 5,
            Status::Busy => 6,
            Status::Invalid => 7,
        }
    }

    const fn from_code(code: u8) -> Option<Status> {
        Some(match code {
            0 => Status::Ok,
            1 => Status::Refused,
            2 => Status::Gone,
            3 => Status::Stall,
            4 => Status::Timeout,
            5 => Status::Failed,
            6 => Status::Busy,
            7 => Status::Invalid,
            _ => return None,
        })
    }
}

/// A message on either channel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Message {
    /// Host → devmgr: an interface to bind, with the driver's end of its
    /// channel. `key` is the host's, the same for one plugged-in device's
    /// interface across its drivers' restarts and new at every plug.
    Attach {
        /// The interface's key.
        key: u64,
        /// What it is.
        identity: Identity,
    },
    /// devmgr → class driver, its first message on its bootstrap channel,
    /// with the interface channel: the number devmgr gives the device it
    /// drives, which names it (`ttyUSB<number>`) and stays the same across
    /// the driver's restarts.
    Bind {
        /// The number.
        number: u8,
    },
    /// Host → class driver, first: the interface, with the shared VMO.
    Hello {
        /// As ATTACH's.
        key: u64,
        /// What it is.
        identity: Identity,
        /// How many of `endpoints` there are.
        count: usize,
        /// Its endpoints.
        endpoints: [Endpoint; MAX_ENDPOINTS],
    },
    /// Host → driver: a CONTROL or OPEN ended; `value` is how many bytes a
    /// control data stage moved.
    Done {
        /// The request's tag.
        tag: u32,
        /// How it ended.
        status: Status,
        /// What it gave.
        value: u32,
    },
    /// Host → driver: IN slot `slot` holds `len` bytes from `endpoint`.
    In {
        /// The endpoint's address.
        endpoint: u8,
        /// The slot.
        slot: u8,
        /// How many bytes.
        len: u16,
    },
    /// Host → driver: SEND `tag` is queued whole; its slot is free.
    Sent {
        /// The SEND's tag.
        tag: u32,
        /// How it ended: [`Status::Ok`], or why the rest was dropped.
        status: Status,
    },
    /// Host → driver: everything sent to `endpoint` was taken by the
    /// device.
    Empty {
        /// The endpoint's address.
        endpoint: u8,
    },
    /// Host → driver: the endpoint stalled and `bytes` it held were lost.
    Lost {
        /// The endpoint's address.
        endpoint: u8,
        /// How many.
        bytes: u32,
    },
    /// Driver → host: a control transfer, its data stage at [`CONTROL_AT`].
    Control {
        /// Answered in DONE.
        tag: u32,
        /// The setup packet.
        setup: [u8; 8],
    },
    /// Driver → host: open bulk endpoint `endpoint` as a stream.
    Open {
        /// Answered in DONE.
        tag: u32,
        /// The endpoint's address.
        endpoint: u8,
    },
    /// Driver → host: send `len` bytes of OUT slot `slot` to `endpoint`.
    Send {
        /// Answered in SENT.
        tag: u32,
        /// The endpoint's address.
        endpoint: u8,
        /// The slot.
        slot: u8,
        /// How many bytes.
        len: u16,
    },
    /// Driver → host: IN slot `slot` is read and may be filled again.
    Free {
        /// The slot.
        slot: u8,
    },
}

/// Why bytes did not decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Malformed;

/// An encoded message.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MAX_BYTES],
    len: usize,
}

impl Encoded {
    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

struct Writer {
    bytes: [u8; MAX_BYTES],
    at: usize,
}

impl Writer {
    fn put(&mut self, source: &[u8]) {
        if let Some(slot) = self.bytes.get_mut(self.at..self.at + source.len()) {
            slot.copy_from_slice(source);
        }
        self.at += source.len();
    }

    fn u8(&mut self, value: u8) {
        self.put(&[value]);
    }

    fn u16(&mut self, value: u16) {
        self.put(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.put(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.put(&value.to_le_bytes());
    }

    fn text(&mut self, text: &Text) {
        self.u8(u8::try_from(text.len).unwrap_or(0));
        self.put(&text.bytes);
    }

    fn identity(&mut self, identity: &Identity) {
        self.u16(identity.vendor);
        self.u16(identity.product);
        self.u16(identity.release);
        self.put(&[
            identity.class,
            identity.subclass,
            identity.protocol,
            identity.interface,
            identity.speed,
        ]);
        self.u32(identity.path);
        self.text(&identity.manufacturer);
        self.text(&identity.product_name);
        self.text(&identity.serial);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Malformed> {
        let slice = self.bytes.get(self.at..self.at + N).ok_or(Malformed)?;
        self.at += N;
        let mut out = [0; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(u8::from_le_bytes(self.take()?))
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        Ok(u16::from_le_bytes(self.take()?))
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, Malformed> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn text(&mut self) -> Result<Text, Malformed> {
        let len = usize::from(self.u8()?);
        let bytes: [u8; TEXT_BYTES] = self.take()?;
        if len > TEXT_BYTES || bytes.iter().skip(len).any(|&byte| byte != 0) {
            return Err(Malformed);
        }
        Ok(Text { bytes, len })
    }

    fn identity(&mut self) -> Result<Identity, Malformed> {
        let vendor = self.u16()?;
        let product = self.u16()?;
        let release = self.u16()?;
        let [class, subclass, protocol, interface, speed] = self.take()?;
        if speed > 3 {
            return Err(Malformed);
        }
        Ok(Identity {
            vendor,
            product,
            release,
            class,
            subclass,
            protocol,
            interface,
            speed,
            path: self.u32()?,
            manufacturer: self.text()?,
            product_name: self.text()?,
            serial: self.text()?,
        })
    }

    fn status(&mut self) -> Result<Status, Malformed> {
        Status::from_code(self.u8()?).ok_or(Malformed)
    }

    fn done(&self) -> Result<(), Malformed> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(Malformed)
        }
    }
}

fn in_slot(slot: u8) -> Result<u8, Malformed> {
    if usize::from(slot) < IN_SLOTS {
        Ok(slot)
    } else {
        Err(Malformed)
    }
}

impl Message {
    /// The message's bytes.
    #[must_use]
    pub fn encode(&self) -> Encoded {
        let mut out = Writer {
            bytes: [0; MAX_BYTES],
            at: 0,
        };
        match self {
            Message::Attach { key, identity } => {
                out.u8(KIND_ATTACH);
                out.u64(*key);
                out.identity(identity);
            }
            Message::Hello {
                key,
                identity,
                count,
                endpoints,
            } => {
                out.u8(KIND_HELLO);
                out.u8(VERSION);
                out.u64(*key);
                out.identity(identity);
                let count = (*count).min(MAX_ENDPOINTS);
                out.u8(u8::try_from(count).unwrap_or(0));
                for endpoint in endpoints.iter().take(count) {
                    out.u8(endpoint.address);
                    out.u8(endpoint.attributes);
                    out.u16(endpoint.max_packet);
                    out.u8(endpoint.interval);
                }
            }
            Message::Bind { number } => {
                out.u8(KIND_BIND);
                out.u8(*number);
            }
            Message::Done { tag, status, value } => {
                out.u8(KIND_DONE);
                out.u32(*tag);
                out.u8(status.code());
                out.u32(*value);
            }
            Message::In {
                endpoint,
                slot,
                len,
            } => {
                out.u8(KIND_IN);
                out.u8(*endpoint);
                out.u8(*slot);
                out.u16(*len);
            }
            Message::Sent { tag, status } => {
                out.u8(KIND_SENT);
                out.u32(*tag);
                out.u8(status.code());
            }
            Message::Empty { endpoint } => {
                out.u8(KIND_EMPTY);
                out.u8(*endpoint);
            }
            Message::Lost { endpoint, bytes } => {
                out.u8(KIND_LOST);
                out.u8(*endpoint);
                out.u32(*bytes);
            }
            Message::Control { tag, setup } => {
                out.u8(KIND_CONTROL);
                out.u32(*tag);
                out.put(setup);
            }
            Message::Open { tag, endpoint } => {
                out.u8(KIND_OPEN);
                out.u32(*tag);
                out.u8(*endpoint);
            }
            Message::Send {
                tag,
                endpoint,
                slot,
                len,
            } => {
                out.u8(KIND_SEND);
                out.u32(*tag);
                out.u8(*endpoint);
                out.u8(*slot);
                out.u16(*len);
            }
            Message::Free { slot } => {
                out.u8(KIND_FREE);
                out.u8(*slot);
            }
        }
        Encoded {
            bytes: out.bytes,
            len: out.at.min(MAX_BYTES),
        }
    }

    /// Decode `bytes`, strictly.
    ///
    /// # Errors
    ///
    /// [`Malformed`] for an unknown kind, a length other than the kind's, a
    /// field out of range: a slot past its region, a length past a slot, a
    /// HELLO of another version or more than [`MAX_ENDPOINTS`] endpoints.
    pub fn decode(bytes: &[u8]) -> Result<Message, Malformed> {
        let mut read = Reader { bytes, at: 0 };
        let message = match read.u8()? {
            KIND_ATTACH => Message::Attach {
                key: read.u64()?,
                identity: read.identity()?,
            },
            KIND_HELLO => {
                if read.u8()? != VERSION {
                    return Err(Malformed);
                }
                let key = read.u64()?;
                let identity = read.identity()?;
                let count = usize::from(read.u8()?);
                if count > MAX_ENDPOINTS {
                    return Err(Malformed);
                }
                let mut endpoints = [Endpoint::default(); MAX_ENDPOINTS];
                for endpoint in endpoints.iter_mut().take(count) {
                    *endpoint = Endpoint {
                        address: read.u8()?,
                        attributes: read.u8()?,
                        max_packet: read.u16()?,
                        interval: read.u8()?,
                    };
                }
                Message::Hello {
                    key,
                    identity,
                    count,
                    endpoints,
                }
            }
            KIND_BIND => Message::Bind { number: read.u8()? },
            KIND_DONE => Message::Done {
                tag: read.u32()?,
                status: read.status()?,
                value: read.u32()?,
            },
            KIND_IN => {
                let endpoint = read.u8()?;
                let slot = in_slot(read.u8()?)?;
                let len = read.u16()?;
                if usize::from(len) > IN_SLOT_BYTES {
                    return Err(Malformed);
                }
                Message::In {
                    endpoint,
                    slot,
                    len,
                }
            }
            KIND_SENT => Message::Sent {
                tag: read.u32()?,
                status: read.status()?,
            },
            KIND_EMPTY => Message::Empty {
                endpoint: read.u8()?,
            },
            KIND_LOST => Message::Lost {
                endpoint: read.u8()?,
                bytes: read.u32()?,
            },
            KIND_CONTROL => Message::Control {
                tag: read.u32()?,
                setup: read.take()?,
            },
            KIND_OPEN => Message::Open {
                tag: read.u32()?,
                endpoint: read.u8()?,
            },
            KIND_SEND => {
                let tag = read.u32()?;
                let endpoint = read.u8()?;
                let slot = read.u8()?;
                let len = read.u16()?;
                if usize::from(slot) >= OUT_SLOTS || usize::from(len) > OUT_SLOT_BYTES {
                    return Err(Malformed);
                }
                Message::Send {
                    tag,
                    endpoint,
                    slot,
                    len,
                }
            }
            KIND_FREE => Message::Free {
                slot: in_slot(read.u8()?)?,
            },
            _ => return Err(Malformed),
        };
        read.done()?;
        Ok(message)
    }
}

/// `bmRequestType`'s recipient bits.
const RECIPIENT: u8 = 0x1F;
const TO_DEVICE: u8 = 0;
const TO_INTERFACE: u8 = 1;
const TO_ENDPOINT: u8 = 2;
/// `bmRequestType`'s type bits: standard.
const TYPE: u8 = 0x60;
const STANDARD: u8 = 0;

/// The standard requests a class driver may send its device: reads alone.
/// An endpoint's halt is cleared by the host, which resets its own side of
/// the endpoint with it, and alternate settings are not driven.
const GET_STATUS: u8 = 0;
const GET_DESCRIPTOR: u8 = 6;
const GET_INTERFACE: u8 = 10;

/// Whether a class driver of interface `interface`, with `endpoints`, may
/// send the control request `setup`, as usbfs decides for a claimed
/// interface:
///
/// * to the device, a class or vendor request -- an FTDI chip's are all
///   vendor requests to the device -- or a standard read (`GET_STATUS`,
///   `GET_DESCRIPTOR`); never `SET_ADDRESS`, `SET_CONFIGURATION` or
///   `SET_FEATURE`, which belong to the host;
/// * to an interface, only to its own, `wIndex`'s low byte its number, and
///   of the standard requests only the reads;
/// * to an endpoint, only to one of its own, by address, and of the
///   standard requests only `GET_STATUS`;
/// * to anything else, nothing.
#[must_use]
pub fn allowed(setup: &[u8; 8], interface: u8, endpoints: &[Endpoint]) -> bool {
    let [request_type, request, _, _, index, ..] = *setup;
    let standard = request_type & TYPE == STANDARD;
    match request_type & RECIPIENT {
        TO_DEVICE => !standard || matches!(request, GET_STATUS | GET_DESCRIPTOR),
        TO_INTERFACE => {
            index == interface
                && (!standard || matches!(request, GET_STATUS | GET_DESCRIPTOR | GET_INTERFACE))
        }
        TO_ENDPOINT => {
            endpoints.iter().any(|endpoint| endpoint.address == index)
                && (!standard || request == GET_STATUS)
        }
        _ => false,
    }
}
