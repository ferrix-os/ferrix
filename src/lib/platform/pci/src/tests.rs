//! Tests for configuration space, against a bus of fake functions.
//!
//! The fake answers as hardware does — all ones for an empty slot, writes to a
//! BAR masked by the bits the device implements — and records one thing
//! hardware would not tell you: whether a BAR was probed while its function
//! was still decoding, which is the mistake [`crate::bar::size`] exists to not
//! make. The layouts are shaped like QEMU's `q35` and `virt` machines, which
//! are the ones stage 10 is brought up on.

extern crate std;

use core::cell::Cell;
use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use crate::bar::{self, Bar, Bars, Region};
use crate::capability::{
    self, BarOffset, Capabilities, Capability, ExtendedCapabilities, ID_MSIX, ID_PCI_EXPRESS,
    ID_VENDOR, MsiX,
};
use crate::ecam::{Ecam, Layout, Registers, Window};
use crate::header::{
    self, BusNumbers, COMMAND, COMMAND_BUS_MASTER, COMMAND_MEMORY_SPACE, Class, Endpoint,
    HeaderKind, Identity, STATUS_CAPABILITIES_LIST,
};
use crate::virtio::{self, Location, SharedMemory, Transport};
use crate::walk::{Function, Reason, Unfollowed, Walk};
use crate::{Address, CONFIG_SPACE_SIZE, ConfigSpace, PciError};

// ---------------------------------------------------------------------------
// A bus of fake functions
// ---------------------------------------------------------------------------

/// One function's configuration space and the BAR bits it implements.
#[derive(Clone, Debug)]
struct Device {
    bytes: Vec<u8>,
    /// Writable bits of each BAR slot. Zero for a slot not implemented.
    bar_masks: [u32; 6],
}

impl Device {
    fn new(vendor: u16, device: u16, class: [u8; 3], header_type: u8) -> Self {
        let mut d = Device {
            bytes: vec![0; usize::from(CONFIG_SPACE_SIZE)],
            bar_masks: [0; 6],
        };
        d.put16(0x00, vendor);
        d.put16(0x02, device);
        d.bytes[0x09] = class[2];
        d.bytes[0x0A] = class[1];
        d.bytes[0x0B] = class[0];
        d.bytes[0x0E] = header_type;
        d
    }

    fn endpoint(vendor: u16, device: u16) -> Self {
        Device::new(vendor, device, [0x01, 0x00, 0x00], 0x00)
    }

    fn bridge(primary: u8, secondary: u8, subordinate: u8) -> Self {
        let mut d = Device::new(0x1B36, 0x000C, [0x06, 0x04, 0x00], 0x01);
        d.bytes[0x18] = primary;
        d.bytes[0x19] = secondary;
        d.bytes[0x1A] = subordinate;
        d
    }

    fn put8(&mut self, at: u16, value: u8) {
        self.bytes[usize::from(at)] = value;
    }

    fn put16(&mut self, at: u16, value: u16) {
        let at = usize::from(at);
        self.bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put32(&mut self, at: u16, value: u32) {
        let at = usize::from(at);
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn get16(&self, at: u16) -> u16 {
        let at = usize::from(at);
        u16::from_le_bytes([self.bytes[at], self.bytes[at + 1]])
    }

    fn get32(&self, at: u16) -> u32 {
        let at = usize::from(at);
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap())
    }

    /// A BAR in `index` holding `value`, implementing `mask`'s bits.
    fn bar(&mut self, index: usize, value: u32, mask: u32) {
        self.put32(0x10 + 4 * index as u16, value);
        self.bar_masks[index] = mask;
    }

    /// Turn the capability list on and point it at `first`.
    fn capabilities(&mut self, first: u8) {
        let status = self.get16(0x06) | STATUS_CAPABILITIES_LIST;
        self.put16(0x06, status);
        self.put8(0x34, first);
    }

    /// A standard capability at `at` with `id`, pointing on to `next`.
    fn capability(&mut self, at: u16, id: u8, next: u8) {
        self.put8(at, id);
        self.put8(at + 1, next);
    }

    /// A virtio vendor capability at `at`.
    fn virtio_capability(
        &mut self,
        at: u16,
        next: u8,
        cfg_type: u8,
        bar: u8,
        offset: u32,
        len: u32,
    ) {
        let cap_len = if cfg_type == virtio::CFG_NOTIFY {
            20
        } else {
            16
        };
        self.capability(at, ID_VENDOR, next);
        self.put8(at + 2, cap_len);
        self.put8(at + 3, cfg_type);
        self.put8(at + 4, bar);
        self.put32(at + 8, offset);
        self.put32(at + 12, len);
        if cfg_type == virtio::CFG_NOTIFY {
            self.put32(at + 16, 4);
        }
    }
}

/// Functions at addresses, and a record of anything done wrongly to them.
#[derive(Debug, Default)]
struct Bus {
    functions: BTreeMap<Address, Device>,
    /// Set if a BAR was written while its function was decoding memory or I/O.
    probed_while_decoding: bool,
    /// How many identity reads each function received, to count scans.
    reads: Cell<usize>,
}

impl Bus {
    fn new() -> Self {
        Bus::default()
    }

    fn put(&mut self, address: Address, device: Device) {
        let _ = self.functions.insert(address, device);
    }

    fn device(&self, address: Address) -> &Device {
        &self.functions[&address]
    }

    fn device_mut(&mut self, address: Address) -> &mut Device {
        self.functions.get_mut(&address).unwrap()
    }

    fn bytes(&self, function: Address, offset: u16, width: usize) -> Option<&[u8]> {
        let at = usize::from(offset);
        self.functions.get(&function)?.bytes.get(at..at + width)
    }
}

impl ConfigSpace for Bus {
    fn read8(&self, function: Address, offset: u16) -> u8 {
        self.bytes(function, offset, 1).map_or(u8::MAX, |b| b[0])
    }

