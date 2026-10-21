// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/intel_pcode.c
// (__snb_pcode_rw, gen7_check_mailbox_status), gt/intel_llc.c
// (gen6_update_ring_freq, calc_ia_freq for Gen9+) and
// gt/intel_gt_pm_debugfs.c (the ring frequency table's read):
// Copyright © 2013-2021 Intel Corporation (intel_pcode.c)
// Copyright © 2019 Intel Corporation (intel_llc.c, intel_gt_pm_debugfs.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! The PCU's mailbox (`GEN6_PCODE_MAILBOX`), and the one table i915 writes
//! through it on an LLC part without `SLPC`: the minimum ring frequency for
//! each GT frequency (`intel_llc_enable`, run by `intel_gt_resume` right
//! after `intel_rps_enable`). The mailbox registers are outside every
//! forcewake domain (i915 uses its `_fw` accessors on them).

use crate::generated::i915_reg as r;
use crate::io::{Clock, Mmio, wait_for_register};
use crate::rps::{FreqCaps, GEN9_FREQ_SCALER};

/// Why a mailbox request failed (`gen7_check_mailbox_status`'s errnos).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PcodeError {
    /// The mailbox was still busy with an earlier request (`-EAGAIN`).
    Busy,
    /// No answer within the timeout (`-ETIMEDOUT`), or the PCU's own timeout.
    Timeout,
    /// `GEN6_PCODE_ILLEGAL_CMD` or `GEN11_PCODE_ILLEGAL_SUBCOMMAND` (`-ENXIO`).
    Illegal,
    /// `GEN7_PCODE_ILLEGAL_DATA` (`-EINVAL`).
    IllegalData,
    /// `GEN11_PCODE_LOCKED` (`-EBUSY`).
    Locked,
    /// `GEN11_PCODE_REJECTED` (`-EACCES`).
    Rejected,
    /// `GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE` (`-EOVERFLOW`).
    OutOfRange,
}

/// `gen7_check_mailbox_status` (graphics version 12 is past 6). An unknown
/// code is success, as i915's `MISSING_CASE` returns 0.
pub const fn gen7_check_mailbox_status(mbox: u32) -> Result<(), PcodeError> {
    match mbox & r::GEN6_PCODE_ERROR_MASK {
        r::GEN7_PCODE_TIMEOUT => Err(PcodeError::Timeout),
        r::GEN6_PCODE_ILLEGAL_CMD | r::GEN11_PCODE_ILLEGAL_SUBCOMMAND => Err(PcodeError::Illegal),
        r::GEN7_PCODE_ILLEGAL_DATA => Err(PcodeError::IllegalData),
        r::GEN11_PCODE_LOCKED => Err(PcodeError::Locked),
        r::GEN11_PCODE_REJECTED => Err(PcodeError::Rejected),
        r::GEN7_PCODE_MIN_FREQ_TABLE_GT_RATIO_OUT_OF_RANGE => Err(PcodeError::OutOfRange),
        _ => Ok(()),
    }
}

/// `__snb_pcode_rw`: `val` and `val1` to the data registers, the command
/// with `GEN6_PCODE_READY`, then the wait for the PCU to clear it within
/// `timeout_us` (i915's fast and slow timeouts together); for a read, the
/// data registers read back. Returns `(DATA, DATA1)` for a read, the values
/// written for a write.
///
/// # Errors
///
/// [`PcodeError`].
pub fn snb_pcode_rw<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    mbox: u32,
    val: u32,
    val1: u32,
    timeout_us: u64,
    is_read: bool,
) -> Result<(u32, u32), PcodeError> {
    let mailbox = r::GEN6_PCODE_MAILBOX.offset();
    if mmio.read32(mailbox) & r::GEN6_PCODE_READY != 0 {
        return Err(PcodeError::Busy);
    }
    mmio.write32(r::GEN6_PCODE_DATA.offset(), val);
    mmio.write32(r::GEN6_PCODE_DATA1.offset(), val1);
    mmio.write32(mailbox, r::GEN6_PCODE_READY | mbox);
    let status = wait_for_register(mmio, clock, mailbox, r::GEN6_PCODE_READY, 0, timeout_us)
        .map_err(|_| PcodeError::Timeout)?;
    let out = if is_read {
        (
            mmio.read32(r::GEN6_PCODE_DATA.offset()),
            mmio.read32(r::GEN6_PCODE_DATA1.offset()),
        )
    } else {
        (val, val1)
    };
    gen7_check_mailbox_status(status).map(|()| out)
}

