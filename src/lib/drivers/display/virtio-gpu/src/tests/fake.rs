//! A virtio-gpu device on the host: its registers, its control and cursor
//! queues, and the 2D commands, served against memory reached only by device
//! address.
//!
//! The device keeps a host copy of every resource, as QEMU does, fills it
//! from the guest backing on `TRANSFER_TO_HOST_2D`, and keeps the scanout's
//! resource as "the screen" on `RESOURCE_FLUSH`, which is what the tests look
//! at. Every way the driver breaks the protocol goes to `protocol_errors`.

use core::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use ferrix_virtio::gpu::{
    CMD_GET_DISPLAY_INFO, CMD_MOVE_CURSOR, CMD_RESOURCE_ATTACH_BACKING, CMD_RESOURCE_CREATE_2D,
    CMD_RESOURCE_DETACH_BACKING, CMD_RESOURCE_FLUSH, CMD_RESOURCE_UNREF, CMD_SET_SCANOUT,
    CMD_SUBMIT_3D, CMD_TRANSFER_TO_HOST_2D, CMD_UPDATE_CURSOR, CURSOR_LEN, CURSOR_SIZE,
    DeviceConfig, MAX_SCANOUTS, PAGE_SIZE, RESP_ERR_INVALID_PARAMETER,
    RESP_ERR_INVALID_RESOURCE_ID, RESP_OK_DISPLAY_INFO, RESP_OK_NODATA,
};
use ferrix_virtio::pci::{
    CONFIG_MSIX_VECTOR, CommonConfig, DEVICE_FEATURE, DEVICE_FEATURE_SELECT, DEVICE_STATUS,
    DRIVER_FEATURE, DRIVER_FEATURE_SELECT, FEATURE_ACCESS_PLATFORM, FEATURE_VERSION_1, NO_VECTOR,
    NUM_QUEUES, QUEUE_DESC, QUEUE_DEVICE, QUEUE_DRIVER, QUEUE_ENABLE, QUEUE_MSIX_VECTOR,
    QUEUE_NOTIFY_OFF, QUEUE_SELECT, QUEUE_SIZE, STATUS_DEVICE_NEEDS_RESET, STATUS_DRIVER_OK,
};
use ferrix_virtio::{Descriptor, Layout, QueueMemory, SplitQueueDevice};

use crate::{CommandArea, DevicePages, ISR_QUEUE, Transport};

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

    pub(super) fn read(&self, address: u64) -> Option<u8> {
        let page = address & !(PAGE_SIZE - 1);
        self.pages
            .borrow()
            .get(&page)
            .and_then(|bytes| bytes.get((address - page) as usize).copied())
    }

    pub(super) fn write(&self, address: u64, value: u8) -> bool {
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

impl Region {
    pub(super) fn write_bytes(&self, offset: usize, bytes: &[u8]) {
        for (index, &byte) in bytes.iter().enumerate() {
            let at = offset + index;
            let _ = self
                .bus
                .write(self.device[at / PAGE] + (at % PAGE) as u64, byte);
        }
    }
}

impl DevicePages for Region {
    fn device_pages(&self) -> &[u64] {
        &self.device
    }
}

impl CommandArea for Region {
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
        CommandArea::read_u8(self, offset)
    }
    fn write_u8(&mut self, offset: usize, value: u8) {
        CommandArea::write_u8(self, offset, value);
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

/// What the device gets wrong.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Misbehave {
    /// Complete a chain claiming this many bytes written instead.
    pub(super) written: Option<u32>,
    /// Answer with this response type instead.
    pub(super) response: Option<u32>,
    /// Refuse every command of this type.
    pub(super) refuse: Option<u32>,
    /// Set `DEVICE_NEEDS_RESET`.
    pub(super) needs_reset: bool,
}

/// One queue's registers, as the driver set them.
#[derive(Debug)]
struct Queue {
    size: u16,
    vector: u16,
    addresses: [u64; 3],
    ring: Option<SplitQueueDevice<RingView>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            size: 64,
            vector: NO_VECTOR,
            addresses: [0; 3],
            ring: None,
        }
    }

    /// A write to one half of one of the three parts' addresses.
    fn set_address(&mut self, offset: u32, value: u32) {
        for (index, base) in [QUEUE_DESC, QUEUE_DRIVER, QUEUE_DEVICE]
            .into_iter()
            .enumerate()
        {
            let address = &mut self.addresses[index];
            if offset == base {
                *address = (*address & !0xFFFF_FFFF) | u64::from(value);
            } else if offset == base + 4 {
                *address = (*address & 0xFFFF_FFFF) | u64::from(value) << 32;
            }
        }
    }
}

