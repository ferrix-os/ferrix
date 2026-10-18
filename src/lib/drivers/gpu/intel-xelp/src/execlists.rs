// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_execlists_submission.c,
// gt/intel_lrc.c, gt/intel_lrc.h, gt/intel_lrc_reg.h and gt/intel_engine.h:
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c, intel_lrc.c)
// Copyright © 2014-2018 Intel Corporation (intel_lrc_reg.h)
// Copyright © 2019 Intel Corporation (intel_engine.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Execlist submission on Gen12.0, as i915 does it without `GuC`: a context is
//! named by a 64-bit *descriptor* (its image's GGTT address, its addressing
//! mode and a context id), written into the engine's submit queue and loaded
//! with `EL_CTRL_LOAD`. The engine reports each switch in the *context
//! status buffer*, twelve 64-bit entries in the hardware status page, which
//! `process_csb` walks from its read pointer to the engine's write pointer.

use crate::engine::{EL_CTRL_LOAD, ring_execlist_control, ring_execlist_sq_contents};
use crate::reg::{Reg, field_get, genmask};

/// `INTEL_LEGACY_32B_CONTEXT`.
pub const INTEL_LEGACY_32B_CONTEXT: u32 = 1;
/// `INTEL_LEGACY_64B_CONTEXT`: a 4-level PPGTT.
pub const INTEL_LEGACY_64B_CONTEXT: u32 = 3;
/// `GEN8_CTX_VALID`.
pub const GEN8_CTX_VALID: u32 = 1 << 0;
/// `GEN8_CTX_PRIVILEGE`.
pub const GEN8_CTX_PRIVILEGE: u32 = 1 << 8;
/// `GEN8_CTX_ADDRESSING_MODE_SHIFT`.
pub const GEN8_CTX_ADDRESSING_MODE_SHIFT: u32 = 3;
/// `CTX_DESC_FORCE_RESTORE`.
pub const CTX_DESC_FORCE_RESTORE: u64 = 1 << 2;
/// `CTX_GTT_ADDRESS_MASK`.
pub const CTX_GTT_ADDRESS_MASK: u32 = genmask(31, 12);
/// `GEN11_SW_CTX_ID_SHIFT`.
pub const GEN11_SW_CTX_ID_SHIFT: u32 = 37;
/// `GEN11_ENGINE_INSTANCE_SHIFT`.
pub const GEN11_ENGINE_INSTANCE_SHIFT: u32 = 48;
/// `GEN11_ENGINE_CLASS_SHIFT`.
pub const GEN11_ENGINE_CLASS_SHIFT: u32 = 61;
/// `GEN11_MAX_CONTEXT_HW_ID`, exclusive.
pub const GEN11_MAX_CONTEXT_HW_ID: u32 = 1 << 11;
/// `GEN12_MAX_CONTEXT_HW_ID`.
pub const GEN12_MAX_CONTEXT_HW_ID: u32 = GEN11_MAX_CONTEXT_HW_ID - 1;

/// `lrc_descriptor`: the low dword, from the context image's GGTT offset
/// (page-aligned) and whether its VM is 4-level. Gen12 sets no
/// `GEN8_CTX_L3LLC_COHERENT` (Gen8 only).
#[must_use]
pub const fn lrc_descriptor(ggtt_offset: u32, vm_is_4lvl: bool) -> u32 {
    let mode = if vm_is_4lvl {
        INTEL_LEGACY_64B_CONTEXT
    } else {
        INTEL_LEGACY_32B_CONTEXT
    };
    (ggtt_offset & CTX_GTT_ADDRESS_MASK)
        | (mode << GEN8_CTX_ADDRESSING_MODE_SHIFT)
        | GEN8_CTX_VALID
        | GEN8_CTX_PRIVILEGE
}

/// The engine's part of the context id, `execlists->ccid`, as
/// `intel_execlists_submission_setup` makes it for Gen11 to 12.50.
#[must_use]
pub const fn engine_ccid(class: u8, instance: u8) -> u32 {
    ((instance as u32) << (GEN11_ENGINE_INSTANCE_SHIFT - 32))
        | ((class as u32) << (GEN11_ENGINE_CLASS_SHIFT - 32))
}

