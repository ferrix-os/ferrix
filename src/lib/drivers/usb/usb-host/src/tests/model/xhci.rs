//! A model of an xHCI controller, over the same devices and hubs as the
//! EHCI model's.
//!
//! The controller walks the rings the driver writes -- cycle bits, link
//! TRBs and all -- when a doorbell is rung, and writes its events to the
//! ring the driver's segment table names. A command or a control transfer
//! finishes in the doorbell's write; an interrupt endpoint is served as
//! time passes and a report waits. What it checks as it goes, and records
//! in [`Xhci::violations`], is what a real controller would get silently
//! wrong or answer with an error nobody reads: a `PORTSC` write that
//! disables the port, a slot addressed with the wrong route, port, speed
//! or translator, a device behind a hub the controller was never told is
//! one, a setup stage whose transfer type contradicts its data stage, an
//! endpoint configured with the wrong type or interval, a doorbell rung on
//! a halted endpoint, and a `SET_ADDRESS` software sent itself.

use std::collections::VecDeque;
use std::format;
use std::rc::Rc;
use std::string::String;
use std::vec;
use std::vec::Vec;

use super::{Device, Model};
use crate::usb::Speed;
use crate::{Clock, Dma, MILLISECOND, Registers};
use std::cell::RefCell;

const CAPLENGTH: u32 = 0x20;
const OP: u32 = CAPLENGTH;
const RUNTIME: u32 = 0x1000;
const DOORBELLS: u32 = 0x2000;
const EXTENDED: u32 = 0x800;
/// Root ports: two USB 2 ports, then two USB 3 ports.
pub(in crate::tests) const PORTS: usize = 4;
const SLOTS: usize = 8;
/// Scratchpad pages the controller asks for.
pub(in crate::tests) const SCRATCHPAD: usize = 2;
/// Pages of the area: the driver's fixed seven, the scratchpad, the pool.
pub(in crate::tests) const AREA_PAGES: usize = 7 + SCRATCHPAD + 96;

const SUCCESS: u8 = 1;
const TRANSACTION: u8 = 4;
const TRB_ERROR: u8 = 5;
const STALL: u8 = 6;
const SHORT: u8 = 13;
const CONTEXT_STATE: u8 = 19;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EpState {
    #[default]
    Disabled,
    Running,
    Halted,
    Stopped,
}

#[derive(Clone, Copy, Debug, Default)]
struct Ep {
    state: EpState,
    kind: u32,
    max_packet: u32,
    dequeue: u64,
    cycle: bool,
}

#[derive(Clone, Debug)]
struct Slot {
    /// The model's device, once addressed.
    device: Option<usize>,
    context: u64,
    eps: [Ep; 32],
    hub_ports: Option<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Port {
    child: Option<usize>,
    enabled: bool,
    power: bool,
    connect_change: bool,
    reset_change: bool,
    reset_until: Option<u64>,
}

/// A disk behind a bulk-only mass-storage interface.
#[derive(Clone, Debug)]
pub(in crate::tests) struct Disk {
    pub(in crate::tests) device: usize,
    pub(in crate::tests) blocks: Vec<u8>,
    pub(in crate::tests) block_bytes: usize,
    /// What the next IN transfers send, in order.
    to_host: VecDeque<Vec<u8>>,
    /// The command whose OUT data is still to come: its tag, LBA and bytes.
    writing: Option<(u32, usize, usize)>,
    /// Every command block received.
    pub(in crate::tests) commands: Vec<Vec<u8>>,
    /// Stall the next bulk IN transfer, once.
    pub(in crate::tests) stall_in: bool,
    /// Fail every command with a check condition.
    pub(in crate::tests) failing: bool,
    /// Cache flushes asked for.
    pub(in crate::tests) flushes: u32,
    /// The block `blocks` starts at: a disk whose blocks below it are not
    /// held, so one of more than 2^32 blocks fits a test. Its capacity is
    /// `base` plus what `blocks` holds.
    pub(in crate::tests) base: usize,
    /// Answer the next command with a status wrapper whose tag is not the
    /// one sent, once.
    pub(in crate::tests) wrong_tag: bool,
    /// Answer the next read with less than half its data and a status that
    /// passed, the residue saying what is missing, once.
    pub(in crate::tests) short_read: bool,
}

/// The controller and its bus.
#[derive(Debug)]
pub(in crate::tests) struct Xhci {
    /// The devices, and the requests they answer.
    pub(in crate::tests) world: Model,
    pub(in crate::tests) memory: Vec<u8>,
    /// Where each page of the area is, as the controller sees it.
    pub(in crate::tests) pages: Vec<u64>,
    /// Contexts are 64 bytes.
    pub(in crate::tests) wide_contexts: bool,
    /// Pointers are 64 bits.
    pub(in crate::tests) wide_pointers: bool,
    /// Firmware owns the controller, and whether it lets go when asked.
    pub(in crate::tests) firmware_owned: bool,
    pub(in crate::tests) firmware_lets_go: bool,
    os_owned: bool,
    pub(in crate::tests) legacy_control: u32,
    cmd: u32,
    sts: u32,
    config: u32,
    crcr: u64,
    /// Where the controller reads its next command, and with which cycle.
    command_dequeue: u64,
    command_cycle: bool,
    dcbaap: u64,
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    erdp: u64,
    event_index: usize,
    event_cycle: bool,
    ports: [Port; PORTS],
    slots: Vec<Option<Slot>>,
    pub(in crate::tests) disks: Vec<Disk>,
    pub(in crate::tests) violations: Vec<String>,
    /// Every command's type, in order.
    pub(in crate::tests) commands: Vec<u32>,
    /// Every `CLEAR_FEATURE(ENDPOINT_HALT)` a device received: the device
    /// and the endpoint address.
    pub(in crate::tests) cleared_halts: Vec<(usize, u8)>,
    /// The devices sent the transport's reset, in order.
    pub(in crate::tests) transport_resets: Vec<usize>,
    /// Answer no command: a controller that has stopped.
    pub(in crate::tests) mute: bool,
    /// Refuse to halt, for the wedged case.
    pub(in crate::tests) wedged: bool,
    /// Events written.
    pub(in crate::tests) events: u64,
}

pub(in crate::tests) type Shared = Rc<RefCell<Xhci>>;

impl Xhci {
    pub(in crate::tests) fn new() -> Shared {
        let world = Rc::try_unwrap(Model::new())
            .expect("the only reference")
            .into_inner();
        // Scattered and out of order, a few of them past 4 GiB.
        let pages = (0..AREA_PAGES as u64)
            .map(|page| {
                // Eleven shares no factor with the page count, so no two
                // pages land on one address.
                let low = 0x4000_0000 + ((page * 11 + 3) % AREA_PAGES as u64) * 0x1000;
                if page % 3 == 0 {
                    0x1_0000_0000 + low
                } else {
                    low
                }
            })
            .collect::<Vec<u64>>();
        let distinct: std::collections::BTreeSet<u64> = pages_of(&pages);
        assert_eq!(distinct.len(), AREA_PAGES, "every page has its own address");
        Rc::new(RefCell::new(Xhci {
            world,
            memory: vec![0xA5; AREA_PAGES * 4096],
            pages,
            wide_contexts: false,
            wide_pointers: true,
            firmware_owned: true,
            firmware_lets_go: true,
            os_owned: false,
            legacy_control: 0xE000_E011,
            cmd: 0,
            sts: 1,
            config: 0,
            crcr: 0,
            command_dequeue: 0,
            command_cycle: true,
            dcbaap: 0,
            iman: 0,
            imod: 0,
            erstsz: 0,
            erstba: 0,
            erdp: 0,
            event_index: 0,
            event_cycle: true,
            ports: [Port {
                power: true,
                ..Port::default()
            }; PORTS],
            slots: vec![None; SLOTS + 1],
            disks: Vec::new(),
            violations: Vec::new(),
            commands: Vec::new(),
            cleared_halts: Vec::new(),
            transport_resets: Vec::new(),
            mute: false,
            wedged: false,
            events: 0,
        }))
    }

