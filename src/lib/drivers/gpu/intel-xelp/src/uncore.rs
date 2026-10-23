// Translated from Linux v7.0, drivers/gpu/drm/i915/intel_uncore.c,
// intel_uncore.h and gt/intel_gt_regs.h:
// Copyright © 2013 Intel Corporation (intel_uncore.c)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Forcewake, as `intel_uncore.c` does it on Gen12.
//!
//! The GT powers its blocks down when idle. A register in a powered-down
//! block reads as garbage and drops writes, so before the driver touches
//! one it *wakes* the block's domain: it sets `FORCEWAKE_KERNEL` in the
//! domain's request register and waits for the bit in its ack register.
//! Which domain a register needs is a table by offset
//! (`__gen12_fw_ranges`). Some registers are *shadowed*: the hardware takes
//! writes to them without a wake (`gen12_shadowed_regs`).
//!
//! The tables are i915's own, generated from `i915/intel_uncore.c`
//! ([`crate::generated`]). The request sequence below is
//! `fw_domains_get_with_fallback`, which i915 uses for every Gen11 and later
//! part, its fallback toggle included.

use crate::generated::intel_uncore::{GEN12_FW_RANGES, GEN12_SHADOWED_REGS};
use crate::io::{Clock, Mmio, wait_for_register};
use crate::reg::{Reg, bit, masked_bit_disable, masked_bit_enable};

/// `enum forcewake_domains`: one bit per `enum forcewake_domain_id`.
pub type ForcewakeDomains = u32;

/// `FORCEWAKE_RENDER`, `BIT(FW_DOMAIN_ID_RENDER)`.
pub const FORCEWAKE_RENDER: ForcewakeDomains = 1 << 0;
/// `FORCEWAKE_GT`, `BIT(FW_DOMAIN_ID_GT)`: the GT, the blitter included.
pub const FORCEWAKE_GT: ForcewakeDomains = 1 << 1;
/// `FORCEWAKE_MEDIA`, `BIT(FW_DOMAIN_ID_MEDIA)`: Gen9's, unused on Gen11+.
pub const FORCEWAKE_MEDIA: ForcewakeDomains = 1 << 2;
/// `FORCEWAKE_MEDIA_VDBOX0`, `BIT(FW_DOMAIN_ID_MEDIA_VDBOX0)`.
pub const FORCEWAKE_MEDIA_VDBOX0: ForcewakeDomains = 1 << 3;
/// `FORCEWAKE_MEDIA_VDBOX2`, `BIT(FW_DOMAIN_ID_MEDIA_VDBOX2)`.
pub const FORCEWAKE_MEDIA_VDBOX2: ForcewakeDomains = 1 << 5;
/// `FORCEWAKE_MEDIA_VEBOX0`, `BIT(FW_DOMAIN_ID_MEDIA_VEBOX0)`.
pub const FORCEWAKE_MEDIA_VEBOX0: ForcewakeDomains = 1 << 11;

/// `enum forcewake_domain_id`'s count, `FW_DOMAIN_ID_COUNT`.
pub const FW_DOMAIN_ID_COUNT: u32 = 16;

/// `struct intel_forcewake_range`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FwRange {
    /// First byte offset of the range.
    pub start: u32,
    /// Last byte offset of the range, inclusive.
    pub end: u32,
    /// The domains a register in it needs; 0 for none.
    pub domains: ForcewakeDomains,
}

/// `struct i915_mmio_range`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MmioRange {
    /// First byte offset.
    pub start: u32,
    /// Last byte offset, inclusive.
    pub end: u32,
}

/// `FORCEWAKE_RENDER_GEN9`.
pub const FORCEWAKE_RENDER_GEN9: Reg = Reg(0xa278);
/// `FORCEWAKE_ACK_RENDER_GEN9`.
pub const FORCEWAKE_ACK_RENDER_GEN9: Reg = Reg(0xd84);
/// `FORCEWAKE_GT_GEN9`.
pub const FORCEWAKE_GT_GEN9: Reg = Reg(0xa188);
/// `FORCEWAKE_ACK_GT_GEN9`.
pub const FORCEWAKE_ACK_GT_GEN9: Reg = Reg(0x0013_0044);

/// `FORCEWAKE_MEDIA_VDBOX_GEN11(n)`.
#[must_use]
pub const fn forcewake_media_vdbox_gen11(n: u32) -> Reg {
    Reg(0xa540 + n * 4)
}

