//! The standard requests and descriptors of USB 2.0's chapter 9, and the two
//! HID class requests the boot protocol needs.

/// A device's speed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    /// 1.5 Mbit/s.
    Low,
    /// 12 Mbit/s.
    Full,
    /// 480 Mbit/s.
    High,
    /// 5 Gbit/s and faster: USB 3's `SuperSpeed` and `SuperSpeedPlus`, which
    /// differ in nothing this driver does.
    Super,
}

impl Speed {
    /// The largest control packet every device of this speed takes before
    /// its device descriptor has said its own.
    #[must_use]
    pub const fn default_max_packet(self) -> u16 {
        match self {
            Speed::Super => 512,
            Speed::High => 64,
            Speed::Low | Speed::Full => 8,
        }
    }
}

/// An eight-byte SETUP packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Setup {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`.
    pub length: u16,
}

/// `bmRequestType`: device to host.
pub const DIRECTION_IN: u8 = 0x80;
/// `bmRequestType`: a class request.
pub const TYPE_CLASS: u8 = 0x20;
/// `bmRequestType`: to an interface.
pub const RECIPIENT_INTERFACE: u8 = 0x01;
/// `bmRequestType`: to another recipient: a hub's port.
pub const RECIPIENT_OTHER: u8 = 0x03;

/// `GET_STATUS`.
pub const GET_STATUS: u8 = 0;
/// `CLEAR_FEATURE`.
pub const CLEAR_FEATURE: u8 = 1;
/// `SET_FEATURE`.
pub const SET_FEATURE: u8 = 3;
/// `SET_ADDRESS`.
pub const SET_ADDRESS: u8 = 5;
/// `GET_DESCRIPTOR`.
pub const GET_DESCRIPTOR: u8 = 6;
/// `SET_CONFIGURATION`.
pub const SET_CONFIGURATION: u8 = 9;
/// HID's `SET_IDLE`.
pub const SET_IDLE: u8 = 0x0A;
/// HID's `SET_PROTOCOL`.
pub const SET_PROTOCOL: u8 = 0x0B;
/// HID's `SET_REPORT`.
pub const SET_REPORT: u8 = 0x09;

/// The device descriptor's type.
pub const DEVICE: u8 = 1;
/// The configuration descriptor's.
pub const CONFIGURATION: u8 = 2;
/// A string descriptor's.
pub const STRING: u8 = 3;
/// An interface descriptor's.
pub const INTERFACE: u8 = 4;
/// An endpoint descriptor's.
pub const ENDPOINT: u8 = 5;
/// The hub class's descriptor's.
pub const HUB: u8 = 0x29;
/// The HID class descriptor's, which follows a HID interface's.
pub const HID: u8 = 0x21;
/// A HID report descriptor's.
pub const REPORT: u8 = 0x22;
/// `SET_REPORT`'s report type for an output report.
pub const REPORT_OUTPUT: u8 = 2;

/// The hub class.
pub const CLASS_HUB: u8 = 9;
/// The HID class.
pub const CLASS_HID: u8 = 3;
/// HID's boot interface subclass.
pub const SUBCLASS_BOOT: u8 = 1;
/// A boot interface's protocol: a keyboard.
pub const PROTOCOL_KEYBOARD: u8 = 1;
/// A boot interface's protocol: a mouse.
pub const PROTOCOL_MOUSE: u8 = 2;

/// US English, the language a device is asked for its strings in when it
/// lists none.
pub const LANGUAGE_US: u16 = 0x0409;

