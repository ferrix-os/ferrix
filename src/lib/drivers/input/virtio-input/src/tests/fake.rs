//! A virtio-input device on the host: its registers, its configuration
//! queries, its event queue, and QEMU 9.2.4's way of sending events.
//!
//! The configuration tables mirror `hw/input/virtio-input-hid.c`'s keyboard,
//! mouse, tablet and multi-touch devices as `src/lib/drivers/virtio`'s input tests build
//! them, the keyboard's key bitmap abbreviated to `KEY_ESC`, `KEY_A` and
//! `KEY_MEDIA` as theirs is. [`Device::send`] is `virtio_input_send` in
//! `hw/input/virtio-input.c`: nothing before `DRIVER_OK`, events held until
//! their `SYN_REPORT`, and the whole report dropped without a word when the
//! queue lacks a buffer for any of its events. Every way the driver breaks the
//! protocol goes to `protocol_errors`.

use core::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use ferrix_virtio::input::{ConfigSelect, DeviceConfig, EVENT_LEN, Event};
use ferrix_virtio::pci::{
    CONFIG_MSIX_VECTOR, CommonConfig, DEVICE_FEATURE, DEVICE_FEATURE_SELECT, DEVICE_STATUS,
    DRIVER_FEATURE, DRIVER_FEATURE_SELECT, FEATURE_ACCESS_PLATFORM, FEATURE_VERSION_1, NO_VECTOR,
    NUM_QUEUES, QUEUE_DESC, QUEUE_DEVICE, QUEUE_DRIVER, QUEUE_ENABLE, QUEUE_MSIX_VECTOR,
    QUEUE_NOTIFY_OFF, QUEUE_SELECT, QUEUE_SIZE, STATUS_DEVICE_NEEDS_RESET, STATUS_DRIVER_OK,
    STATUS_FEATURES_OK,
};
use ferrix_virtio::{Descriptor, Layout, PAGE_SIZE, QueueMemory, SplitQueueDevice};

use crate::{DevicePages, EventArea, ISR_QUEUE, Transport};

pub(super) const PAGE: usize = PAGE_SIZE as usize;

/// Pinned pages, reached by device address.
#[derive(Debug, Default)]
pub(super) struct Bus {
    pages: RefCell<BTreeMap<u64, Vec<u8>>>,
    next: RefCell<u64>,
}

impl Bus {
    pub(super) fn new() -> Rc<Self> {
        let bus = Self::default();
        *bus.next.borrow_mut() = 0x10_0000_0000;
        Rc::new(bus)
    }

    /// Pin `count` pages, consecutive or each apart from the last.
    pub(super) fn pin(self: &Rc<Self>, count: usize, scattered: bool) -> Region {
        let mut device = Vec::new();
        for _ in 0..count {
            let address = *self.next.borrow();
            let _ = self.pages.borrow_mut().insert(address, vec![0; PAGE]);
            device.push(address);
            *self.next.borrow_mut() += if scattered { 3 * PAGE_SIZE } else { PAGE_SIZE };
        }
        *self.next.borrow_mut() += 16 * PAGE_SIZE;
        Region {
            bus: Rc::clone(self),
            device,
        }
    }

    fn read(&self, address: u64) -> Option<u8> {
        let page = address & !(PAGE_SIZE - 1);
        self.pages
            .borrow()
            .get(&page)
            .and_then(|bytes| bytes.get((address - page) as usize).copied())
    }

    fn write(&self, address: u64, value: u8) -> bool {
        let page = address & !(PAGE_SIZE - 1);
        match self.pages.borrow_mut().get_mut(&page) {
            Some(bytes) => {
                bytes[(address - page) as usize] = value;
                true
            }
            None => false,
        }
    }
}

/// A pinned region as the driver sees it.
#[derive(Clone, Debug)]
pub(super) struct Region {
    bus: Rc<Bus>,
    device: Vec<u64>,
}

