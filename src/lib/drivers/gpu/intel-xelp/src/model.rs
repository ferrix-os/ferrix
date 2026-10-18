//! A model of one Xe-LP engine's execlist port, good enough to run what the
//! driver writes: the submit queue and its load, the context image's ring
//! registers, a command streamer for the commands the driver and M2's
//! tests emit, the context status buffer in the status page, and the
//! PPGTT walk. Like the GC400's front-end model (`docs/GPU.md` §6.3), it
//! checks the driver's half of the protocol; the hardware's half is
//! checked on the box.
//!
//! Memory is objects in the GGTT (each contiguous, by its GGTT offset) and
//! pages by device address, which the PPGTT maps.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::commands::gpu;
use crate::engine::{EL_CTRL_LOAD, EngineInfo, ring_execlist_control, ring_execlist_sq_contents};
use crate::execlists::{GEN12_IDLE_CTX_ID, I915_HWS_CSB_BUF0_INDEX, ICL_HWS_CSB_WRITE_INDEX};
use crate::io::{Clock, Mmio, StatusPage};
use crate::lrc::{self, ctx};
use crate::ppgtt::{Ppgtt, PtMemory};

/// Page-table memory: tables by device address, from 0x1000 up, at most
/// `limit` live at once.
#[derive(Default)]
pub(crate) struct PtModel {
    pub(crate) pages: BTreeMap<u64, Vec<u64>>,
    pub(crate) next: u64,
    pub(crate) limit: Option<usize>,
    pub(crate) freed: Vec<u64>,
}

impl PtMemory for PtModel {
    fn alloc_pt(&mut self) -> Option<u64> {
        if self.limit.is_some_and(|limit| self.pages.len() >= limit) {
            return None;
        }
        self.next += 0x1000;
        // Not zeroed: a table must be filled by whoever takes it.
        let _ = self.pages.insert(self.next, std::vec![0xdead_beef; 512]);
        Some(self.next)
    }

    fn free_pt(&mut self, table: u64) {
        assert!(
            self.pages.remove(&table).is_some(),
            "freed twice: {table:#x}"
        );
        self.freed.push(table);
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        self.pages[&table][index]
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        self.pages.get_mut(&table).expect("a live table")[index] = value;
    }
}

/// A clock that moves only when waited on.
#[derive(Default)]
pub(crate) struct Ticks {
    pub(crate) now: u64,
}

impl Clock for Ticks {
    fn now_us(&mut self) -> u64 {
        self.now
    }

    fn delay_us(&mut self, us: u32) {
        self.now += u64::from(us);
    }
}

/// How a context's run ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ran {
    /// The ring ran to its tail.
    Idle,
    /// The step budget ran out (a batch that never ends, a semaphore that
    /// never passes): the engine is hung on this context.
    Hung,
    /// Nothing was loaded.
    Nothing,
}

/// The GPU.
#[derive(Default)]
pub(crate) struct Gpu {
    /// GGTT objects by their first byte.
    pub(crate) objects: BTreeMap<u32, Vec<u32>>,
    /// Pages a PPGTT maps, by device address.
    pub(crate) pages: BTreeMap<u64, Vec<u32>>,
    /// The page tables.
    pub(crate) tables: PtModel,
    /// Every VM, by its PML4.
    pub(crate) vms: BTreeMap<u64, Ppgtt>,
    /// Registers: what was written.
    pub(crate) regs: BTreeMap<u32, u32>,
    /// The engine's base and its status page.
    pub(crate) base: u32,
    pub(crate) hwsp_ggtt: u32,
    /// What the last load put in the ports.
    pub(crate) loaded: Option<[u64; 2]>,
    /// The engine's own CSB write pointer.
    pub(crate) csb_write: u32,
    /// `MI_USER_INTERRUPT`s seen.
    pub(crate) interrupts: u32,
    /// Registers `MI_LOAD_REGISTER_IMM` wrote, in order.
    pub(crate) lri: Vec<(u32, u32)>,
    /// When set, CSB entries go only to the MMIO copy, never the status
    /// page (`HSDES#22011327657`).
    pub(crate) csb_to_mmio_only: bool,
    /// Whether `STOP_RING` is set, a reset requested, and one never made
    /// ready (an engine that does not answer `RING_RESET_CTL`).
    pub(crate) stopped: bool,
    pub(crate) reset_requested: bool,
    pub(crate) never_ready: bool,
    /// This engine's `GEN6_GDRST` domain, and how many resets it had.
    pub(crate) reset_domain: u32,
    pub(crate) resets: u32,
    /// Runs whose image was restore-inhibited.
    pub(crate) inhibited_runs: u32,
    /// Steps the command streamer may take in one run.
    pub(crate) budget: u32,
}

