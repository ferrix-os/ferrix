//! A model of an EHCI controller and a bus behind it.
//!
//! The controller walks the schedules the driver writes into [`Memory`] a
//! frame at a time as the [`Time`] advances, executing one queue head's
//! qTDs against the devices: every qTD of a control transfer in the frame
//! it is found, one interrupt qTD per pipe per frame. A transfer moves its
//! whole data stage at once. What it checks as it goes, and records in
//! [`Model::violations`], is what a real controller would silently get
//! wrong: a queue head for a full- or low-speed device that names the wrong
//! hub or port for its split transactions, or no complete-split mask, or a
//! speed other than the device's.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::format;
use std::rc::Rc;
use std::string::String;
use std::vec;
use std::vec::Vec;

use crate::ehci::AREA_BYTES;
use crate::usb::Speed;
use crate::{Clock, Dma, MILLISECOND, Registers};

pub(super) mod ftdi;
pub(super) mod xhci;

/// Where each page of the area is, as the controller sees it: scattered
/// and out of order, as a pin of single frames gives them.
pub(super) const PAGES: [u32; 4] = [0x4003_0000, 0x4001_0000, 0x4007_0000, 0x4000_2000];

const CAPLENGTH: u32 = 0x10;
const OP: u32 = CAPLENGTH;

/// A device on the bus.
#[derive(Clone, Debug)]
pub(super) struct Device {
    pub(super) speed: Speed,
    pub(super) address: u8,
    /// The address `SET_ADDRESS` gave, taken once its status stage is done.
    pending_address: Option<u8>,
    pub(super) configured: u8,
    pub(super) boot_protocol: [bool; 4],
    pub(super) device_descriptor: Vec<u8>,
    pub(super) configuration: Vec<u8>,
    pub(super) strings: Vec<(u8, Vec<u8>)>,
    /// The hub's ports, for a hub.
    pub(super) hub: Option<Vec<HubPort>>,
    /// Interrupt IN reports waiting, by endpoint number.
    pub(super) reports: Vec<(u8, VecDeque<Vec<u8>>)>,
    /// The data stage the last SETUP asked for.
    control_in: Vec<u8>,
    /// Refuse `SET_PROTOCOL` with a stall.
    pub(super) stall_protocol: bool,
    /// Stall every interrupt IN packet.
    pub(super) stall_interrupts: bool,
    /// Each interface's report descriptor.
    pub(super) report_descriptors: Vec<Vec<u8>>,
    /// Refuse `GET_DESCRIPTOR` of a report descriptor with a stall.
    pub(super) stall_report_descriptor: bool,
    /// The interface a `SET_REPORT` just announced, until its data comes.
    expect_output: Option<(u8, u16)>,
    /// Every output report received: the interface, `wValue`, the bytes.
    pub(super) outputs: Vec<(u8, u16, Vec<u8>)>,
    /// Every SETUP it received, for the tests.
    pub(super) setups: Vec<[u8; 8]>,
}

/// A hub's port.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HubPort {
    pub(super) child: Option<usize>,
    pub(super) powered: bool,
    pub(super) enabled: bool,
    pub(super) connection_change: bool,
    pub(super) reset_change: bool,
    /// When a reset started finishes.
    pub(super) reset_until: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RootPort {
    child: Option<usize>,
    enabled: bool,
    connect_change: bool,
    reset: bool,
    power: bool,
    owner: bool,
}

/// The controller and its bus.
#[derive(Debug)]
pub(super) struct Model {
    pub(super) memory: Vec<u8>,
    pub(super) now: u64,
    cmd: u32,
    sts: u32,
    intr: u32,
    periodic_base: u32,
    async_addr: u32,
    configflag: u32,
    roots: [RootPort; 2],
    pub(super) devices: Vec<Device>,
    /// Frames run so far.
    frame: u64,
    pub(super) violations: Vec<String>,
    /// Asynchronous schedule starts, for counting control transfers.
    pub(super) async_runs: u32,
    /// Refuse to halt, for the wedged case.
    pub(super) wedged: bool,
    /// Every split interrupt pipe's queue head, hub and start mask seen.
    split_starts: Vec<(u32, u8, u32)>,
}

pub(super) type Shared = Rc<RefCell<Model>>;