    pub(in crate::tests) fn add(&mut self, device: Device) -> usize {
        self.world.add(device)
    }

    /// Give `device` a disk of `blocks` blocks of 512 bytes.
    pub(in crate::tests) fn add_disk(&mut self, device: usize, blocks: usize) {
        let bytes = (0..blocks * 512)
            .map(|at| (at / 512 + at % 251) as u8)
            .collect();
        self.disks.push(Disk {
            device,
            blocks: bytes,
            block_bytes: 512,
            to_host: VecDeque::new(),
            writing: None,
            commands: Vec::new(),
            stall_in: false,
            failing: false,
            flushes: 0,
            base: 0,
            wrong_tag: false,
            short_read: false,
        });
    }

    /// Plug `device` into root port `port`, counting from zero.
    pub(in crate::tests) fn plug_root(&mut self, port: usize, device: usize) {
        self.world.devices[device].address = 0;
        self.world.devices[device].configured = 0;
        self.ports[port].child = Some(device);
        self.ports[port].connect_change = true;
        // A USB 3 port trains its link and enables itself.
        self.ports[port].enabled = port >= 2;
        self.port_event(port);
    }

    pub(in crate::tests) fn unplug_root(&mut self, port: usize) {
        self.ports[port].child = None;
        self.ports[port].connect_change = true;
        self.ports[port].enabled = false;
        self.port_event(port);
    }

    pub(in crate::tests) fn plug(&mut self, hub: usize, port: usize, device: usize) {
        self.world.plug(hub, port, device);
    }

    pub(in crate::tests) fn unplug(&mut self, hub: usize, port: usize) {
        self.world.unplug(hub, port);
    }

    pub(in crate::tests) fn report(&mut self, device: usize, endpoint: u8, bytes: &[u8]) {
        self.world.report(device, endpoint, bytes);
    }

    /// How many slots are in use.
    pub(in crate::tests) fn slots_used(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Whether the interrupter is asking for service.
    pub(in crate::tests) fn interrupting(&self) -> bool {
        self.iman & 1 != 0
    }

    fn violation(&mut self, text: String) {
        self.violations.push(text);
    }

    // -----------------------------------------------------------------------
    // Registers
    // -----------------------------------------------------------------------

    fn read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => 0x0110_0000 | CAPLENGTH,
            0x04 => SLOTS as u32 | (1 << 8) | ((PORTS as u32) << 24),
            0x08 => (SCRATCHPAD as u32) << 27,
            0x10 => {
                u32::from(self.wide_pointers)
                    | (u32::from(self.wide_contexts) << 2)
                    | ((EXTENDED >> 2) << 16)
            }
            0x14 => DOORBELLS,
            0x18 => RUNTIME,
            // Extended capabilities: legacy support, USB 2 on ports 1 and
            // 2, USB 3 on ports 3 and 4.
            0x800 => {
                1 | (4 << 8)
                    | (u32::from(self.firmware_owned) << 16)
                    | (u32::from(self.os_owned) << 24)
            }
            0x804 => self.legacy_control,
            0x810 => 2 | (4 << 8) | (0x0200 << 16),
            0x814 | 0x824 => 0x2042_5355,
            0x818 => 1 | (2 << 8),
            0x81C => 0,
            0x820 => 2 | (0x0300 << 16),
            0x828 => 3 | (2 << 8),
            0x82C => 0,
            _ if (OP + 0x400..OP + 0x400 + 16 * PORTS as u32).contains(&offset) => {
                let at = offset - OP - 0x400;
                if at.is_multiple_of(16) {
                    self.portsc((at / 16) as usize)
                } else {
                    0
                }
            }
            _ => match offset.wrapping_sub(OP) {
                0x00 => self.cmd,
                0x04 => self.sts,
                0x08 => 1,
                0x18 => self.crcr as u32 & 0x8,
                0x30 => self.dcbaap as u32,
                0x34 => (self.dcbaap >> 32) as u32,
                0x38 => self.config,
                _ => match offset.wrapping_sub(RUNTIME) {
                    0x20 => self.iman,
                    0x24 => self.imod,
                    0x28 => self.erstsz,
                    0x30 => self.erstba as u32,
                    0x34 => (self.erstba >> 32) as u32,
                    0x38 => self.erdp as u32,
                    0x3C => (self.erdp >> 32) as u32,
                    _ => 0,
                },
            },
        }
    }

    fn speed_id(speed: Speed) -> u32 {
        match speed {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
        }
    }

    fn portsc(&self, port: usize) -> u32 {
        let state = &self.ports[port];
        let mut value = 0;
        if let Some(child) = state.child {
            value |= 1;
            if state.enabled {
                value |= Self::speed_id(self.world.devices[child].speed) << 10;
            }
        }
        if state.enabled {
            value |= 1 << 1;
        }
        if state.reset_until.is_some() {
            value |= 1 << 4;
        }
        if state.power {
            value |= 1 << 9;
        }
        if state.connect_change {
            value |= 1 << 17;
        }
        if state.reset_change {
            value |= 1 << 21;
        }
        // Wake on connect, set as firmware might leave it: a write that
        // drops it shows up.
        value | (1 << 25)
    }