impl DevicePages for Region {
    fn device_pages(&self) -> &[u64] {
        &self.device
    }
}

impl EventArea for Region {
    fn read_u8(&self, offset: usize) -> u8 {
        self.device
            .get(offset / PAGE)
            .and_then(|page| self.bus.read(page + (offset % PAGE) as u64))
            .unwrap_or(0)
    }
    fn write_u8(&mut self, offset: usize, value: u8) {
        if let Some(page) = self.device.get(offset / PAGE) {
            let _ = self.bus.write(page + (offset % PAGE) as u64, value);
        }
    }
}

#[expect(
    unsafe_code,
    reason = "AUDIT: QueueMemory is an unsafe trait; this implementation only indexes Vecs through checked lookups"
)]
// SAFETY: every access is a checked lookup of a page the bus owns and a miss
// reads zero; nothing is dereferenced as a struct; driver and device are
// stepped one after the other, so the barrier has nothing to order.
unsafe impl QueueMemory for Region {
    fn read_u8(&self, offset: usize) -> u8 {
        EventArea::read_u8(self, offset)
    }
    fn write_u8(&mut self, offset: usize, value: u8) {
        EventArea::write_u8(self, offset, value);
    }
    // One side at a time here, never both at once, so a `u16` of two byte
    // accesses is as whole as one.
    fn read_u16(&self, offset: usize) -> u16 {
        u16::from_le_bytes([
            QueueMemory::read_u8(self, offset),
            QueueMemory::read_u8(self, offset + 1),
        ])
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        let [low, high] = value.to_le_bytes();
        QueueMemory::write_u8(self, offset, low);
        QueueMemory::write_u8(self, offset + 1, high);
    }

    fn barrier(&self) {}
}

/// The device's view of the rings.
#[derive(Debug)]
struct RingView {
    bus: Rc<Bus>,
    layout: Layout,
    descriptors: u64,
    driver: u64,
    device: u64,
}

impl RingView {
    fn address(&self, offset: usize) -> u64 {
        let layout = &self.layout;
        if offset < layout.available_ring {
            self.descriptors + offset as u64
        } else if offset < layout.used_ring {
            self.driver + (offset - layout.available_ring) as u64
        } else {
            self.device + (offset - layout.used_ring) as u64
        }
    }
}

#[expect(
    unsafe_code,
    reason = "AUDIT: QueueMemory is an unsafe trait; every access goes through the bus's checked lookup"
)]
// SAFETY: as `Region`'s.
unsafe impl QueueMemory for RingView {
    fn read_u8(&self, offset: usize) -> u8 {
        self.bus.read(self.address(offset)).unwrap_or(0)
    }
    fn write_u8(&mut self, offset: usize, value: u8) {
        let _ = self.bus.write(self.address(offset), value);
    }
    // One side at a time here, never both at once, so a `u16` of two byte
    // accesses is as whole as one.
    fn read_u16(&self, offset: usize) -> u16 {
        u16::from_le_bytes([
            QueueMemory::read_u8(self, offset),
            QueueMemory::read_u8(self, offset + 1),
        ])
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        let [low, high] = value.to_le_bytes();
        QueueMemory::write_u8(self, offset, low);
        QueueMemory::write_u8(self, offset + 1, high);
    }

    fn barrier(&self) {}
}

/// One configuration answer: `select`, `subsel`, and the union's bytes, whose
/// length is `size`.
pub(super) type Answer = (u8, u8, Vec<u8>);

/// A QEMU name: `sizeof` the literal, so the NUL is counted.
pub(super) fn qemu_name(name: &[u8]) -> Answer {
    let mut payload = name.to_vec();
    payload.push(0);
    (0x01, 0, payload)
}

/// QEMU's ids: `BUS_VIRTUAL`, vendor 0x0627.
pub(super) fn qemu_devids(product: u16, version: u16) -> Answer {
    let mut payload = vec![0x06, 0x00, 0x27, 0x06];
    payload.extend_from_slice(&product.to_le_bytes());
    payload.extend_from_slice(&version.to_le_bytes());
    (0x03, 0, payload)
}

