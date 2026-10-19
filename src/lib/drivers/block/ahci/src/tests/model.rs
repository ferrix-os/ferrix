//! A model of an AHCI HBA: registers, "physical" memory it reaches by
//! address, a disk in a vector, and the faults a test asks for.
//!
//! It is written from the specification's side, not the driver's: it finds
//! the command list by `PxCLB`, the command table by the header's `CTBA`,
//! reads the command FIS and PRDT by field offset, and walks the PRDT
//! itself. So a driver that encoded a command wrongly meets a model that
//! reads it as an HBA would.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::vec;
use std::vec::Vec;

use crate::regs::{self, PAGE};
use crate::{Clock, Memory, Registers};

/// One port.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Port {
    pub(crate) clb: u64,
    pub(crate) fb: u64,
    pub(crate) is: u32,
    pub(crate) ie: u32,
    pub(crate) cmd: u32,
    pub(crate) tfd: u32,
    pub(crate) sig: u32,
    pub(crate) ssts: u32,
    pub(crate) serr: u32,
    pub(crate) ci: u32,
    pub(crate) sctl: u32,
    /// COMRESETs seen, and what the link and device are after one.
    pub(crate) comresets: u32,
    pub(crate) after_reset: Option<(u32, u32, u32)>,
    /// Once on, the FIS receive engine never stops: FR stays set.
    pub(crate) stuck_fr: bool,
}

/// A command the model saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) port: usize,
    pub(crate) opcode: u8,
    pub(crate) lba: u64,
    pub(crate) count: u32,
    pub(crate) write: bool,
    pub(crate) prds: Vec<(u64, u32)>,
}

/// The HBA.
#[derive(Debug)]
pub(crate) struct Model {
    memory: BTreeMap<u64, Vec<u8>>,
    pub(crate) cap: u32,
    pub(crate) ghc: u32,
    pub(crate) pi: u32,
    pub(crate) cap2: u32,
    pub(crate) bohc: u32,
    pub(crate) ports: Vec<Port>,
    pub(crate) disk: Vec<u8>,
    pub(crate) sector_size: usize,
    pub(crate) identify: [u16; 256],
    /// The next command ends with `ERR`: status and error.
    pub(crate) fail_next: Option<(u8, u8)>,
    /// Commands never complete.
    pub(crate) hang: bool,
    pub(crate) seen: Vec<Seen>,
    pub(crate) flushes: u32,
}

/// An ATA string into IDENTIFY words, first character in the high byte.
fn ata_string(words: &mut [u16], text: &str) {
    let mut bytes = vec![b' '; words.len() * 2];
    bytes[..text.len()].copy_from_slice(text.as_bytes());
    for (i, word) in words.iter_mut().enumerate() {
        *word = u16::from_be_bytes([bytes[2 * i], bytes[2 * i + 1]]);
    }
}

impl Model {
    /// One port with an ATA disk of `sectors` sectors of `sector_size`.
    pub(crate) fn new(sectors: u64, sector_size: usize) -> Self {
        let mut identify = [0u16; 256];
        identify[83] = 1 << 10;
        for i in 0..4 {
            identify[100 + i] = (sectors >> (16 * i)) as u16;
        }
        let lba28 = sectors.min(0x0FFF_FFFF);
        identify[60] = lba28 as u16;
        identify[61] = (lba28 >> 16) as u16;
        identify[106] = 0x4000;
        if sector_size != 512 {
            identify[106] |= 1 << 12;
            let words = (sector_size / 2) as u32;
            identify[117] = words as u16;
            identify[118] = (words >> 16) as u16;
        }
        ata_string(&mut identify[27..47], "FERRIX MODEL DISK");
        ata_string(&mut identify[10..20], "SN-0042");
        let mut ports = vec![Port::default(); 32];
        ports[0] = Port {
            sig: regs::SIG_ATA,
            ssts: 0x113,
            tfd: 0x50,
            ..Port::default()
        };
        Model {
            memory: BTreeMap::new(),
            cap: regs::CAP_S64A | regs::CAP_SCLO | (31 << 8) | 5,
            ghc: 0,
            pi: 1,
            cap2: 0,
            bohc: 0,
            ports,
            disk: vec![0; sectors as usize * sector_size],
            sector_size,
            identify,
            fail_next: None,
            hang: false,
            seen: Vec::new(),
            flushes: 0,
        }
    }

    pub(crate) fn read_byte(&mut self, address: u64) -> u8 {
        let page = self
            .memory
            .entry(address & !(PAGE as u64 - 1))
            .or_insert_with(|| vec![0; PAGE]);
        page[(address as usize) % PAGE]
    }

    pub(crate) fn write_byte(&mut self, address: u64, value: u8) {
        let page = self
            .memory
            .entry(address & !(PAGE as u64 - 1))
            .or_insert_with(|| vec![0; PAGE]);
        page[(address as usize) % PAGE] = value;
    }