impl Model {
    pub(super) fn new() -> Shared {
        Rc::new(RefCell::new(Model {
            memory: vec![0xA5; AREA_BYTES],
            now: 0,
            cmd: 0,
            sts: 1 << 12,
            intr: 0,
            periodic_base: 0,
            async_addr: 0,
            configflag: 0,
            roots: [RootPort::default(); 2],
            devices: Vec::new(),
            frame: 0,
            violations: Vec::new(),
            async_runs: 0,
            wedged: false,
            split_starts: Vec::new(),
        }))
    }

    /// `USBSTS`.
    pub(super) fn read_status(&self) -> u32 {
        self.sts
    }

    pub(super) fn add(&mut self, device: Device) -> usize {
        self.devices.push(device);
        self.devices.len() - 1
    }

    /// Plug `device` into root port `port`.
    pub(super) fn plug_root(&mut self, port: usize, device: usize) {
        self.roots[port].child = Some(device);
        self.roots[port].connect_change = true;
        self.roots[port].enabled = false;
    }

    /// Plug `device` into port `port` (from one) of the hub `hub`.
    pub(super) fn plug(&mut self, hub: usize, port: usize, device: usize) {
        self.devices[device].address = 0;
        self.devices[device].configured = 0;
        let ports = self.devices[hub].hub.as_mut().expect("a hub");
        ports[port - 1].child = Some(device);
        ports[port - 1].connection_change = true;
        ports[port - 1].enabled = false;
    }

    /// Unplug whatever is in port `port` of `hub`.
    pub(super) fn unplug(&mut self, hub: usize, port: usize) {
        let ports = self.devices[hub].hub.as_mut().expect("a hub");
        ports[port - 1].child = None;
        ports[port - 1].connection_change = true;
        ports[port - 1].enabled = false;
    }

    /// Queue a report on `device`'s interrupt endpoint `endpoint`.
    pub(super) fn report(&mut self, device: usize, endpoint: u8, bytes: &[u8]) {
        let queues = &mut self.devices[device].reports;
        match queues.iter_mut().find(|(number, _)| *number == endpoint) {
            Some((_, queue)) => queue.push_back(bytes.to_vec()),
            None => queues.push((endpoint, VecDeque::from([bytes.to_vec()]))),
        }
    }

    // -----------------------------------------------------------------------
    // Registers
    // -----------------------------------------------------------------------