/// A bitmap as `virtio_input_extend_config` builds one: as many bytes as
/// reach the highest bit.
pub(super) fn qemu_bits(select: u8, subsel: u8, bits: &[u16]) -> Answer {
    let top = bits.iter().copied().max().unwrap_or(0);
    let mut payload = vec![0u8; usize::from(top / 8) + 1];
    for &bit in bits {
        payload[usize::from(bit / 8)] |= 1 << (bit % 8);
    }
    (select, subsel, payload)
}

/// An axis as QEMU's tables give it: only `min` and `max`.
pub(super) fn qemu_abs(axis: u8, min: i32, max: i32) -> Answer {
    let mut payload = Vec::new();
    for field in [min, max, 0, 0, 0] {
        payload.extend_from_slice(&field.to_le_bytes());
    }
    (0x12, axis, payload)
}

/// `BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`,
/// `BTN_TOUCH`, `BTN_GEAR_DOWN`, `BTN_GEAR_UP`: `keymap_button`.
pub(super) const BUTTONS: [u16; 8] = [0x110, 0x111, 0x112, 0x113, 0x114, 0x14a, 0x150, 0x151];

/// `virtio_keyboard_config` and its key bitmap.
pub(super) fn keyboard() -> Vec<Answer> {
    vec![
        qemu_name(b"QEMU Virtio Keyboard"),
        qemu_devids(1, 1),
        (0x11, 0x14, vec![0]),
        (0x11, 0x11, vec![0b111]),
        qemu_bits(0x11, 0x01, &[1, 30, 226]),
    ]
}

/// `virtio_mouse_config_v2`, `wheel-axis` being on by default.
pub(super) fn mouse() -> Vec<Answer> {
    vec![
        qemu_name(b"QEMU Virtio Mouse"),
        qemu_devids(2, 2),
        // REL_X and REL_Y, and REL_WHEEL, which is bit 8: the second byte's low bit.
        (0x11, 0x02, vec![0b11, 0b1]),
        qemu_bits(0x11, 0x01, &BUTTONS),
    ]
}

/// `virtio_tablet_config_v2`.
pub(super) fn tablet() -> Vec<Answer> {
    vec![
        qemu_name(b"QEMU Virtio Tablet"),
        qemu_devids(3, 2),
        (0x11, 0x03, vec![0b11]),
        // No REL_X or REL_Y; REL_WHEEL alone, bit 8.
        (0x11, 0x02, vec![0, 0b1]),
        qemu_abs(0x00, 0, 0x7fff),
        qemu_abs(0x01, 0, 0x7fff),
        qemu_bits(0x11, 0x01, &BUTTONS),
    ]
}

/// `virtio_multitouch_config`: multi-touch axes and `INPUT_PROP_DIRECT`.
pub(super) fn multitouch() -> Vec<Answer> {
    vec![
        qemu_name(b"QEMU Virtio MultiTouch"),
        qemu_devids(3, 1),
        qemu_abs(0x2f, 0, 10),
        qemu_abs(0x39, 0, 10),
        qemu_abs(0x35, 0, 0x7fff),
        qemu_abs(0x36, 0, 0x7fff),
        qemu_bits(0x11, 0x01, &[0x110, 0x14a, 0x151]),
        qemu_bits(0x10, 0, &[1]),
        qemu_bits(0x11, 0x03, &[0x2f, 0x35, 0x36, 0x39]),
    ]
}

/// What the device gets wrong.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Misbehave {
    /// Complete each buffer claiming this many bytes written.
    pub(super) written: Option<u32>,
    /// Set `DEVICE_NEEDS_RESET`.
    pub(super) needs_reset: bool,
    /// Keep `device_status` from reading zero after a reset.
    pub(super) never_reset: bool,
    /// Complete each event as it comes rather than holding its report: not
    /// what QEMU does, but what a device may.
    pub(super) no_hold: bool,
    /// Keep the MSI-X vector at `NO_VECTOR` whatever is asked.
    pub(super) drop_vector: bool,
}