    fn read16(&self, function: Address, offset: u16) -> u16 {
        self.bytes(function, offset, 2)
            .map_or(u16::MAX, |b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn read32(&self, function: Address, offset: u16) -> u32 {
        if offset == 0 {
            self.reads.set(self.reads.get() + 1);
        }
        self.bytes(function, offset, 4)
            .map_or(u32::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn write16(&mut self, function: Address, offset: u16, value: u16) {
        if let Some(device) = self.functions.get_mut(&function)
            && usize::from(offset) + 2 <= device.bytes.len()
        {
            device.put16(offset, value);
        }
    }

    fn write32(&mut self, function: Address, offset: u16, value: u32) {
        let Some(device) = self.functions.get_mut(&function) else {
            return;
        };
        if usize::from(offset) + 4 > device.bytes.len() {
            return;
        }
        let slots = HeaderKind::from_register(device.bytes[0x0E]).bar_slots();
        let slot = offset.checked_sub(0x10).map(|o| o / 4);
        match slot {
            Some(slot) if offset.is_multiple_of(4) && slot < u16::from(slots) => {
                let slot = usize::from(slot);
                if device.get16(COMMAND) & 0x3 != 0 {
                    self.probed_while_decoding = true;
                }
                // Bits the device implements take the value written. Of the
                // rest, the low four keep what they held — type and flag bits
                // are hardwired — and address bits below the aperture read
                // zero, as they do on hardware.
                let mask = device.bar_masks[slot];
                let old = device.get32(offset);
                device.put32(offset, (value & mask) | (old & !mask & 0xF));
            }
            _ => device.put32(offset, value),
        }
    }
}

fn at(bus: u8, device: u8, function: u8) -> Address {
    Address::new(0, bus, device, function).unwrap()
}

// ---------------------------------------------------------------------------
// Addresses and ECAM
// ---------------------------------------------------------------------------

#[test]
fn an_address_refuses_device_and_function_numbers_that_cannot_exist() {
    assert!(Address::new(0, 255, 31, 7).is_some(), "the last function");
    assert!(Address::new(0, 0, 32, 0).is_none(), "device 32");
    assert!(Address::new(0, 0, 0, 8).is_none(), "function 8");
}

#[test]
fn an_address_prints_as_lspci_does() {
    let address = Address::new(1, 0x3a, 0x1f, 2).unwrap();
    assert_eq!(std::format!("{address}"), "0001:3a:1f.2", "display");
    assert_eq!(std::format!("{address:?}"), "0001:3a:1f.2", "debug");
}

#[test]
fn an_ecam_offset_is_bus_device_function_and_register() {
    let window = Window::new(0, 0, 255).unwrap();
    assert_eq!(window.len(), 256 << 20, "a megabyte per bus");
    assert_eq!(window.offset(at(0, 0, 0), 0, 4), Some(0), "the first byte");
    assert_eq!(
        window.offset(at(3, 0x1f, 2), 0x10, 4),
        Some(3 << 20 | 0x1f << 15 | 2 << 12 | 0x10),
        "BAR0 of 03:1f.2"
    );
}

#[test]
fn an_ecam_window_starting_above_bus_zero_counts_from_its_first_bus() {
    let window = Window::new(0, 0x80, 0x8f).unwrap();
    assert_eq!(window.len(), 16 << 20, "sixteen buses");
    assert_eq!(
        window.offset(at(0x80, 1, 0), 0, 1),
        Some(1 << 15),
        "first bus"
    );
    assert_eq!(
        window.offset(at(0x7f, 1, 0), 0, 1),
        None,
        "below the window"
    );
    assert_eq!(
        window.offset(at(0x90, 1, 0), 0, 1),
        None,
        "above the window"
    );
    assert!(Window::new(0, 2, 1).is_none(), "an empty range");
}

#[test]
fn a_cam_offset_is_a_sixteenth_of_ecams() {
    let window = Window::with_layout(Layout::Cam, 0, 0, 0).unwrap();
    assert_eq!(window.layout(), Layout::Cam, "layout");
    assert_eq!(window.len(), 64 << 10, "64 KiB for its one bus");
    assert_eq!(
        window.offset(at(0, 1, 0), 0, 4),
        Some(0x800),
        "crosvm's first device, 00:01.0"
    );
    assert_eq!(
        window.offset(at(0, 0x1f, 7), 0x3c, 4),
        Some(0x1f << 11 | 7 << 8 | 0x3c),
        "the interrupt register of 00:1f.7"
    );
    assert!(
        window.offset(at(0, 0, 0), 252, 4).is_some(),
        "the last conventional register"
    );
    assert_eq!(
        window.offset(at(0, 0, 0), 256, 4),
        None,
        "no extended space"
    );
    assert_eq!(
        window.offset(at(0, 0, 0), 254, 4),
        None,
        "straddles the end"
    );
    assert_eq!(window.offset(at(1, 0, 0), 0, 4), None, "past its bus");
}

#[test]
fn cam_answers_all_ones_past_a_functions_256_bytes() {
    let window = Window::with_layout(Layout::Cam, 0, 0, 1).unwrap();
    let mut cam = Ecam::new(window, Buffer(vec![0; window.len() as usize]));
    let function = at(1, 3, 1);
    cam.write32(function, 0xfc, 0x1234_5678);
    assert_eq!(cam.read32(function, 0xfc), 0x1234_5678, "round trip");
    assert_eq!(
        cam.read32(at(1, 3, 2), 0),
        0,
        "the next function starts 256 bytes on"
    );
    cam.write32(function, 0x100, 0xDEAD_BEEF);
    assert_eq!(cam.read32(function, 0x100), u32::MAX, "no extended space");
    assert_eq!(cam.read32(at(1, 3, 2), 0), 0, "and the write went nowhere");
}

#[test]
fn an_ecam_access_may_not_run_past_the_functions_space_or_segment() {
    let window = Window::new(0, 0, 0).unwrap();
    assert!(
        window.offset(at(0, 0, 0), 4092, 4).is_some(),
        "the last register"
    );
    assert_eq!(
        window.offset(at(0, 0, 0), 4093, 4),
        None,
        "three bytes over"
    );
    assert_eq!(window.offset(at(0, 0, 0), 4096, 1), None, "past the end");
    assert_eq!(window.offset(at(0, 0, 0), u16::MAX, 4), None, "no wrap");
    let other = Address::new(1, 0, 0, 0).unwrap();
    assert_eq!(window.offset(other, 0, 4), None, "another segment");
}

/// A byte buffer as a mapped window.
#[derive(Debug)]
struct Buffer(Vec<u8>);

impl Registers for Buffer {
    fn read8(&self, offset: u64) -> u8 {
        self.0[offset as usize]
    }
    fn read16(&self, offset: u64) -> u16 {
        let o = offset as usize;
        u16::from_le_bytes([self.0[o], self.0[o + 1]])
    }
    fn read32(&self, offset: u64) -> u32 {
        let o = offset as usize;
        u32::from_le_bytes(self.0[o..o + 4].try_into().unwrap())
    }
    fn write16(&mut self, offset: u64, value: u16) {
        let o = offset as usize;
        self.0[o..o + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn write32(&mut self, offset: u64, value: u32) {
        let o = offset as usize;
        self.0[o..o + 4].copy_from_slice(&value.to_le_bytes());
    }
}

#[test]
fn ecam_answers_all_ones_outside_its_window_and_discards_writes_there() {
    let window = Window::new(0, 0, 1).unwrap();
    let mut ecam = Ecam::new(window, Buffer(vec![0; window.len() as usize]));
    let inside = at(1, 2, 3);
    ecam.write32(inside, 0x10, 0xDEAD_BEEF);
    assert_eq!(ecam.read32(inside, 0x10), 0xDEAD_BEEF, "round trip");
    assert_eq!(ecam.read8(inside, 0x11), 0xBE, "byte width");
    ecam.write16(inside, 0x04, 0x0406);
    assert_eq!(ecam.read16(inside, 0x04), 0x0406, "word width");

    let outside = at(2, 0, 0);
    ecam.write32(outside, 0, 0);
    assert_eq!(
        ecam.read32(outside, 0),
        u32::MAX,
        "bus 2 is not in the window"
    );
    assert_eq!(ecam.read16(inside, 4095), u16::MAX, "straddles the end");
    assert_eq!(ecam.window(), window, "window");
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

#[test]
fn the_four_answers_of_an_empty_slot_are_all_absent() {
    for ids in [0, u32::MAX, 0x0000_FFFF, 0xFFFF_0000] {
        assert!(header::is_absent(ids), "{ids:#x}");
    }
    assert!(!header::is_absent(0x29C0_8086), "q35's host bridge");
}

#[test]
fn identity_reads_ids_class_and_header_type() {
    let mut bus = Bus::new();
    let mut lpc = Device::new(0x8086, 0x2918, [0x06, 0x01, 0x00], 0x80);
    lpc.put8(0x08, 0x02);
    bus.put(at(0, 0x1f, 0), lpc);

    let identity = Identity::read(&bus, at(0, 0x1f, 0)).unwrap();
    assert_eq!(identity.vendor, 0x8086, "vendor");
    assert_eq!(identity.device, 0x2918, "device");
    assert_eq!(identity.revision, 2, "revision");
    assert_eq!(
        identity.class,
        Class {
            base: 0x06,
            sub: 0x01,
            interface: 0
        },
        "class"
    );
    assert_eq!(identity.kind, HeaderKind::Endpoint, "kind");
    assert!(identity.multifunction, "multifunction");
    assert_eq!(Identity::read(&bus, at(0, 0x1e, 0)), None, "empty slot");
}

#[test]
fn a_header_kind_names_its_bars_and_capabilities_pointer() {
    assert_eq!(
        HeaderKind::from_register(0x81),
        HeaderKind::Bridge,
        "flag masked"
    );
    assert_eq!(HeaderKind::Endpoint.bar_slots(), 6, "endpoint");
    assert_eq!(HeaderKind::Bridge.bar_slots(), 2, "bridge");
    assert_eq!(HeaderKind::CardBus.bar_slots(), 0, "cardbus");
    assert_eq!(
        HeaderKind::CardBus.capabilities_pointer(),
        Some(0x14),
        "cardbus"
    );
    assert_eq!(
        HeaderKind::from_register(0x7f),
        HeaderKind::Reserved(0x7f),
        "reserved"
    );
    assert_eq!(
        HeaderKind::Reserved(0x7f).capabilities_pointer(),
        None,
        "reserved"
    );
    assert_eq!(HeaderKind::Reserved(0x7f).code(), 0x7f, "code");
}

#[test]
fn bus_numbers_and_endpoint_fields_are_read_only_from_their_own_header_type() {
    let mut bus = Bus::new();
    bus.put(at(0, 1, 0), Device::bridge(0, 1, 4));
    let mut disk = Device::endpoint(0x1AF4, 0x1001);
    disk.put16(0x2C, 0x1AF4);
    disk.put16(0x2E, 2);
    disk.put8(0x3C, 11);
    disk.put8(0x3D, 1);
    bus.put(at(0, 2, 0), disk);

    assert_eq!(
        BusNumbers::read(&bus, at(0, 1, 0)),
        Ok(BusNumbers {
            primary: 0,
            secondary: 1,
            subordinate: 4
        }),
        "bridge"
    );
    assert_eq!(
        BusNumbers::read(&bus, at(0, 2, 0)),
        Err(PciError::HeaderType {
            function: at(0, 2, 0),
            kind: 0
        }),
        "an endpoint has no bus numbers"
    );
    assert_eq!(
        Endpoint::read(&bus, at(0, 2, 0)),
        Ok(Endpoint {
            subsystem_vendor: 0x1AF4,
            subsystem: 2,
            interrupt_pin: 1,
            interrupt_line: 11
        }),
        "endpoint"
    );
    assert!(
        Endpoint::read(&bus, at(0, 1, 0)).is_err(),
        "a bridge is no endpoint"
    );
}

// ---------------------------------------------------------------------------
// BARs
// ---------------------------------------------------------------------------

#[test]
fn bars_are_decoded_in_slot_order_with_the_upper_half_of_a_wide_bar_skipped() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(0x1AF4, 0x1042);
    d.bar(0, 0x0000_C001, 0);
    d.bar(1, 0xFEBD_1000, 0);
    d.bar(4, 0x0000_000C | 0x8000_0000, 0);
    d.put32(0x24, 0x0000_0001);
    bus.put(at(0, 3, 0), d);

    let bars: Vec<_> = Bars::new(&bus, at(0, 3, 0), HeaderKind::Endpoint).collect();
    assert_eq!(
        bars,
        vec![
            Ok((0, Bar::Io { port: 0xC000 })),
            Ok((
                1,
                Bar::Memory {
                    address: 0xFEBD_1000,
                    wide: false,
                    prefetchable: false
                }
            )),
            Ok((
                2,
                Bar::Memory {
                    address: 0,
                    wide: false,
                    prefetchable: false
                }
            )),
            Ok((
                3,
                Bar::Memory {
                    address: 0,
                    wide: false,
                    prefetchable: false
                }
            )),
            Ok((
                4,
                Bar::Memory {
                    address: 0x1_8000_0000,
                    wide: true,
                    prefetchable: true
                }
            )),
        ],
        "slot 5 is the upper half of slot 4"
    );
    assert_eq!(
        bar::read(&bus, at(0, 3, 0), HeaderKind::Endpoint, 5),
        Err(PciError::NoSuchBar {
            function: at(0, 3, 0),
            index: 5
        }),
        "an upper half is not a BAR"
    );
    assert_eq!(
        bar::read(&bus, at(0, 3, 0), HeaderKind::Bridge, 2),
        Err(PciError::NoSuchBar {
            function: at(0, 3, 0),
            index: 2
        }),
        "a bridge has two slots"
    );
}

#[test]
fn a_wide_bar_in_the_last_slot_and_a_reserved_type_are_refused() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.bar(5, 0x4, 0);
    bus.put(at(0, 1, 0), d);
    let mut d = Device::endpoint(1, 1);
    d.bar(0, 0x2, 0);
    bus.put(at(0, 2, 0), d);

    assert_eq!(
        bar::read(&bus, at(0, 1, 0), HeaderKind::Endpoint, 5),
        Err(PciError::TruncatedBar {
            function: at(0, 1, 0),
            index: 5
        }),
        "truncated"
    );
    let bars: Vec<_> = Bars::new(&bus, at(0, 2, 0), HeaderKind::Endpoint).collect();
    assert_eq!(
        bars,
        vec![Err(PciError::ReservedBarType {
            function: at(0, 2, 0),
            index: 0
        })],
        "a reserved type ends the walk"
    );
}

/// A function with decoding and bus mastering on, and BARs of several kinds.
fn sized_device() -> (Bus, Address) {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(0x1AF4, 0x1042);
    d.put16(COMMAND, COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER | 1);
    // A 4 KiB 32-bit memory BAR.
    d.bar(1, 0xFEBD_1000, 0xFFFF_F000);
    // A 8 GiB 64-bit prefetchable BAR at 0x8_0000_0000.
    d.bar(2, 0x0000_000C, 0x0000_0000);
    d.bar(3, 0x0000_0008, 0xFFFF_FFFE);
    // A 64-bit BAR on a device that decodes 40 address bits.
    d.bar(4, 0xE000_000C, 0xF000_0000);
    d.bar(5, 0x0000_0080, 0x0000_00FF);
    // A 32-port I/O BAR on a 16-bit decoder.
    d.bar(0, 0x0000_C041, 0x0000_FFE0);
    bus.put(at(0, 3, 0), d);
    (bus, at(0, 3, 0))
}

#[test]
fn sizing_finds_each_kind_of_aperture() {
    let (mut bus, f) = sized_device();
    let kind = HeaderKind::Endpoint;
    assert_eq!(
        bar::size(&mut bus, f, kind, 0).unwrap().map(|r| r.size),
        Some(32),
        "I/O on a 16-bit decoder"
    );
    let memory = bar::size(&mut bus, f, kind, 1).unwrap().unwrap();
    assert_eq!(memory.size, 0x1000, "32-bit memory");
    assert_eq!(memory.last(), 0xFEBD_1FFF, "last byte");
    assert!(
        memory.contains(0xFFC, 4) && !memory.contains(0xFFD, 4),
        "contains"
    );
    assert!(!memory.contains(u64::MAX, 2), "no wrap");

    let wide = bar::size(&mut bus, f, kind, 2).unwrap().unwrap();
    assert_eq!(wide.size, 8 << 30, "8 GiB, all in the upper half");
    assert_eq!(wide.bar.address(), 0x8_0000_0000, "address");

    let partial = bar::size(&mut bus, f, kind, 4).unwrap().unwrap();
    assert_eq!(partial.size, 0x1000_0000, "256 MiB below a 40-bit decoder");
    assert_eq!(partial.bar.address(), 0x80_E000_0000, "address");
}

#[test]
fn sizing_restores_every_register_and_never_probes_a_decoding_function() {
    let (mut bus, f) = sized_device();
    let before = bus.device(f).bytes.clone();
    for index in [0, 1, 2, 4] {
        let _ = bar::size(&mut bus, f, HeaderKind::Endpoint, index).unwrap();
    }
    assert!(
        !bus.probed_while_decoding,
        "decoding was switched off first"
    );
    assert_eq!(bus.device(f).bytes, before, "every register as it was");
}

#[test]
fn an_unimplemented_bar_sizes_to_nothing() {
    let mut bus = Bus::new();
    bus.put(at(0, 1, 0), Device::endpoint(1, 1));
    assert_eq!(
        bar::size(&mut bus, at(0, 1, 0), HeaderKind::Endpoint, 0),
        Ok(None),
        "32-bit"
    );
    let mut d = Device::endpoint(1, 1);
    d.bar(0, 0x4, 0);
    bus.put(at(0, 2, 0), d);
    assert_eq!(
        bar::size(&mut bus, at(0, 2, 0), HeaderKind::Endpoint, 0),
        Ok(None),
        "64-bit, both halves hardwired"
    );
}

#[test]
fn a_mask_that_is_not_one_run_is_refused_and_still_restored() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.put16(COMMAND, COMMAND_MEMORY_SPACE);
    d.bar(0, 0xF000_0000, 0xF0F0_0000);
    bus.put(at(0, 1, 0), d);
    let before = bus.device(at(0, 1, 0)).bytes.clone();
    assert_eq!(
        bar::size(&mut bus, at(0, 1, 0), HeaderKind::Endpoint, 0),
        Err(PciError::BarMask {
            function: at(0, 1, 0),
            index: 0,
            mask: 0xF0F0_0000
        }),
        "two runs"
    );
    assert_eq!(bus.device(at(0, 1, 0)).bytes, before, "restored anyway");
}

#[test]
fn a_misaligned_placement_is_refused_and_the_top_of_each_width_is_not() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    // An address a 4 KiB BAR cannot hold, as a device that answers its
    // address and its probe inconsistently presents it.
    d.bar(0, 0xFEBD_1800, 0xFFFF_F000);
    d.bar(1, 0xFF00_0000, 0xFF00_0000);
    d.bar(2, 0xFF00_1000, 0xFFFF_F000);
    bus.put(at(0, 1, 0), d);
    let f = at(0, 1, 0);
    assert_eq!(
        bar::size(&mut bus, f, HeaderKind::Endpoint, 0),
        Err(PciError::BarPlacement {
            function: f,
            index: 0
        }),
        "not a multiple of 4 KiB"
    );
    let top = bar::size(&mut bus, f, HeaderKind::Endpoint, 1)
        .unwrap()
        .unwrap();
    assert_eq!(top.last(), 0xFFFF_FFFF, "ends at 4 GiB exactly");
    assert!(
        bar::size(&mut bus, f, HeaderKind::Endpoint, 2).is_ok(),
        "fits"
    );

    let mut d = Device::endpoint(1, 1);
    d.bar(0, 0xFFFF_F004, 0xFFFF_F000);
    d.bar(1, 0xFFFF_FFFF, 0xFFFF_FFFF);
    bus.put(at(0, 2, 0), d);
    let top = bar::size(&mut bus, at(0, 2, 0), HeaderKind::Endpoint, 0)
        .unwrap()
        .unwrap();
    assert_eq!(top.last(), u64::MAX, "a wide BAR may end at the very top");
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[test]
fn the_standard_list_is_walked_in_order_with_reserved_bits_masked() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.capabilities(0x40);
    d.capability(0x40, ID_PCI_EXPRESS, 0x6B);
    d.capability(0x68, ID_MSIX, 0x84);
    d.capability(0x84, ID_VENDOR, 0x00);
    bus.put(at(0, 1, 0), d);

    let found: Vec<_> = Capabilities::new(&bus, at(0, 1, 0))
        .map(|c| c.map(|c| (c.id, c.offset)))
        .collect();
    assert_eq!(
        found,
        vec![
            Ok((ID_PCI_EXPRESS, 0x40)),
            Ok((ID_MSIX, 0x68)),
            Ok((ID_VENDOR, 0x84))
        ],
        "0x6B is 0x68 with reserved bits set"
    );
    assert_eq!(
        capability::find(&bus, at(0, 1, 0), ID_MSIX)
            .unwrap()
            .map(|c| c.offset),
        Some(0x68),
        "find"
    );
    assert_eq!(
        capability::find(&bus, at(0, 1, 0), 0x01),
        Ok(None),
        "absent"
    );
}

#[test]
fn only_a_pcie_port_forwards_a_requester_id_unchanged() {
    let mut bus = Bus::new();
    // Port types 4, 5 and 6 forward; a PCIe-to-PCI bridge (7), an endpoint
    // (0) and a bridge with no Express capability do not.
    for (device, kind) in [(1, 4u16), (2, 5), (3, 6), (4, 7), (5, 0)] {
        let mut d = Device::bridge(0, device, device);
        d.capabilities(0x40);
        d.capability(0x40, ID_PCI_EXPRESS, 0x00);
        d.put16(0x42, kind << 4 | 0x2);
        bus.put(at(0, device, 0), d);
    }
    bus.put(at(0, 6, 0), Device::bridge(0, 6, 6));
    let forwards: Vec<_> = (1..=6)
        .map(|device| capability::forwards_requester(&bus, at(0, device, 0)).unwrap())
        .collect();
    assert_eq!(forwards, vec![true, true, true, false, false, false]);
}

#[test]
fn no_list_is_walked_without_the_status_bit_or_on_an_empty_slot() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.put8(0x34, 0x40);
    d.capability(0x40, ID_MSIX, 0);
    bus.put(at(0, 1, 0), d);
    assert_eq!(
        Capabilities::new(&bus, at(0, 1, 0)).count(),
        0,
        "status bit clear"
    );
    assert_eq!(
        Capabilities::new(&bus, at(0, 9, 0)).count(),
        0,
        "nothing there"
    );
}

#[test]
fn a_pointer_into_the_header_is_refused() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.capabilities(0x40);
    d.capability(0x40, ID_MSIX, 0x10);
    bus.put(at(0, 1, 0), d);
    let found: Vec<_> = Capabilities::new(&bus, at(0, 1, 0)).collect();
    assert_eq!(
        found[1],
        Err(PciError::CapabilityPointer {
            function: at(0, 1, 0),
            at: 0x41,
            pointer: 0x10
        }),
        "names the pointer's own offset"
    );
    assert_eq!(found.len(), 2, "and ends there");
}

#[test]
fn a_capability_list_that_loops_ends_in_an_error() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.capabilities(0x40);
    d.capability(0x40, ID_MSIX, 0x50);
    d.capability(0x50, ID_VENDOR, 0x40);
    bus.put(at(0, 1, 0), d);
    let mut d = Device::endpoint(1, 1);
    d.capabilities(0xFC);
    d.capability(0xFC, ID_VENDOR, 0xFC);
    bus.put(at(0, 2, 0), d);

