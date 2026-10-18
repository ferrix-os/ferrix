//! The Intel Xe-LP GPU's driver process (the N150's Alder Lake-N): a ring-3
//! program over `ferrix-intel-xelp`, which translates Linux's i915
//! (`docs/INTEL-GPU.md` §3.6). This is M1, "the device answers":
//!
//! 1. START, as an engine's: the device alone. devmgr does not wait for
//!    it. It is started only under `ferrix.intelgpu`, without
//!    `ferrix.efifb`; otherwise the kernel has yielded the function and the
//!    first mapping is refused, which the driver says and stops on.
//! 2. BAR 0 mapped uncached, whole: the registers in its lower 8 MiB and
//!    the GGTT's entries in its upper 8 MiB (`gen6_gttadr_offset`).
//! 3. The configuration space read, never written: the GGTT's size
//!    (`GMCH_CTRL`) and stolen memory's base (`BSM`).
//! 4. Whether `GuC` has locked the `WOPCM` since the last reset
//!    (`uc_is_wopcm_locked`): an execlists driver cannot start then, and
//!    says so (M0 found it on every warm boot from the inspection Linux).
//! 5. Forcewake of the render and GT domains, as i915 takes it.
//! 6. The GT's identity: the slices, dual-subslices and EUs from the fuses,
//!    the engines the media fuses leave, the command streamer's timestamp
//!    frequency, and `RP0`, `RP1` and `RPn`. Each line can be compared with what
//!    i915 printed on the same box (M0).
//! 7. The firmware's lit plane and the GGTT entries behind it, read only,
//!    and how many of the GGTT's entries the firmware left present.
//!
//! Nothing is written to the GPU but the forcewake requests. After the
//! lines it stays: a driver that exited would have devmgr quiesce the
//! device. The exit status is the step that failed ([`Step`]).

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::ptr;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_intel_xelp::device_info::{self, DeviceInfo as Platform, engine};
use ferrix_intel_xelp::engine::ENGINES;
use ferrix_intel_xelp::generated::intel_gt_regs::GEN11_GT_VEBOX_VDBOX_DISABLE;
use ferrix_intel_xelp::io::{Clock, Mmio};
use ferrix_intel_xelp::{clock, gtt, plane, rps, sseu, uc, uncore};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{ApertureInfo, IoMappingSpec};
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::{Device, IoMapping};
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// BAR 0's size on Gen8+ (`gen6_gttmmadr_size`).
const GTTMMADR_BYTES: u64 = 16 << 20;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// The device is not one this driver drives.
    Identity = 2,
    /// BAR 0 could not be found or mapped: under `ferrix.efifb`, or without
    /// `ferrix.intelgpu`, the kernel yields the function.
    Registers = 3,
    /// A forcewake request was never acknowledged.
    Forcewake = 4,
    /// The idle port could not be made.
    Port = 5,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(never) => match never {},
        Err(step) => {
            say(format_args!("intel: stopped at {step:?}"));
            step as i32
        }
    }
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

/// A line on standard error, which a native process has open on the
/// console.
fn say(arguments: fmt::Arguments<'_>) {
    let mut line = Line::default();
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.as_bytes());
}

/// One console line.
struct Line {
    bytes: [u8; 256],
    len: usize,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            bytes: [0; 256],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BAR 0, time
// ---------------------------------------------------------------------------

/// BAR 0, mapped uncached.
struct Bar0 {
    _mapping: IoMapping<Kernel>,
    base: usize,
}

impl Bar0 {
    /// Where `offset` is, for an aligned four-byte access inside the BAR.
    fn at(&self, offset: u64) -> Option<usize> {
        (offset + 4 <= GTTMMADR_BYTES && offset.is_multiple_of(4))
            .then(|| self.base + offset as usize)
    }