/// The device.
#[derive(Debug)]
pub(super) struct Device {
    bus: Rc<Bus>,
    pub(super) offered: u64,
    pub(super) accepted: u64,
    device_select: u32,
    driver_select: u32,
    pub(super) status: u8,
    queue_select: u16,
    queue_max: u16,
    queue_size: u16,
    queue_vector: u16,
    addresses: [u64; 3],
    ring: Option<SplitQueueDevice<RingView>>,
    status_size: u16,
    status_addresses: [u64; 3],
    /// The status queue as enabled: its size and its three addresses. The
    /// device takes nothing from it, since nothing is ever posted there.
    pub(super) status_queue: Option<(u16, [u64; 3])>,
    answers: Vec<Answer>,
    block: Vec<u8>,
    active: bool,
    held: VecDeque<u16>,
    pending: Vec<Event>,
    /// Events written into the driver's buffers.
    pub(super) delivered: usize,
    /// Reports dropped for want of buffers.
    pub(super) dropped_reports: usize,
    /// Events sent before `DRIVER_OK`, and so discarded.
    pub(super) inactive: usize,
    pub(super) misbehave: Misbehave,
    pub(super) protocol_errors: Vec<&'static str>,
    /// The MSI-X vector configuration changes are raised on.
    pub(super) config_vector: u16,
    /// How many times the driver read the device status.
    pub(super) status_reads: core::cell::Cell<u32>,
    /// ISR bits the next acknowledgement answers besides [`ISR_QUEUE`].
    pub(super) isr: u8,
}

impl Device {
    /// A device answering `answers`, its block sized as QEMU sizes it: the
    /// longest answer plus the 8-byte header.
    pub(super) fn new(bus: Rc<Bus>, answers: Vec<Answer>) -> Self {
        let longest = answers
            .iter()
            .map(|answer| answer.2.len())
            .max()
            .unwrap_or(0);
        Self {
            bus,
            offered: FEATURE_VERSION_1 | FEATURE_ACCESS_PLATFORM,
            accepted: 0,
            device_select: 0,
            driver_select: 0,
            status: 0,
            queue_select: 0,
            queue_max: 64,
            queue_size: 64,
            queue_vector: NO_VECTOR,
            addresses: [0; 3],
            ring: None,
            status_size: 64,
            status_addresses: [0; 3],
            status_queue: None,
            answers,
            block: vec![0; longest + 8],
            active: false,
            held: VecDeque::new(),
            pending: Vec::new(),
            delivered: 0,
            dropped_reports: 0,
            inactive: 0,
            misbehave: Misbehave::default(),
            protocol_errors: Vec::new(),
            config_vector: NO_VECTOR,
            status_reads: core::cell::Cell::new(0),
            isr: 0,
        }
    }

    /// The largest queue the device allows.
    pub(super) fn set_queue_max(&mut self, max: u16) {
        self.queue_max = max;
        self.queue_size = max;
    }

    fn register(&self, offset: u32) -> u32 {
        let selected = self.queue_select <= 1;
        match offset {
            DEVICE_FEATURE => match self.device_select {
                0 => self.offered as u32,
                1 => (self.offered >> 32) as u32,
                _ => 0,
            },
            NUM_QUEUES => 2,
            DEVICE_STATUS => {
                self.status_reads.set(self.status_reads.get() + 1);
                u32::from(self.status)
                    | if self.misbehave.needs_reset && self.status != 0 {
                        u32::from(STATUS_DEVICE_NEEDS_RESET)
                    } else {
                        0
                    }
            }
            QUEUE_SELECT => u32::from(self.queue_select),
            CONFIG_MSIX_VECTOR => u32::from(self.config_vector),
            QUEUE_SIZE if self.queue_select == 1 => u32::from(self.status_size),
            QUEUE_SIZE if selected => u32::from(self.queue_size),
            QUEUE_MSIX_VECTOR if self.queue_select == 0 => u32::from(self.queue_vector),
            QUEUE_ENABLE if self.queue_select == 0 => u32::from(self.ring.is_some()),
            QUEUE_ENABLE if self.queue_select == 1 => u32::from(self.status_queue.is_some()),
            QUEUE_NOTIFY_OFF => 5,
            _ => 0,
        }
    }