impl Gpu {
    pub(crate) fn new(engine: &EngineInfo, hwsp_ggtt: u32) -> Self {
        let mut gpu = Gpu {
            base: engine.mmio_base,
            hwsp_ggtt,
            budget: 10_000,
            reset_domain: crate::reset::reset_domain(engine).unwrap_or(0),
            ..Gpu::default()
        };
        gpu.add_object(hwsp_ggtt, 4096);
        gpu
    }

    /// A GGTT object of `bytes`, zeroed.
    pub(crate) fn add_object(&mut self, ggtt: u32, bytes: u32) {
        let _ = self
            .objects
            .insert(ggtt, std::vec![0; (bytes / 4) as usize]);
    }

    /// The object holding GGTT byte `address`, and the dword index in it.
    fn object_at(&mut self, address: u32) -> Option<(&mut Vec<u32>, usize)> {
        let (&start, object) = self.objects.range_mut(..=address).next_back()?;
        let index = ((address - start) / 4) as usize;
        (index < object.len()).then_some((object, index))
    }

    pub(crate) fn object(&mut self, ggtt: u32) -> &mut Vec<u32> {
        self.objects.get_mut(&ggtt).expect("an object")
    }

    pub(crate) fn ggtt_read(&mut self, address: u32) -> u32 {
        self.object_at(address).map_or(0, |(o, i)| o[i])
    }

    pub(crate) fn ggtt_write(&mut self, address: u32, value: u32) {
        if let Some((object, index)) = self.object_at(address) {
            object[index] = value;
        }
    }

    /// A page a PPGTT may map, zeroed.
    pub(crate) fn add_page(&mut self, address: u64) {
        let _ = self.pages.insert(address, std::vec![0; 1024]);
    }

    fn ppgtt_address(&mut self, pml4: u64, address: u64) -> Option<(u64, usize)> {
        let vm = *self.vms.get(&pml4)?;
        let device = vm.translate(&mut self.tables, address)?;
        Some((device & !0xfff, ((device & 0xfff) / 4) as usize))
    }

    pub(crate) fn ppgtt_read(&mut self, pml4: u64, address: u64) -> u32 {
        match self.ppgtt_address(pml4, address) {
            Some((page, index)) => self.pages.get(&page).map_or(0, |p| p[index]),
            None => 0, // the scratch page
        }
    }

    pub(crate) fn ppgtt_write(&mut self, pml4: u64, address: u64, value: u32) {
        if let Some((page, index)) = self.ppgtt_address(pml4, address)
            && let Some(page) = self.pages.get_mut(&page)
        {
            page[index] = value;
        }
    }

    fn hwsp_write(&mut self, index: usize, value: u32) {
        let hwsp = self.hwsp_ggtt;
        self.object(hwsp)[index] = value;
    }