    fn read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => 0x0100_0000 | CAPLENGTH,
            0x04 => 0x1212,
            0x08 => 0x0002_A026,
            _ => match offset.wrapping_sub(OP) {
                0x00 => self.cmd,
                0x04 => self.sts,
                0x08 => self.intr,
                0x14 => self.periodic_base,
                0x18 => self.async_addr,
                0x40 => self.configflag,
                0x44 => self.portsc(0),
                0x48 => self.portsc(1),
                _ => 0,
            },
        }
    }

    fn portsc(&self, port: usize) -> u32 {
        let root = &self.roots[port];
        let mut value = 0;
        if let Some(child) = root.child {
            value |= 1;
            if self.devices[child].speed == Speed::Low {
                value |= 0b01 << 10;
            }
        }
        if root.connect_change {
            value |= 1 << 1;
        }
        if root.enabled {
            value |= 1 << 2;
        }
        if root.reset {
            value |= 1 << 8;
        }
        if root.power {
            value |= 1 << 12;
        }
        if root.owner {
            value |= 1 << 13;
        }
        value
    }

    fn write(&mut self, offset: u32, value: u32) {
        match offset.wrapping_sub(OP) {
            0x00 => {
                if value & 2 != 0 {
                    self.cmd = 0;
                    self.sts = 1 << 12;
                    self.intr = 0;
                    self.configflag = 0;
                    return;
                }
                self.cmd = value;
                if value & 1 != 0 {
                    self.sts &= !(1 << 12);
                } else if !self.wedged {
                    self.sts |= 1 << 12;
                }
            }
            0x04 => self.sts &= !(value & 0x3F),
            0x08 => self.intr = value,
            0x14 => self.periodic_base = value,
            0x18 => self.async_addr = value,
            0x40 => self.configflag = value,
            0x44 => self.write_portsc(0, value),
            0x48 => self.write_portsc(1, value),
            _ => {}
        }
    }

    fn write_portsc(&mut self, port: usize, value: u32) {
        let root = &mut self.roots[port];
        if value & (1 << 1) != 0 {
            root.connect_change = false;
        }
        root.power = value & (1 << 12) != 0;
        root.owner = value & (1 << 13) != 0;
        let reset = value & (1 << 8) != 0;
        if reset {
            root.enabled = false;
        }
        if root.reset && !reset {
            // A reset ends: a high-speed device is enabled, in its default
            // state.
            if let Some(child) = root.child
                && self.devices[child].speed == Speed::High
            {
                root.enabled = true;
                self.devices[child].address = 0;
            }
        }
        root.reset = reset;
    }

    // -----------------------------------------------------------------------
    // Time
    // -----------------------------------------------------------------------

    fn advance(&mut self, nanos: u64) {
        let until = self.now + nanos.max(1);
        self.now = until;
        // Schedule status follows the enables at once, as far as any wait
        // can tell.
        let running = self.cmd & 1 != 0;
        let set = |sts: &mut u32, bit: u32, on: bool| {
            if on {
                *sts |= bit;
            } else {
                *sts &= !bit;
            }
        };
        set(&mut self.sts, 1 << 14, running && self.cmd & (1 << 4) != 0);
        let async_on = running && self.cmd & (1 << 5) != 0;
        if async_on && self.sts & (1 << 15) == 0 {
            self.async_runs += 1;
        }
        set(&mut self.sts, 1 << 15, async_on);
        while self.frame * MILLISECOND < until {
            self.frame += 1;
            if running {
                self.run_frame();
            }
        }
    }

    fn run_frame(&mut self) {
        if self.sts & (1 << 14) != 0 {
            let entry = self.read32(self.periodic_base + 4 * (self.frame as u32 % 1024));
            let mut link = entry;
            for _ in 0..64 {
                if link & 1 != 0 {
                    break;
                }
                let qh = link & !0x1F;
                self.run_qh(qh, true);
                link = self.read32(qh);
            }
        }
        if self.sts & (1 << 15) != 0 {
            let start = self.async_addr & !0x1F;
            let mut qh = start;
            for _ in 0..8 {
                self.run_qh(qh, false);
                qh = self.read32(qh) & !0x1F;
                if qh == start {
                    break;
                }
            }
        }
    }

    /// Run a queue head: every qTD it can for an asynchronous one, one for a
    /// periodic one with a start mask.
    fn run_qh(&mut self, qh: u32, periodic: bool) {
        let capabilities = self.read32(qh + 8);
        if periodic && capabilities & 0xFF == 0 {
            return;
        }
        for _ in 0..if periodic { 1 } else { 8 } {
            let overlay = self.read32(qh + 24);
            if overlay & (1 << 6) != 0 {
                return;
            }
            let next = self.read32(qh + 16);
            if next & 1 != 0 {
                return;
            }
            let qtd = next & !0x1F;
            let token = self.read32(qtd + 8);
            if token & (1 << 7) == 0 {
                return;
            }
            if !self.run_qtd(qh, qtd, token, periodic) {
                return;
            }
        }
    }

    /// Execute one qTD: whether it finished, as against a NAK.
    fn run_qtd(&mut self, qh: u32, qtd: u32, token: u32, periodic: bool) -> bool {
        let characteristics = self.read32(qh + 4);
        let capabilities = self.read32(qh + 8);
        let address = (characteristics & 0x7F) as u8;
        let endpoint = ((characteristics >> 8) & 0xF) as u8;
        let eps = (characteristics >> 12) & 3;
        let pid = (token >> 8) & 3;
        let total = ((token >> 16) & 0x7FFF) as usize;
        let buffer = self.read32(qtd + 12);

        let Some(device) = self.find(address) else {
            // Nobody answers: three tries, then a transaction error.
            self.finish(qh, qtd, token, total, Some(1 << 3));
            return true;
        };
        self.check_route(device, characteristics, capabilities, periodic);
        if periodic && self.devices[device].speed != Speed::High {
            let hub = ((capabilities >> 16) & 0x7F) as u8;
            self.check_split_budget(qh, device, hub, capabilities);
        }
        let _ = eps;

        let outcome = match (pid, endpoint) {
            (2, 0) => {
                let mut setup = [0_u8; 8];
                for (i, byte) in setup.iter_mut().enumerate() {
                    *byte = self.read8(buffer + i as u32);
                }
                match self.setup(device, setup) {
                    Ok(()) => Ok(8),
                    Err(()) => Err(()),
                }
            }
            (1, 0) => {
                let data = std::mem::take(&mut self.devices[device].control_in);
                let moved = data.len().min(total);
                for (i, &byte) in data.iter().take(moved).enumerate() {
                    self.write8(buffer + i as u32, byte);
                }
                if total == 0 {
                    self.status_done(device);
                }
                Ok(moved)
            }
            (0, 0) => {
                if total == 0 {
                    self.status_done(device);
                } else if let Some((interface, value)) = self.devices[device].expect_output.take() {
                    let bytes = (0..total as u32).map(|i| self.read8(buffer + i)).collect();
                    self.devices[device].outputs.push((interface, value, bytes));
                }
                Ok(total)
            }
            (1, _) if self.devices[device].stall_interrupts => Err(()),
            (1, number) => {
                let report = self.devices[device]
                    .reports
                    .iter_mut()
                    .find(|(ep, _)| *ep == number)
                    .and_then(|(_, queue)| queue.pop_front());
                let Some(report) = report else {
                    return false;
                };
                let moved = report.len().min(total);
                for (i, &byte) in report.iter().take(moved).enumerate() {
                    self.write8(buffer + i as u32, byte);
                }
                Ok(moved)
            }
            _ => Err(()),
        };
        match outcome {
            Ok(moved) => self.finish(qh, qtd, token, total - moved, None),
            Err(()) => self.finish(qh, qtd, token, total, Some(0)),
        }
        true
    }

    /// Write a qTD's token back, and move the queue head on or halt it.
    fn finish(&mut self, qh: u32, qtd: u32, token: u32, left: usize, error: Option<u32>) {
        let mut done = (token & !(0x7FFF << 16) & !(1 << 7)) | ((left as u32) << 16);
        if let Some(bits) = error {
            done |= (1 << 6) | bits;
        }
        self.write32(qtd + 8, done);
        self.write32(qh + 12, qtd);
        self.write32(qh + 24, done);
        if error.is_none() {
            let next = self.read32(qtd);
            self.write32(qh + 16, next);
        }
        if token & (1 << 15) != 0 || error.is_some() {
            self.sts |= 1;
        }
        if error.is_some() {
            self.sts |= 2;
        }
    }

    /// The device answering `address`: reachable through enabled ports, and
    /// for address 0 the one in its default state.
    fn find(&self, address: u8) -> Option<usize> {
        self.reachable()
            .into_iter()
            .find(|&device| self.devices[device].address == address)
    }

    fn reachable(&self) -> Vec<usize> {
        let mut found = Vec::new();
        let mut stack: Vec<usize> = self
            .roots
            .iter()
            .filter(|root| root.enabled && !root.owner)
            .filter_map(|root| root.child)
            .collect();
        while let Some(device) = stack.pop() {
            found.push(device);
            if let Some(ports) = &self.devices[device].hub {
                stack.extend(
                    ports
                        .iter()
                        .filter(|port| port.enabled && port.powered)
                        .filter_map(|port| port.child),
                );
            }
        }
        found
    }

    /// The hub whose translator `device` is reached through, and the port on
    /// it: the nearest high-speed hub above a full- or low-speed device.
    fn translator(&self, device: usize) -> Option<(u8, u8)> {
        let mut child = device;
        loop {
            let (hub, port) = self.parent(child)?;
            if self.devices[hub].speed == Speed::High {
                return Some((self.devices[hub].address, port as u8));
            }
            child = hub;
        }
    }

    fn parent(&self, device: usize) -> Option<(usize, usize)> {
        self.devices.iter().enumerate().find_map(|(index, hub)| {
            let ports = hub.hub.as_ref()?;
            let port = ports.iter().position(|port| port.child == Some(device))?;
            Some((index, port + 1))
        })
    }

    fn check_route(
        &mut self,
        device: usize,
        characteristics: u32,
        capabilities: u32,
        periodic: bool,
    ) {
        let speed = self.devices[device].speed;
        let eps = (characteristics >> 12) & 3;
        let expected = match speed {
            Speed::Full => 0,
            Speed::Low => 1,
            // No SuperSpeed device is ever on an EHCI bus.
            Speed::High | Speed::Super => 2,
        };
        if eps != expected {
            self.violations.push(format!(
                "device {device}: speed field {eps}, device is {speed:?}"
            ));
        }
        if speed == Speed::High {
            return;
        }
        let hub = ((capabilities >> 16) & 0x7F) as u8;
        let port = ((capabilities >> 23) & 0x7F) as u8;
        if Some((hub, port)) != self.translator(device) {
            self.violations.push(format!(
                "device {device}: split to hub {hub} port {port}, it is behind {:?}",
                self.translator(device)
            ));
        }
        if periodic && (capabilities >> 8) & 0xFF == 0 {
            self.violations.push(format!(
                "device {device}: a split interrupt pipe with no complete mask"
            ));
        }

        let control_flag = characteristics & (1 << 27) != 0;
        if !periodic && !control_flag {
            self.violations.push(format!(
                "device {device}: a split control endpoint without C"
            ));
        }
    }

    /// A split interrupt pipe's completes must come two to four microframes
    /// after its start, inside the frame, and no two pipes behind one hub's
    /// translator may start in the same microframe: the budget the DK board
    /// showed four such starts in microframe 0 overrun.
    fn check_split_budget(&mut self, qh: u32, device: usize, hub: u8, capabilities: u32) {
        let start = capabilities & 0xFF;
        let complete = (capabilities >> 8) & 0xFF;
        let first = start.trailing_zeros();
        if start.count_ones() != 1 || first > 3 || complete != 0x1C << first {
            self.violations.push(format!(
                "device {device}: split masks start {start:#04x} complete {complete:#04x}"
            ));
        }
        // A queue head's own earlier entry is replaced: a pipe reopened in
        // the same slot is the same pipe.
        self.split_starts.retain(|&(other, _, _)| other != qh);
        if self
            .split_starts
            .iter()
            .any(|&(_, other_hub, other_start)| other_hub == hub && other_start == start)
        {
            self.violations.push(format!(
                "device {device}: a second split start in microframe {first} behind hub {hub}"
            ));
        }
        self.split_starts.push((qh, hub, start));
    }

    // -----------------------------------------------------------------------
    // Devices
    // -----------------------------------------------------------------------

    fn status_done(&mut self, device: usize) {
        if let Some(address) = self.devices[device].pending_address.take() {
            self.devices[device].address = address;
        }
    }

    fn setup(&mut self, device: usize, setup: [u8; 8]) -> Result<(), ()> {
        self.devices[device].setups.push(setup);
        let request_type = setup[0];
        let request = setup[1];
        let value = u16::from_le_bytes([setup[2], setup[3]]);
        let index = u16::from_le_bytes([setup[4], setup[5]]);
        let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
        let dev = &mut self.devices[device];
        let reply: Vec<u8> = match (request_type, request) {
            (0x80, 6) => {
                let kind = (value >> 8) as u8;
                let number = value as u8;
                match kind {
                    1 => dev.device_descriptor.clone(),
                    2 => dev.configuration.clone(),
                    3 => dev
                        .strings
                        .iter()
                        .find(|(i, _)| *i == number)
                        .map(|(_, s)| s.clone())
                        .ok_or(())?,
                    _ => return Err(()),
                }
            }
            (0x00, 5) => {
                dev.pending_address = Some(value as u8);
                Vec::new()
            }
            (0x00, 9) => {
                dev.configured = value as u8;
                Vec::new()
            }
            (0x21, 0x0B) => {
                if dev.stall_protocol {
                    return Err(());
                }
                dev.boot_protocol[usize::from(index)] = value == 0;
                Vec::new()
            }
            (0x21, 0x0A) => Vec::new(),
            (0x21, 0x09) => {
                dev.expect_output = Some((index as u8, value));
                Vec::new()
            }
            (0x81, 6) if value == 0x2200 => {
                if dev.stall_report_descriptor {
                    return Err(());
                }
                dev.report_descriptors
                    .get(usize::from(index))
                    .ok_or(())?
                    .clone()
            }
            (0xA0, 6) if value >> 8 == 0x29 => {
                let ports = dev.hub.as_ref().ok_or(())?.len() as u8;
                vec![9, 0x29, ports, 0xE9, 0, 50, 100, 0, 0xFF]
            }
            (0xA3, 0) => self.hub_status(device, index)?,
            (0x23, 3) => {
                self.hub_set(device, index, value)?;
                Vec::new()
            }
            (0x23, 1) => {
                self.hub_clear(device, index, value)?;
                Vec::new()
            }
            // An FTDI chip's vendor requests, which `setups` keeps.
            (0x40, _) => Vec::new(),
            _ => return Err(()),
        };
        let mut reply = reply;
        reply.truncate(length);
        self.devices[device].control_in = reply;
        Ok(())
    }

    fn hub_port(&mut self, hub: usize, port: u16) -> Result<&mut HubPort, ()> {
        let ports = self.devices[hub].hub.as_mut().ok_or(())?;
        ports.get_mut(usize::from(port).wrapping_sub(1)).ok_or(())
    }

    fn hub_status(&mut self, hub: usize, port: u16) -> Result<Vec<u8>, ()> {
        let now = self.now;
        let state = *self.hub_port(hub, port)?;
        if let Some(until) = state.reset_until
            && now >= until
        {
            let finished = self.hub_port(hub, port)?;
            finished.reset_until = None;
            finished.reset_change = true;
            finished.enabled = finished.child.is_some();
            if let Some(child) = finished.child {
                self.devices[child].address = 0;
            }
        }
        let state = *self.hub_port(hub, port)?;
        let mut status = 0_u16;
        let mut change = 0_u16;
        if let Some(child) = state.child {
            status |= 1;
            match self.devices[child].speed {
                Speed::Low => status |= 1 << 9,
                Speed::High => status |= 1 << 10,
                Speed::Full | Speed::Super => {}
            }
        }
        if state.enabled {
            status |= 1 << 1;
        }
        if state.reset_until.is_some() {
            status |= 1 << 4;
        }
        if state.powered {
            status |= 1 << 8;
        }
        if state.connection_change {
            change |= 1;
        }
        if state.reset_change {
            change |= 1 << 4;
        }
        let mut bytes = status.to_le_bytes().to_vec();
        bytes.extend_from_slice(&change.to_le_bytes());
        Ok(bytes)
    }

    fn hub_set(&mut self, hub: usize, port: u16, feature: u16) -> Result<(), ()> {
        let now = self.now;
        let state = self.hub_port(hub, port)?;
        match feature {
            8 => state.powered = true,
            4 if state.powered => {
                state.enabled = false;
                state.reset_until = Some(now + 20 * MILLISECOND);
            }
            _ => return Err(()),
        }
        Ok(())
    }

    fn hub_clear(&mut self, hub: usize, port: u16, feature: u16) -> Result<(), ()> {
        let state = self.hub_port(hub, port)?;
        match feature {
            16 => state.connection_change = false,
            20 => state.reset_change = false,
            17..=19 => {}
            _ => return Err(()),
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Memory, by device address
    // -----------------------------------------------------------------------

    fn offset(address: u32) -> usize {
        let page = PAGES
            .iter()
            .position(|&base| base == address & !0xFFF)
            .unwrap_or_else(|| panic!("the controller reached {address:#x}, outside the area"));
        page * 4096 + (address & 0xFFF) as usize
    }

    fn read32(&self, address: u32) -> u32 {
        let at = Self::offset(address);
        u32::from_le_bytes(self.memory[at..at + 4].try_into().expect("four bytes"))
    }

    fn write32(&mut self, address: u32, value: u32) {
        let at = Self::offset(address);
        self.memory[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn read8(&self, address: u32) -> u8 {
        self.memory[Self::offset(address)]
    }

    fn write8(&mut self, address: u32, value: u8) {
        let at = Self::offset(address);
        self.memory[at] = value;
    }
}

// ---------------------------------------------------------------------------
// The driver's view
// ---------------------------------------------------------------------------

/// The registers.
#[derive(Debug)]
pub(super) struct Regs(pub Shared);
/// The memory.
#[derive(Debug)]
pub(super) struct Memory(pub Shared);
/// The time.
#[derive(Debug)]
pub(super) struct Time(pub Shared);

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
        let page = PAGES.get(offset / 4096)?;
        Some(page + (offset % 4096) as u32)
    }
    fn barrier(&self) {}
}

impl Clock for Time {
    fn now_nanos(&self) -> u64 {
        self.0.borrow().now
    }
    fn sleep_nanos(&mut self, nanos: u64) {
        self.0.borrow_mut().advance(nanos);
    }
}

// ---------------------------------------------------------------------------
// The DK board's bus
// ---------------------------------------------------------------------------

fn string(text: &str) -> Vec<u8> {
    let mut bytes = vec![0, 3];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes[0] = bytes.len() as u8;
    bytes
}

fn device_descriptor(
    class: u8,
    max_packet: u8,
    vendor: u16,
    product: u16,
    release: u16,
) -> Vec<u8> {
    let mut bytes = vec![18, 1, 0x00, 0x02, class, 0, 0, max_packet];
    bytes.extend_from_slice(&vendor.to_le_bytes());
    bytes.extend_from_slice(&product.to_le_bytes());
    bytes.extend_from_slice(&release.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 0, 1]);
    bytes
}