/// A cursor command as the device read it off the cursor queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CursorSeen {
    /// `UPDATE_CURSOR` or `MOVE_CURSOR`.
    pub(super) code: u32,
    pub(super) scanout: u32,
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) resource: u32,
    pub(super) hot: (u32, u32),
}

/// A resource: its size, its backing, and the host copy.
#[derive(Debug, Default)]
pub(super) struct Resource {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) backing: Vec<(u64, u32)>,
    pub(super) host: Vec<u8>,
}

/// The device.
#[derive(Debug)]
pub(super) struct Device {
    bus: Rc<Bus>,
    offered: u64,
    pub(super) accepted: u64,
    device_select: u32,
    driver_select: u32,
    status: u8,
    queue_select: u16,
    queues: [Queue; 2],
    config: Vec<u8>,
    pub(super) mode: (u32, u32),
    pub(super) resources: BTreeMap<u32, Resource>,
    pub(super) scanout: Option<u32>,
    /// The scanout's pixels as last flushed.
    pub(super) screen: Vec<u8>,
    pub(super) commands: Vec<u32>,
    /// Every cursor command served, in order.
    pub(super) cursor_log: Vec<CursorSeen>,
    /// The cursor's image as the last `UPDATE_CURSOR` read it, as QEMU keeps
    /// a copy: the resource's host copy at the time.
    pub(super) cursor_image: Vec<u8>,
    pub(super) misbehave: Misbehave,
    pub(super) protocol_errors: Vec<&'static str>,
    /// Every `SUBMIT_3D` stream, as the device read it out of its chain.
    pub(super) streams: Vec<Vec<u8>>,
    /// Hand the chains of one [`Device::serve`] back last first, which a
    /// device may: a completion names its chain, not its turn.
    pub(super) complete_backwards: bool,
    /// The MSI-X vector configuration changes are raised on.
    pub(super) config_vector: u16,
    /// How many times the driver read the device status.
    pub(super) status_reads: core::cell::Cell<u32>,
}

impl Device {
    pub(super) fn new(bus: Rc<Bus>, mode: (u32, u32)) -> Self {
        let mut config = vec![0u8; 16];
        config[8..12].copy_from_slice(&1u32.to_le_bytes());
        Self {
            bus,
            offered: FEATURE_VERSION_1 | FEATURE_ACCESS_PLATFORM | 1 << 1,
            accepted: 0,
            device_select: 0,
            driver_select: 0,
            status: 0,
            queue_select: 0,
            queues: [Queue::new(), Queue::new()],
            config,
            mode,
            resources: BTreeMap::new(),
            scanout: None,
            screen: Vec::new(),
            commands: Vec::new(),
            cursor_log: Vec::new(),
            cursor_image: Vec::new(),
            misbehave: Misbehave::default(),
            status_reads: core::cell::Cell::new(0),
            protocol_errors: Vec::new(),
            streams: Vec::new(),
            complete_backwards: false,
            config_vector: NO_VECTOR,
        }
    }

    /// The queue `QUEUE_SELECT` names, if the device has it.
    fn selected(&self) -> Option<&Queue> {
        self.queues.get(usize::from(self.queue_select))
    }

    /// The MSI-X vector the cursor queue was given.
    pub(super) fn cursor_vector(&self) -> u16 {
        self.queues[1].vector
    }

    /// Whether the driver asks to be interrupted when a cursor command is
    /// done.
    pub(super) fn cursor_interrupts_wanted(&self) -> bool {
        self.queues[1]
            .ring
            .as_ref()
            .is_some_and(SplitQueueDevice::driver_wants_interrupt)
    }

    /// The configuration block, for a test to raise an event in.
    pub(super) fn config_mut(&mut self) -> &mut [u8] {
        &mut self.config
    }