    fn reset(&mut self) {
        if !self.misbehave.never_reset {
            self.status = 0;
        }
        self.ring = None;
        self.status_queue = None;
        self.status_size = 64;
        self.active = false;
        self.held.clear();
        self.pending.clear();
        self.queue_size = self.queue_max;
        self.config_vector = NO_VECTOR;
    }

    fn set_register(&mut self, offset: u32, value: u32) {
        match offset {
            DEVICE_FEATURE_SELECT => self.device_select = value,
            DRIVER_FEATURE_SELECT => self.driver_select = value,
            DRIVER_FEATURE => match self.driver_select {
                0 => self.accepted = (self.accepted & !0xFFFF_FFFF) | u64::from(value),
                _ => self.accepted = (self.accepted & 0xFFFF_FFFF) | u64::from(value) << 32,
            },
            DEVICE_STATUS if value == 0 => self.reset(),
            DEVICE_STATUS => {
                if value as u8 & STATUS_DRIVER_OK != 0 {
                    if self.ring.is_none() {
                        self.protocol_errors
                            .push("DRIVER_OK before the event queue");
                    }
                    self.active = true;
                }
                self.status = value as u8;
            }
            QUEUE_SELECT => self.queue_select = value as u16,
            CONFIG_MSIX_VECTOR if !self.misbehave.drop_vector => {
                self.config_vector = value as u16;
            }
            QUEUE_SIZE if self.queue_select == 0 => self.queue_size = value as u16,
            QUEUE_SIZE if self.queue_select == 1 => self.status_size = value as u16,
            QUEUE_MSIX_VECTOR if self.queue_select == 0 && !self.misbehave.drop_vector => {
                self.queue_vector = value as u16;
            }
            QUEUE_ENABLE => self.enable(),
            _ => {
                for (index, base) in [QUEUE_DESC, QUEUE_DRIVER, QUEUE_DEVICE]
                    .into_iter()
                    .enumerate()
                {
                    let addresses = match self.queue_select {
                        0 => &mut self.addresses,
                        1 => &mut self.status_addresses,
                        _ => continue,
                    };
                    if offset == base {
                        addresses[index] = (addresses[index] & !0xFFFF_FFFF) | u64::from(value);
                    } else if offset == base + 4 {
                        addresses[index] =
                            (addresses[index] & 0xFFFF_FFFF) | u64::from(value) << 32;
                    }
                }
            }
        }
    }

    fn enable(&mut self) {
        if self.queue_select == 1 {
            self.enable_status();
            return;
        }
        if self.queue_select != 0 {
            self.protocol_errors
                .push("a queue past the status queue enabled");
            return;
        }
        if self.status & STATUS_FEATURES_OK == 0 {
            self.protocol_errors
                .push("a queue enabled before FEATURES_OK");
        }
        let layout = Layout::for_size(self.queue_size).expect("a valid size");
        self.ring = Some(SplitQueueDevice::new(
            layout,
            RingView {
                bus: Rc::clone(&self.bus),
                layout,
                descriptors: self.addresses[0],
                driver: self.addresses[1],
                device: self.addresses[2],
            },
        ));
    }