/// A configuration of HID interfaces: `(protocol, subclass, endpoint,
/// packet)` each.
fn hid_configuration(interfaces: &[(u8, u8, u8, u16, &[u8])]) -> Vec<u8> {
    let mut bytes = vec![9, 2, 0, 0, interfaces.len() as u8, 1, 0, 0xA0, 49];
    for (number, &(protocol, subclass, endpoint, packet, report)) in interfaces.iter().enumerate() {
        bytes.extend_from_slice(&[9, 4, number as u8, 0, 1, 3, subclass, protocol, 0]);
        let length = (report.len() as u16).to_le_bytes();
        bytes.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, length[0], length[1]]);
        let size = packet.to_le_bytes();
        bytes.extend_from_slice(&[7, 5, 0x80 | endpoint, 3, size[0], size[1], 1]);
    }
    let total = (bytes.len() as u16).to_le_bytes();
    bytes[2] = total[0];
    bytes[3] = total[1];
    bytes
}

fn device(
    speed: Speed,
    descriptor: Vec<u8>,
    configuration: Vec<u8>,
    strings: &[(u8, &str)],
) -> Device {
    let mut all = vec![(0, vec![4, 3, 0x09, 0x04])];
    all.extend(strings.iter().map(|&(index, text)| (index, string(text))));
    Device {
        speed,
        address: 0,
        pending_address: None,
        configured: 0,
        boot_protocol: [false; 4],
        device_descriptor: descriptor,
        configuration,
        strings: all,
        hub: None,
        reports: Vec::new(),
        control_in: Vec::new(),
        stall_protocol: false,
        stall_interrupts: false,
        report_descriptors: Vec::new(),
        stall_report_descriptor: false,
        expect_output: None,
        outputs: Vec::new(),
        setups: Vec::new(),
    }
}