    let found: Vec<_> = Capabilities::new(&bus, at(0, 1, 0)).collect();
    assert_eq!(found.len(), 3, "two entries, then the loop");
    assert_eq!(
        found[2],
        Err(PciError::CapabilityLoop {
            function: at(0, 1, 0),
            at: 0x40
        }),
        "two-cycle"
    );
    let found: Vec<_> = Capabilities::new(&bus, at(0, 2, 0)).collect();
    assert_eq!(found.len(), 2, "the last offset, pointing at itself");
    assert!(
        matches!(found[1], Err(PciError::CapabilityLoop { at: 0xFC, .. })),
        "self-loop"
    );
}

#[test]
fn the_extended_list_is_walked_and_bounded_the_same_way() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    // AER version 1 at 0x100, pointing at ACS at 0x148, which ends the list.
    d.put32(0x100, 0x1482_0001);
    d.put32(0x148, 0x0001_000D);
    bus.put(at(0, 1, 0), d);
    let found: Vec<_> = ExtendedCapabilities::new(&bus, at(0, 1, 0))
        .map(|c| c.map(|c| (c.id, c.version, c.offset)))
        .collect();
    assert_eq!(
        found,
        vec![
            Ok((capability::EXTENDED_ID_AER, 2, 0x100)),
            Ok((capability::EXTENDED_ID_ACS, 1, 0x148))
        ],
        "two entries"
    );

