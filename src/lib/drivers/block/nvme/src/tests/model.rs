//! A model of an NVMe controller: registers, queues in "physical" memory it
//! reaches by address, a namespace in a vector, and the faults a test asks
//! for.
//!
//! It is written from the specification's side, not the driver's: it finds
//! the queues by the addresses in `ASQ`, `ACQ` and the create commands,
//! reads submission entries by field offset, and walks PRPs itself. So a
//! driver that encoded a command wrongly meets a model that reads it as a
//! controller would.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use crate::regs::PAGE;
use crate::{Clock, Memory, Registers};

/// One completion queue the model posts into.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cq {
    pub(crate) base: u64,
    pub(crate) entries: u16,
    pub(crate) tail: u16,
    pub(crate) phase: bool,
    pub(crate) vector: u16,
    pub(crate) interrupts: bool,
}

/// One submission queue the model reads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sq {
    pub(crate) base: u64,
    pub(crate) entries: u16,
    pub(crate) head: u16,
    pub(crate) cq: u16,
}

/// A command the model saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) queue: u16,
    pub(crate) opcode: u8,
    pub(crate) id: u16,
    pub(crate) nsid: u32,
    pub(crate) prp1: u64,
    pub(crate) prp2: u64,
    pub(crate) dwords: [u32; 6],
}

/// The host memory buffer as Set Features described it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct HostMemory {
    pub(crate) flags: u32,
    pub(crate) pages: u32,
    pub(crate) chunks: Vec<(u64, u32)>,
}

/// The controller.
#[derive(Debug)]
pub(crate) struct Model {
    memory: BTreeMap<u64, Vec<u8>>,
    pub(crate) cap: u64,
    pub(crate) cc: u32,
    pub(crate) csts: u32,
    pub(crate) aqa: u32,
    pub(crate) asq: u64,
    pub(crate) acq: u64,
    sqs: BTreeMap<u16, Sq>,
    cqs: BTreeMap<u16, Cq>,
    pub(crate) disk: Vec<u8>,
    pub(crate) block_shift: u8,
    pub(crate) metadata: u16,
    pub(crate) protection: u8,
    pub(crate) version: u32,
    pub(crate) mdts: u8,
    pub(crate) write_cache: bool,
    pub(crate) apst: bool,
    pub(crate) hmpre: u32,
    pub(crate) hmmin: u32,
    pub(crate) hmminds: u32,
    pub(crate) hmmaxd: u16,
    pub(crate) refuse_host_memory: bool,
    pub(crate) granted_queues: u16,
    pub(crate) model_name: &'static [u8],
    pub(crate) vendor: u16,
    /// I/O commands to swallow without a completion.
    pub(crate) drop_io: u32,
    /// Whether Abort brings a swallowed command back, as aborted.
    pub(crate) abort_works: bool,
    /// Statuses to complete the next I/O commands with, first first.
    pub(crate) fail_io: Vec<u16>,
    /// `CSTS.RDY` never follows `CC.EN`.
    pub(crate) stuck: bool,
    /// Set `CSTS.CFS` when the next I/O command arrives, swallowing it.
    pub(crate) fatal_on_io: bool,
    /// Every register reads all ones.
    pub(crate) gone: bool,
    pub(crate) seen: Vec<Seen>,
    pub(crate) host_memory: Option<HostMemory>,
    pub(crate) features: Vec<(u32, u32)>,
    pub(crate) resets: u32,
    pub(crate) shutdowns: u32,
    pub(crate) register_writes: Vec<(u32, u32)>,
    swallowed: Vec<(u16, u16)>,
}

impl Model {
    /// A controller with a namespace of `blocks` blocks of `1 << shift`
    /// bytes, patterned so every block differs.
    pub(crate) fn new(blocks: usize, shift: u8) -> Self {
        let mut disk = vec![0_u8; blocks << shift];
        for (index, byte) in disk.iter_mut().enumerate() {
            *byte = (index as u64).wrapping_mul(2_654_435_761).to_le_bytes()[3];
        }
        Model {
            memory: BTreeMap::new(),
            // MQES 2047, TO 4 (2 s), DSTRD 0, CSS NVM, MPSMIN 0, MPSMAX 4.
            cap: 0x7FF | (4 << 24) | (1 << 37) | (4 << 52),
            cc: 0,
            csts: 0,
            aqa: 0,
            asq: 0,
            acq: 0,
            sqs: BTreeMap::new(),
            cqs: BTreeMap::new(),
            disk,
            block_shift: shift,
            metadata: 0,
            protection: 0,
            version: 0x0001_0400,
            mdts: 7,
            write_cache: true,
            apst: true,
            hmpre: 0,
            hmmin: 0,
            hmminds: 0,
            hmmaxd: 0,
            refuse_host_memory: false,
            granted_queues: 64,
            model_name: b"Ferrix Model Controller",
            vendor: 0x1B36,
            drop_io: 0,
            abort_works: false,
            fail_io: Vec::new(),
            stuck: false,
            fatal_on_io: false,
            gone: false,
            seen: Vec::new(),
            host_memory: None,
            features: Vec::new(),
            resets: 0,
            shutdowns: 0,
            register_writes: Vec::new(),
            swallowed: Vec::new(),
        }
    }