/// The USB2514B, as U-Boot's `usb info` shows it on the board.
pub(super) fn usb2514b() -> Device {
    let configuration = vec![
        9, 2, 25, 0, 1, 1, 0, 0xE0, 1, 9, 4, 0, 0, 1, 9, 0, 1, 0, 7, 5, 0x81, 3, 1, 0, 12,
    ];
    let mut hub = device(
        Speed::High,
        device_descriptor(9, 64, 0x0424, 0x2514, 0x0BB3),
        configuration,
        &[],
    );
    hub.hub = Some(vec![HubPort::default(); 4]);
    hub
}

/// The Logitech G502, at full speed: a boot mouse and a second HID
/// interface.
pub(super) fn g502() -> Device {
    let mut mouse = device(
        Speed::Full,
        device_descriptor(0, 64, 0x046D, 0xC08B, 0x7400),
        hid_configuration(&[(2, 1, 1, 8, &G502_MOUSE), (0, 0, 2, 20, &G502_KEYS)]),
        &[(1, "Logitech"), (2, "G502 HERO Gaming Mouse")],
    );
    mouse.report_descriptors = vec![G502_MOUSE.to_vec(), G502_KEYS.to_vec()];
    mouse
}

/// A gaming mouse's first interface in the report protocol: sixteen
/// buttons, sixteen-bit X and Y, the wheel, and `AC Pan`: eight bytes, no
/// report IDs.
pub(super) const G502_MOUSE: [u8; 67] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81,
    0x02, //
    0x05, 0x01, 0x16, 0x01, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x09, 0x30, 0x09, 0x31,
    0x81, 0x06, //
    0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, //
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, //
    0xC0, 0xC0,
];