    let mut d = Device::endpoint(1, 1);
    d.put32(0x100, 0x1001_0001);
    bus.put(at(0, 2, 0), d);
    let found: Vec<_> = ExtendedCapabilities::new(&bus, at(0, 2, 0)).collect();
    assert!(
        matches!(
            found.last(),
            Some(Err(PciError::CapabilityLoop { at: 0x100, .. }))
        ),
        "self-loop: {found:?}"
    );

    let mut d = Device::endpoint(1, 1);
    d.put32(0x100, 0x0801_0001);
    bus.put(at(0, 3, 0), d);
    let found: Vec<_> = ExtendedCapabilities::new(&bus, at(0, 3, 0)).collect();
    assert!(
        matches!(
            found.last(),
            Some(Err(PciError::CapabilityPointer { pointer: 0x80, .. }))
        ),
        "into legacy space: {found:?}"
    );

    bus.put(at(0, 4, 0), Device::endpoint(1, 1));
    assert_eq!(
        ExtendedCapabilities::new(&bus, at(0, 4, 0)).count(),
        0,
        "zero header"
    );
    assert_eq!(
        ExtendedCapabilities::new(&bus, at(0, 9, 0)).count(),
        0,
        "no function"
    );
}

#[test]
fn msix_is_decoded_from_its_capability() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.capabilities(0x98);
    d.capability(0x98, ID_MSIX, 0);
    d.put16(0x9A, 0x8000 | 0x40 | 2);
    d.put32(0x9C, 0x0000_0001);
    d.put32(0xA0, 0x0000_0801);
    bus.put(at(0, 1, 0), d);

    let cap = capability::find(&bus, at(0, 1, 0), ID_MSIX)
        .unwrap()
        .unwrap();
    let msix = MsiX::read(&bus, cap).unwrap();
    assert_eq!(msix.table_size, 67, "0x42 + 1 vectors");
    assert!(msix.enabled && !msix.function_masked, "control bits");
    assert_eq!(msix.table, BarOffset { bar: 1, offset: 0 }, "table");
    assert_eq!(
        msix.pending,
        BarOffset {
            bar: 1,
            offset: 0x800
        },
        "pending"
    );
    assert_eq!(msix.table_len(), 67 * 16, "table bytes");
    assert_eq!(msix.pending_len(), 16, "two words of pending bits");
}