    /// GGTT entry `index`: two 32-bit reads, low then high, as i915's
    /// `gen8_ggtt_read_entry` reads the 64-bit entry.
    fn ggtt_entry(&self, index: u64) -> Option<u64> {
        let offset = gtt::GTTADR_OFFSET + index * 8;
        let low = self.at(offset)?;
        let high = self.at(offset + 4)?;
        // SAFETY: both inside BAR 0, which the kernel mapped for this process
        // and `self` keeps, aligned; device memory, read volatile.
        let low = unsafe { device_read(low as *const u32) };
        // SAFETY: as above.
        let high = unsafe { device_read(high as *const u32) };
        Some(u64::from(low) | u64::from(high) << 32)
    }
}

impl Mmio for Bar0 {
    fn read32(&mut self, offset: u32) -> u32 {
        let Some(address) = self
            .at(u64::from(offset))
            .filter(|_| u64::from(offset) < gtt::GTTADR_OFFSET)
        else {
            return 0;
        };
        // SAFETY: inside BAR 0's register half, mapped for this process and
        // kept by `self`, aligned.
        unsafe { device_read(address as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let Some(address) = self
            .at(u64::from(offset))
            .filter(|_| u64::from(offset) < gtt::GTTADR_OFFSET)
        else {
            return;
        };
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { device_write(address as *mut u32, value) }
    }
}

/// Read a register as one plain load, never inlined (KVM's MMIO emulation
/// refuses a load with writeback, `yt6801` says more).
///
/// # Safety
///
/// `at` is mapped device memory, aligned.
#[inline(never)]
unsafe fn device_read(at: *const u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { ptr::read_volatile(at) }
}

/// Write a register as one plain store: see [`device_read`].
///
/// # Safety
///
/// `at` is mapped, writable device memory, aligned.
#[inline(never)]
unsafe fn device_write(at: *mut u32, value: u32) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}

/// Time: the monotonic clock, and sleeps on a port nothing is bound to.
struct Time {
    idle: Port<Kernel>,
}

impl Clock for Time {
    fn now_us(&mut self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0) / 1_000
    }

    fn delay_us(&mut self, us: u32) {
        let until = ferrix_rt::linux::monotonic_nanos()
            .unwrap_or(0)
            .saturating_add(u64::from(us) * 1_000);
        let _ = self.idle.wait(Deadline::At(until));
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn started(boot: &Channel<Kernel>) -> Result<Device<Kernel>, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 1 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(_)) => Ok(device),
        _ => Err(Step::Start),
    }
}

/// BAR 0's aperture, whole.
fn bar0(device: &Device<Kernel>, apertures: u32) -> Option<ApertureInfo> {
    (0..apertures as usize)
        .filter_map(|index| device.aperture(index).ok())
        .find(|aperture| {
            aperture.bar == 0 && aperture.offset == 0 && aperture.len >= GTTMMADR_BYTES
        })
}

fn run(boot: &Channel<Kernel>) -> Result<core::convert::Infallible, Step> {
    let device = started(boot)?;
    let info = device.info().map_err(|_| Step::Identity)?;
    let Some(platform) = device_info::lookup(info.vendor_id, info.device_id) else {
        say(format_args!(
            "intel: {:04x}:{:04x} is not a device this driver drives",
            info.vendor_id, info.device_id
        ));
        return Err(Step::Identity);
    };
    say(format_args!(
        "intel: {:#010x} {:04x}:{:04x} class {:06x}, ADL-N (adl_p_info, subplatform N), apertures {} vectors {}",
        info.location, info.vendor_id, info.device_id, info.class, info.apertures, info.vectors
    ));
    for index in 0..info.apertures as usize {
        if let Ok(aperture) = device.aperture(index) {
            say(format_args!(
                "intel: aperture {index}: bar {} at {:#x}, {:#x} bytes, flags {:#x}",
                aperture.bar, aperture.phys, aperture.len, aperture.flags
            ));
        }
    }

    let Some(aperture) = bar0(&device, info.apertures) else {
        say(format_args!("intel: no whole 16 MiB BAR 0 to map"));
        return Err(Step::Registers);
    };
    let mapping = device
        .io_mapping(IoMappingSpec {
            phys: aperture.phys,
            len: GTTMMADR_BYTES,
        })
        .map_err(|error| {
            say(format_args!(
                "intel: BAR 0 refused ({error:?}): the kernel lends 00:02.0 only under ferrix.intelgpu without ferrix.efifb"
            ));
            Step::Registers
        })?;
    let base = mapping.map(None).map_err(|_| Step::Registers)?;
    let mut bar = Bar0 {
        _mapping: mapping,
        base,
    };
    let mut time = Time {
        idle: port::create(Kernel).map_err(|_| Step::Port)?,
    };

    say_config(&device);

    let locked = uc::uc_is_wopcm_locked(&mut bar);
    say(format_args!(
        "intel: WOPCM {}",
        if locked {
            "locked: GuC ran since the last reset; execlists need an FLR first (i915's __uc_check_hw)"
        } else {
            "unlocked: execlists may start"
        }
    ));

    let domains = uncore::fw_domains_init(platform.platform_engine_mask);
    let wanted = uncore::FORCEWAKE_RENDER | uncore::FORCEWAKE_GT;
    let started = time.now_us();
    if let Err(error) = uncore::fw_domains_get(&mut bar, &mut time, &domains, wanted) {
        say(format_args!("intel: forcewake failed: {error:?}"));
        return Err(Step::Forcewake);
    }
    say(format_args!(
        "intel: forcewake render and GT acknowledged in {} us",
        time.now_us().saturating_sub(started)
    ));

    say_gt(&mut bar, &platform);
    say_plane_and_ggtt(&mut bar, &device);

    uncore::fw_domains_put(&mut bar, &domains, wanted);
    say(format_args!("intel: M1: the device answers"));
    stay(&time)
}