    fn page(&mut self, address: u64) -> &mut Vec<u8> {
        self.memory
            .entry(address & !(PAGE as u64 - 1))
            .or_insert_with(|| vec![0; PAGE])
    }

    pub(crate) fn read_byte(&mut self, address: u64) -> u8 {
        let within = (address % PAGE as u64) as usize;
        self.page(address)[within]
    }

    pub(crate) fn write_byte(&mut self, address: u64, value: u8) {
        let within = (address % PAGE as u64) as usize;
        self.page(address)[within] = value;
    }

    fn read(&mut self, address: u64, out: &mut [u8]) {
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = self.read_byte(address + index as u64);
        }
    }

    fn write(&mut self, address: u64, bytes: &[u8]) {
        for (index, byte) in bytes.iter().enumerate() {
            self.write_byte(address + index as u64, *byte);
        }
    }

    fn le64(&mut self, address: u64) -> u64 {
        let mut bytes = [0; 8];
        self.read(address, &mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn register_read(&mut self, offset: u32) -> u32 {
        if self.gone {
            return u32::MAX;
        }
        match offset {
            0x00 => self.cap as u32,
            0x04 => (self.cap >> 32) as u32,
            0x08 => self.version,
            0x14 => self.cc,
            0x1C => self.csts,
            0x24 => self.aqa,
            _ => 0,
        }
    }

    fn register_write(&mut self, offset: u32, value: u32) {
        self.register_writes.push((offset, value));
        match offset {
            0x14 => self.configure(value),
            0x24 => self.aqa = value,
            0x28 => self.asq = (self.asq & !0xFFFF_FFFF) | u64::from(value),
            0x2C => self.asq = (self.asq & 0xFFFF_FFFF) | (u64::from(value) << 32),
            0x30 => self.acq = (self.acq & !0xFFFF_FFFF) | u64::from(value),
            0x34 => self.acq = (self.acq & 0xFFFF_FFFF) | (u64::from(value) << 32),
            offset if offset >= 0x1000 => self.doorbell(offset, value),
            _ => {}
        }
    }

    fn configure(&mut self, value: u32) {
        let was = self.cc & 1 != 0;
        let now = value & 1 != 0;
        self.cc = value;
        if was && !now {
            self.resets += 1;
            self.sqs.clear();
            self.cqs.clear();
            self.swallowed.clear();
            self.host_memory = None;
            if !self.stuck {
                self.csts = 0;
            }
        }
        if !was && now && !self.stuck {
            self.csts = 1;
            let entries = (self.aqa & 0xFFF) as u16 + 1;
            let _ = self.sqs.insert(
                0,
                Sq {
                    base: self.asq,
                    entries,
                    head: 0,
                    cq: 0,
                },
            );
            let _ = self.cqs.insert(
                0,
                Cq {
                    base: self.acq,
                    entries: ((self.aqa >> 16) & 0xFFF) as u16 + 1,
                    tail: 0,
                    phase: true,
                    vector: 0,
                    interrupts: true,
                },
            );
        }
        if now && (value >> 14) & 3 == 1 {
            self.shutdowns += 1;
            self.csts = (self.csts & !0xC) | (2 << 2);
        }
    }

    fn doorbell(&mut self, offset: u32, value: u32) {
        let stride = 4 << ((self.cap >> 32) & 0xF);
        let index = (offset - 0x1000) / stride;
        let queue = (index / 2) as u16;
        if index % 2 == 1 {
            // A completion queue's head: the model never fills one.
            return;
        }
        while let Some(sq) = self.sqs.get(&queue).copied() {
            if u32::from(sq.head) == value {
                break;
            }
            let at = sq.base + u64::from(sq.head) * 64;
            let mut entry = [0_u8; 64];
            self.read(at, &mut entry);
            if let Some(sq) = self.sqs.get_mut(&queue) {
                sq.head = (sq.head + 1) % sq.entries;
            }
            self.execute(queue, &entry);
        }
    }

    fn execute(&mut self, queue: u16, entry: &[u8; 64]) {
        let dword = |index: usize| {
            let at = 40 + (index - 10) * 4;
            u32::from_le_bytes(entry[at..at + 4].try_into().unwrap())
        };
        let seen = Seen {
            queue,
            opcode: entry[0],
            id: u16::from_le_bytes([entry[2], entry[3]]),
            nsid: u32::from_le_bytes(entry[4..8].try_into().unwrap()),
            prp1: u64::from_le_bytes(entry[24..32].try_into().unwrap()),
            prp2: u64::from_le_bytes(entry[32..40].try_into().unwrap()),
            dwords: [
                dword(10),
                dword(11),
                dword(12),
                dword(13),
                dword(14),
                dword(15),
            ],
        };
        self.seen.push(seen);
        let outcome = if queue == 0 {
            self.admin(&seen)
        } else {
            self.io(&seen)
        };
        if let Some((result, status)) = outcome {
            self.post(queue, seen.id, result, status);
        }
    }

    /// Write a completion for command `id` of submission queue `queue`.
    pub(crate) fn post(&mut self, queue: u16, id: u16, result: u32, status: u16) {
        let Some(sq) = self.sqs.get(&queue).copied() else {
            return;
        };
        let Some(cq) = self.cqs.get(&sq.cq).copied() else {
            return;
        };
        let mut entry = [0_u8; 16];
        entry[0..4].copy_from_slice(&result.to_le_bytes());
        entry[8..10].copy_from_slice(&sq.head.to_le_bytes());
        entry[10..12].copy_from_slice(&queue.to_le_bytes());
        entry[12..14].copy_from_slice(&id.to_le_bytes());
        entry[14..16].copy_from_slice(&((status << 1) | u16::from(cq.phase)).to_le_bytes());
        self.write(cq.base + u64::from(cq.tail) * 16, &entry);
        if let Some(cq) = self.cqs.get_mut(&sq.cq) {
            cq.tail += 1;
            if cq.tail == cq.entries {
                cq.tail = 0;
                cq.phase = !cq.phase;
            }
        }
    }

    fn admin(&mut self, seen: &Seen) -> Option<(u32, u16)> {
        let [d10, d11, d12, d13, d14, d15] = seen.dwords;
        match seen.opcode {
            0x06 => Some((0, self.identify(seen, d10 & 0xFF))),
            0x09 => Some(self.set_feature(seen, [d10, d11, d12, d13, d14, d15])),
            0x0A => Some((u32::from(self.write_cache), 0)),
            0x05 => {
                let _ = self.cqs.insert(
                    d10 as u16,
                    Cq {
                        base: seen.prp1,
                        entries: (d10 >> 16) as u16 + 1,
                        tail: 0,
                        phase: true,
                        vector: (d11 >> 16) as u16,
                        interrupts: d11 & 2 != 0,
                    },
                );
                Some((0, if d11 & 1 == 1 { 0 } else { 0x4002 }))
            }
            0x01 => {
                if !self.cqs.contains_key(&((d11 >> 16) as u16)) {
                    // Completion Queue Invalid.
                    return Some((0, 0x4100));
                }
                let _ = self.sqs.insert(
                    d10 as u16,
                    Sq {
                        base: seen.prp1,
                        entries: (d10 >> 16) as u16 + 1,
                        head: 0,
                        cq: (d11 >> 16) as u16,
                    },
                );
                Some((0, 0))
            }
            0x08 => {
                let target = (d10 as u16, (d10 >> 16) as u16);
                let held = self.swallowed.iter().position(|held| *held == target);
                match held {
                    Some(at) if self.abort_works => {
                        let _ = self.swallowed.remove(at);
                        // Command Abort Requested, retry allowed.
                        self.post(target.0, target.1, 0, 0x07);
                        Some((0, 0))
                    }
                    _ => Some((1, 0)),
                }
            }
            _ => Some((0, 0x4001)),
        }
    }

    fn set_feature(&mut self, seen: &Seen, dwords: [u32; 6]) -> (u32, u16) {
        let [feature, value, d12, d13, d14, d15] = dwords;
        self.features.push((feature, value));
        match feature {
            0x07 => {
                let granted = u32::from(self.granted_queues - 1);
                (granted | (granted << 16), 0)
            }
            0x0D => {
                if self.refuse_host_memory {
                    return (0, 0x4002);
                }
                let list = u64::from(d13) | (u64::from(d14) << 32);
                let mut chunks = Vec::new();
                for index in 0..u64::from(d15) {
                    let address = self.le64(list + index * 16);
                    let size = self.le64(list + index * 16 + 8) as u32;
                    chunks.push((address, size));
                }
                self.host_memory = (value & 1 != 0).then_some(HostMemory {
                    flags: value,
                    pages: d12,
                    chunks,
                });
                (0, 0)
            }
            0x0C => {
                // The table must be there to read, whatever the enable bit.
                let _ = self.le64(seen.prp1);
                (0, if seen.prp1 == 0 { 0x4002 } else { 0 })
            }
            _ => (0, 0),
        }
    }

    fn identify(&mut self, seen: &Seen, cns: u32) -> u16 {
        let mut page = vec![0_u8; PAGE];
        match cns {
            0x01 => {
                page[0..2].copy_from_slice(&self.vendor.to_le_bytes());
                page[4..24].copy_from_slice(b"MODEL-SERIAL-0001   ");
                page[24..64].fill(b' ');
                page[24..24 + self.model_name.len()].copy_from_slice(self.model_name);
                page[64..72].copy_from_slice(b"1.0     ");
                page[77] = self.mdts;
                page[80..84].copy_from_slice(&self.version.to_le_bytes());
                page[88..92].copy_from_slice(&2_000_000_u32.to_le_bytes());
                page[258] = 3;
                page[263] = 4;
                page[265] = u8::from(self.apst);
                page[272..276].copy_from_slice(&self.hmpre.to_le_bytes());
                page[276..280].copy_from_slice(&self.hmmin.to_le_bytes());
                page[332..336].copy_from_slice(&self.hmminds.to_le_bytes());
                page[336..338].copy_from_slice(&self.hmmaxd.to_le_bytes());
                page[512] = 0x66;
                page[513] = 0x44;
                page[516..520].copy_from_slice(&1_u32.to_le_bytes());
                page[525] = u8::from(self.write_cache);
            }
            0x00 => {
                if seen.nsid != 1 {
                    return 0x400B;
                }
                let blocks = (self.disk.len() >> self.block_shift) as u64;
                page[0..8].copy_from_slice(&blocks.to_le_bytes());
                page[8..16].copy_from_slice(&blocks.to_le_bytes());
                page[25] = 1;
                // Format 1 is in use; format 0 is another size.
                page[26] = 1;
                page[29] = self.protection;
                page[128 + 2] = if self.block_shift == 9 { 12 } else { 9 };
                page[132..134].copy_from_slice(&self.metadata.to_le_bytes());
                page[134] = self.block_shift;
            }
            0x02 => page[0..4].copy_from_slice(&1_u32.to_le_bytes()),
            _ => return 0x4002,
        }
        self.write(seen.prp1, &page);
        0
    }

    fn io(&mut self, seen: &Seen) -> Option<(u32, u16)> {
        if self.fatal_on_io {
            self.fatal_on_io = false;
            self.csts |= 2;
            return None;
        }
        if self.drop_io > 0 {
            self.drop_io -= 1;
            self.swallowed.push((seen.queue, seen.id));
            return None;
        }
        if !self.fail_io.is_empty() {
            return Some((0, self.fail_io.remove(0)));
        }
        if seen.opcode == 0x00 {
            return Some((0, 0));
        }
        if seen.nsid != 1 || !matches!(seen.opcode, 0x01 | 0x02) {
            return Some((0, 0x4001));
        }
        let lba = u64::from(seen.dwords[0]) | (u64::from(seen.dwords[1]) << 32);
        let blocks = u64::from(seen.dwords[2] & 0xFFFF) + 1;
        let start = (lba << self.block_shift) as usize;
        let length = (blocks << self.block_shift) as usize;
        if start + length > self.disk.len() {
            // LBA Out of Range.
            return Some((0, 0x4080));
        }
        let mut done = 0;
        for (address, bytes) in self.regions(seen.prp1, seen.prp2, length) {
            let range = start + done..start + done + bytes;
            if seen.opcode == 0x02 {
                let data = self.disk[range].to_vec();
                self.write(address, &data);
            } else {
                let mut data = vec![0; bytes];
                self.read(address, &mut data);
                self.disk[range].copy_from_slice(&data);
            }
            done += bytes;
        }
        Some((0, 0))
    }

    /// The memory a transfer of `length` bytes names, as the specification
    /// reads the two PRP entries.
    fn regions(&mut self, prp1: u64, prp2: u64, length: usize) -> Vec<(u64, usize)> {
        let first = (PAGE - (prp1 % PAGE as u64) as usize).min(length);
        let mut out = vec![(prp1, first)];
        let mut left = length - first;
        if left == 0 {
            return out;
        }
        if left <= PAGE {
            assert_eq!(prp2 % PAGE as u64, 0, "PRP2 names a whole page");
            out.push((prp2, left));
            return out;
        }
        let mut list = prp2;
        let mut slot = 0;
        while left > 0 {
            assert_eq!(list % PAGE as u64, 0, "a PRP list is a whole page");
            let entry = self.le64(list + slot * 8);
            if slot == 511 && left > PAGE {
                list = entry;
                slot = 0;
                continue;
            }
            assert_eq!(entry % PAGE as u64, 0, "a listed page is a whole page");
            let bytes = left.min(PAGE);
            out.push((entry, bytes));
            left -= bytes;
            slot += 1;
        }
        out
    }

    /// The commands seen with `opcode` on an I/O queue.
    pub(crate) fn io_seen(&self, opcode: u8) -> Vec<Seen> {
        self.seen
            .iter()
            .copied()
            .filter(|seen| seen.queue != 0 && seen.opcode == opcode)
            .collect()
    }

    /// The admin commands seen with `opcode`.
    pub(crate) fn admin_seen(&self, opcode: u8) -> Vec<Seen> {
        self.seen
            .iter()
            .copied()
            .filter(|seen| seen.queue == 0 && seen.opcode == opcode)
            .collect()
    }

    /// The completion queue made as `queue`.
    pub(crate) fn cq(&self, queue: u16) -> Option<Cq> {
        self.cqs.get(&queue).copied()
    }

    /// The submission queue made as `queue`.
    pub(crate) fn sq(&self, queue: u16) -> Option<Sq> {
        self.sqs.get(&queue).copied()
    }
}

/// The model, shared between the driver's parts and the test.
pub(crate) type Shared = Rc<RefCell<Model>>;

/// The model's registers.
#[derive(Debug)]
pub(crate) struct Regs(pub(crate) Shared);

impl Registers for Regs {
    fn read(&self, offset: u32) -> u32 {
        self.0.borrow_mut().register_read(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        self.0.borrow_mut().register_write(offset, value);
    }
}

/// Pages of the model's memory, at addresses that are neither ascending
/// nor adjacent.
#[derive(Debug)]
pub(crate) struct Mem {
    model: Shared,
    pages: Vec<u64>,
}

impl Mem {
    /// `count` pages somewhere above `base`.
    pub(crate) fn new(model: &Shared, base: u64, count: usize) -> Self {
        let pages = (0..count)
            .map(|index| base + ((count - 1 - index) as u64) * 3 * PAGE as u64)
            .collect();
        Mem {
            model: Rc::clone(model),
            pages,
        }
    }

    fn address(&self, offset: usize) -> u64 {
        self.pages[offset / PAGE] + (offset % PAGE) as u64
    }

    /// Fill `length` bytes from `offset` with a pattern seeded by `seed`.
    pub(crate) fn fill(&mut self, offset: usize, length: usize, seed: u8) {
        for index in 0..length {
            self.write_u8(offset + index, seed.wrapping_add((index % 251) as u8));
        }
    }

    /// The `length` bytes from `offset`.
    pub(crate) fn bytes(&self, offset: usize, length: usize) -> Vec<u8> {
        (0..length)
            .map(|index| self.read_u8(offset + index))
            .collect()
    }
}

impl Memory for Mem {
    fn pages(&self) -> &[u64] {
        &self.pages
    }

    fn read_u8(&self, offset: usize) -> u8 {
        let address = self.address(offset);
        self.model.borrow_mut().read_byte(address)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        let address = self.address(offset);
        self.model.borrow_mut().write_byte(address, value);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        assert_eq!(offset % 2, 0, "an aligned u16");
        u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset + 1)])
    }
}

/// A clock a test moves.
#[derive(Debug, Clone)]
pub(crate) struct FakeClock(pub(crate) Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        self.0.get()
    }

    fn pause(&mut self, nanos: u64) {
        self.0.set(self.0.get() + nanos);
    }
}