impl Setup {
    /// The packet's bytes, little-endian as the bus carries them.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        let index = self.index.to_le_bytes();
        let length = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            value[0],
            value[1],
            index[0],
            index[1],
            length[0],
            length[1],
        ]
    }

    /// Whether the data stage, if any, is device to host.
    #[must_use]
    pub const fn is_in(&self) -> bool {
        self.request_type & DIRECTION_IN != 0
    }

    /// `GET_DESCRIPTOR` of a standard descriptor.
    #[must_use]
    pub const fn get_descriptor(kind: u8, index: u8, language: u16, length: u16) -> Setup {
        Setup {
            request_type: DIRECTION_IN,
            request: GET_DESCRIPTOR,
            value: ((kind as u16) << 8) | index as u16,
            index: language,
            length,
        }
    }

    /// `SET_ADDRESS`.
    #[must_use]
    pub const fn set_address(address: u8) -> Setup {
        Setup {
            request_type: 0,
            request: SET_ADDRESS,
            value: address as u16,
            index: 0,
            length: 0,
        }
    }

    /// `CLEAR_FEATURE(ENDPOINT_HALT)` to the endpoint at `address`, its
    /// direction bit included.
    #[must_use]
    pub const fn clear_endpoint_halt(address: u8) -> Setup {
        Setup {
            // Recipient: endpoint. The feature selector, ENDPOINT_HALT, is 0.
            request_type: 0x02,
            request: CLEAR_FEATURE,
            value: 0,
            index: address as u16,
            length: 0,
        }
    }

    /// `SET_CONFIGURATION`.
    #[must_use]
    pub const fn set_configuration(value: u8) -> Setup {
        Setup {
            request_type: 0,
            request: SET_CONFIGURATION,
            value: value as u16,
            index: 0,
            length: 0,
        }
    }

    /// HID's `SET_PROTOCOL` of the boot protocol, which is protocol 0.
    #[must_use]
    pub const fn set_boot_protocol(interface: u8) -> Setup {
        Setup {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_PROTOCOL,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }

    /// HID's `SET_PROTOCOL` of the report protocol, protocol 1: reports as
    /// the report descriptor lays them out.
    #[must_use]
    pub const fn set_report_protocol(interface: u8) -> Setup {
        Setup {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_PROTOCOL,
            value: 1,
            index: interface as u16,
            length: 0,
        }
    }

    /// `GET_DESCRIPTOR` of an interface's report descriptor.
    #[must_use]
    pub const fn get_report_descriptor(interface: u8, length: u16) -> Setup {
        Setup {
            request_type: DIRECTION_IN | RECIPIENT_INTERFACE,
            request: GET_DESCRIPTOR,
            value: (REPORT as u16) << 8,
            index: interface as u16,
            length,
        }
    }

    /// HID's `SET_REPORT` of output report `report`, `length` bytes.
    #[must_use]
    pub const fn set_output_report(interface: u8, report: u8, length: u16) -> Setup {
        Setup {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_REPORT,
            value: ((REPORT_OUTPUT as u16) << 8) | report as u16,
            index: interface as u16,
            length,
        }
    }

    /// HID's `SET_IDLE` with a duration of zero: report only on a change.
    #[must_use]
    pub const fn set_idle_forever(interface: u8) -> Setup {
        Setup {
            request_type: TYPE_CLASS | RECIPIENT_INTERFACE,
            request: SET_IDLE,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }
}

/// What a device descriptor says that the driver uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeviceDescriptor {
    /// `bDeviceClass`.
    pub class: u8,
    /// `bMaxPacketSize0`.
    pub max_packet: u8,
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// `bcdDevice`.
    pub release: u16,
    /// `iManufacturer`.
    pub manufacturer: u8,
    /// `iProduct`.
    pub product_name: u8,
    /// `iSerialNumber`: zero for none. The bulk-only transport requires a
    /// mass-storage device to have one (its section 4.1.1).
    pub serial: u8,
}

impl DeviceDescriptor {
    /// Read the eighteen bytes of a device descriptor.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<DeviceDescriptor> {
        let word = |at: usize| Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
        if bytes.len() < 18 || bytes.get(1) != Some(&DEVICE) {
            return None;
        }
        Some(DeviceDescriptor {
            class: *bytes.get(4)?,
            max_packet: *bytes.get(7)?,
            vendor: word(8)?,
            product: word(10)?,
            release: word(12)?,
            manufacturer: *bytes.get(14)?,
            product_name: *bytes.get(15)?,
            serial: *bytes.get(16)?,
        })
    }
}

/// Whether `size` is a control endpoint's packet size a device may have.
#[must_use]
pub const fn is_control_packet(size: u8) -> bool {
    matches!(size, 8 | 16 | 32 | 64)
}

/// One interface of a configuration, with its first interrupt IN endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
    /// Its first interrupt IN endpoint: the address, without the direction
    /// bit, and the largest packet.
    pub interrupt_in: Option<(u8, u16)>,
    /// That endpoint's `bInterval`.
    pub interval: u8,
    /// Its first bulk IN endpoint, the same way.
    pub bulk_in: Option<(u8, u16)>,
    /// Its first bulk OUT endpoint.
    pub bulk_out: Option<(u8, u16)>,
    /// For a HID interface, how long its report descriptor is, as its HID
    /// class descriptor says; 0 when it says nothing.
    pub report_length: u16,
}

/// The most interfaces a configuration is read for.
pub const MAX_INTERFACES: usize = 8;

/// A configuration descriptor and what follows it, as far as the driver
/// needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Configuration {
    /// `bConfigurationValue`.
    pub value: u8,
    /// The interfaces of alternate setting 0, in order.
    pub interfaces: [Interface; MAX_INTERFACES],
    /// How many of them there are.
    pub count: usize,
}

impl Configuration {
    /// The total length a configuration's first nine bytes give.
    #[must_use]
    pub fn total_length(header: &[u8]) -> Option<u16> {
        if header.get(1) != Some(&CONFIGURATION) {
            return None;
        }
        Some(u16::from_le_bytes([*header.get(2)?, *header.get(3)?]))
    }