/// `FORCEWAKE_ACK_MEDIA_VDBOX_GEN11(n)`.
#[must_use]
pub const fn forcewake_ack_media_vdbox_gen11(n: u32) -> Reg {
    Reg(0xd50 + n * 4)
}

/// `FORCEWAKE_MEDIA_VEBOX_GEN11(n)`.
#[must_use]
pub const fn forcewake_media_vebox_gen11(n: u32) -> Reg {
    Reg(0xa560 + n * 4)
}

/// `FORCEWAKE_ACK_MEDIA_VEBOX_GEN11(n)`.
#[must_use]
pub const fn forcewake_ack_media_vebox_gen11(n: u32) -> Reg {
    Reg(0xd70 + n * 4)
}

/// `FORCEWAKE_KERNEL`: the driver's request bit, and its ack.
pub const FORCEWAKE_KERNEL: u32 = bit(0);
/// `FORCEWAKE_KERNEL_FALLBACK`: the bit toggled to kick a lost ack.
pub const FORCEWAKE_KERNEL_FALLBACK: u32 = bit(15);

/// `FORCEWAKE_ACK_TIMEOUT_MS`.
pub const FORCEWAKE_ACK_TIMEOUT_MS: u64 = 50;

/// `NEEDS_FORCE_WAKE(reg)`: offsets outside these never need a wake.
#[must_use]
pub const fn needs_force_wake(offset: u32) -> bool {
    offset < 0x40000 || offset >= 0x0011_6000
}

/// `find_fw_domain`: the domains the table gives `offset`, by binary search
/// (`BSEARCH` with `fw_range_cmp`); 0 for an offset no range holds.
#[must_use]
pub fn find_fw_domain(offset: u32) -> ForcewakeDomains {
    GEN12_FW_RANGES
        .binary_search_by(|range| range_cmp(range.start, range.end, offset))
        .ok()
        .and_then(|index| GEN12_FW_RANGES.get(index))
        .map_or(0, |range| range.domains)
}

/// `is_shadowed`.
#[must_use]
pub fn is_shadowed(offset: u32) -> bool {
    GEN12_SHADOWED_REGS
        .binary_search_by(|range| range_cmp(range.start, range.end, offset))
        .is_ok()
}

/// `fw_range_cmp` and `mmio_range_cmp`, turned round for `binary_search_by`,
/// which asks how the entry compares with the key.
fn range_cmp(start: u32, end: u32, key: u32) -> core::cmp::Ordering {
    if end < key {
        core::cmp::Ordering::Less
    } else if start > key {
        core::cmp::Ordering::Greater
    } else {
        core::cmp::Ordering::Equal
    }
}

/// `__fwtable_reg_read_fw_domains`.
#[must_use]
pub fn read_fw_domains(reg: Reg) -> ForcewakeDomains {
    if needs_force_wake(reg.offset()) {
        find_fw_domain(reg.offset())
    } else {
        0
    }
}

/// `__fwtable_reg_write_fw_domains`.
#[must_use]
pub fn write_fw_domains(reg: Reg) -> ForcewakeDomains {
    let offset = reg.offset();
    if needs_force_wake(offset) && !is_shadowed(offset) {
        find_fw_domain(offset)
    } else {
        0
    }
}

/// `struct intel_uncore_forcewake_domain`'s registers: where a domain is
/// requested and where its ack is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FwDomain {
    /// `d->mask`, one of the `FORCEWAKE_*` bits.
    pub mask: ForcewakeDomains,
    /// `d->reg_set`.
    pub reg_set: Reg,
    /// `d->reg_ack`.
    pub reg_ack: Reg,
}