/// `__execlists_schedule_in`'s `ce->lrc.ccid` below 12.55: the free tag
/// (0 to 62, from `engine->context_tag`) plus one, shifted to the software
/// context id, with the engine's part.
#[must_use]
pub const fn context_ccid(tag: u32, engine_ccid: u32) -> u32 {
    ((1 + tag) << (GEN11_SW_CTX_ID_SHIFT - 32)) | engine_ccid
}

/// `ce->lrc.desc`: `lrca` in the low dword, `ccid` in the high.
#[must_use]
pub const fn descriptor(lrca: u32, ccid: u32) -> u64 {
    lrca as u64 | (ccid as u64) << 32
}

/// `execlists_num_ports`: `port_mask` is 1 (`intel_engine_cs.c`).
pub const NUM_PORTS: u32 = 2;

/// `execlists_submit_ports` with `write_desc` for an engine with the submit
/// queue (`HAS_LOGICAL_RING_ELSQ`): every port is written, the empty ones
/// with 0, from the last to the first ("the submit queue is not cleared
/// after being submitted ... so we always write the same number of elsq
/// entries"), then `EL_CTRL_LOAD`. The writes, in order.
#[must_use]
pub fn submit_ports(
    base: u32,
    ports: [u64; NUM_PORTS as usize],
) -> [(Reg, u32); 2 * NUM_PORTS as usize + 1] {
    let queue = ring_execlist_sq_contents(base).offset();
    let mut writes = [(Reg(0), 0); 2 * NUM_PORTS as usize + 1];
    let mut at = 0;
    for port in (0..NUM_PORTS).rev() {
        let desc = ports.get(port as usize).copied().unwrap_or(0);
        if let Some(slot) = writes.get_mut(at) {
            *slot = (Reg(queue + port * 8), desc as u32);
        }
        if let Some(slot) = writes.get_mut(at + 1) {
            *slot = (Reg(queue + port * 8 + 4), (desc >> 32) as u32);
        }
        at += 2;
    }
    if let Some(slot) = writes.get_mut(at) {
        *slot = (ring_execlist_control(base), EL_CTRL_LOAD);
    }
    writes
}

/// `I915_HWS_CSB_BUF0_INDEX`: the status buffer's first dword in the HWSP.
pub const I915_HWS_CSB_BUF0_INDEX: usize = 0x10;
/// `ICL_HWS_CSB_WRITE_INDEX`: the dword the engine writes its CSB write
/// pointer to, Gen11+.
pub const ICL_HWS_CSB_WRITE_INDEX: usize = 0x2f;
/// `GEN11_CSB_ENTRIES`.
pub const GEN11_CSB_ENTRIES: u32 = 12;
/// `GEN11_CSB_PTR_MASK`.
pub const GEN11_CSB_PTR_MASK: u32 = 0xf;
/// `GEN11_CSB_READ_PTR_MASK`, in `RING_CONTEXT_STATUS_PTR`.
pub const GEN11_CSB_READ_PTR_MASK: u32 = GEN11_CSB_PTR_MASK << 8;
/// `GEN11_CSB_WRITE_PTR_MASK`.
pub const GEN11_CSB_WRITE_PTR_MASK: u32 = GEN11_CSB_PTR_MASK;

/// `GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE`, in the lower dword.
pub const GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE: u32 = 0x1;
/// `GEN12_CSB_SW_CTX_ID_MASK`.
pub const GEN12_CSB_SW_CTX_ID_MASK: u32 = genmask(25, 15);
/// `GEN12_IDLE_CTX_ID`.
pub const GEN12_IDLE_CTX_ID: u32 = 0x7ff;

/// `GEN12_CSB_CTX_VALID(dw)`.
#[must_use]
pub const fn gen12_csb_ctx_valid(dw: u32) -> bool {
    field_get(GEN12_CSB_SW_CTX_ID_MASK, dw) != GEN12_IDLE_CTX_ID
}

/// `GEN12_CTX_SWITCH_DETAIL(dw)`, in the upper dword.
#[must_use]
pub const fn gen12_ctx_switch_detail(dw: u32) -> u32 {
    dw & 0xf
}

/// What a CSB entry means for the ports, by `gen12_csb_parse`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CsbEvent {
    /// `true` from `gen12_csb_parse`: the pending ports were promoted to
    /// active (a new queue took effect).
    Promote,
    /// `false`: the active port completed (or was preempted to idle).
    Complete,
    /// Neither, which i915 calls a bug (`GEM_BUG_ON`): a context going away
    /// with a switch detail, or a promotion with no valid context to.
    Unexpected {
        /// The entry.
        csb: u64,
    },
}