#[test]
fn msix_refuses_a_bar_above_five_and_a_capability_past_the_legacy_space() {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(1, 1);
    d.put32(0x44, 0x6);
    bus.put(at(0, 1, 0), d);
    let f = at(0, 1, 0);
    let cap = |offset| Capability {
        function: f,
        id: ID_MSIX,
        offset,
    };
    assert_eq!(
        MsiX::read(&bus, cap(0x40)),
        Err(PciError::NoSuchBar {
            function: f,
            index: 6
        }),
        "BIR 6"
    );
    assert_eq!(
        MsiX::read(&bus, cap(0xF8)),
        Err(PciError::CapabilityTooShort {
            function: f,
            at: 0xF8,
            len: 8
        }),
        "eight bytes of room"
    );
}

// ---------------------------------------------------------------------------
// The bus walk
// ---------------------------------------------------------------------------

/// Addresses the walk found, and the bridges it did not follow.
fn walk(bus: &Bus, window: core::ops::RangeInclusive<u8>) -> (Vec<Address>, Vec<Unfollowed>) {
    let mut found = Vec::new();
    let mut unfollowed = Vec::new();
    for item in Walk::new(bus, 0, window) {
        match item {
            Ok(Function { address, .. }) => found.push(address),
            Err(u) => unfollowed.push(u),
        }
    }
    (found, unfollowed)
}

/// QEMU's `q35` with no extra devices.
fn q35() -> Bus {
    let mut bus = Bus::new();
    bus.put(at(0, 0, 0), Device::new(0x8086, 0x29C0, [6, 0, 0], 0));
    bus.put(at(0, 1, 0), Device::new(0x1234, 0x1111, [3, 0, 0], 0));
    bus.put(at(0, 0x1f, 0), Device::new(0x8086, 0x2918, [6, 1, 0], 0x80));
    bus.put(at(0, 0x1f, 2), Device::new(0x8086, 0x2922, [1, 6, 1], 0));
    bus.put(at(0, 0x1f, 3), Device::new(0x8086, 0x2930, [0xc, 5, 0], 0));
    bus
}

#[test]
fn the_walk_finds_every_function_of_a_multifunction_device() {
    let (found, unfollowed) = walk(&q35(), 0..=255);
    assert_eq!(
        found,
        vec![
            at(0, 0, 0),
            at(0, 1, 0),
            at(0, 0x1f, 0),
            at(0, 0x1f, 2),
            at(0, 0x1f, 3)
        ],
        "q35"
    );
    assert!(unfollowed.is_empty(), "no bridges");
}