    /// Read a configuration and its interfaces and endpoints. A descriptor
    /// cut short by the buffer ends the walk; interfaces past
    /// [`MAX_INTERFACES`] are left out.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Configuration> {
        if bytes.get(1) != Some(&CONFIGURATION) {
            return None;
        }
        let mut config = Configuration {
            value: *bytes.get(5)?,
            ..Configuration::default()
        };
        // Whether the endpoints being walked belong to a counted interface.
        let mut current: Option<usize> = None;
        let mut at = 0_usize;
        while let Some(&length) = bytes.get(at) {
            let length = usize::from(length);
            let Some(descriptor) = bytes.get(at..at + length).filter(|_| length >= 2) else {
                break;
            };
            match descriptor.get(1).copied() {
                Some(INTERFACE) => current = config.add_interface(descriptor),
                Some(ENDPOINT) => {
                    if let Some(slot) = current.and_then(|index| config.interfaces.get_mut(index)) {
                        note_endpoint(slot, descriptor);
                    }
                }
                Some(HID) => {
                    if let Some(slot) = current.and_then(|index| config.interfaces.get_mut(index)) {
                        note_report_length(slot, descriptor);
                    }
                }
                _ => {}
            }
            at += length;
        }
        Some(config)
    }

    /// Count an interface descriptor, if it is alternate setting 0 and there
    /// is room: the index it went in at.
    fn add_interface(&mut self, descriptor: &[u8]) -> Option<usize> {
        if descriptor.len() < 9 || descriptor.get(3) != Some(&0) {
            return None;
        }
        let slot = self.interfaces.get_mut(self.count)?;
        *slot = Interface {
            number: *descriptor.get(2)?,
            class: *descriptor.get(5)?,
            subclass: *descriptor.get(6)?,
            protocol: *descriptor.get(7)?,
            interrupt_in: None,
            interval: 0,
            bulk_in: None,
            bulk_out: None,
            report_length: 0,
        };
        self.count += 1;
        Some(self.count - 1)
    }

    /// The interfaces read.
    #[must_use]
    pub fn interfaces(&self) -> &[Interface] {
        self.interfaces.get(..self.count).unwrap_or(&[])
    }
}

/// Keep an endpoint descriptor's address if it is the interface's first
/// interrupt IN, bulk IN or bulk OUT endpoint.
fn note_endpoint(interface: &mut Interface, descriptor: &[u8]) {
    let (Some(&address), Some(&attributes), Some(&low), Some(&high)) = (
        descriptor.get(2),
        descriptor.get(3),
        descriptor.get(4),
        descriptor.get(5),
    ) else {
        return;
    };
    let interrupt = attributes & 0x3 == 3;
    let bulk = attributes & 0x3 == 2;
    let inward = address & 0x80 != 0;
    let size = u16::from_le_bytes([low, high]) & 0x7FF;
    let found = Some((address & 0x0F, size));
    if interrupt && inward && interface.interrupt_in.is_none() {
        interface.interrupt_in = found;
        interface.interval = descriptor.get(6).copied().unwrap_or(0);
    } else if bulk && inward && interface.bulk_in.is_none() {
        interface.bulk_in = found;
    } else if bulk && !inward && interface.bulk_out.is_none() {
        interface.bulk_out = found;
    }
}

/// Keep a HID class descriptor's report descriptor length: the first of its
/// class descriptors (`bDescriptorType` at 6, `wDescriptorLength` at 7) of
/// the report type.
fn note_report_length(interface: &mut Interface, descriptor: &[u8]) {
    if let (Some(&REPORT), Some(&low), Some(&high)) =
        (descriptor.get(6), descriptor.get(7), descriptor.get(8))
    {
        interface.report_length = u16::from_le_bytes([low, high]);
    }
}

/// A string descriptor's text as ASCII, a byte for each UTF-16 unit, `?` for
/// what ASCII lacks and NUL or control characters, and trailing spaces
/// dropped, into `out`: how many bytes it holds.
#[must_use]
pub fn string_ascii(descriptor: &[u8], out: &mut [u8]) -> usize {
    if descriptor.get(1) != Some(&STRING) {
        return 0;
    }
    let length = descriptor
        .first()
        .map_or(0, |&length| usize::from(length))
        .min(descriptor.len());
    let units = descriptor.get(2..length).unwrap_or(&[]).chunks_exact(2);
    let mut written = 0_usize;
    for (slot, unit) in out.iter_mut().zip(units) {
        let code = u16::from_le_bytes([
            unit.first().copied().unwrap_or(0),
            unit.get(1).copied().unwrap_or(0),
        ]);
        *slot = match u8::try_from(code) {
            Ok(byte) if (0x20..0x7F).contains(&byte) => byte,
            _ => b'?',
        };
        written += 1;
    }
    while written > 0 && out.get(written - 1) == Some(&b' ') {
        written -= 1;
    }
    written
}

/// The first language a string descriptor 0 lists.
#[must_use]
pub fn first_language(descriptor: &[u8]) -> Option<u16> {
    if descriptor.get(1) != Some(&STRING) || descriptor.first().is_none_or(|&length| length < 4) {
        return None;
    }
    Some(u16::from_le_bytes([
        *descriptor.get(2)?,
        *descriptor.get(3)?,
    ]))
}