/// The configuration-space words M1 needs, read only.
fn say_config(device: &Device<Kernel>) {
    let gmch = device
        .config_read(gtt::SNB_GMCH_CTRL, 2)
        .map_or(0, |value| value as u16);
    let bsm = device.config_read(gtt::INTEL_BSM, 4).unwrap_or(0);
    let gms = (gmch >> gtt::BDW_GMCH_GMS_SHIFT) & gtt::BDW_GMCH_GMS_MASK;
    say(format_args!(
        "intel: GMCH_CTRL {gmch:#06x}: GGTT {} MiB of entries for {} GiB; stolen base {:#x}, GMS {gms:#x}",
        gtt::gen8_get_total_gtt_size(gmch) >> 20,
        gtt::ggtt_total(gmch) >> 30,
        bsm & gtt::INTEL_BSM_MASK,
    ));
}

/// The GT's identity, under forcewake.
fn say_gt(bar: &mut Bar0, platform: &Platform) {
    let fuses = sseu::Fuses::read(bar);
    let info = sseu::gen12_sseu_info_init(fuses);
    say(format_args!(
        "intel: slices {:#x} (enable {:#x}), dual-subslices {:#x}, EUs {:#06x} per DSS, {} EUs",
        info.slice_mask, info.slice_enable, info.subslice_mask, info.eu_mask, info.eu_total
    ));
    let media = bar.read32(GEN11_GT_VEBOX_VDBOX_DISABLE.offset());
    let mask = device_info::engine_mask_apply_media_fuses(platform.platform_engine_mask, media);
    let mut line = Line::default();
    for e in ENGINES.iter().filter(|e| mask & engine::bit(e.id) != 0) {
        let _ = write!(line, " {}", e.name);
    }
    say(format_args!(
        "intel: engines (media fuse {media:#010x}):{}",
        core::str::from_utf8(line.as_bytes()).unwrap_or("?")
    ));
    let cs = clock::gen11_read_clock_frequency(bar);
    let caps = rps::gen6_rps_get_freq_caps(
        bar.read32(rps::GEN6_RP_STATE_CAP.offset()),
        bar.read32(rps::GEN10_FREQ_INFO_REC.offset()),
    );
    say(format_args!(
        "intel: CS timestamp {cs} Hz; RP0 {} MHz, RP1 {} MHz, RPn {} MHz",
        rps::intel_gpu_freq(caps.rp0_freq),
        rps::intel_gpu_freq(caps.rp1_freq),
        rps::intel_gpu_freq(caps.min_freq)
    ));
}

/// The lit plane, and the GGTT behind it, read only.
fn say_plane_and_ggtt(bar: &mut Bar0, device: &Device<Kernel>) {
    let gmch = device
        .config_read(gtt::SNB_GMCH_CTRL, 2)
        .map_or(0, |value| value as u16);
    let entries = gtt::ggtt_total(gmch) / gtt::GTT_PAGE_SIZE;
    for pipe in 0..plane::PIPES {
        let lit = plane::read_plane1(bar, pipe);
        if !lit.enabled() {
            continue;
        }
        let (width, height) = lit.dimensions();
        let first = u64::from(lit.ggtt_offset()) / gtt::GTT_PAGE_SIZE;
        let pte = bar.ggtt_entry(first).unwrap_or(0);
        say(format_args!(
            "intel: pipe {} plane 1 lit, {width}x{height}, stride {:#x}, at GGTT {:#x}: entry {pte:#018x} (page {:#x})",
            pipe,
            lit.stride,
            lit.ggtt_offset(),
            gtt::gen8_ggtt_pte_decode(pte).unwrap_or(0),
        ));
    }
    let mut present = 0_u64;
    let mut first_present = None;
    let mut last_present = 0;
    for index in 0..entries {
        let Some(pte) = bar.ggtt_entry(index) else {
            break;
        };
        if gtt::gen8_ggtt_pte_decode(pte).is_some() {
            present += 1;
            let _ = first_present.get_or_insert(index);
            last_present = index;
        }
    }
    say(format_args!(
        "intel: GGTT {entries} entries, {present} present (first {:#x}, last {last_present:#x}); none written",
        first_present.unwrap_or(0)
    ));
}

/// Stay, for the life of the machine.
fn stay(time: &Time) -> Result<core::convert::Infallible, Step> {
    loop {
        if time.idle.wait(Deadline::Never).is_err() {
            return Err(Step::Port);
        }
    }
}