    /// The status queue, as crosvm requires it enabled: after `FEATURES_OK`,
    /// a power of two long, and in memory clear of the event queue's rings.
    fn enable_status(&mut self) {
        if self.status & STATUS_FEATURES_OK == 0 {
            self.protocol_errors
                .push("a queue enabled before FEATURES_OK");
        }
        if !self.status_size.is_power_of_two() {
            self.protocol_errors.push("a status queue of no valid size");
            return;
        }
        let layout = Layout::for_size(self.status_size).expect("a valid size");
        let status = [
            (self.status_addresses[0], layout.available_ring),
            (
                self.status_addresses[1],
                layout.used_ring - layout.available_ring,
            ),
            (
                self.status_addresses[2],
                layout.total_size - layout.used_ring,
            ),
        ];
        if let Some(event) = self.ring.as_ref().map(|_| self.addresses) {
            let event_layout = Layout::for_size(self.queue_size).expect("a valid size");
            let event = [
                (event[0], event_layout.available_ring),
                (
                    event[1],
                    event_layout.used_ring - event_layout.available_ring,
                ),
                (event[2], event_layout.total_size - event_layout.used_ring),
            ];
            let overlaps = status.iter().any(|&(at, len)| {
                event.iter().any(|&(other, other_len)| {
                    at < other + other_len as u64 && other < at + len as u64
                })
            });
            if overlaps {
                self.protocol_errors
                    .push("the status queue overlaps the event queue");
            }
        }
        self.status_queue = Some((self.status_size, self.status_addresses));
    }

    /// `virtio_input_set_config` then `virtio_input_get_config`: the block
    /// holds the whole matching entry, or zeros.
    fn select(&mut self, offset: u32, value: u8) {
        if self.status & STATUS_FEATURES_OK == 0 {
            self.protocol_errors
                .push("the configuration asked before FEATURES_OK");
        }
        let (mut select, mut subsel) = (self.block[0], self.block[1]);
        match offset {
            0 => select = value,
            1 => subsel = value,
            _ => self.protocol_errors.push("a write past select and subsel"),
        }
        self.block.fill(0);
        self.block[0] = select;
        self.block[1] = subsel;
        if let Some((_, _, payload)) = self
            .answers
            .iter()
            .find(|(s, sub, _)| (*s, *sub) == (select, subsel))
        {
            self.block[2] = payload.len() as u8;
            let len = payload.len().min(self.block.len() - 8);
            self.block[8..8 + len].copy_from_slice(&payload[..len]);
        }
    }

    /// Chains the driver has made available, taken off the ring.
    fn gather(&mut self) {
        let Some(ring) = self.ring.as_mut() else {
            return;
        };
        loop {
            match ring.next_chain() {
                Ok(Some(head)) => self.held.push_back(head),
                Ok(None) => return,
                Err(_) => {
                    self.protocol_errors.push("a corrupt available ring");
                    return;
                }
            }
        }
    }

    /// Buffers the device holds for events.
    pub(super) fn buffers(&mut self) -> usize {
        self.gather();
        self.held.len()
    }

    /// Write `event` into the next held buffer and complete it.
    fn write(&mut self, event: Event) {
        let Some(head) = self.held.pop_front() else {
            return;
        };
        let ring = self.ring.as_mut().expect("a ring holds the chain");
        let mut descriptors = [Descriptor {
            address: 0,
            len: 0,
            flags: 0,
            next: 0,
        }; 4];
        let Ok(count) = ring.read_chain(head, &mut descriptors) else {
            self.protocol_errors.push("an unreadable chain");
            return;
        };
        let [descriptor] = descriptors[..count] else {
            self.protocol_errors
                .push("an event chain of more than one buffer");
            return;
        };
        if !descriptor.is_device_writable() {
            self.protocol_errors
                .push("an event buffer the device cannot write");
        }
        let len = (descriptor.len as usize).min(EVENT_LEN);
        for (index, &byte) in event.to_bytes()[..len].iter().enumerate() {
            let _ = self.bus.write(descriptor.address + index as u64, byte);
        }
        let written = self.misbehave.written.unwrap_or(len as u32);
        ring.complete(head, written).expect("completes");
        self.delivered += 1;
    }