#[test]
fn a_single_function_device_is_not_scanned_past_function_zero() {
    let mut bus = q35();
    // Some devices decode only the device number and answer as the same
    // function at every function number; the header's flag is how a walk
    // avoids reporting them eight times.
    bus.put(at(0, 1, 5), Device::new(0x1234, 0x1111, [3, 0, 0], 0));
    let (found, _) = walk(&bus, 0..=255);
    assert!(!found.contains(&at(0, 1, 5)), "ghost function");
}

#[test]
fn bridges_are_followed_to_the_buses_behind_them() {
    let mut bus = Bus::new();
    bus.put(at(0, 0, 0), Device::new(0x1B36, 0x0008, [6, 0, 0], 0));
    bus.put(at(0, 1, 0), Device::bridge(0, 1, 2));
    bus.put(at(0, 2, 0), Device::bridge(0, 3, 3));
    bus.put(at(1, 0, 0), Device::bridge(1, 2, 2));
    bus.put(at(2, 0, 0), Device::endpoint(0x1AF4, 0x1042));
    bus.put(at(3, 0, 0), Device::endpoint(0x1AF4, 0x1041));
    // Nothing leads here, so nothing should find it.
    bus.put(at(9, 0, 0), Device::endpoint(1, 1));

    let (found, unfollowed) = walk(&bus, 0..=255);
    assert_eq!(
        found,
        vec![
            at(0, 0, 0),
            at(0, 1, 0),
            at(0, 2, 0),
            at(1, 0, 0),
            at(2, 0, 0),
            at(3, 0, 0)
        ],
        "buses in number order"
    );
    assert!(unfollowed.is_empty(), "every bridge configured");
}

#[test]
fn a_bridge_that_cannot_be_followed_is_reported_straight_after_itself() {
    let mut bus = Bus::new();
    bus.put(at(0, 1, 0), Device::bridge(0, 0, 0));
    bus.put(at(0, 2, 0), Device::bridge(0, 0x20, 0x20));
    bus.put(at(0, 3, 0), Device::bridge(0, 5, 4));
    bus.put(at(0, 4, 0), Device::bridge(0, 1, 1));
    bus.put(at(0, 5, 0), Device::bridge(0, 1, 1));
    bus.put(at(1, 0, 0), Device::bridge(1, 1, 1));

    let items: Vec<_> = Walk::new(&bus, 0, 0..=0x1f).collect();
    let reasons: Vec<_> = items
        .iter()
        .filter_map(|item| item.err().map(|u| (u.bridge, u.reason)))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (at(0, 1, 0), Reason::NotBelow),
            (at(0, 2, 0), Reason::OutsideWindow),
            (at(0, 3, 0), Reason::Subordinate),
            (at(0, 5, 0), Reason::AlreadyClaimed),
            (at(1, 0, 0), Reason::NotBelow),
        ],
        "each refusal"
    );
    assert_eq!(
        items[0].map(|f| f.address),
        Ok(at(0, 1, 0)),
        "the bridge first"
    );
    assert!(items[1].is_err(), "then why it was not followed");
}

#[test]
fn every_bus_is_scanned_at_most_once_however_the_bridges_point() {
    let mut bus = Bus::new();
    // Every bridge on every bus claims bus 1, and bus 1's bridges claim bus 0.
    for device in 0..32 {
        bus.put(at(0, device, 0), Device::bridge(0, 1, 255));
        bus.put(at(1, device, 0), Device::bridge(1, 0, 255));
    }
    let _ = walk(&bus, 0..=255);
    assert_eq!(
        bus.reads.get(),
        64,
        "one identity read per slot on two buses"
    );
}

#[test]
fn an_empty_window_walks_nothing_and_a_later_root_is_where_it_starts() {
    let bus = q35();
    let (start, end) = (1, 0);
    let (found, _) = walk(&bus, start..=end);
    assert!(found.is_empty(), "empty");
    let mut bus = Bus::new();
    bus.put(at(0x40, 0, 0), Device::endpoint(1, 1));
    bus.put(at(0, 0, 0), Device::endpoint(1, 1));
    let (found, _) = walk(&bus, 0x40..=0x4f);
    assert_eq!(
        found,
        vec![at(0x40, 0, 0)],
        "root is the window's first bus"
    );
}

// ---------------------------------------------------------------------------
// virtio
// ---------------------------------------------------------------------------

#[test]
fn a_virtio_device_type_comes_from_the_device_id_or_the_subsystem() {
    let mut identity = Identity {
        vendor: virtio::VENDOR,
        device: 0x1042,
        revision: 1,
        class: Class::from_register(0),
        kind: HeaderKind::Endpoint,
        multifunction: false,
    };
    assert_eq!(
        virtio::device_type(&identity, 0),
        Some(virtio::TYPE_BLOCK),
        "modern"
    );
    identity.device = 0x1001;
    assert_eq!(
        virtio::device_type(&identity, 2),
        Some(virtio::TYPE_BLOCK),
        "transitional"
    );
    identity.device = 0x10F0;
    assert_eq!(virtio::device_type(&identity, 2), None, "not a virtio ID");
    identity.vendor = 0x8086;
    identity.device = 0x1042;
    assert_eq!(virtio::device_type(&identity, 2), None, "another vendor");
}

/// A modern virtio-blk-pci as QEMU lays one out: every block in BAR 4.
fn virtio_blk() -> (Bus, Address) {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(0x1AF4, 0x1042);
    d.bar(4, 0x0000_000C, 0xFFFF_C000);
    d.bar(5, 0x0000_0080, 0xFFFF_FFFF);
    d.capabilities(0x84);
    d.capability(0x98, ID_MSIX, 0x84);
    d.virtio_capability(0x84, 0x74, virtio::CFG_PCI, 0, 0, 0);
    // Notification is 20 bytes, so the next one starts at 0x74.
    d.virtio_capability(0x74, 0x60, virtio::CFG_DEVICE, 4, 0x2000, 0x1000);
    d.virtio_capability(0x60, 0x50, virtio::CFG_NOTIFY, 4, 0x3000, 0x1000);
    d.virtio_capability(0x50, 0x40, virtio::CFG_ISR, 4, 0x1000, 0x1000);
    d.virtio_capability(0x40, 0x00, virtio::CFG_COMMON, 4, 0x0000, 0x1000);
    bus.put(at(0, 3, 0), d);
    (bus, at(0, 3, 0))
}

#[test]
fn the_virtio_transport_is_found_in_its_capabilities() {
    let (bus, f) = virtio_blk();
    let transport = Transport::find(&bus, f).unwrap().unwrap();
    let location = |capability, offset| Location {
        capability,
        bar: 4,
        offset,
        length: 0x1000,
    };
    assert_eq!(transport.common, location(0x40, 0), "common");
    assert_eq!(transport.isr, location(0x50, 0x1000), "isr");
    assert_eq!(transport.notify, location(0x60, 0x3000), "notify");
    assert_eq!(transport.notify_multiplier, 4, "multiplier");
    assert_eq!(transport.device, Some(location(0x74, 0x2000)), "device");
}