    fn write(&mut self, offset: u32, value: u32) {
        match offset {
            0x800 => {
                if value & (1 << 24) != 0 {
                    self.os_owned = true;
                }
                if value & (1 << 16) == 0 {
                    self.firmware_owned = false;
                }
                return;
            }
            0x804 => {
                // The top three bits clear when written one.
                self.legacy_control =
                    (value & 0x1FFF_FFFF) | (self.legacy_control & !value & 0xE000_0000);
                return;
            }
            _ => {}
        }
        if (OP + 0x400..OP + 0x400 + 16 * PORTS as u32).contains(&offset) {
            let at = offset - OP - 0x400;
            if at.is_multiple_of(16) {
                self.write_portsc((at / 16) as usize, value);
            }
            return;
        }
        if (DOORBELLS..DOORBELLS + 4 * (SLOTS as u32 + 1)).contains(&offset) {
            self.doorbell(((offset - DOORBELLS) / 4) as usize, value);
            return;
        }
        match offset.wrapping_sub(OP) {
            0x00 => {
                if value & 2 != 0 {
                    self.reset();
                    return;
                }
                self.cmd = value;
                if value & 1 != 0 {
                    self.sts &= !1;
                    self.check_start();
                } else if !self.wedged {
                    self.sts |= 1;
                }
            }
            0x04 => self.sts &= !(value & 0x41C),
            0x18 => {
                self.crcr = (self.crcr & !0xFFFF_FFFF) | u64::from(value);
                self.command_cycle = value & 1 != 0;
                self.command_dequeue = self.crcr & !0x3F;
            }
            0x1C => {
                self.crcr = (self.crcr & 0xFFFF_FFFF) | (u64::from(value) << 32);
                self.command_dequeue = self.crcr & !0x3F;
            }
            0x30 => self.dcbaap = (self.dcbaap & !0xFFFF_FFFF) | u64::from(value),
            0x34 => self.dcbaap = (self.dcbaap & 0xFFFF_FFFF) | (u64::from(value) << 32),
            0x38 => self.config = value,
            _ => match offset.wrapping_sub(RUNTIME) {
                0x20 => {
                    // Pending clears when written one; enable is kept.
                    let pending = self.iman & 1 & !value;
                    self.iman = (value & 2) | pending;
                }
                0x24 => self.imod = value,
                0x28 => self.erstsz = value,
                0x30 => self.erstba = (self.erstba & !0xFFFF_FFFF) | u64::from(value),
                0x34 => {
                    self.erstba = (self.erstba & 0xFFFF_FFFF) | (u64::from(value) << 32);
                    self.event_index = 0;
                    self.event_cycle = true;
                }
                0x38 => self.erdp = (self.erdp & !0xFFFF_FFFF) | u64::from(value & !0x8),
                0x3C => self.erdp = (self.erdp & 0xFFFF_FFFF) | (u64::from(value) << 32),
                _ => {}
            },
        }
    }

    fn reset(&mut self) {
        self.cmd = 0;
        self.sts = 1;
        self.config = 0;
        self.crcr = 0;
        self.dcbaap = 0;
        self.iman = 0;
        self.erstsz = 0;
        self.erstba = 0;
        self.erdp = 0;
        self.slots = vec![None; SLOTS + 1];
    }

    /// What must hold when the controller is started.
    fn check_start(&mut self) {
        if self.firmware_owned && !self.os_owned {
            self.violation("started while firmware owned the controller".into());
        }
        if self.legacy_control & 0xE011 != 0 {
            self.violation(format!(
                "started with SMIs enabled: {:#x}",
                self.legacy_control
            ));
        }
        if self.config & 0xFF == 0 || self.config & 0xFF > SLOTS as u32 {
            self.violation(format!("CONFIG enables {} slots", self.config & 0xFF));
        }
        if self.erstsz != 1 || self.erstba == 0 || self.dcbaap == 0 || self.crcr & !0x3F == 0 {
            self.violation("started without its rings and arrays".into());
            return;
        }
        let array = self.read64(self.dcbaap);
        for index in 0..SCRATCHPAD as u64 {
            let page = self.read64(array + 8 * index);
            if page == 0 || !page.is_multiple_of(4096) || !self.pages.contains(&page) {
                self.violation(format!("scratchpad page {index} is {page:#x}"));
            } else {
                // The controller's own: it writes what it likes there.
                self.write32(page, 0xDEAD_BEEF);
            }
        }
    }

    fn write_portsc(&mut self, port: usize, value: u32) {
        if value & (1 << 1) != 0 {
            self.violation(format!("PORTSC {port} written with PED set: {value:#x}"));
            self.ports[port].enabled = false;
        }
        if value & (1 << 25) == 0 {
            self.violation(format!("PORTSC {port} write dropped a wake enable"));
        }
        let now = self.world.now;
        let usb3 = port >= 2;
        let state = &mut self.ports[port];
        state.power = value & (1 << 9) != 0;
        if value & (1 << 17) != 0 {
            state.connect_change = false;
        }
        if value & (1 << 21) != 0 {
            state.reset_change = false;
        }
        let reset = value & (1 << 4) != 0 || (usb3 && value & (1 << 31) != 0);
        if reset && state.child.is_some() {
            state.enabled = false;
            state.reset_until = Some(now + 50 * MILLISECOND);
        }
    }

    fn port_event(&mut self, port: usize) {
        if self.cmd & 1 != 0 {
            self.sts |= 1 << 4;
            self.event(
                ((port as u64) + 1) << 24,
                u32::from(SUCCESS) << 24,
                34 << 10,
            );
        }
    }

    // -----------------------------------------------------------------------
    // Time
    // -----------------------------------------------------------------------