/// `gen12_csb_parse` with `__gen12_csb_parse`; the cases i915 asserts
/// against are reported, not trusted.
#[must_use]
pub const fn gen12_csb_parse(csb: u64) -> CsbEvent {
    let lower = csb as u32;
    let upper = (csb >> 32) as u32;
    let ctx_to_valid = gen12_csb_ctx_valid(lower);
    let ctx_away_valid = gen12_csb_ctx_valid(upper);
    let new_queue = lower & GEN12_CTX_STATUS_SWITCHED_TO_NEW_QUEUE != 0;
    if !ctx_away_valid || new_queue {
        if ctx_to_valid {
            CsbEvent::Promote
        } else {
            CsbEvent::Unexpected { csb }
        }
    } else if gen12_ctx_switch_detail(upper) != 0 {
        CsbEvent::Unexpected { csb }
    } else {
        CsbEvent::Complete
    }
}

/// The CSB entries `process_csb` reads, from the one after `head` (the last
/// read) to `tail` (the engine's write pointer) inclusive, wrapping at
/// [`GEN11_CSB_ENTRIES`]. `None` for a pointer out of range, which i915
/// treats as a hardware fault and resets.
pub fn csb_indices(head: u32, tail: u32) -> Option<impl Iterator<Item = u32>> {
    if head >= GEN11_CSB_ENTRIES || tail >= GEN11_CSB_ENTRIES {
        return None;
    }
    let count = (tail + GEN11_CSB_ENTRIES - head) % GEN11_CSB_ENTRIES;
    Some((1..=count).map(move |step| (head + step) % GEN11_CSB_ENTRIES))
}

/// `INTEL_HWS_CSB_WRITE_INDEX` reset value and `csb_head` after
/// `reset_csb_pointers`: the last entry, so that the first one the engine
/// writes after a reset, entry 0, is the first one read.
pub const CSB_RESET_VALUE: u32 = GEN11_CSB_ENTRIES - 1;

/// `reset_csb_pointers`: the engine's pointers set back to the last entry
/// over MMIO (twice, "for luck and our trusty paranoia"), the status page's
/// copy of the write pointer with them, and every entry poisoned with `-1`
/// so that one the engine did not write is seen. Returns the read pointer
/// to start from.
pub fn reset_csb_pointers<M, P>(mmio: &mut M, hwsp: &mut P, base: u32) -> u32
where
    M: crate::io::Mmio + ?Sized,
    P: crate::io::StatusPage + ?Sized,
{
    let ptr = crate::engine::ring_context_status_ptr(base).offset();
    let value = (0xffff << 16) | (CSB_RESET_VALUE << 8) | CSB_RESET_VALUE;
    mmio.write32(ptr, value);
    let _posting = mmio.read32(ptr);
    hwsp.write32(ICL_HWS_CSB_WRITE_INDEX, CSB_RESET_VALUE);
    for dword in 0..2 * GEN11_CSB_ENTRIES as usize {
        hwsp.write32(I915_HWS_CSB_BUF0_INDEX + dword, u32::MAX);
    }
    mmio.write32(ptr, value);
    let _posting = mmio.read32(ptr);
    CSB_RESET_VALUE
}

/// `GEN12_CSB_SW_CTX_ID_MASK`'s field in a CSB dword: the software context
/// id of the context switched to (lower dword) or away from (upper).
#[must_use]
pub const fn gen12_csb_sw_ctx_id(dw: u32) -> u32 {
    field_get(GEN12_CSB_SW_CTX_ID_MASK, dw)
}

/// The software context id a descriptor's `ccid` carries, as the CSB
/// reports it: the bits above `GEN11_SW_CTX_ID_SHIFT`.
#[must_use]
pub const fn ccid_sw_ctx_id(ccid: u32) -> u32 {
    (ccid >> (GEN11_SW_CTX_ID_SHIFT - 32)) & GEN12_IDLE_CTX_ID
}

/// How long `wa_csb_read` waits for an entry still reading `-1`
/// (`tgl,dg1:HSDES#22011327657`) before it reads the MMIO copy.
pub const CSB_READ_WAIT_US: u64 = 10;