    /// The engine writes one status-buffer entry and its write pointer.
    pub(crate) fn csb(&mut self, entry: u64) {
        self.csb_write = (self.csb_write + 1) % 12;
        let index = self.csb_write as usize;
        if self.csb_to_mmio_only {
            let (status, idx) = if index >= 6 {
                (0x3c0, index - 6)
            } else {
                (0x370, index)
            };
            let offset = self.base + status + 8 * idx as u32;
            let _ = self.regs.insert(offset, entry as u32);
            let _ = self.regs.insert(offset + 4, (entry >> 32) as u32);
        } else {
            self.hwsp_write(I915_HWS_CSB_BUF0_INDEX + 2 * index, entry as u32);
            self.hwsp_write(
                I915_HWS_CSB_BUF0_INDEX + 2 * index + 1,
                (entry >> 32) as u32,
            );
        }
        let write = self.csb_write;
        self.hwsp_write(ICL_HWS_CSB_WRITE_INDEX, write);
    }

    /// Run what the last load put in port 0: promote it (a CSB entry
    /// switching from idle to it), restore its ring from its image, run
    /// the ring from head to tail, save the head, then a CSB entry for its completion.
    /// A hung run writes no completion and leaves the context loaded.
    pub(crate) fn run(&mut self) -> Ran {
        let Some([desc, _]) = self.loaded.take() else {
            return Ran::Nothing;
        };
        let sw_id = ((desc >> 37) as u32) & GEN12_IDLE_CTX_ID;
        // Promotion: to the new context (lower), away from idle (upper).
        self.csb((u64::from(GEN12_IDLE_CTX_ID << 15) << 32) | u64::from(sw_id << 15) | 1);
        let image_ggtt = (desc as u32) & !0xfff;
        let state = image_ggtt + lrc::LRC_STATE_OFFSET;
        let read_reg = |gpu: &mut Gpu, at: usize| gpu.ggtt_read(state + 4 * at as u32);
        let start = read_reg(self, ctx::RING_START);
        let mut head = read_reg(self, ctx::RING_HEAD);
        let tail = read_reg(self, ctx::RING_TAIL);
        let size = (read_reg(self, ctx::RING_CTL) & !0xfff) + 4096;
        let inhibit =
            read_reg(self, ctx::CONTEXT_CONTROL) & lrc::CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT != 0;
        let pml4 = (u64::from(read_reg(self, ctx::PDP0_UDW)) << 32)
            | u64::from(read_reg(self, ctx::PDP0_LDW));
        self.lri.clear();
        let mut steps = self.budget;
        let mut hung = false;
        while head != tail {
            let Some(next) = self.step(
                Space::Ring { start, size },
                u64::from(head),
                pml4,
                image_ggtt,
                &mut steps,
            ) else {
                hung = true;
                break;
            };
            head = (next as u32) % size;
        }
        // Context save: the ring's head, and on a first run the registers.
        self.ggtt_write(state + 4 * ctx::RING_HEAD as u32, head);
        if hung {
            self.loaded = Some([desc, 0]);
            return Ran::Hung;
        }
        // A restore-inhibited image is saved whole by the hardware; what it
        // saves beyond the ring registers is the box's to show.
        self.inhibited_runs += u32::from(inhibit);
        // Completion: to idle (lower), away from this context (upper).
        self.csb((u64::from(sw_id << 15) << 32) | u64::from(GEN12_IDLE_CTX_ID << 15));
        Ran::Idle
    }

    fn fetch(&mut self, space: Space, at: u64, pml4: u64) -> u32 {
        match space {
            Space::Ring { start, size } => self.ggtt_read(start + (at as u32 % size)),
            Space::Ggtt => self.ggtt_read(at as u32),
            Space::Ppgtt => self.ppgtt_read(pml4, at),
        }
    }