#[test]
fn the_virtio_transport_is_checked_against_the_sized_bar() {
    let (mut bus, f) = virtio_blk();
    let transport = Transport::find(&bus, f).unwrap().unwrap();
    let region: Region = bar::size(&mut bus, f, HeaderKind::Endpoint, 4)
        .unwrap()
        .unwrap();
    assert_eq!(region.size, 0x4000, "16 KiB");
    assert_eq!(transport.verify(&[region]), Ok(()), "every block fits");
    assert_eq!(
        transport.verify(&[]),
        Err(PciError::VirtioRegion {
            function: f,
            at: 0x40
        }),
        "no BAR 4"
    );
    let small = Region {
        size: 0x3000,
        ..region
    };
    assert_eq!(
        transport.verify(&[small]),
        Err(PciError::VirtioRegion {
            function: f,
            at: 0x60
        }),
        "notify runs past a 12 KiB BAR"
    );
}

#[test]
fn a_virtio_transport_takes_the_first_of_each_type_and_skips_reserved_bars() {
    let (mut bus, f) = virtio_blk();
    let d = bus.device_mut(f);
    // A second common block after the first, and one ahead of it naming a
    // reserved BAR, which a driver must ignore.
    d.virtio_capability(0x40, 0xB0, virtio::CFG_COMMON, 4, 0x0000, 0x1000);
    d.virtio_capability(0xB0, 0xC0, virtio::CFG_COMMON, 4, 0x3800, 0x800);
    d.virtio_capability(0xC0, 0x00, virtio::CFG_COMMON, 4, 0x0100, 0x100);
    d.virtio_capability(0x84, 0xD0, virtio::CFG_PCI, 0, 0, 0);
    d.virtio_capability(0xD0, 0x74, virtio::CFG_COMMON, 7, 0x0100, 0x100);
    let transport = Transport::find(&bus, f).unwrap().unwrap();
    assert_eq!(transport.common.capability, 0x40, "first usable one");
}

#[test]
fn a_virtio_transport_missing_a_block_is_none_and_a_short_capability_is_an_error() {
    let (mut bus, f) = virtio_blk();
    // Drop the ISR capability by linking past it.
    bus.device_mut(f).put8(0x61, 0x40);
    assert_eq!(Transport::find(&bus, f), Ok(None), "legacy-only");

    let (mut bus, f) = virtio_blk();
    bus.device_mut(f).put8(0x62, 16);
    assert_eq!(
        Transport::find(&bus, f),
        Err(PciError::CapabilityTooShort {
            function: f,
            at: 0x60,
            len: 16
        }),
        "a notify capability without its multiplier"
    );

    let mut bus = Bus::new();
    let mut d = Device::endpoint(0x1AF4, 0x1042);
    d.capabilities(0xF4);
    d.capability(0xF4, ID_VENDOR, 0);
    d.put8(0xF6, 16);
    bus.put(at(0, 1, 0), d);
    assert_eq!(
        Transport::find(&bus, at(0, 1, 0)),
        Err(PciError::CapabilityTooShort {
            function: at(0, 1, 0),
            at: 0xF4,
            len: 12
        }),
        "runs past the legacy space"
    );
}

/// A virtio-gpu-pci with `hostmem` set, as QEMU 10.2 lays it out
/// (`virtio_gpu_pci_base_realize`): the registers move to BAR 2, and BAR 4 is
/// a 64-bit prefetchable window of 1 GiB named by a shared memory capability
/// with id 1, `struct virtio_pci_cap64`: bar at 4, id at 5, offset at 8,
/// length at 12, and their high halves at 16 and 20.
fn virtio_gpu_with_hostmem() -> (Bus, Address) {
    let mut bus = Bus::new();
    let mut d = Device::endpoint(0x1AF4, 0x1050);
    d.bar(2, 0x0000_000C, 0xFFFF_C000);
    d.bar(3, 0x0000_0080, 0xFFFF_FFFF);
    d.bar(4, 0x0000_000C, 0xC000_0000);
    d.bar(5, 0x0000_0100, 0xFFFF_FFFF);
    d.capabilities(0xA0);
    d.capability(0xA0, ID_VENDOR, 0x40);
    d.put8(0xA2, 24);
    d.put8(0xA3, virtio::CFG_SHARED_MEMORY);
    d.put8(0xA4, 4);
    d.put8(0xA5, 1);
    d.put32(0xA8, 0);
    d.put32(0xAC, 0x4000_0000);
    d.put32(0xB0, 0);
    d.put32(0xB4, 0);
    d.virtio_capability(0x40, 0x00, virtio::CFG_COMMON, 2, 0x0000, 0x1000);
    bus.put(at(0, 4, 0), d);
    (bus, at(0, 4, 0))
}

#[test]
fn a_shared_memory_region_is_found_by_its_id_with_wide_fields() {
    let (mut bus, f) = virtio_gpu_with_hostmem();
    let found = SharedMemory::find(&bus, f, 1).unwrap().unwrap();
    assert_eq!(
        found,
        SharedMemory {
            capability: 0xA0,
            bar: 4,
            id: 1,
            offset: 0,
            length: 0x4000_0000,
        }
    );
    assert_eq!(SharedMemory::find(&bus, f, 2), Ok(None), "no region 2");
    let region: Region = bar::size(&mut bus, f, HeaderKind::Endpoint, 4)
        .unwrap()
        .unwrap();
    assert_eq!(region.size, 0x4000_0000);
    assert!(found.fits(&region), "the whole BAR is the window");

    // A region past 4 GiB carries its high halves.
    bus.device_mut(f).put32(0xB0, 1);
    bus.device_mut(f).put32(0xB4, 2);
    let wide = SharedMemory::find(&bus, f, 1).unwrap().unwrap();
    assert_eq!((wide.offset, wide.length), (1 << 32, 0x2_4000_0000));
    assert!(!wide.fits(&region), "and no longer fits a 1 GiB BAR");
}

#[test]
fn a_short_or_reserved_shared_memory_capability_is_not_a_region() {
    let (mut bus, f) = virtio_gpu_with_hostmem();
    bus.device_mut(f).put8(0xA2, 16);
    assert_eq!(
        SharedMemory::find(&bus, f, 1),
        Err(PciError::CapabilityTooShort {
            function: f,
            at: 0xA0,
            len: 16
        }),
        "a capability without its high halves"
    );
    let (mut bus, f) = virtio_gpu_with_hostmem();
    bus.device_mut(f).put8(0xA4, 7);
    assert_eq!(SharedMemory::find(&bus, f, 1), Ok(None), "a reserved BAR");
    let (bus, f) = virtio_gpu_with_hostmem();
    let found = SharedMemory::find(&bus, f, 1).unwrap().unwrap();
    let empty = SharedMemory { length: 0, ..found };
    let region = Region {
        index: 4,
        bar: Bar::Memory {
            address: 0x100_0000_0000,
            prefetchable: true,
            wide: true,
        },
        size: 0x4000_0000,
    };
    assert!(!empty.fits(&region), "a region of nothing is no window");
}

#[test]
fn a_non_vendor_capability_is_not_a_virtio_one() {
    let (bus, f) = virtio_blk();
    let cap = Capability {
        function: f,
        id: ID_MSIX,
        offset: 0x98,
    };
    assert_eq!(virtio::read(&bus, cap), Ok(None), "MSI-X");
}