/// `csb_read` with `wa_csb_read`: entry `index` from the status page; if it
/// still reads `-1`, waited for, then taken from the engine's MMIO copy
/// (`GEN8_EXECLISTS_STATUS_BUF`, entries 6 and on at
/// `GEN11_EXECLISTS_STATUS_BUF2`). The entry is then consumed: set to `-1`
/// "so that we can spot its future reuse".
pub fn csb_read<M, P, C>(mmio: &mut M, hwsp: &mut P, clock: &mut C, base: u32, index: u32) -> u64
where
    M: crate::io::Mmio + ?Sized,
    P: crate::io::StatusPage + ?Sized,
    C: crate::io::Clock + ?Sized,
{
    use crate::generated::intel_lrc_reg::{GEN8_EXECLISTS_STATUS_BUF, GEN11_EXECLISTS_STATUS_BUF2};
    let at = I915_HWS_CSB_BUF0_INDEX + 2 * index as usize;
    let read = |hwsp: &mut P| u64::from(hwsp.read32(at)) | (u64::from(hwsp.read32(at + 1)) << 32);
    let mut entry = read(hwsp);
    if entry == u64::MAX {
        let start = clock.now_us();
        while entry == u64::MAX && clock.now_us().saturating_sub(start) < CSB_READ_WAIT_US {
            clock.delay_us(1);
            entry = read(hwsp);
        }
        if entry == u64::MAX {
            let (status, idx) = if index >= 6 {
                (GEN11_EXECLISTS_STATUS_BUF2, index - 6)
            } else {
                (GEN8_EXECLISTS_STATUS_BUF, index)
            };
            let offset = base + status + 8 * idx;
            entry = u64::from(mmio.read32(offset)) | (u64::from(mmio.read32(offset + 4)) << 32);
        }
    }
    hwsp.write32(at, u32::MAX);
    hwsp.write32(at + 1, u32::MAX);
    entry
}

/// What a context in a port is to the caller: an id of its choosing, and
/// the descriptor it was submitted with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Port<T> {
    /// The caller's context.
    pub context: T,
    /// The 64-bit descriptor written into the submit queue.
    pub desc: u64,
}

/// What [`Execlists::process_csb`] saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CsbReport {
    /// Entries read.
    pub entries: u32,
    /// Of them, promotions of the pending ports.
    pub promoted: u32,
    /// Of them, completions of the active port.
    pub completed: u32,
    /// `execlists->error_interrupt |= ERROR_CSB`: an entry that made no
    /// sense for the ports (a promotion with nothing pending, a completion
    /// with nothing active, or one [`gen12_csb_parse`] calls unexpected), or
    /// a write pointer out of range. i915 then resets the engine.
    pub error: Option<u64>,
}

/// `struct intel_engine_execlists`' ports: what the driver submitted and the
/// engine has not taken yet (`pending`), what the engine runs (`inflight`
/// from `active`), and how far the status buffer has been read.
#[derive(Clone, Copy, Debug)]
pub struct Execlists<T: Copy> {
    /// `csb_head`.
    pub csb_head: u32,
    pending: [Option<Port<T>>; NUM_PORTS as usize],
    inflight: [Option<Port<T>>; NUM_PORTS as usize],
    active: usize,
}

impl<T: Copy> Default for Execlists<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy> Execlists<T> {
    /// Ports empty, the read pointer as [`reset_csb_pointers`] leaves it.
    #[must_use]
    pub const fn new() -> Self {
        Execlists {
            csb_head: CSB_RESET_VALUE,
            pending: [None; NUM_PORTS as usize],
            inflight: [None; NUM_PORTS as usize],
            active: 0,
        }
    }

    /// `execlists_active`: the port the engine runs, if any.
    #[must_use]
    pub fn active(&self) -> Option<Port<T>> {
        self.inflight.get(self.active).copied().flatten()
    }