/// `intel_uncore_fw_domains_init`'s Gen11+ branch, for the engines of
/// `engine_mask` (`platform_engine_mask` less what the fuses removed): GT
/// always, render when there is a render engine, and a domain per video
/// decode and enhancement engine.
#[must_use]
pub fn fw_domains_init(engine_mask: u32) -> FwDomains {
    use crate::device_info::engine;
    let mut domains = FwDomains {
        list: [None; FW_DOMAIN_ID_COUNT as usize],
    };
    domains.put(1, FORCEWAKE_GT_GEN9, FORCEWAKE_ACK_GT_GEN9);
    if engine_mask & engine::bit(engine::RCS0) != 0 {
        domains.put(0, FORCEWAKE_RENDER_GEN9, FORCEWAKE_ACK_RENDER_GEN9);
    }
    for i in 0..8 {
        if engine_mask & engine::bit(engine::VCS0 + i) != 0 {
            domains.put(
                3 + i,
                forcewake_media_vdbox_gen11(i),
                forcewake_ack_media_vdbox_gen11(i),
            );
        }
    }
    for i in 0..4 {
        if engine_mask & engine::bit(engine::VECS0 + i) != 0 {
            domains.put(
                11 + i,
                forcewake_media_vebox_gen11(i),
                forcewake_ack_media_vebox_gen11(i),
            );
        }
    }
    domains
}

/// The domains a device has, by `enum forcewake_domain_id`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FwDomains {
    list: [Option<FwDomain>; FW_DOMAIN_ID_COUNT as usize],
}

impl FwDomains {
    fn put(&mut self, id: u32, reg_set: Reg, reg_ack: Reg) {
        if let Some(slot) = self.list.get_mut(id as usize) {
            *slot = Some(FwDomain {
                mask: 1 << id,
                reg_set,
                reg_ack,
            });
        }
    }

    /// `uncore->fw_domains`: the mask of every domain present.
    #[must_use]
    pub fn mask(&self) -> ForcewakeDomains {
        self.list
            .iter()
            .flatten()
            .fold(0, |mask, domain| mask | domain.mask)
    }

    /// `for_each_fw_domain_masked`: the present domains of `mask`, in id order.
    pub fn masked(&self, mask: ForcewakeDomains) -> impl Iterator<Item = &FwDomain> {
        self.list
            .iter()
            .flatten()
            .filter(move |domain| domain.mask & mask != 0)
    }
}

/// Why a forcewake request failed: the domain, and what its ack read last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForcewakeError {
    /// `fw_domain_wait_ack_clear`: a previous request never cleared.
    AckNotCleared {
        /// The domain's bit.
        domain: ForcewakeDomains,
        /// Its ack register's last value.
        ack: u32,
    },
    /// `fw_domain_wait_ack_set`: the request was never acknowledged.
    AckNotSet {
        /// The domain's bit.
        domain: ForcewakeDomains,
        /// Its ack register's last value.
        ack: u32,
    },
    /// The ack read all ones: `"MMIO unreliable (forcewake register returns
    /// 0xFFFFFFFF)!"`, after which i915 wedges the GT.
    MmioUnreliable {
        /// The domain's bit.
        domain: ForcewakeDomains,
    },
}

/// `fw_domains_get_with_fallback`: request every domain of `mask`, then
/// wait for each ack. On a lost ack, `fw_domain_wait_ack_with_fallback`
/// toggles the fallback bit up to ten times (`WaRsForcewakeAddDelayForAck`).
pub fn fw_domains_get<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domains: &FwDomains,
    mask: ForcewakeDomains,
) -> Result<(), ForcewakeError> {
    for domain in domains.masked(mask) {
        fw_domain_wait_ack_clear_fallback(mmio, clock, domain)?;
        // fw_domain_get
        mmio.write32(domain.reg_set.offset(), masked_bit_enable(FORCEWAKE_KERNEL));
    }
    for domain in domains.masked(mask) {
        fw_domain_wait_ack_set_fallback(mmio, clock, domain)?;
    }
    Ok(())
}

/// `fw_domains_put`.
pub fn fw_domains_put<M: Mmio + ?Sized>(mmio: &mut M, domains: &FwDomains, mask: ForcewakeDomains) {
    for domain in domains.masked(mask) {
        mmio.write32(
            domain.reg_set.offset(),
            masked_bit_disable(FORCEWAKE_KERNEL),
        );
    }
}

/// `fw_domains_reset` with `fw_domain_reset`: clear every request bit but
/// bit 12 on Gen12 (`WaRsClearFWBitsAtReset`), without waiting for acks.
pub fn fw_domains_reset<M: Mmio + ?Sized>(
    mmio: &mut M,
    domains: &FwDomains,
    mask: ForcewakeDomains,
) {
    for domain in domains.masked(mask) {
        mmio.write32(domain.reg_set.offset(), masked_bit_disable(0xefff));
    }
}