/// Its second: macro keys as a keyboard (report 1), media keys (report 3)
/// and a vendor report (0x10) nothing maps.
pub(super) const G502_KEYS: [u8; 89] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF,
    0x00, 0x19, 0x00, 0x2A, 0xFF, 0x00, 0x81, 0x00, 0xC0, //
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x03, 0x15, 0x00, 0x26, 0xFF, 0x03, 0x19, 0x00, 0x2A,
    0xFF, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xC0, //
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x10, 0x75, 0x08, 0x95, 0x06, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x09, 0x01, 0x81, 0x00, 0xC0,
];

/// A keyboard's second interface: media keys (report 1) and power, sleep
/// and wake (report 2).
pub(super) const MEDIA_KEYS: [u8; 52] = [
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x03, 0x19, 0x00, 0x2A,
    0xFF, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xC0, //
    0x05, 0x01, 0x09, 0x80, 0xA1, 0x01, 0x85, 0x02, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x05, 0x81, 0x01, 0xC0,
];

/// The SEM keyboard, at low speed: a boot keyboard and a second HID
/// interface.
pub(super) fn sem_keyboard() -> Device {
    let boot = crate::hid::BOOT_KEYBOARD;
    let mut keyboard = device(
        Speed::Low,
        device_descriptor(0, 8, 0x1A2C, 0x2124, 0x0116),
        hid_configuration(&[(1, 1, 1, 8, &boot), (0, 0, 2, 8, &MEDIA_KEYS)]),
        &[(1, "SEM"), (2, "USB Keyboard  ")],
    );
    keyboard.report_descriptors = vec![boot.to_vec(), MEDIA_KEYS.to_vec()];
    keyboard
}