    /// `virtio_input_send`.
    pub(super) fn send(&mut self, event: Event) {
        if !self.active {
            self.inactive += 1;
            return;
        }
        self.gather();
        if self.misbehave.no_hold {
            if self.held.is_empty() {
                self.dropped_reports += 1;
            } else {
                self.write(event);
            }
            return;
        }
        self.pending.push(event);
        if !event.is_report() {
            return;
        }
        let report: Vec<Event> = core::mem::take(&mut self.pending);
        if self.held.len() < report.len() {
            self.dropped_reports += 1;
            return;
        }
        for event in report {
            self.write(event);
        }
    }

    /// Move `used.idx` on by `by` without completing anything.
    pub(super) fn jump_used_index(&mut self, by: u16) {
        let Some(ring) = self.ring.as_ref() else {
            return;
        };
        let at = self.addresses[2] + 2;
        let index = ring.used_index().wrapping_add(by);
        for (offset, byte) in index.to_le_bytes().into_iter().enumerate() {
            let _ = self.bus.write(at + offset as u64, byte);
        }
    }
}

/// The driver's handle on the device.
#[derive(Clone, Debug)]
pub(super) struct Handle {
    pub(super) device: Rc<RefCell<Device>>,
    pub(super) doorbells: Rc<RefCell<usize>>,
}

impl CommonConfig for Handle {
    fn read8(&self, offset: u32) -> u8 {
        self.device.borrow().register(offset) as u8
    }
    fn read16(&self, offset: u32) -> u16 {
        self.device.borrow().register(offset) as u16
    }
    fn read32(&self, offset: u32) -> u32 {
        self.device.borrow().register(offset)
    }
    fn write8(&mut self, offset: u32, value: u8) {
        self.device
            .borrow_mut()
            .set_register(offset, u32::from(value));
    }
    fn write16(&mut self, offset: u32, value: u16) {
        self.device
            .borrow_mut()
            .set_register(offset, u32::from(value));
    }
    fn write32(&mut self, offset: u32, value: u32) {
        self.device.borrow_mut().set_register(offset, value);
    }
}

impl DeviceConfig for Handle {
    fn config_len(&self) -> u32 {
        self.device.borrow().block.len() as u32
    }
    fn config_read8(&self, offset: u32) -> u8 {
        // Past the block, QEMU's virtio_config_modern_readb reads all ones.
        self.device
            .borrow()
            .block
            .get(offset as usize)
            .copied()
            .unwrap_or(0xff)
    }
    fn config_read16(&self, offset: u32) -> u16 {
        u16::from_le_bytes([self.config_read8(offset), self.config_read8(offset + 1)])
    }
    fn config_read32(&self, offset: u32) -> u32 {
        u32::from_le_bytes([
            self.config_read8(offset),
            self.config_read8(offset + 1),
            self.config_read8(offset + 2),
            self.config_read8(offset + 3),
        ])
    }
}

impl ConfigSelect for Handle {
    fn config_write8(&mut self, offset: u32, value: u8) {
        self.device.borrow_mut().select(offset, value);
    }
}

impl Transport for Handle {
    fn notify(&mut self, queue: u16, notify_off: u16) {
        *self.doorbells.borrow_mut() += 1;
        if queue != 0 || notify_off != 5 {
            self.device
                .borrow_mut()
                .protocol_errors
                .push("a doorbell for the wrong queue");
        }
        if self.device.borrow().status & STATUS_DRIVER_OK == 0 {
            self.device
                .borrow_mut()
                .protocol_errors
                .push("a doorbell before DRIVER_OK");
        }
    }
    fn queue_vector(&self) -> u16 {
        1
    }
    fn acknowledge_interrupt(&mut self) -> u8 {
        ISR_QUEUE | core::mem::take(&mut self.device.borrow_mut().isr)
    }
}