/// `forcewake_early_sanitize`'s `intel_uncore_forcewake_reset`, then the
/// wait the first `fw_domains_get` would make: every present domain's
/// request bits are cleared ([`fw_domains_reset`]), whoever set them, and
/// each ack must then read clear, with the fallback kick if one is lost.
/// i915 does this at every load, before its first forcewake.
///
/// A driver that died holding forcewake leaves the request and its ack set:
/// the hardware keeps them, and nothing in the kernel's quiesce touches
/// them. Without this, the next driver's first [`fw_domains_get`] waits for
/// that ack to clear and fails with [`ForcewakeError::AckNotCleared`] (the
/// N150, 2026-10-23, after `kill -9` of `intel`).
///
/// Returns the domains whose kernel ack was found set, so the caller can say
/// it released a predecessor's forcewake. An ack that never clears is the
/// error, and the caller must stop: the GT may still be held by something
/// this driver does not know.
pub fn fw_domains_sanitize<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domains: &FwDomains,
) -> Result<ForcewakeDomains, ForcewakeError> {
    let mut held = 0;
    for domain in domains.masked(domains.mask()) {
        let ack = mmio.read32(domain.reg_ack.offset());
        if ack == u32::MAX {
            return Err(ForcewakeError::MmioUnreliable {
                domain: domain.mask,
            });
        }
        if ack & FORCEWAKE_KERNEL != 0 {
            held |= domain.mask;
        }
    }
    fw_domains_reset(mmio, domains, domains.mask());
    for domain in domains.masked(domains.mask()) {
        fw_domain_wait_ack_clear_fallback(mmio, clock, domain)?;
    }
    Ok(held)
}

/// `__wait_for_ack`.
fn wait_ack<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domain: &FwDomain,
    ack: u32,
    value: u32,
) -> Result<u32, u32> {
    wait_for_register(
        mmio,
        clock,
        domain.reg_ack.offset(),
        ack,
        value,
        FORCEWAKE_ACK_TIMEOUT_MS * 1000,
    )
}

/// `fw_domain_wait_ack_clear_fallback`, ending in
/// `fw_domain_wait_ack_clear`'s verdict.
fn fw_domain_wait_ack_clear_fallback<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domain: &FwDomain,
) -> Result<(), ForcewakeError> {
    if wait_ack(mmio, clock, domain, FORCEWAKE_KERNEL, 0).is_ok() {
        return Ok(());
    }
    if wait_ack_with_fallback(mmio, clock, domain, false) {
        return Ok(());
    }
    match wait_ack(mmio, clock, domain, FORCEWAKE_KERNEL, 0) {
        Ok(_) => Ok(()),
        Err(u32::MAX) => Err(ForcewakeError::MmioUnreliable {
            domain: domain.mask,
        }),
        Err(ack) => Err(ForcewakeError::AckNotCleared {
            domain: domain.mask,
            ack,
        }),
    }
}

/// `fw_domain_wait_ack_set_fallback`, ending in `fw_domain_wait_ack_set`.
fn fw_domain_wait_ack_set_fallback<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domain: &FwDomain,
) -> Result<(), ForcewakeError> {
    if wait_ack(mmio, clock, domain, FORCEWAKE_KERNEL, FORCEWAKE_KERNEL).is_ok() {
        return Ok(());
    }
    if wait_ack_with_fallback(mmio, clock, domain, true) {
        return Ok(());
    }
    wait_ack(mmio, clock, domain, FORCEWAKE_KERNEL, FORCEWAKE_KERNEL)
        .map(|_| ())
        .map_err(|ack| ForcewakeError::AckNotSet {
            domain: domain.mask,
            ack,
        })
}

/// `fw_domain_wait_ack_with_fallback`: toggle the fallback bit, with a
/// pause growing by 10 us a pass, until the kernel bit's ack reads `set`, at
/// most ten passes. Whether it did.
fn wait_ack_with_fallback<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    domain: &FwDomain,
    set: bool,
) -> bool {
    let value = if set { FORCEWAKE_KERNEL } else { 0 };
    let mut pass: u32 = 1;
    loop {
        let _cleared = wait_ack(mmio, clock, domain, FORCEWAKE_KERNEL_FALLBACK, 0);
        mmio.write32(
            domain.reg_set.offset(),
            masked_bit_enable(FORCEWAKE_KERNEL_FALLBACK),
        );
        // "Give gt some time to relax before the polling frenzy"
        clock.delay_us(10 * pass);
        let _set = wait_ack(
            mmio,
            clock,
            domain,
            FORCEWAKE_KERNEL_FALLBACK,
            FORCEWAKE_KERNEL_FALLBACK,
        );
        let detected = mmio.read32(domain.reg_ack.offset()) & FORCEWAKE_KERNEL == value;
        mmio.write32(
            domain.reg_set.offset(),
            masked_bit_disable(FORCEWAKE_KERNEL_FALLBACK),
        );
        if detected {
            return true;
        }
        if pass >= 10 {
            return false;
        }
        pass += 1;
    }
}