#[test]
fn every_error_prints_the_function_it_concerns() {
    let f = at(0, 3, 0);
    let errors = [
        PciError::CapabilityPointer {
            function: f,
            at: 0x34,
            pointer: 0x10,
        },
        PciError::CapabilityLoop {
            function: f,
            at: 0x40,
        },
        PciError::CapabilityTooShort {
            function: f,
            at: 0x40,
            len: 4,
        },
        PciError::ReservedBarType {
            function: f,
            index: 0,
        },
        PciError::TruncatedBar {
            function: f,
            index: 5,
        },
        PciError::NoSuchBar {
            function: f,
            index: 6,
        },
        PciError::BarMask {
            function: f,
            index: 0,
            mask: 3,
        },
        PciError::BarPlacement {
            function: f,
            index: 1,
        },
        PciError::HeaderType {
            function: f,
            kind: 1,
        },
        PciError::VirtioRegion {
            function: f,
            at: 0x40,
        },
    ];
    for error in errors {
        let text = std::format!("{error}");
        assert!(text.starts_with("0000:00:03.0: "), "{text}");
    }
}

// ---------------------------------------------------------------------------
// MSI-X
// ---------------------------------------------------------------------------

use crate::msix;

fn region(index: u8, size: u64) -> Region {
    Region {
        index,
        bar: Bar::Memory {
            address: 0x8000_0000,
            wide: true,
            prefetchable: false,
        },
        size,
    }
}

fn msix_at(table: (u8, u32), pending: (u8, u32), vectors: u16) -> MsiX {
    MsiX {
        table_size: vectors,
        enabled: false,
        function_masked: false,
        table: BarOffset {
            bar: table.0,
            offset: table.1,
        },
        pending: BarOffset {
            bar: pending.0,
            offset: pending.1,
        },
    }
}

#[test]
fn a_bar_without_msix_is_mappable_whole() {
    let r = region(4, 0x4000);
    assert_eq!(
        msix::mappable(&r, None, 0x1000).as_slice(),
        &[(0, 0x4000)],
        "whole"
    );
    assert!(
        msix::withheld(&r, None, 0x1000).as_slice().is_empty(),
        "nothing withheld"
    );
}

#[test]
fn qemus_virtio_msix_bar_is_withheld_entirely_and_its_register_bar_is_not() {
    // virtio-rng-pci: two vectors' table at 0 and pending bits at 0x800, both
    // in the 4 KiB BAR 1; the virtio registers in the 16 KiB BAR 4.
    let table = msix_at((1, 0), (1, 0x800), 2);
    assert!(
        msix::mappable(&region(1, 0x1000), Some(&table), 0x1000)
            .as_slice()
            .is_empty(),
        "BAR 1 is all table"
    );
    assert_eq!(
        msix::mappable(&region(4, 0x4000), Some(&table), 0x1000).as_slice(),
        &[(0, 0x4000)],
        "BAR 4 untouched"
    );
}

#[test]
fn the_table_and_pending_pages_are_cut_out_of_a_shared_bar() {
    // A table in page 1 and pending bits in page 3 of a 16 KiB BAR.
    let table = msix_at((0, 0x1010), (0, 0x3008), 4);
    let r = region(0, 0x4000);
    assert_eq!(
        msix::withheld(&r, Some(&table), 0x1000).as_slice(),
        &[(0x1000, 0x1000), (0x3000, 0x1000)],
        "rounded out to pages"
    );
    assert_eq!(
        msix::mappable(&r, Some(&table), 0x1000).as_slice(),
        &[(0, 0x1000), (0x2000, 0x1000)],
        "the pages between"
    );
}

#[test]
fn a_table_spanning_pages_merges_with_its_pending_bits() {
    // 300 vectors is 4800 bytes of table from 0x800, running into the page the
    // pending bits start in.
    let table = msix_at((2, 0x800), (2, 0x1800), 300);
    let r = region(2, 0x3000);
    assert_eq!(
        msix::withheld(&r, Some(&table), 0x1000).as_slice(),
        &[(0, 0x2000)],
        "one merged span"
    );
    assert_eq!(
        msix::mappable(&r, Some(&table), 0x1000).as_slice(),
        &[(0x2000, 0x1000)],
        "rest"
    );
}

#[test]
fn a_structure_past_the_bar_or_in_another_bar_withholds_nothing_here() {
    let past = msix_at((0, 0xFFFF_F000), (0, 0xFFFF_FFF8), 2048);
    let r = region(0, 0x2000);
    assert_eq!(
        msix::mappable(&r, Some(&past), 0x1000).as_slice(),
        &[(0, 0x2000)],
        "past the end"
    );
    let elsewhere = msix_at((3, 0), (3, 0x100), 1);
    assert_eq!(
        msix::mappable(&r, Some(&elsewhere), 0x1000).as_slice(),
        &[(0, 0x2000)],
        "BAR 3"
    );
}

#[test]
fn table_entries_are_bounded_by_the_table() {
    let table = msix_at((1, 0x2000), (1, 0x3000), 3);
    assert_eq!(msix::entry_offset(&table, 0), Some(0x2000), "first");
    assert_eq!(msix::entry_offset(&table, 2), Some(0x2020), "last");
    assert_eq!(msix::entry_offset(&table, 3), None, "past the end");
}

#[test]
fn a_local_apic_message_carries_the_destination_and_the_vector() {
    assert_eq!(
        msix::local_apic_message(3, 0x41),
        msix::Message {
            address: 0xFEE0_3000,
            data: 0x41
        },
        "APIC 3, vector 0x41"
    );
}

#[test]
fn a_gicv2m_frame_says_which_spis_it_raises() {
    // QEMU's virt: SPIs 48 to 111, which are GIC identifiers 80 to 143.
    let spis = msix::gicv2m_spis(80 << 16 | 64).unwrap();
    assert_eq!(
        spis,
        msix::SpiRange {
            first: 80,
            count: 64
        },
        "QEMU's range"
    );
    assert!(spis.contains(80) && spis.contains(143), "both ends");
    assert!(
        !spis.contains(79) && !spis.contains(144),
        "neither neighbour"
    );
    assert_eq!(msix::gicv2m_spis(80 << 16), None, "no SPIs");
    assert_eq!(msix::gicv2m_spis(16 << 16 | 4), None, "a private interrupt");
    assert_eq!(msix::gicv2m_spis(1000 << 16 | 64), None, "past 1019");
    assert_eq!(
        msix::gicv2m_message(0x0802_0000, 81),
        Some(msix::Message {
            address: 0x0802_0040,
            data: 81
        }),
        "SETSPI with the identifier"
    );
    assert_eq!(msix::gicv2m_message(u64::MAX, 81), None, "overflow");
}

#[test]
fn a_virtio_block_in_an_io_bar_fits_nothing() {
    let (bus, f) = virtio_blk();
    let transport = Transport::find(&bus, f).unwrap().unwrap();
    let io = Region {
        index: 4,
        bar: Bar::Io { port: 0xC000 },
        size: 0x4000,
    };
    assert_eq!(
        transport.verify(&[io]),
        Err(PciError::VirtioRegion {
            function: f,
            at: 0x40
        }),
        "an I/O BAR is not memory"
    );
}

#[test]
fn a_requester_id_packs_bus_device_and_function() {
    let id = |bus, device, function| {
        Address::new(0, bus, device, function)
            .expect("in range")
            .requester_id()
    };
    assert_eq!(id(0, 2, 0), 0x0010, "00:02.0, where QEMU puts virtio-rng");
    assert_eq!(id(0xff, 31, 7), 0xffff, "the last function of the last bus");
    assert_eq!(id(1, 0, 1), 0x0101, "a function on a secondary bus");
}