/// The board: the hub on root port 0, the mouse on its port 1 and the
/// keyboard on its port 2. The device indices: hub, mouse, keyboard.
pub(super) fn dk_board() -> (Shared, usize, usize, usize) {
    let model = Model::new();
    let (hub, mouse, keyboard) = {
        let mut m = model.borrow_mut();
        let hub = m.add(usb2514b());
        let mouse = m.add(g502());
        let keyboard = m.add(sem_keyboard());
        m.plug_root(0, hub);
        m.plug(hub, 1, mouse);
        m.plug(hub, 2, keyboard);
        (hub, mouse, keyboard)
    };
    (model, hub, mouse, keyboard)
}

/// QEMU's `usb-tablet` (`hw/usb/dev-hid.c`, `qemu_tablet_hid_report_descriptor`):
/// three buttons, X and Y from 0 to 32767, and a relative wheel; no report
/// IDs.
pub(super) const QEMU_TABLET: [u8; 74] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x05, 0x81, 0x01, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xFF, 0x7F, 0x35, 0x00, 0x46, 0xFF, 0x7F,
    0x75, 0x10, 0x95, 0x02, 0x81, 0x02, //
    0x05, 0x01, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x06, //
    0xC0, 0xC0,
];

/// The `JetKVM`'s keyboard (`internal/usbgadget/hid_keyboard.go` of
/// jetkvm/kvm at 8f6c3001b, 2026-10-05): the boot layout, keys 0 to 0x65.
pub(super) const JETKVM_KEYBOARD: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x03, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