    fn register(&self, offset: u32) -> u32 {
        let queue = self.selected();
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
            QUEUE_SIZE => queue.map_or(0, |queue| u32::from(queue.size)),
            QUEUE_MSIX_VECTOR => queue.map_or(0, |queue| u32::from(queue.vector)),
            QUEUE_ENABLE => queue.map_or(0, |queue| u32::from(queue.ring.is_some())),
            QUEUE_NOTIFY_OFF => 5 + u32::from(self.queue_select),
            _ => 0,
        }
    }

    fn set_register(&mut self, offset: u32, value: u32) {
        match offset {
            DEVICE_FEATURE_SELECT => self.device_select = value,
            DRIVER_FEATURE_SELECT => self.driver_select = value,
            DRIVER_FEATURE => match self.driver_select {
                0 => self.accepted = (self.accepted & !0xFFFF_FFFF) | u64::from(value),
                _ => self.accepted = (self.accepted & 0xFFFF_FFFF) | u64::from(value) << 32,
            },
            DEVICE_STATUS => {
                if value == 0 {
                    self.status = 0;
                    for queue in &mut self.queues {
                        queue.ring = None;
                    }
                    self.resources.clear();
                    self.scanout = None;
                } else {
                    if value as u8 & STATUS_DRIVER_OK != 0
                        && self.queues.iter().any(|queue| queue.ring.is_none())
                    {
                        self.protocol_errors.push("DRIVER_OK before both queues");
                    }
                    self.status = value as u8;
                }
            }
            QUEUE_SELECT => self.queue_select = value as u16,
            CONFIG_MSIX_VECTOR => self.config_vector = value as u16,
            _ => {
                let bus = Rc::clone(&self.bus);
                let Some(queue) = self.queues.get_mut(usize::from(self.queue_select)) else {
                    self.protocol_errors.push("a queue the device has not got");
                    return;
                };
                match offset {
                    QUEUE_SIZE => queue.size = value as u16,
                    QUEUE_MSIX_VECTOR => queue.vector = value as u16,
                    QUEUE_ENABLE => {
                        let layout = Layout::for_size(queue.size).expect("a valid size");
                        queue.ring = Some(SplitQueueDevice::new(
                            layout,
                            RingView {
                                bus,
                                layout,
                                descriptors: queue.addresses[0],
                                driver: queue.addresses[1],
                                device: queue.addresses[2],
                            },
                        ));
                    }
                    _ => queue.set_address(offset, value),
                }
            }
        }
    }

    fn copy_out(&self, address: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|index| self.bus.read(address + index as u64).unwrap_or(0))
            .collect()
    }

    /// Serve every cursor command the driver has published, as QEMU's
    /// `virtio_gpu_handle_cursor` does: read it, act on it, give the chain
    /// back with nothing written.
    pub(super) fn serve_cursor(&mut self) {
        loop {
            let Some(ring) = self.queues[1].ring.as_mut() else {
                return;
            };
            let Ok(Some(head)) = ring.next_chain() else {
                return;
            };
            let mut descriptors = [Descriptor {
                address: 0,
                len: 0,
                flags: 0,
                next: 0,
            }; 4];
            let count = ring
                .read_chain(head, &mut descriptors)
                .expect("a readable chain");
            let [descriptor] = descriptors[..count] else {
                self.protocol_errors
                    .push("a cursor command that is not one buffer");
                return;
            };
            if descriptor.flags & 2 != 0 || descriptor.len as usize != CURSOR_LEN {
                self.protocol_errors
                    .push("a cursor command of the wrong shape");
                return;
            }
            let bytes = self.copy_out(descriptor.address, CURSOR_LEN);
            let field = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let seen = CursorSeen {
                code: field(0),
                scanout: field(24),
                x: field(28) as i32,
                y: field(32) as i32,
                resource: field(40),
                hot: (field(44), field(48)),
            };
            if seen.code != CMD_UPDATE_CURSOR && seen.code != CMD_MOVE_CURSOR {
                self.protocol_errors
                    .push("a cursor command of no known type");
            }
            if seen.code == CMD_UPDATE_CURSOR && seen.resource != 0 {
                match self.resources.get(&seen.resource) {
                    Some(image) if (image.width, image.height) == (CURSOR_SIZE, CURSOR_SIZE) => {
                        self.cursor_image = image.host.clone();
                    }
                    _ => self
                        .protocol_errors
                        .push("a cursor image that is not a 64x64 resource"),
                }
            }
            self.cursor_log.push(seen);
            let ring = self.queues[1].ring.as_mut().expect("still there");
            ring.complete(head, 0).expect("completes");
        }
    }

    /// Serve every chain the driver has published.
    pub(super) fn serve(&mut self) {
        let mut finished = Vec::new();
        while let Some(ring) = self.queues[0].ring.as_mut() {
            let Ok(Some(head)) = ring.next_chain() else {
                break;
            };
            let mut descriptors = [Descriptor {
                address: 0,
                len: 0,
                flags: 0,
                next: 0,
            }; 64];
            let count = ring
                .read_chain(head, &mut descriptors)
                .expect("a readable chain");
            let (readable, writable): (Vec<&Descriptor>, Vec<&Descriptor>) = descriptors[..count]
                .iter()
                .partition(|descriptor| descriptor.flags & 2 == 0);
            let mut request = Vec::new();
            for descriptor in readable {
                request.extend(self.copy_out(descriptor.address, descriptor.len as usize));
            }
            let [response] = writable.as_slice() else {
                self.protocol_errors
                    .push("a chain without exactly one response buffer");
                break;
            };
            let (code, mut answer) = self.execute(&request);
            if let Some(other) = self.misbehave.response {
                answer[..4].copy_from_slice(&other.to_le_bytes());
            }
            if answer.len() > response.len as usize {
                self.protocol_errors.push("a response buffer too short");
            }
            for (index, &byte) in answer.iter().enumerate() {
                let _ = self.bus.write(response.address + index as u64, byte);
            }
            self.commands.push(code);
            let written = self.misbehave.written.unwrap_or(answer.len() as u32);
            finished.push((head, written));
        }
        if self.complete_backwards {
            finished.reverse();
        }
        let Some(ring) = self.queues[0].ring.as_mut() else {
            return;
        };
        for (head, written) in finished {
            ring.complete(head, written).expect("completes");
        }
    }

    /// `TRANSFER_TO_HOST_2D`: copy the rectangle from the guest backing into
    /// the host copy, as QEMU's `virtio_gpu_transfer_to_host_2d` does.
    fn transfer(&mut self, request: &[u8]) -> u32 {
        let field = |at: usize| u32::from_le_bytes(request[at..at + 4].try_into().unwrap());
        let (x, y, width, height) = (field(24), field(28), field(32), field(36));
        let offset = u64::from_le_bytes(request[40..48].try_into().unwrap()) as usize;
        let id = field(48);
        let Some(target) = self.resources.get(&id) else {
            return RESP_ERR_INVALID_RESOURCE_ID;
        };
        if x + width > target.width || y + height > target.height {
            return RESP_ERR_INVALID_PARAMETER;
        }
        let stride = target.width as usize * 4;
        let mut backing = Vec::new();
        for &(address, len) in &target.backing {
            backing.extend(self.copy_out(address, len as usize));
        }
        let target = self.resources.get_mut(&id).expect("found above");
        for row in 0..height as usize {
            let source = offset + row * stride;
            let dest = (y as usize + row) * stride + x as usize * 4;
            let len = width as usize * 4;
            target.host[dest..dest + len].copy_from_slice(&backing[source..source + len]);
        }
        RESP_OK_NODATA
    }

    fn execute(&mut self, request: &[u8]) -> (u32, Vec<u8>) {
        let field = |at: usize| u32::from_le_bytes(request[at..at + 4].try_into().unwrap());
        let code = field(0);
        let answer = |kind: u32| {
            let mut bytes = vec![0u8; 24];
            bytes[..4].copy_from_slice(&kind.to_le_bytes());
            bytes
        };
        if self.misbehave.refuse == Some(code) {
            return (code, answer(RESP_ERR_INVALID_PARAMETER));
        }
        let resource = |at: usize| field(at);
        let result = match code {
            CMD_GET_DISPLAY_INFO => {
                let mut bytes = answer(RESP_OK_DISPLAY_INFO);
                for index in 0..MAX_SCANOUTS {
                    let (width, height, enabled) = if index == 0 {
                        (self.mode.0, self.mode.1, 1)
                    } else {
                        (0, 0, 0)
                    };
                    for value in [0, 0, width, height, enabled, 0] {
                        bytes.extend_from_slice(&u32::to_le_bytes(value));
                    }
                }
                return (code, bytes);
            }
            CMD_RESOURCE_CREATE_2D => {
                let (width, height) = (field(32), field(36));
                let _ = self.resources.insert(
                    resource(24),
                    Resource {
                        width,
                        height,
                        backing: Vec::new(),
                        host: vec![0; (width * height * 4) as usize],
                    },
                );
                RESP_OK_NODATA
            }
            CMD_RESOURCE_ATTACH_BACKING => {
                let count = field(28) as usize;
                let entries = (0..count)
                    .map(|index| {
                        let at = 32 + index * 16;
                        (
                            u64::from_le_bytes(request[at..at + 8].try_into().unwrap()),
                            field(at + 8),
                        )
                    })
                    .collect();
                match self.resources.get_mut(&resource(24)) {
                    Some(target) => {
                        target.backing = entries;
                        RESP_OK_NODATA
                    }
                    None => RESP_ERR_INVALID_RESOURCE_ID,
                }
            }
            CMD_SET_SCANOUT => {
                let id = field(44);
                self.scanout = (id != 0).then_some(id);
                RESP_OK_NODATA
            }
            CMD_TRANSFER_TO_HOST_2D => self.transfer(request),
            // `virgl_cmd_submit_3d`: the stream is the `size` bytes after the
            // header and its padding, wherever in the chain they are.
            CMD_SUBMIT_3D => {
                let size = field(24) as usize;
                match request.get(32..32 + size) {
                    Some(stream) if request.len() == 32 + size => {
                        self.streams.push(stream.to_vec());
                        RESP_OK_NODATA
                    }
                    _ => {
                        self.protocol_errors
                            .push("a stream of another length than its size");
                        RESP_ERR_INVALID_PARAMETER
                    }
                }
            }
            CMD_RESOURCE_FLUSH => {
                if self.scanout == Some(resource(40))
                    && let Some(target) = self.resources.get(&resource(40))
                {
                    self.screen = target.host.clone();
                }
                RESP_OK_NODATA
            }
            CMD_RESOURCE_DETACH_BACKING => match self.resources.get_mut(&resource(24)) {
                Some(target) => {
                    target.backing.clear();
                    RESP_OK_NODATA
                }
                None => RESP_ERR_INVALID_RESOURCE_ID,
            },
            CMD_RESOURCE_UNREF => match self.resources.remove(&resource(24)) {
                Some(_) => RESP_OK_NODATA,
                None => RESP_ERR_INVALID_RESOURCE_ID,
            },
            _ => RESP_ERR_INVALID_PARAMETER,
        };
        (code, answer(result))
    }
}