    /// Whether a submission is still waiting for the engine to take it
    /// (`execlists->pending[0]`): i915 submits nothing more until it is.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.pending.iter().any(Option::is_some)
    }

    /// Whether nothing is pending or running, as far as the ports know.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        !self.is_pending() && self.active().is_none()
    }

    /// `execlists_submit_ports`: write `ports` into the submit queue and
    /// load it. Returns `false`, writing nothing, while an earlier
    /// submission is pending or when the first port is empty.
    pub fn submit<M: crate::io::Mmio + ?Sized>(
        &mut self,
        mmio: &mut M,
        base: u32,
        ports: [Option<Port<T>>; NUM_PORTS as usize],
    ) -> bool {
        if self.is_pending() || ports[0].is_none() {
            return false;
        }
        let descs = ports.map(|port| port.map_or(0, |p| p.desc));
        for (reg, value) in submit_ports(base, descs) {
            mmio.write32(reg.offset(), value);
        }
        self.pending = ports;
        true
    }

    /// `process_csb`: every entry from the one after [`Self::csb_head`] to
    /// the write pointer the engine left in the status page, each read by
    /// [`csb_read`] and parsed by [`gen12_csb_parse`]. A promotion makes the
    /// pending ports the running ones; every context that leaves the engine
    /// (its port completed, or replaced by a promotion) is handed to
    /// `out`, as i915 hands it to `post_process_csb`.
    pub fn process_csb<M, P, C>(
        &mut self,
        mmio: &mut M,
        hwsp: &mut P,
        clock: &mut C,
        base: u32,
        mut out: impl FnMut(Port<T>),
    ) -> CsbReport
    where
        M: crate::io::Mmio + ?Sized,
        P: crate::io::StatusPage + ?Sized,
        C: crate::io::Clock + ?Sized,
    {
        let mut report = CsbReport::default();
        let tail = hwsp.read32(ICL_HWS_CSB_WRITE_INDEX);
        let Some(indices) = csb_indices(self.csb_head, tail) else {
            // i915 trusts the u8 it reads; a pointer out of range is as bad
            // as an entry that makes no sense.
            report.error = Some(u64::from(tail));
            return report;
        };
        self.csb_head = tail;
        for index in indices {
            let csb = csb_read(mmio, hwsp, clock, base, index);
            report.entries += 1;
            match gen12_csb_parse(csb) {
                CsbEvent::Promote => {
                    if self.pending[0].is_none() {
                        report.error = Some(csb);
                        break;
                    }
                    for port in self.inflight.iter().skip(self.active).flatten() {
                        out(*port);
                    }
                    self.inflight = self.pending;
                    self.active = 0;
                    self.pending = [None; NUM_PORTS as usize];
                    report.promoted += 1;
                }
                CsbEvent::Complete => {
                    let Some(port) = self.active() else {
                        report.error = Some(csb);
                        break;
                    };
                    out(port);
                    self.active += 1;
                    report.completed += 1;
                }
                CsbEvent::Unexpected { csb } => {
                    report.error = Some(csb);
                    break;
                }
            }
        }
        let _posting = mmio.read32(crate::engine::ring_context_status_ptr(base).offset());
        report
    }

    /// The ports' part of an engine reset (`execlists_reset_rewind`'s
    /// `reset_csb_pointers`, then `cancel_port_requests`): every context
    /// still in a port is handed to `out`, and the ports and the read
    /// pointer start over.
    pub fn reset<M, P>(
        &mut self,
        mmio: &mut M,
        hwsp: &mut P,
        base: u32,
        mut out: impl FnMut(Port<T>),
    ) where
        M: crate::io::Mmio + ?Sized,
        P: crate::io::StatusPage + ?Sized,
    {
        for port in self
            .pending
            .iter()
            .chain(self.inflight.iter().skip(self.active))
            .flatten()
        {
            out(*port);
        }
        *self = Self::new();
        self.csb_head = reset_csb_pointers(mmio, hwsp, base);
    }
}

/// `intel_ring_direction(ring, next, prev)` for a ring of `size` bytes:
/// positive when `next` is ahead of `prev`. `ring->wrap` is
/// `BITS_PER_TYPE(ring->size) - ilog2(size)`: the offset's bits are shifted
/// to the top, so the sign says which way is shorter.
#[must_use]
pub const fn intel_ring_direction(size: u32, next: u32, prev: u32) -> i32 {
    (next.wrapping_sub(prev) << (u32::BITS - size.trailing_zeros())) as i32
}

/// `execlists_update_context`: the descriptor to submit for a context whose
/// image says `CTX_RING_TAIL` is `prev`, now that its ring runs to `tail`,
/// with `CTX_DESC_FORCE_RESTORE` when the tail does not advance
/// (`WaIdleLiteRestore`: "We should never submit the context with the same
/// `RING_TAIL` twice"). The caller writes `tail` into the image first.
#[must_use]
pub const fn update_context_desc(desc: u64, ring_size: u32, tail: u32, prev: u32) -> u64 {
    if intel_ring_direction(ring_size, tail, prev) <= 0 {
        desc | CTX_DESC_FORCE_RESTORE
    } else {
        desc
    }
}