    pub(crate) fn read_bytes(&mut self, address: u64, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| self.read_byte(address + i as u64))
            .collect()
    }

    pub(crate) fn write_bytes(&mut self, address: u64, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate() {
            self.write_byte(address + i as u64, byte);
        }
    }

    fn read_u32(&mut self, address: u64) -> u32 {
        let b = self.read_bytes(address, 4);
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    fn write_u32(&mut self, address: u64, value: u32) {
        self.write_bytes(address, &value.to_le_bytes());
    }

    fn read_reg(&self, offset: u32) -> u32 {
        if offset < regs::PORTS {
            return match offset {
                regs::CAP => self.cap,
                regs::GHC => self.ghc,
                regs::PI => self.pi,
                regs::VS => 0x0001_0301,
                regs::CAP2 => self.cap2,
                regs::BOHC => self.bohc,
                _ => 0,
            };
        }
        let index = ((offset - regs::PORTS) / regs::PORT_STRIDE) as usize;
        let port = &self.ports[index];
        match (offset - regs::PORTS) % regs::PORT_STRIDE {
            regs::PX_CLB => port.clb as u32,
            regs::PX_CLBU => (port.clb >> 32) as u32,
            regs::PX_FB => port.fb as u32,
            regs::PX_FBU => (port.fb >> 32) as u32,
            regs::PX_IS => port.is,
            regs::PX_IE => port.ie,
            regs::PX_CMD => port.cmd,
            regs::PX_TFD => port.tfd,
            regs::PX_SIG => port.sig,
            regs::PX_SSTS => port.ssts,
            regs::PX_SCTL => port.sctl,
            regs::PX_SERR => port.serr,
            regs::PX_CI => port.ci,
            _ => 0,
        }
    }

    fn write_reg(&mut self, offset: u32, value: u32) {
        if offset < regs::PORTS {
            match offset {
                regs::GHC => self.ghc = value & !regs::GHC_HR,
                regs::BOHC if value & regs::BOHC_OOS != 0 => self.bohc = regs::BOHC_OOS,
                _ => {}
            }
            return;
        }
        let index = ((offset - regs::PORTS) / regs::PORT_STRIDE) as usize;
        let port = &mut self.ports[index];
        match (offset - regs::PORTS) % regs::PORT_STRIDE {
            regs::PX_CLB => port.clb = (port.clb & !0xFFFF_FFFF) | u64::from(value),
            regs::PX_CLBU => port.clb = (port.clb & 0xFFFF_FFFF) | (u64::from(value) << 32),
            regs::PX_FB => port.fb = (port.fb & !0xFFFF_FFFF) | u64::from(value),
            regs::PX_FBU => port.fb = (port.fb & 0xFFFF_FFFF) | (u64::from(value) << 32),
            regs::PX_IS => port.is &= !value,
            regs::PX_IE => port.ie = value,
            regs::PX_SERR => port.serr &= !value,
            regs::PX_SCTL => {
                assert_eq!(port.cmd & regs::CMD_ST, 0, "a COMRESET with PxCMD.ST set");
                let was = port.sctl & 0xF;
                port.sctl = value;
                if was == 1 && value & 0xF == 0 {
                    port.comresets += 1;
                    if let Some((ssts, tfd, sig)) = port.after_reset {
                        (port.ssts, port.tfd, port.sig) = (ssts, tfd, sig);
                    }
                } else if value & 0xF == 1 {
                    port.ssts &= !0xF;
                }
            }
            regs::PX_CMD => {
                let kept = regs::CMD_ST | regs::CMD_SUD | regs::CMD_POD | regs::CMD_FRE;
                let stuck = port.stuck_fr && port.cmd & regs::CMD_FR != 0;
                port.cmd = value & kept;
                if stuck {
                    port.cmd |= regs::CMD_FR;
                }
                if value & regs::CMD_ST != 0 {
                    port.cmd |= regs::CMD_CR;
                } else {
                    port.ci = 0;
                }
                if value & regs::CMD_FRE != 0 {
                    port.cmd |= regs::CMD_FR;
                }
                if value & regs::CMD_CLO != 0 {
                    port.tfd &= !(regs::TFD_BSY | regs::TFD_DRQ);
                }
            }
            regs::PX_CI => {
                assert_ne!(
                    port.cmd & regs::CMD_ST,
                    0,
                    "PxCI written with PxCMD.ST clear"
                );
                port.ci |= value;
                if value & 1 != 0 {
                    self.execute(index);
                }
            }
            _ => {}
        }
    }

    /// Run slot 0 of port `index`'s command list.
    fn execute(&mut self, index: usize) {
        let clb = self.ports[index].clb;
        let flags = self.read_u32(clb);
        let table = u64::from(self.read_u32(clb + 8)) | (u64::from(self.read_u32(clb + 12)) << 32);
        assert_eq!(table % 128, 0, "the command table is 128-byte aligned");
        assert_eq!(flags & 0x1F, 5, "a five-dword command FIS");
        // A real HBA ends a command with C set as soon as its FIS is sent,
        // before any data: IDENTIFY then reads back what was in memory.
        assert_eq!(flags & (1 << 10), 0, "the C bit on a command with data");
        let fis = self.read_bytes(table, 16);
        assert_eq!(fis[0], 0x27, "a Register Host to Device FIS");
        assert_eq!(fis[1] & 0x80, 0x80, "the FIS's C bit");
        let lba = u64::from_le_bytes([fis[4], fis[5], fis[6], fis[8], fis[9], fis[10], 0, 0]);
        let count = u32::from(u16::from_le_bytes([fis[12], fis[13]]));
        let write = flags & (1 << 6) != 0;
        let mut prds = Vec::new();
        for i in 0..(flags >> 16) as u64 {
            let at = table + 0x80 + i * 16;
            let addr = u64::from(self.read_u32(at)) | (u64::from(self.read_u32(at + 4)) << 32);
            let dbc = self.read_u32(at + 12) & 0x003F_FFFF;
            assert_eq!(dbc & 1, 1, "an even byte count");
            prds.push((addr, dbc + 1));
        }
        self.seen.push(Seen {
            port: index,
            opcode: fis[2],
            lba,
            count,
            write,
            prds: prds.clone(),
        });
        if self.hang {
            return;
        }
        if let Some((status, error)) = self.fail_next.take() {
            return self.fail(index, status, error);
        }
        let moved = match fis[2] {
            0xEC if !write => {
                let bytes: Vec<u8> = self.identify.iter().flat_map(|w| w.to_le_bytes()).collect();
                self.scatter(&prds, &bytes)
            }
            0x25 | 0x35 => self.rw(fis[2] == 0x35, write, lba, count, &prds),
            0xEA => {
                self.flushes += 1;
                Some(0)
            }
            _ => None,
        };
        let Some(moved) = moved else {
            return self.fail(index, 0x51, 0x04);
        };
        self.write_u32(clb + 4, moved as u32);
        let port = &mut self.ports[index];
        port.tfd = 0x50;
        port.is |= 1;
        port.ci &= !1;
    }

    fn rw(
        &mut self,
        is_write: bool,
        w_bit: bool,
        lba: u64,
        count: u32,
        prds: &[(u64, u32)],
    ) -> Option<usize> {
        let count = if count == 0 { 65536 } else { count as usize };
        let start = lba as usize * self.sector_size;
        let len = count * self.sector_size;
        let total: usize = prds.iter().map(|p| p.1 as usize).sum();
        if is_write != w_bit || total != len || start + len > self.disk.len() {
            return None;
        }
        if is_write {
            let mut at = start;
            for &(addr, n) in prds {
                let bytes = self.read_bytes(addr, n as usize);
                self.disk[at..at + n as usize].copy_from_slice(&bytes);
                at += n as usize;
            }
            Some(len)
        } else {
            let bytes = self.disk[start..start + len].to_vec();
            self.scatter(prds, &bytes)
        }
    }

    fn scatter(&mut self, prds: &[(u64, u32)], bytes: &[u8]) -> Option<usize> {
        let mut at = 0;
        for &(addr, n) in prds {
            let n = (n as usize).min(bytes.len() - at);
            self.write_bytes(addr, &bytes[at..at + n]);
            at += n;
        }
        (at == bytes.len()).then_some(at)
    }

    fn fail(&mut self, index: usize, status: u8, error: u8) {
        let port = &mut self.ports[index];
        port.tfd = u32::from(status) | (u32::from(error) << 8);
        port.is |= regs::IS_TFES;
    }
}

pub(crate) type Shared = Rc<RefCell<Model>>;

/// The model's registers.
#[derive(Debug)]
pub(crate) struct Regs(pub(crate) Shared);

impl Registers for Regs {
    fn read(&self, offset: u32) -> u32 {
        self.0.borrow().read_reg(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        self.0.borrow_mut().write_reg(offset, value);
    }
}

/// Control memory: pages of the model's memory.
#[derive(Debug)]
pub(crate) struct Mem {
    pub(crate) model: Shared,
    pub(crate) pages: Vec<u64>,
}

impl Mem {
    fn address(&self, offset: usize) -> u64 {
        self.pages[offset / PAGE] + (offset % PAGE) as u64
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

/// A clock that moves only when paused.
#[derive(Debug)]
pub(crate) struct FakeClock(pub(crate) Rc<Cell<u64>>);

impl Clock for FakeClock {
    fn now_nanos(&self) -> u64 {
        self.0.get()
    }

    fn pause(&mut self, nanos: u64) {
        self.0.set(self.0.get() + nanos);
    }
}