    /// One command at `at` in `space`; the address of the next, or `None`
    /// when the budget ran out.
    #[allow(clippy::too_many_lines, reason = "a decoder, one arm a command")]
    fn step(
        &mut self,
        space: Space,
        at: u64,
        pml4: u64,
        image: u32,
        steps: &mut u32,
    ) -> Option<u64> {
        *steps = steps.checked_sub(1)?;
        let dw0 = self.fetch(space, at, pml4);
        let client = dw0 >> 29;
        let opcode = (dw0 >> 23) & 0x3f;
        let len = match (client, opcode) {
            (0, 0x00..=0x0f) => 1,
            _ => (dw0 & 0xff) as usize + 2,
        };
        let cmd: Vec<u32> = (0..len as u64)
            .map(|i| self.fetch(space, at + 4 * i, pml4))
            .collect();
        let next = at + 4 * len as u64;
        let pphwsp = image;
        match (client, opcode) {
            (0, 0x02) => self.interrupts += 1,
            (0, 0x0a) if !matches!(space, Space::Ring { .. }) => return Some(u64::MAX),
            (0, 0x20) => {
                if dw0 & gpu::MI_USE_GGTT != 0 {
                    self.ggtt_write(cmd[1], cmd[3]);
                } else {
                    let address = u64::from(cmd[1]) | (u64::from(cmd[2]) << 32);
                    self.ppgtt_write(pml4, address, cmd[3]);
                }
            }
            (0, 0x22) => {
                for pair in cmd[1..].chunks(2) {
                    if let [reg, value] = *pair {
                        self.lri.push((reg, value));
                        let _ = self.regs.insert(reg, value);
                    }
                }
            }
            (0, 0x1c) => {
                // MI_SEMAPHORE_WAIT, polling until memory equals the data.
                if dw0 & gpu::MI_SEMAPHORE_REGISTER_POLL == 0 {
                    let value = self.ggtt_read(cmd[2]);
                    if value != cmd[1] {
                        *steps = 0;
                        return None;
                    }
                }
            }
            (0, 0x24) => {
                // MI_STORE_REGISTER_MEM to the GGTT.
                let value = self.regs.get(&cmd[1]).copied().unwrap_or(0);
                self.ggtt_write(cmd[2], value);
            }
            (0, 0x26) if dw0 & gpu::MI_FLUSH_DW_OP_STOREDW != 0 => {
                if dw0 & gpu::MI_FLUSH_DW_STORE_INDEX != 0 {
                    self.ggtt_write(pphwsp + cmd[1], cmd[3]);
                } else if cmd[1] & gpu::MI_FLUSH_DW_USE_GTT != 0 {
                    self.ggtt_write(cmd[1] & !7, cmd[3]);
                }
            }
            (0, 0x31) => {
                let address = u64::from(cmd[1]) | (u64::from(cmd[2]) << 32);
                if !matches!(space, Space::Ring { .. }) {
                    // From a batch: a chain, not a call.
                    return Some(address);
                }
                let batch_space = if dw0 & (1 << 8) != 0 {
                    Space::Ppgtt
                } else {
                    Space::Ggtt
                };
                let mut pc = address;
                while pc != u64::MAX {
                    pc = self.step(batch_space, pc, pml4, image, steps)?;
                }
            }
            (3, _) if cmd[1] & gpu::PIPE_CONTROL_QW_WRITE != 0 => {
                if cmd[1] & gpu::PIPE_CONTROL_GLOBAL_GTT_IVB != 0 {
                    self.ggtt_write(cmd[2], cmd[4]);
                } else if cmd[1] & gpu::PIPE_CONTROL_STORE_DATA_INDEX != 0 {
                    self.ggtt_write(pphwsp + cmd[2], cmd[4]);
                }
            }
            (2, _) if (dw0 >> 22) & 0x7f == 0x42 => self.fast_copy(&cmd, pml4),
            _ => {}
        }
        Some(next)
    }