/// `snb_pcode_write`'s timeouts: 250 us fast, 1 ms slow.
pub const PCODE_WRITE_TIMEOUT_US: u64 = 250 + 1_000;
/// `snb_pcode_read`'s: 500 us fast, 20 ms slow.
pub const PCODE_READ_TIMEOUT_US: u64 = 500 + 20_000;

/// The GT frequencies the ring table has rows for, in 50 MHz units
/// (`intel_rps_get_min_raw_freq` and `_max_raw_freq` on Gen9+: the caps
/// divided by `GEN9_FREQ_SCALER`), lowest first.
#[must_use]
pub const fn ring_table_rows(caps: FreqCaps) -> (u32, u32) {
    (
        caps.min_freq / GEN9_FREQ_SCALER,
        caps.rp0_freq / GEN9_FREQ_SCALER,
    )
}

/// `calc_ia_freq` and the request `gen6_update_ring_freq` writes for GT
/// frequency `gpu_freq` (50 MHz units) on Gen9+: "`ring_freq` = 2 * GT.
/// `ring_freq` is in 100MHz units", no IA frequency.
#[must_use]
pub const fn ring_freq_request(gpu_freq: u32) -> u32 {
    let ring_freq = gpu_freq;
    let ia_freq = 0;
    (ia_freq << r::GEN6_PCODE_FREQ_IA_RATIO_SHIFT)
        | (ring_freq << r::GEN6_PCODE_FREQ_RING_RATIO_SHIFT)
        | gpu_freq
}

/// `gen6_update_ring_freq` (`intel_llc_enable`) for an LLC part: one
/// `GEN6_PCODE_WRITE_MIN_FREQ_TABLE` request per GT frequency, RP0 down to
/// `RPn`. i915 ignores each request's error (`snb_pcode_write` only logs it);
/// so does this, and returns how many were refused and the first refusal.
pub fn gen6_update_ring_freq<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    caps: FreqCaps,
) -> (u32, Option<PcodeError>) {
    let (min, max) = ring_table_rows(caps);
    let mut refused = 0;
    let mut first = None;
    if max <= min {
        return (0, None);
    }
    for gpu_freq in (min..=max).rev() {
        if let Err(error) = snb_pcode_rw(
            mmio,
            clock,
            r::GEN6_PCODE_WRITE_MIN_FREQ_TABLE,
            ring_freq_request(gpu_freq),
            0,
            PCODE_WRITE_TIMEOUT_US,
            false,
        ) {
            refused += 1;
            first = first.or(Some(error));
        }
    }
    (refused, first)
}

/// One row of the table as `i915_ring_freq_table` reads it back: the GT
/// frequency asked (50 MHz units) and the effective IA and ring frequencies
/// the PCU answers, in MHz.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RingRow {
    /// The GT frequency, in 50 MHz units.
    pub gpu_freq: u32,
    /// The effective CPU frequency, MHz (`(ia_freq & 0xff) * 100`).
    pub ia_mhz: u32,
    /// The effective ring frequency, MHz (`((ia_freq >> 8) & 0xff) * 100`).
    pub ring_mhz: u32,
}

/// `i915_ring_freq_table`'s read of the row for `gpu_freq` (50 MHz units).
///
/// # Errors
///
/// [`PcodeError`].
pub fn read_ring_row<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    gpu_freq: u32,
) -> Result<RingRow, PcodeError> {
    let (ia_freq, _) = snb_pcode_rw(
        mmio,
        clock,
        r::GEN6_PCODE_READ_MIN_FREQ_TABLE,
        gpu_freq,
        0,
        PCODE_READ_TIMEOUT_US,
        true,
    )?;
    Ok(RingRow {
        gpu_freq,
        ia_mhz: (ia_freq & 0xff) * 100,
        ring_mhz: ((ia_freq >> 8) & 0xff) * 100,
    })
}