/// How long a domain woken for an access stays awake after its last use:
/// `fw_domain_arm_timer`'s `NSEC_PER_MSEC`, re-armed while the domain is
/// used again before it fires.
pub const FORCEWAKE_RELEASE_NS: u64 = 1_000_000;

/// i915's forcewake per access (`__force_wake_auto` and
/// `intel_uncore_fw_release_timer`): before each register access the
/// domains its offset needs (`__fwtable_reg_read_fw_domains`, or the write
/// one, which leaves shadowed registers out) are woken if they are not
/// awake; a millisecond after they were last used, they are released, so
/// the GT can enter RC6 while the driver does not touch it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AutoForcewake {
    domains: FwDomains,
    awake: ForcewakeDomains,
    /// `domain->active`: used since the release timer was armed.
    used: bool,
    due_ns: Option<u64>,
    /// Wakes that failed (i915 says each and goes on), and the last one's
    /// error.
    pub failures: u32,
    /// See [`AutoForcewake::failures`].
    pub last_error: Option<ForcewakeError>,
}

impl AutoForcewake {
    /// Per-access forcewake over `domains`, with `held` awake already (the
    /// bring-up's hold, handed over at `now_ns` and released like any other).
    #[must_use]
    pub fn new(domains: FwDomains, held: ForcewakeDomains, now_ns: u64) -> Self {
        let awake = held & domains.mask();
        AutoForcewake {
            domains,
            awake,
            used: false,
            due_ns: (awake != 0).then_some(now_ns.saturating_add(FORCEWAKE_RELEASE_NS)),
            failures: 0,
            last_error: None,
        }
    }

    /// The domains awake now.
    #[must_use]
    pub const fn awake(&self) -> ForcewakeDomains {
        self.awake
    }

    /// When [`AutoForcewake::release`] must next run, while a domain is
    /// awake.
    #[must_use]
    pub const fn due(&self) -> Option<u64> {
        self.due_ns
    }

    /// Before an access to `offset` (a write if `write`): wake what it
    /// needs.
    pub fn access<M: Mmio + ?Sized, C: Clock + ?Sized>(
        &mut self,
        mmio: &mut M,
        clock: &mut C,
        offset: u32,
        write: bool,
    ) {
        let need = if write {
            write_fw_domains(Reg(offset))
        } else {
            read_fw_domains(Reg(offset))
        } & self.domains.mask();
        if need == 0 {
            return;
        }
        self.used = true;
        let missing = need & !self.awake;
        if missing == 0 {
            return;
        }
        if let Err(error) = fw_domains_get(mmio, clock, &self.domains, missing) {
            self.failures = self.failures.saturating_add(1);
            self.last_error = Some(error);
        }
        // Requested whatever the ack said, as fw_domains_get_with_fallback
        // leaves it: released by the timer like the rest.
        self.awake |= missing;
        if self.due_ns.is_none() {
            let now = clock.now_us().saturating_mul(1_000);
            self.due_ns = Some(now.saturating_add(FORCEWAKE_RELEASE_NS));
        }
    }

    /// `intel_uncore_fw_release_timer` at `now_ns`: when due, the domains
    /// are released unless used since the timer was armed, which re-arms it.
    pub fn release<M: Mmio + ?Sized>(&mut self, mmio: &mut M, now_ns: u64) {
        let Some(due) = self.due_ns else {
            return;
        };
        if now_ns < due {
            return;
        }
        if self.used {
            self.used = false;
            self.due_ns = Some(now_ns.saturating_add(FORCEWAKE_RELEASE_NS));
            return;
        }
        fw_domains_put(mmio, &self.domains, self.awake);
        self.awake = 0;
        self.due_ns = None;
    }
}