    /// `XY_FAST_COPY_BLT` with 32-bpp pixels: rows of `width` pixels at
    /// the two pitches.
    fn fast_copy(&mut self, cmd: &[u32], pml4: u64) {
        let dst_pitch = u64::from(cmd[1] & 0xffff);
        let rows = u64::from(cmd[3] >> 16);
        let width = u64::from(cmd[3] & 0xffff);
        let dst = u64::from(cmd[4]) | (u64::from(cmd[5]) << 32);
        let src_pitch = u64::from(cmd[7] & 0xffff);
        let src = u64::from(cmd[8]) | (u64::from(cmd[9]) << 32);
        for row in 0..rows {
            for x in 0..width {
                let value = self.ppgtt_read(pml4, src + row * src_pitch + 4 * x);
                self.ppgtt_write(pml4, dst + row * dst_pitch + 4 * x, value);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Space {
    Ring { start: u32, size: u32 },
    Ggtt,
    Ppgtt,
}

impl Mmio for Gpu {
    fn read32(&mut self, offset: u32) -> u32 {
        let value = self.regs.get(&offset).copied().unwrap_or(0);
        if offset == crate::engine::ring_mi_mode(self.base).offset() {
            // MODE_IDLE once STOP_RING was asked for.
            return if self.stopped {
                value | crate::generated::intel_engine_regs::MODE_IDLE
            } else {
                value
            };
        }
        if offset == crate::generated::intel_engine_regs::ring_reset_ctl(self.base).offset() {
            return if self.reset_requested && !self.never_ready {
                crate::generated::intel_engine_regs::RESET_CTL_READY_TO_RESET
            } else {
                0
            };
        }
        if offset == crate::generated::intel_gt_regs::GEN6_GDRST.offset() {
            return 0; // the reset is done at once
        }
        value
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let _ = self.regs.insert(offset, value);
        let masked = |bit: u32| (value >> 16) & bit != 0;
        if offset == crate::engine::ring_mi_mode(self.base).offset()
            && masked(crate::engine::STOP_RING)
        {
            self.stopped = value & crate::engine::STOP_RING != 0;
        }
        let reset_ctl = crate::generated::intel_engine_regs::ring_reset_ctl(self.base).offset();
        let request = crate::generated::intel_engine_regs::RESET_CTL_REQUEST_RESET;
        if offset == reset_ctl && masked(request) {
            self.reset_requested = value & request != 0;
        }
        if offset == crate::generated::intel_gt_regs::GEN6_GDRST.offset()
            && value & self.reset_domain != 0
        {
            // The engine's domain reset: whatever it ran is gone, and its
            // status buffer starts again at entry 0.
            self.loaded = None;
            self.csb_write = 11;
            self.resets += 1;
        }
        if offset == ring_execlist_control(self.base).offset() && value == EL_CTRL_LOAD {
            let queue = ring_execlist_sq_contents(self.base).offset();
            let desc = |gpu: &Gpu, port: u32| {
                u64::from(gpu.regs[&(queue + port * 8)])
                    | (u64::from(gpu.regs[&(queue + port * 8 + 4)]) << 32)
            };
            self.loaded = Some([desc(self, 0), desc(self, 1)]);
        }
        // reset_csb_pointers: the write pointer follows.
        if offset == crate::engine::ring_context_status_ptr(self.base).offset() {
            self.csb_write = value & 0xf;
        }
    }
}

/// The driver's view of the GPU's registers, beside [`Status`] over the
/// same model.
pub(crate) struct Regs<'a>(pub(crate) &'a core::cell::RefCell<Gpu>);

impl Mmio for Regs<'_> {
    fn read32(&mut self, offset: u32) -> u32 {
        self.0.borrow_mut().read32(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        self.0.borrow_mut().write32(offset, value);
    }
}

/// The driver's view of the status page.
pub(crate) struct Status<'a>(pub(crate) &'a core::cell::RefCell<Gpu>);

impl StatusPage for Status<'_> {
    fn read32(&mut self, index: usize) -> u32 {
        let mut gpu = self.0.borrow_mut();
        let hwsp = gpu.hwsp_ggtt;
        gpu.object(hwsp)[index]
    }

    fn write32(&mut self, index: usize, value: u32) {
        let mut gpu = self.0.borrow_mut();
        let hwsp = gpu.hwsp_ggtt;
        gpu.object(hwsp)[index] = value;
    }
}