/// The driver's handle on the device.
#[derive(Clone, Debug)]
pub(super) struct Handle {
    pub(super) device: Rc<RefCell<Device>>,
    pub(super) doorbells: Rc<RefCell<usize>>,
    /// Doorbells rung for the cursor queue, apart.
    pub(super) cursor_doorbells: Rc<RefCell<usize>>,
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
        self.device.borrow().config.as_slice().config_len()
    }
    fn config_read8(&self, offset: u32) -> u8 {
        self.device.borrow().config.as_slice().config_read8(offset)
    }
    fn config_read16(&self, offset: u32) -> u16 {
        self.device.borrow().config.as_slice().config_read16(offset)
    }
    fn config_read32(&self, offset: u32) -> u32 {
        self.device.borrow().config.as_slice().config_read32(offset)
    }
}

impl Transport for Handle {
    fn notify(&mut self, queue: u16, notify_off: u16) {
        match (queue, notify_off) {
            (0, 5) => *self.doorbells.borrow_mut() += 1,
            (1, 6) => *self.cursor_doorbells.borrow_mut() += 1,
            _ => self
                .device
                .borrow_mut()
                .protocol_errors
                .push("a doorbell for the wrong queue"),
        }
    }
    fn queue_vector(&self) -> u16 {
        1
    }
    fn acknowledge_interrupt(&mut self) -> u8 {
        ISR_QUEUE
    }
    fn config_write32(&mut self, offset: u32, value: u32) {
        let mut device = self.device.borrow_mut();
        if offset == 4 {
            let read = u32::from_le_bytes(device.config[0..4].try_into().unwrap());
            device.config[0..4].copy_from_slice(&(read & !value).to_le_bytes());
        }
    }
}