/// Its absolute pointer (`hid_mouse_absolute.go`, the same commit): report
/// 1 is five buttons and X and Y from 0 to 32767, report 2 the wheel and
/// `AC Pan`, both relative.
pub(super) const JETKVM_ABSOLUTE: [u8; 93] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x01, 0xA1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x05, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x05, 0x81, 0x02,
    0x95, 0x01, 0x75, 0x03, 0x81, 0x03, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xFF, 0x7F, 0x36, 0x00, 0x00, 0x46,
    0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0, //
    0x85, 0x02, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x35, 0x00, 0x45, 0x00, 0x75, 0x08, 0x95, 0x01,
    0x81, 0x06, //
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81,
    0x06, //
    0xC0,
];

/// Its relative mouse (`hid_mouse_relative.go`, the same commit), a boot
/// mouse: eight buttons, X, Y and the wheel, then `AC Pan`; no report IDs.
pub(super) const JETKVM_RELATIVE: [u8; 61] = [
    0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x15, 0x00, 0x25, 0x01, 0x95, 0x08, 0x75, 0x01, 0x81,
    0x02, //
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, //
    0x05, 0x0C, 0x0A, 0x38, 0x02, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81,
    0x06, //
    0xc0, 0xc0,
];

/// The `JetKVM` as its gadget presents itself, at full speed here: 1d6b:0104,
/// a boot keyboard on interface 0, the absolute pointer, report protocol,
/// on interface 1, and the relative mouse, a boot mouse, on interface 2.
pub(super) fn jetkvm() -> Device {
    let mut kvm = device(
        Speed::Full,
        device_descriptor(0, 64, 0x1D6B, 0x0104, 0x0100),
        hid_configuration(&[
            (1, 1, 1, 8, &JETKVM_KEYBOARD),
            (2, 0, 2, 6, &JETKVM_ABSOLUTE),
            (2, 1, 3, 5, &JETKVM_RELATIVE),
        ]),
        &[(1, "JetKVM"), (2, "JetKVM USB Emulation Device")],
    );
    kvm.report_descriptors = vec![
        JETKVM_KEYBOARD.to_vec(),
        JETKVM_ABSOLUTE.to_vec(),
        JETKVM_RELATIVE.to_vec(),
    ];
    kvm
}