    fn advance(&mut self, nanos: u64) {
        self.world.now += nanos.max(1);
        let now = self.world.now;
        for port in 0..PORTS {
            if let Some(until) = self.ports[port].reset_until
                && now >= until
            {
                let state = &mut self.ports[port];
                state.reset_until = None;
                state.reset_change = true;
                state.enabled = state.child.is_some();
                if let Some(child) = state.child {
                    self.world.devices[child].address = 0;
                }
                self.port_event(port);
            }
        }
        if self.cmd & 1 == 0 {
            return;
        }
        for slot in 1..=SLOTS {
            for dci in 2..32 {
                let running = self.slots[slot]
                    .as_ref()
                    .is_some_and(|state| state.eps[dci].state == EpState::Running);
                if running {
                    self.run_endpoint(slot, dci);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    fn event(&mut self, parameter: u64, status: u32, control: u32) {
        let segment = self.read64(self.erstba);
        let size = self.read32(self.erstba + 8) as usize;
        let at = segment + 16 * self.event_index as u64;
        self.write64(at, parameter);
        self.write32(at + 8, status);
        self.write32(at + 12, (control & !1) | u32::from(self.event_cycle));
        self.event_index += 1;
        if self.event_index == size {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
        self.events += 1;
        self.sts |= 1 << 3;
        if self.iman & 2 != 0 {
            self.iman |= 1;
        }
    }

    fn transfer_event(&mut self, slot: usize, dci: usize, trb: u64, code: u8, left: u32) {
        self.event(
            trb,
            (u32::from(code) << 24) | (left & 0x00FF_FFFF),
            (32 << 10) | ((dci as u32) << 16) | ((slot as u32) << 24),
        );
    }

    // -----------------------------------------------------------------------
    // Doorbells
    // -----------------------------------------------------------------------

    fn doorbell(&mut self, slot: usize, target: u32) {
        if self.cmd & 1 == 0 {
            self.violation(format!("doorbell {slot} rung while halted"));
            return;
        }
        if slot == 0 {
            self.run_commands();
            return;
        }
        let dci = (target & 0xFF) as usize;
        let Some(state) = self.slots[slot].as_ref() else {
            self.violation(format!(
                "doorbell rung for slot {slot}, which is not enabled"
            ));
            return;
        };
        match state.eps.get(dci).map(|ep| ep.state) {
            Some(EpState::Running) => {}
            Some(EpState::Stopped) => {
                if let Some(state) = self.slots[slot].as_mut() {
                    state.eps[dci].state = EpState::Running;
                }
            }
            Some(EpState::Halted) => {
                self.violation(format!("doorbell rung for halted endpoint {slot}/{dci}"));
                return;
            }
            _ => {
                self.violation(format!("doorbell rung for disabled endpoint {slot}/{dci}"));
                return;
            }
        }
        self.run_endpoint(slot, dci);
    }

    /// The next TRB of a ring at `pointer` with consumer cycle `cycle`, if
    /// software has handed one over: its address and words, with links
    /// followed.
    fn next_trb(&mut self, pointer: &mut u64, cycle: &mut bool) -> Option<(u64, u64, u32, u32)> {
        for _ in 0..2 {
            let control = self.read32(*pointer + 12);
            if (control & 1 != 0) != *cycle {
                return None;
            }
            let parameter = self.read64(*pointer);
            let status = self.read32(*pointer + 8);
            if (control >> 10) & 0x3F == 6 {
                if control & 2 != 0 {
                    *cycle = !*cycle;
                }
                *pointer = parameter & !0xF;
                continue;
            }
            let at = *pointer;
            *pointer += 16;
            return Some((at, parameter, status, control));
        }
        None
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn run_commands(&mut self) {
        let mut pointer = self.command_dequeue;
        let mut cycle = self.command_cycle;
        while let Some((at, parameter, _, control)) = self.next_trb(&mut pointer, &mut cycle) {
            self.command_dequeue = pointer;
            self.command_cycle = cycle;
            let kind = (control >> 10) & 0x3F;
            self.commands.push(kind);
            if self.mute {
                continue;
            }
            let slot = (control >> 24) as usize;
            let dci = ((control >> 16) & 0x1F) as usize;
            let (code, slot) = match kind {
                9 => match (1..=SLOTS).find(|&slot| self.slots[slot].is_none()) {
                    Some(free) => {
                        self.slots[free] = Some(Slot {
                            device: None,
                            context: 0,
                            eps: [Ep::default(); 32],
                            hub_ports: None,
                        });
                        (SUCCESS, free)
                    }
                    None => (9, 0),
                },
                10 => {
                    let code = if self.slots.get(slot).is_some_and(Option::is_some) {
                        self.slots[slot] = None;
                        SUCCESS
                    } else {
                        11
                    };
                    (code, slot)
                }
                11 => (self.address_device(slot, parameter, control), slot),
                12 => (self.configure(slot, parameter, control), slot),
                13 => (self.evaluate(slot, parameter), slot),
                14 => (self.endpoint_command(slot, dci, EpState::Halted), slot),
                15 => (self.endpoint_command(slot, dci, EpState::Running), slot),
                16 => (self.set_dequeue(slot, dci, parameter), slot),
                _ => (TRB_ERROR, 0),
            };
            self.event(
                at,
                u32::from(code) << 24,
                (33 << 10) | ((slot as u32) << 24),
            );
        }
    }

    fn context_bytes(&self) -> u64 {
        if self.wide_contexts { 64 } else { 32 }
    }

    /// Word `word` of context `index` of the context at `base`.
    fn context(&self, base: u64, index: u64, word: u64) -> u32 {
        self.read32(base + index * self.context_bytes() + word * 4)
    }

    /// The device at `route` below root port `number` (from one), through
    /// enabled ports only.
    fn device_at(&self, number: u32, route: u32) -> Option<usize> {
        let port = self.ports.get((number as usize).wrapping_sub(1))?;
        if !port.enabled {
            return None;
        }
        let mut device = port.child?;
        let mut route = route;
        while route != 0 {
            let hub = self.world.devices[device].hub.as_ref()?;
            let below = hub.get((route & 0xF) as usize - 1)?;
            if !below.enabled || !below.powered {
                return None;
            }
            device = below.child?;
            route >>= 4;
        }
        Some(device)
    }

    fn slot_of(&self, device: usize) -> Option<usize> {
        self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|slot| slot.device == Some(device))
        })
    }

    fn address_device(&mut self, slot: usize, input: u64, control: u32) -> u8 {
        if self.slots.get(slot).is_none_or(Option::is_none) {
            return 11;
        }
        if control & (1 << 9) != 0 {
            self.violation("Address Device with BSR set".into());
        }
        let add = self.context(input, 0, 1);
        let drop = self.context(input, 0, 0);
        if add != 0b11 || drop != 0 {
            self.violation(format!("Address Device with add {add:#x} drop {drop:#x}"));
        }
        let first = self.context(input, 1, 0);
        let second = self.context(input, 1, 1);
        let third = self.context(input, 1, 2);
        let route = first & 0xF_FFFF;
        let root = (second >> 16) & 0xFF;
        if first >> 27 != 1 {
            self.violation(format!(
                "Address Device with {} context entries",
                first >> 27
            ));
        }
        let Some(device) = self.device_at(root, route) else {
            // Nobody answers the SET_ADDRESS.
            return TRANSACTION;
        };
        let speed = self.world.devices[device].speed;
        if (first >> 20) & 0xF != Self::speed_id(speed) {
            self.violation(format!("slot {slot}'s speed is {}", (first >> 20) & 0xF));
        }
        // Every hub on the way must have been declared one.
        let mut child = device;
        while let Some((hub, port)) = self.world.parent(child) {
            let declared = self
                .slot_of(hub)
                .and_then(|hub_slot| self.slots[hub_slot].as_ref())
                .and_then(|state| state.hub_ports);
            if declared.is_none_or(|ports| usize::from(ports) < port) {
                self.violation(format!(
                    "device {device} addressed behind hub {hub}, which the controller was not told is one"
                ));
            }
            child = hub;
        }
        // A full- or low-speed device behind a high-speed hub names that
        // hub's slot and port.
        let translator = (speed != Speed::High && speed != Speed::Super)
            .then(|| self.translator_of(device))
            .flatten();
        let named = (third & 0xFF, (third >> 8) & 0xFF);
        let expected = translator.map_or((0, 0), |(hub, port)| {
            (self.slot_of(hub).unwrap_or(0) as u32, port as u32)
        });
        if named != expected {
            self.violation(format!(
                "slot {slot} names translator {named:?}, not {expected:?}"
            ));
        }
        let ep_second = self.context(input, 2, 1);
        let dequeue = self.read64(input + 2 * self.context_bytes() + 8);
        let max_packet = ep_second >> 16;
        if (ep_second >> 3) & 7 != 4 || (ep_second >> 1) & 3 != 3 {
            self.violation(format!("slot {slot}'s control endpoint is {ep_second:#x}"));
        }
        if max_packet != u32::from(speed.default_max_packet()) {
            self.violation(format!("slot {slot}'s first packet size is {max_packet}"));
        }
        if dequeue & 1 == 0 || !self.pages.contains(&(dequeue & !0xFFF)) {
            self.violation(format!("slot {slot}'s control ring is {dequeue:#x}"));
        }
        let output = self.read64(self.dcbaap + 8 * slot as u64);
        if !self.pages.contains(&output) {
            self.violation(format!("slot {slot}'s output context is {output:#x}"));
            return 17;
        }
        // The controller sends SET_ADDRESS itself: an address of its own
        // choosing, which is not the slot's number.
        let address = 40 + slot as u8;
        self.world.devices[device].address = address;
        let state = self.slots[slot].as_mut().expect("checked above");
        state.device = Some(device);
        state.context = output;
        state.eps[1] = Ep {
            state: EpState::Running,
            kind: 4,
            max_packet,
            dequeue: dequeue & !0xF,
            cycle: true,
        };
        for word in 0..3 {
            let value = self.context(input, 1, word);
            self.write32(output + word * 4, value);
        }
        // Addressed: slot state 2 (Table 6-7).
        self.write32(output + 12, u32::from(address) | (2 << 27));
        SUCCESS
    }

    /// The nearest high-speed hub above `device`, and the port on it.
    fn translator_of(&self, device: usize) -> Option<(usize, usize)> {
        let mut child = device;
        loop {
            let (hub, port) = self.world.parent(child)?;
            if self.world.devices[hub].speed == Speed::High {
                return Some((hub, port));
            }
            child = hub;
        }
    }

    /// The `bInterval` and attributes of `device`'s endpoint at `address`.
    fn endpoint_descriptor(&self, device: usize, address: u8) -> Option<(u8, u8)> {
        let bytes = &self.world.devices[device].configuration;
        let mut at = 0;
        while at + 2 <= bytes.len() {
            let length = usize::from(bytes[at]).max(2);
            if bytes[at + 1] == 5 && bytes.get(at + 2) == Some(&address) {
                return Some((*bytes.get(at + 6)?, *bytes.get(at + 3)?));
            }
            at += length;
        }
        None
    }

    fn configure(&mut self, slot: usize, input: u64, _control: u32) -> u8 {
        let Some(device) = self
            .slots
            .get(slot)
            .and_then(Option::as_ref)
            .and_then(|s| s.device)
        else {
            return 11;
        };
        let drop = self.context(input, 0, 0);
        let add = self.context(input, 0, 1);
        if add & 1 == 0 || add & 2 != 0 || drop & 3 != 0 {
            self.violation(format!(
                "Configure Endpoint with add {add:#x} drop {drop:#x}"
            ));
        }
        let first = self.context(input, 1, 0);
        let entries = first >> 27;
        let speed = self.world.devices[device].speed;
        for dci in 2..32_usize {
            if drop & (1 << dci) != 0 {
                self.slots[slot].as_mut().expect("found above").eps[dci] = Ep::default();
            }
            if add & (1 << dci) == 0 {
                continue;
            }
            if entries < dci as u32 {
                self.violation(format!(
                    "endpoint {dci} added past {entries} context entries"
                ));
            }
            let index = dci as u64 + 1;
            let zeroth = self.context(input, index, 0);
            let second = self.context(input, index, 1);
            let dequeue = self.read64(input + index * self.context_bytes() + 8);
            let kind = (second >> 3) & 7;
            let address = (dci / 2) as u8 | if dci % 2 == 1 { 0x80 } else { 0 };
            match self.endpoint_descriptor(device, address) {
                Some((interval, attributes)) => {
                    let wanted = match (attributes & 3, dci % 2) {
                        (3, 1) => 7,
                        (2, 1) => 6,
                        (2, 0) => 2,
                        _ => 0,
                    };
                    if kind != wanted {
                        self.violation(format!("endpoint {address:#x} configured as type {kind}"));
                    }
                    // Table 6-12, worked the long way round.
                    let microframes = match speed {
                        Speed::Low | Speed::Full => u32::from(interval) * 8,
                        Speed::High | Speed::Super => 1 << u32::from(interval).saturating_sub(1),
                    };
                    let exponent = (zeroth >> 16) & 0xFF;
                    if wanted == 7 && !(1 << exponent <= microframes && microframes < 2 << exponent)
                    {
                        self.violation(format!(
                            "endpoint {address:#x} polled every 2^{exponent} microframes, not {microframes}"
                        ));
                    }
                }
                None => self.violation(format!("device {device} has no endpoint {address:#x}")),
            }
            if (second >> 1) & 3 != 3 {
                self.violation(format!(
                    "endpoint {address:#x} has error count {}",
                    (second >> 1) & 3
                ));
            }
            if dequeue & 1 == 0 || !self.pages.contains(&(dequeue & !0xFFF)) {
                self.violation(format!("endpoint {address:#x}'s ring is {dequeue:#x}"));
            }
            self.slots[slot].as_mut().expect("found above").eps[dci] = Ep {
                state: EpState::Running,
                kind,
                max_packet: second >> 16,
                dequeue: dequeue & !0xF,
                cycle: true,
            };
        }
        let output = self.slots[slot].as_ref().expect("found above").context;
        for word in 0..3 {
            let value = self.context(input, 1, word);
            self.write32(output + word * 4, value);
        }
        if first & (1 << 26) != 0 {
            let ports = (self.context(input, 1, 1) >> 24) as u8;
            self.slots[slot].as_mut().expect("found above").hub_ports = Some(ports);
            if self.world.devices[device].hub.as_ref().map(Vec::len) != Some(usize::from(ports)) {
                self.violation(format!("device {device} declared a hub of {ports} ports"));
            }
        }
        SUCCESS
    }

    fn evaluate(&mut self, slot: usize, input: u64) -> u8 {
        if self.slots.get(slot).is_none_or(Option::is_none) {
            return 11;
        }
        let add = self.context(input, 0, 1);
        if add & 2 != 0 {
            let max_packet = self.context(input, 2, 1) >> 16;
            self.slots[slot].as_mut().expect("checked above").eps[1].max_packet = max_packet;
        }
        SUCCESS
    }

    /// Reset Endpoint (from halted) or Stop Endpoint (from running): both
    /// leave it stopped.
    fn endpoint_command(&mut self, slot: usize, dci: usize, from: EpState) -> u8 {
        let Some(state) = self.slots.get_mut(slot).and_then(Option::as_mut) else {
            return 11;
        };
        let ep = &mut state.eps[dci];
        if ep.state == from {
            ep.state = EpState::Stopped;
            SUCCESS
        } else {
            CONTEXT_STATE
        }
    }

    fn set_dequeue(&mut self, slot: usize, dci: usize, parameter: u64) -> u8 {
        let Some(state) = self.slots.get_mut(slot).and_then(Option::as_mut) else {
            return 11;
        };
        let ep = &mut state.eps[dci];
        if ep.state != EpState::Stopped {
            return CONTEXT_STATE;
        }
        ep.dequeue = parameter & !0xF;
        ep.cycle = parameter & 1 != 0;
        SUCCESS
    }

    // -----------------------------------------------------------------------
    // Transfers
    // -----------------------------------------------------------------------

    fn run_endpoint(&mut self, slot: usize, dci: usize) {
        loop {
            let Some(state) = self.slots[slot].as_ref() else {
                return;
            };
            let ep = state.eps[dci];
            if ep.state != EpState::Running {
                return;
            }
            let device = state.device;
            let mut pointer = ep.dequeue;
            let mut cycle = ep.cycle;
            let Some(first) = self.next_trb(&mut pointer, &mut cycle) else {
                return;
            };
            // A device that is gone answers nothing.
            let reachable = device.filter(|&device| {
                let context = self.slots[slot].as_ref().map_or(0, |state| state.context);
                let route = self.read32(context) & 0xF_FFFF;
                let root = (self.read32(context + 4) >> 16) & 0xFF;
                self.device_at(root, route) == Some(device)
            });
            let Some(device) = reachable else {
                self.halt(slot, dci, first.0, TRANSACTION, first.2 & 0x1_FFFF);
                return;
            };
            let advanced = if dci == 1 {
                self.run_control(slot, device, first, &mut pointer, &mut cycle)
            } else if ep.kind == 7 {
                self.run_interrupt(slot, dci, device, first)
            } else {
                self.run_bulk(slot, dci, device, first)
            };
            if !advanced {
                return;
            }
            if let Some(state) = self.slots[slot].as_mut()
                && state.eps[dci].state == EpState::Running
            {
                state.eps[dci].dequeue = pointer;
                state.eps[dci].cycle = cycle;
            }
        }
    }

    fn halt(&mut self, slot: usize, dci: usize, trb: u64, code: u8, left: u32) {
        if let Some(state) = self.slots[slot].as_mut() {
            state.eps[dci].state = EpState::Halted;
        }
        self.transfer_event(slot, dci, trb, code, left);
    }

    fn run_control(
        &mut self,
        slot: usize,
        device: usize,
        first: (u64, u64, u32, u32),
        pointer: &mut u64,
        cycle: &mut bool,
    ) -> bool {
        let (at, parameter, status, control) = first;
        if (control >> 10) & 0x3F != 2 || control & (1 << 6) == 0 || status & 0x1_FFFF != 8 {
            self.violation(format!("a control transfer began with {control:#x}"));
            self.halt(slot, 1, at, TRB_ERROR, 0);
            return false;
        }
        let setup = parameter.to_le_bytes();
        let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        let inward = setup[0] & 0x80 != 0;
        let wanted_type = match (length, inward) {
            (0, _) => 0,
            (_, true) => 3,
            (_, false) => 2,
        };
        if (control >> 16) & 3 != wanted_type {
            self.violation(format!(
                "setup {setup:?} has transfer type {}",
                (control >> 16) & 3
            ));
        }
        if setup[0] == 0 && setup[1] == 5 {
            self.violation("software sent SET_ADDRESS itself".into());
        }
        let max_packet = self.slots[slot].as_ref().map_or(0, |s| s.eps[1].max_packet);
        let own = u32::from(self.world.devices[device].device_descriptor[7]);
        let own = if own == 9 { 512 } else { own };
        if length > 8 && max_packet != own {
            self.violation(format!(
                "a request of {length} bytes to a control endpoint of {own} set up as {max_packet}"
            ));
        }
        // The halt-clearing request is the model's to note; the rest the
        // devices answer.
        let answered = if setup[0] == 0x02 && setup[1] == 1 && setup[2] == 0 {
            self.cleared_halts.push((device, setup[4]));
            self.world.devices[device].control_in = Vec::new();
            Ok(())
        } else if setup[0] == 0x21 && setup[1] == 0xFF {
            // Bulk-Only Mass Storage Reset: the disk forgets the command it
            // was in and waits for the next wrapper.
            self.transport_resets.push(device);
            for disk in self.disks.iter_mut().filter(|disk| disk.device == device) {
                disk.to_host.clear();
                disk.writing = None;
            }
            Ok(())
        } else {
            self.world.setup(device, setup)
        };
        if answered.is_err() {
            // The stall is reported for the stage it came in; the status
            // and data stages after it are never run.
            self.halt(slot, 1, at, STALL, 0);
            return false;
        }
        let mut next = self.next_trb(pointer, cycle);
        if length > 0 {
            let Some((data_at, buffer, data_status, data_control)) = next else {
                self.violation("a setup stage with no data stage after it".into());
                return false;
            };
            if (data_control >> 10) & 0x3F != 3 || (data_control & (1 << 16) != 0) != inward {
                self.violation(format!("a data stage of {data_control:#x} for {setup:?}"));
            }
            let asked = (data_status & 0x1_FFFF) as usize;
            if inward {
                let data = std::mem::take(&mut self.world.devices[device].control_in);
                let moved = data.len().min(asked);
                for (i, &byte) in data.iter().take(moved).enumerate() {
                    self.write8(buffer + i as u64, byte);
                }
                if moved < asked && data_control & (1 << 2) != 0 {
                    self.transfer_event(slot, 1, data_at, SHORT, (asked - moved) as u32);
                }
            } else if let Some((interface, value)) = self.world.devices[device].expect_output.take()
            {
                let bytes = (0..asked as u64).map(|i| self.read8(buffer + i)).collect();
                self.world.devices[device]
                    .outputs
                    .push((interface, value, bytes));
            }
            next = self.next_trb(pointer, cycle);
        }
        let Some((status_at, _, _, status_control)) = next else {
            self.violation("a control transfer with no status stage".into());
            return false;
        };
        let status_in = status_control & (1 << 16) != 0;
        if (status_control >> 10) & 0x3F != 4 || status_in != !(inward && length > 0) {
            self.violation(format!(
                "a status stage of {status_control:#x} for {setup:?}"
            ));
        }
        self.world.status_done(device);
        if status_control & (1 << 5) != 0 {
            self.transfer_event(slot, 1, status_at, SUCCESS, 0);
        }
        true
    }

    fn run_interrupt(
        &mut self,
        slot: usize,
        dci: usize,
        device: usize,
        first: (u64, u64, u32, u32),
    ) -> bool {
        let (at, buffer, status, control) = first;
        let asked = (status & 0x1_FFFF) as usize;
        if (control >> 10) & 0x3F != 1 || control & (1 << 5) == 0 {
            self.violation(format!("an interrupt request of {control:#x}"));
        }
        if self.world.devices[device].stall_interrupts {
            self.halt(slot, dci, at, STALL, asked as u32);
            return false;
        }
        let number = (dci / 2) as u8;
        let report = self.world.devices[device]
            .reports
            .iter_mut()
            .find(|(endpoint, _)| *endpoint == number)
            .and_then(|(_, queue)| queue.pop_front());
        let Some(report) = report else {
            return false;
        };
        let moved = report.len().min(asked);
        for (i, &byte) in report.iter().take(moved).enumerate() {
            self.write8(buffer + i as u64, byte);
        }
        let code = if moved < asked { SHORT } else { SUCCESS };
        self.transfer_event(slot, dci, at, code, (asked - moved) as u32);
        true
    }

    fn run_bulk(
        &mut self,
        slot: usize,
        dci: usize,
        device: usize,
        first: (u64, u64, u32, u32),
    ) -> bool {
        let (at, buffer, status, control) = first;
        let asked = (status & 0x1_FFFF) as usize;
        if (control >> 10) & 0x3F != 1 || control & (1 << 5) == 0 {
            self.violation(format!("a bulk request of {control:#x}"));
        }
        let Some(disk) = self.disks.iter().position(|disk| disk.device == device) else {
            self.halt(slot, dci, at, STALL, asked as u32);
            return false;
        };
        if dci % 2 == 1 {
            if std::mem::take(&mut self.disks[disk].stall_in) {
                self.halt(slot, dci, at, STALL, asked as u32);
                return false;
            }
            let Some(data) = self.disks[disk].to_host.pop_front() else {
                return false;
            };
            let moved = data.len().min(asked);
            for (i, &byte) in data.iter().take(moved).enumerate() {
                self.write8(buffer + i as u64, byte);
            }
            let code = if moved < asked { SHORT } else { SUCCESS };
            self.transfer_event(slot, dci, at, code, (asked - moved) as u32);
        } else {
            let bytes: Vec<u8> = (0..asked as u64).map(|i| self.read8(buffer + i)).collect();
            self.disk_out(disk, &bytes);
            self.transfer_event(slot, dci, at, SUCCESS, 0);
        }
        true
    }

    /// Bulk-only transport and the SCSI commands a disk answers.
    fn disk_out(&mut self, index: usize, bytes: &[u8]) {
        let disk = &mut self.disks[index];
        if let Some((tag, lba, left)) = disk.writing {
            let at = lba * disk.block_bytes;
            disk.blocks[at..at + bytes.len()].copy_from_slice(bytes);
            let left = left - bytes.len();
            if left == 0 {
                disk.writing = None;
                disk.to_host.push_back(status_wrapper(tag, 0, 0));
            } else {
                disk.writing = Some((tag, lba + bytes.len() / disk.block_bytes, left));
            }
            return;
        }
        if bytes.len() != 31 || bytes[..4] != *b"USBC" {
            self.violations
                .push(format!("a command wrapper of {} bytes", bytes.len()));
            return;
        }
        let tag = u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes"));
        let length = u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")) as usize;
        let command = bytes[15..15 + usize::from(bytes[14])].to_vec();
        disk.commands.push(command.clone());
        if disk.failing && command[0] != 0x03 {
            if bytes[12] & 0x80 != 0 && length > 0 {
                disk.to_host.push_back(Vec::new());
            }
            disk.to_host
                .push_back(status_wrapper(tag, length as u32, 1));
            return;
        }
        // A tag that is not the one sent, once, on whatever answers next.
        let tag = if std::mem::take(&mut disk.wrong_tag) {
            tag ^ 0x5A5A_0000
        } else {
            tag
        };
        let Some(reply) = disk.reply(tag, length, &command) else {
            return;
        };
        let mut residue = length;
        if let Some(mut data) = reply {
            data.truncate(length);
            residue -= data.len();
            // A page at a time, as the driver asks for it.
            for chunk in data.chunks(4096) {
                disk.to_host.push_back(chunk.to_vec());
            }
        }
        disk.to_host
            .push_back(status_wrapper(tag, residue as u32, 0));
    }

    // -----------------------------------------------------------------------
    // Memory, by device address
    // -----------------------------------------------------------------------

    fn offset(&self, address: u64) -> usize {
        let page = self
            .pages
            .iter()
            .position(|&base| base == address & !0xFFF)
            .unwrap_or_else(|| panic!("the controller reached {address:#x}, outside the area"));
        page * 4096 + (address & 0xFFF) as usize
    }

    fn read32(&self, address: u64) -> u32 {
        let at = self.offset(address);
        u32::from_le_bytes(self.memory[at..at + 4].try_into().expect("four bytes"))
    }

    fn read64(&self, address: u64) -> u64 {
        u64::from(self.read32(address)) | (u64::from(self.read32(address + 4)) << 32)
    }

    fn write32(&mut self, address: u64, value: u32) {
        let at = self.offset(address);
        self.memory[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write64(&mut self, address: u64, value: u64) {
        self.write32(address, value as u32);
        self.write32(address + 4, (value >> 32) as u32);
    }

    fn read8(&self, address: u64) -> u8 {
        self.memory[self.offset(address)]
    }

    fn write8(&mut self, address: u64, value: u8) {
        let at = self.offset(address);
        self.memory[at] = value;
    }
}

impl Disk {
    /// What the disk answers `command` with: its data, or none; or `None`
    /// when it has answered already (a failed status, or a write that
    /// waits for its data).
    fn reply(&mut self, tag: u32, length: usize, command: &[u8]) -> Option<Option<Vec<u8>>> {
        let held = self.blocks.len() / self.block_bytes;
        let block_count = self.base + held;
        // The stored blocks' byte range of `count` blocks at `lba`, or
        // `None` outside them.
        let base = self.base;
        let span = |lba: usize, count: usize, block_bytes: usize| {
            let first = lba.checked_sub(base)?;
            (first + count <= held).then_some(first * block_bytes..(first + count) * block_bytes)
        };
        let reply: Option<Vec<u8>> = match command[0] {
            // TEST UNIT READY.
            0x00 => None,
            // REQUEST SENSE.
            0x03 => Some(vec![
                0x70, 0, 0x06, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x29, 0, 0, 0, 0, 0,
            ]),
            // INQUIRY.
            0x12 => {
                let mut data = vec![0, 0x80, 4, 2, 31, 0, 0, 0];
                data.extend_from_slice(b"FERRIX  MODEL DISK      0001");
                Some(data)
            }
            // READ CAPACITY (10): all ones past what 32 bits name.
            0x25 => {
                let last = u32::try_from(block_count - 1).unwrap_or(u32::MAX);
                let mut data = last.to_be_bytes().to_vec();
                data.extend_from_slice(&(self.block_bytes as u32).to_be_bytes());
                Some(data)
            }
            // SERVICE ACTION IN (16), READ CAPACITY (16).
            0x9E if command[1] == 0x10 && command.len() == 16 => {
                let mut data = ((block_count - 1) as u64).to_be_bytes().to_vec();
                data.extend_from_slice(&(self.block_bytes as u32).to_be_bytes());
                data.resize(32, 0);
                Some(data)
            }
            // READ (10) and READ (16).
            0x28 | 0x88 => {
                let (lba, count) = transfer_fields(command);
                if command[0] == 0x28 && lba + count > 1 << 32 {
                    panic!("READ (10) cannot name block {lba}");
                }
                let Some(range) = span(lba, count, self.block_bytes) else {
                    self.to_host
                        .push_back(status_wrapper(tag, length as u32, 1));
                    return None;
                };
                let mut data = self.blocks[range].to_vec();
                // Short of a packet boundary, so the transfer it ends in is
                // short, as the transport has a device end its data early.
                if std::mem::take(&mut self.short_read) {
                    data.truncate(data.len() / 2 - 100);
                }
                Some(data)
            }
            // WRITE (10) and WRITE (16).
            0x2A | 0x8A => {
                let (lba, count) = transfer_fields(command);
                let Some(range) = span(lba, count, self.block_bytes) else {
                    self.to_host
                        .push_back(status_wrapper(tag, length as u32, 1));
                    return None;
                };
                self.writing = Some((tag, range.start / self.block_bytes, length));
                return None;
            }
            // SYNCHRONIZE CACHE (10).
            0x35 => {
                self.flushes += 1;
                None
            }
            _ => {
                self.to_host
                    .push_back(status_wrapper(tag, length as u32, 1));
                return None;
            }
        };
        Some(reply)
    }
}

/// A read's or write's first block and block count, from its 10- or
/// 16-byte command block.
fn transfer_fields(command: &[u8]) -> (usize, usize) {
    if command.len() == 16 {
        let lba = u64::from_be_bytes(command[2..10].try_into().expect("eight"));
        let count = u32::from_be_bytes(command[10..14].try_into().expect("four"));
        (lba as usize, count as usize)
    } else {
        let lba = u32::from_be_bytes(command[2..6].try_into().expect("four"));
        let count = u16::from_be_bytes([command[7], command[8]]);
        (lba as usize, usize::from(count))
    }
}

/// A command status wrapper.
fn status_wrapper(tag: u32, residue: u32, status: u8) -> Vec<u8> {
    let mut bytes = b"USBS".to_vec();
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&residue.to_le_bytes());
    bytes.push(status);
    bytes
}

// ---------------------------------------------------------------------------
// The driver's view
// ---------------------------------------------------------------------------

/// The registers.
#[derive(Debug)]
pub(in crate::tests) struct Regs(pub Shared);
/// The memory.
#[derive(Debug)]
pub(in crate::tests) struct Memory(pub Shared);
/// The time.
#[derive(Debug)]
pub(in crate::tests) struct Time(pub Shared);

impl Registers for Regs {
    fn read32(&self, offset: u32) -> u32 {
        self.0.borrow().read(offset)
    }
    fn write32(&mut self, offset: u32, value: u32) {
        self.0.borrow_mut().write(offset, value);
    }
}

impl Dma for Memory {
    fn len(&self) -> usize {
        self.0.borrow().memory.len()
    }
    fn read32(&self, offset: usize) -> u32 {
        assert_eq!(offset % 4, 0, "an aligned word");
        let model = self.0.borrow();
        u32::from_le_bytes(
            model.memory[offset..offset + 4]
                .try_into()
                .expect("four bytes"),
        )
    }
    fn write32(&mut self, offset: usize, value: u32) {
        assert_eq!(offset % 4, 0, "an aligned word");
        self.0.borrow_mut().memory[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn read8(&self, offset: usize) -> u8 {
        self.0.borrow().memory[offset]
    }
    fn write8(&mut self, offset: usize, value: u8) {
        self.0.borrow_mut().memory[offset] = value;
    }
    fn device_address(&self, offset: usize) -> Option<u32> {
        u32::try_from(self.wide_address(offset)?).ok()
    }
    fn wide_address(&self, offset: usize) -> Option<u64> {
        let page = *self.0.borrow().pages.get(offset / 4096)?;
        Some(page + (offset % 4096) as u64)
    }
    fn barrier(&self) {}
}

impl Clock for Time {
    fn now_nanos(&self) -> u64 {
        self.0.borrow().world.now
    }
    fn sleep_nanos(&mut self, nanos: u64) {
        self.0.borrow_mut().advance(nanos);
    }
}

// ---------------------------------------------------------------------------
// Devices only an xHCI bus has
// ---------------------------------------------------------------------------

/// A memory stick: one bulk-only mass-storage interface, bulk IN on
/// endpoint 1 and bulk OUT on endpoint 2.
pub(in crate::tests) fn stick(speed: Speed) -> Device {
    let (packet, control): (u16, u8) = if speed == Speed::Super {
        (1024, 9)
    } else {
        (512, 64)
    };
    let size = packet.to_le_bytes();
    let configuration = vec![
        9, 2, 32, 0, 1, 1, 0, 0x80, 50, //
        9, 4, 0, 0, 2, 8, 6, 0x50, 0, //
        7, 5, 0x81, 2, size[0], size[1], 0, //
        7, 5, 0x02, 2, size[0], size[1], 0,
    ];
    super::device(
        speed,
        stick_descriptor(control),
        configuration,
        &[(1, "Ferrix"), (2, "Model Stick"), (3, "FX0001 STICK")],
    )
}

/// The stick's device descriptor, with its serial number as string 3.
fn stick_descriptor(control: u8) -> Vec<u8> {
    let mut descriptor = super::device_descriptor(0, control, 0x0951, 0x1666, 0x0100);
    descriptor[16] = 3;
    descriptor
}

/// The gaming mouse as a `SuperSpeed` device would present it: a control
/// endpoint of 512 bytes, given as the exponent 9.
pub(in crate::tests) fn super_mouse() -> Device {
    let mut mouse = super::g502();
    mouse.speed = Speed::Super;
    mouse.device_descriptor[7] = 9;
    mouse
}

fn pages_of(pages: &[u64]) -> std::collections::BTreeSet<u64> {
    pages.iter().copied().collect()
}
