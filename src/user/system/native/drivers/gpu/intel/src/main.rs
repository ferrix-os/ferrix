//! The Intel Xe-LP GPU's driver process (the N150's Alder Lake-N): a ring-3
//! program over `ferrix-intel-xelp`, which translates Linux's i915
//! (`docs/INTEL-GPU.md` §3.6). M1, "the device answers", then M2, "a GPU
//! command executes":
//!
//! 1. START, as an engine's: the device alone. devmgr does not wait for
//!    it. It is started only under `ferrix.intelgpu`, without
//!    `ferrix.efifb`; otherwise the kernel has yielded the function and the
//!    first mapping is refused, which the driver says and stops on.
//! 2. BAR 0 mapped uncached, whole: the registers in its lower 8 MiB and
//!    the GGTT's entries in its upper 8 MiB (`gen6_gttadr_offset`).
//! 3. The configuration space read, never written: the GGTT's size
//!    (`GMCH_CTRL`) and stolen memory's base (`BSM`).
//! 4. Forcewake of the render and GT domains, as i915 takes it.
//! 5. Whether `GuC` has locked the `WOPCM` since the last reset
//!    (`uc_is_wopcm_locked`): an execlists driver cannot start then, and
//!    says so (M0 found it on every warm boot from the inspection Linux).
//!    Read under forcewake: the registers are the GT domain's, and i915
//!    reads them in `intel_gt_init_hw`, under every domain's.
//! 6. The GT's identity: the slices, dual-subslices and EUs from the fuses,
//!    the engines the media fuses leave, the command streamer's timestamp
//!    frequency, and `RP0`, `RP1` and `RPn`. Each line can be compared with what
//!    i915 printed on the same box (M0).
//! 7. The firmware's lit plane on pipe A and the GGTT entry behind it, read
//!    only, and the GGTT entries the firmware left present, read within a
//!    time budget.
//!
//! 8. M2 ([`m2`]), unless the `WOPCM` is locked: the PAT, the workarounds and
//!    the MOCS, RCS0 and BCS0 under execlists, a PPGTT, the default context
//!    images, and three commands run and checked. It writes the GPU's
//!    registers and only the GGTT entries of its own window.
//!
//! M1 writes nothing to the GPU but the forcewake requests. After the
//! lines it stays, and once it has the device it stays after a failure too,
//! saying the step that failed ([`Step`]): a driver that exited would have
//! devmgr quiesce the device, which turns its bus mastering off under the
//! plane the firmware lit. Only a failure before it has the device is an
//! exit status.
//!
//! Every step is said before it is taken, and the kernel command line can
//! stop the driver before any of them or have it say every register access
//! first ([`steps`]): booting under `ferrix.intelgpu` hung the N150 box with
//! nothing kept, and these find where.

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

mod m2;
mod steps;

use steps::{At, Steps};

ferrix_rt::entry!(main);

/// BAR 0's size on Gen8+ (`gen6_gttmmadr_size`).
const GTTMMADR_BYTES: u64 = 16 << 20;

/// Where a run stopped: said, and the exit status only before the driver
/// has the device.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub(crate) enum Step {
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
    /// M2: pages could not be pinned, mapped or bound, or a table or ring
    /// had no room.
    Memory = 6,
    /// M2: the submit queue refused a submission.
    Submit = 7,
    /// M2: a request did not complete in time.
    Hung = 8,
    /// M2: a command ran but its result was not what it wrote.
    Wrong = 9,
    /// M2: a request on a context banned after a hang (refused, as i915
    /// refuses it with `EIO`).
    Banned = 10,
    /// Stopped before a step, as `ferrix.intelgpu.stop` asked. Never an
    /// exit status: the driver stays.
    Stopped = 11,
}

impl From<steps::Stopped> for Step {
    fn from(_: steps::Stopped) -> Self {
        Step::Stopped
    }
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
pub(crate) fn say(arguments: fmt::Arguments<'_>) {
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
pub(crate) struct Bar0 {
    _mapping: IoMapping<Kernel>,
    base: usize,
    /// Say every register access, and every GGTT entry written, before it
    /// is made (`ferrix.intelgpu.trace`). The GGTT walk's reads are not
    /// said one by one: its step is.
    trace: bool,
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

    /// Write GGTT entry `index` as one 64-bit store, as `gen8_set_pte`'s
    /// `writeq` does: the range is uncached, and the GPU must never see
    /// half an entry.
    fn set_ggtt_entry(&mut self, index: u64, pte: u64) {
        let offset = gtt::GTTADR_OFFSET + index * 8;
        if offset + 8 > GTTMMADR_BYTES {
            return;
        }
        let address = self.base + offset as usize;
        if self.trace {
            say(format_args!(
                "intel: about to write GGTT entry {index:#x} <- {pte:#018x}"
            ));
        }
        // SAFETY: inside BAR 0's GGTT half, which the kernel mapped for this
        // process and `self` keeps, eight-byte aligned; device memory.
        unsafe { device_write64(address as *mut u64, pte) }
    }
}

/// Write a 64-bit GGTT entry as one plain store: see [`device_read`].
///
/// # Safety
///
/// `at` is mapped, writable device memory, eight-byte aligned.
#[inline(never)]
unsafe fn device_write64(at: *mut u64, value: u64) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}

impl Mmio for Bar0 {
    fn read32(&mut self, offset: u32) -> u32 {
        let Some(address) = self
            .at(u64::from(offset))
            .filter(|_| u64::from(offset) < gtt::GTTADR_OFFSET)
        else {
            return 0;
        };
        if self.trace {
            say(format_args!("intel: about to read {offset:#x}"));
        }
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
        if self.trace {
            say(format_args!(
                "intel: about to write {offset:#x} <- {value:#010x}"
            ));
        }
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
pub(crate) struct Time {
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
    let steps = Steps::from_command_line();
    let mut time = Time {
        idle: port::create(Kernel).map_err(|_| Step::Port)?,
    };
    match m1(&device, info.apertures, &platform, &steps, &mut time) {
        Err(Step::Stopped) => stay(&time),
        Err(step) => {
            // Exiting would have devmgr quiesce the function, and a quiesce
            // turns its bus mastering off (`device_quiesce`), under the
            // plane the firmware lit and the display engine still scans
            // out of memory. i915 never does that with a lit plane: it
            // turns the display off before `pci_disable_device`.
            say(format_args!(
                "intel: stopped at {step:?}; the driver stays, so 00:02.0's bus mastering stays on under the lit screen"
            ));
            stay(&time)
        }
        Ok(()) => {
            say(format_args!(
                "intel: the driver now waits and touches nothing"
            ));
            stay(&time)
        }
    }
}

/// M1, then M2 unless the `WOPCM` is locked; each step said first.
fn m1(
    device: &Device<Kernel>,
    apertures: u32,
    platform: &Platform,
    steps: &Steps,
    time: &mut Time,
) -> Result<(), Step> {
    steps.before(
        At::MapBar0,
        format_args!("map BAR 0, 16 MiB uncached (the kernel's io_mapping; no device access)"),
    )?;
    let Some(aperture) = bar0(device, apertures) else {
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
        trace: steps.trace,
    };

    let run_started = time.now_us();
    steps.before(
        At::Config,
        format_args!("read configuration space GMCH_CTRL 0x50, BSM 0x5c (through the kernel)"),
    )?;
    say_config(device);

    steps.before(
        At::Forcewake,
        format_args!(
            "forcewake render and GT: acks 0xd84 0x130044 clear, write 0xa278 0xa188, acks set"
        ),
    )?;
    let domains = uncore::fw_domains_init(platform.platform_engine_mask);
    let wanted = uncore::FORCEWAKE_RENDER | uncore::FORCEWAKE_GT;
    let started = time.now_us();
    if let Err(error) = uncore::fw_domains_get(&mut bar, time, &domains, wanted) {
        say(format_args!("intel: forcewake failed: {error:?}"));
        return Err(Step::Forcewake);
    }
    say(format_args!(
        "intel: forcewake render and GT acknowledged in {} us",
        time.now_us().saturating_sub(started)
    ));

    steps.before(
        At::Wopcm,
        format_args!("read GUC_WOPCM_SIZE 0xc050, DMA_GUC_WOPCM_OFFSET 0xc340 (GT domain)"),
    )?;
    let locked = uc::uc_is_wopcm_locked(&mut bar);
    say(format_args!(
        "intel: WOPCM {}",
        if locked {
            "locked: GuC ran since the last reset; execlists need an FLR first (i915's __uc_check_hw)"
        } else {
            "unlocked: execlists may start"
        }
    ));

    let (subslice_mask, engine_mask) = say_gt(&mut bar, platform, steps)?;
    let (scanned, last_present, total, plane) = say_plane_and_ggtt(&mut bar, device, time, steps)?;
    say(format_args!(
        "intel: M1: the device answers ({} ms)",
        time.now_us().saturating_sub(run_started) / 1_000
    ));

    if locked {
        say(format_args!("intel: M2 not run: the WOPCM is locked"));
    } else {
        let firmware = m2::Firmware {
            scanned,
            last_present,
            total,
            subslice_mask,
            engine_mask,
            plane,
        };
        run_m2(&mut bar, device, time, firmware, steps)?;
    }
    uncore::fw_domains_put(&mut bar, &domains, wanted);
    Ok(())
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

/// The GT's identity, under forcewake: the dual-subslices and the engines
/// the media fuses leave, for M2.
fn say_gt(bar: &mut Bar0, platform: &Platform, steps: &Steps) -> Result<(u8, u32), Step> {
    steps.before(
        At::Fuses,
        format_args!(
            "read GT_SLICE_ENABLE 0x9138, GEOMETRY_DSS_ENABLE 0x913c, EU_DISABLE 0x9134, VEBOX_VDBOX_DISABLE 0x9140"
        ),
    )?;
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
    steps.before(
        At::Clock,
        format_args!(
            "read CTC_MODE 0xa26c, then RPM_CONFIG0 0xd00 or TIMESTAMP_OVERRIDE 0x44074 as it says"
        ),
    )?;
    let cs = clock::gen11_read_clock_frequency(bar);
    steps.before(
        At::RpCaps,
        format_args!("read RP_STATE_CAP 0x145998, FREQ_INFO_REC 0x145ef0 (MCHBAR mirror)"),
    )?;
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
    Ok((info.subslice_mask, mask))
}

/// How long the read-only GGTT walk may take before it stops: it is
/// diagnosis, not bring-up, and every entry is two uncached reads.
const GGTT_WALK_BUDGET_US: u64 = 200_000;

/// How many absent entries in a row, after a present one, end the walk:
/// the firmware maps its framebuffer as one run at the GGTT's start.
const GGTT_WALK_GAP: u64 = 4096;

/// The lit plane on pipe A (the one the firmware lights; the other pipes'
/// power wells may be off, and i915 reads a plane only with its well on),
/// and the GGTT entries the firmware left present, read only and bounded.
/// Returns how many entries were read, the last present one, the GGTT's
/// size in bytes, and the lit plane for M2's screen blit.
/// What M1's GGTT walk leaves M2: the entries read, the last present one,
/// the GGTT's size in bytes, and the lit plane.
type Walked = (u64, Option<u64>, u64, Option<m2::ScreenPlane>);

fn say_plane_and_ggtt(
    bar: &mut Bar0,
    device: &Device<Kernel>,
    time: &mut Time,
    steps: &Steps,
) -> Result<Walked, Step> {
    let gmch = device
        .config_read(gtt::SNB_GMCH_CTRL, 2)
        .map_or(0, |value| value as u16);
    let entries = gtt::ggtt_total(gmch) / gtt::GTT_PAGE_SIZE;
    steps.before(
        At::Plane,
        format_args!(
            "read pipe A plane 1 PLANE_CTL 0x70180, STRIDE 0x70188, SIZE 0x70190, SURF 0x7019c (display)"
        ),
    )?;
    let lit = plane::read_plane1(bar, 0);
    if lit.enabled() {
        let (width, height) = lit.dimensions();
        let first = u64::from(lit.ggtt_offset()) / gtt::GTT_PAGE_SIZE;
        let pte = bar.ggtt_entry(first).unwrap_or(0);
        say(format_args!(
            "intel: pipe A plane 1 lit, {width}x{height}, stride {:#x}, at GGTT {:#x}: entry {pte:#018x} (page {:#x})",
            lit.stride,
            lit.ggtt_offset(),
            gtt::gen8_ggtt_pte_decode(pte).unwrap_or(0),
        ));
    } else {
        say(format_args!(
            "intel: pipe A plane 1 not lit (ctl {:#010x})",
            lit.ctl
        ));
    }
    steps.before(
        At::GgttWalk,
        format_args!("read GGTT entries from BAR 0 + 8 MiB, {entries} at most, for at most 200 ms"),
    )?;
    let started = time.now_us();
    let mut present = 0_u64;
    let mut first_present = None;
    let mut last_present = 0;
    let mut scanned = 0_u64;
    let mut why = "the end";
    for index in 0..entries {
        if index % 4096 == 0 && time.now_us().saturating_sub(started) > GGTT_WALK_BUDGET_US {
            why = "its time budget";
            break;
        }
        let Some(pte) = bar.ggtt_entry(index) else {
            break;
        };
        scanned += 1;
        if gtt::gen8_ggtt_pte_decode(pte).is_some() {
            present += 1;
            let _ = first_present.get_or_insert(index);
            last_present = index;
        } else if first_present.is_some() && index - last_present >= GGTT_WALK_GAP {
            why = "a gap of 4096 absent entries";
            break;
        }
    }
    say(format_args!(
        "intel: GGTT {entries} entries, {scanned} read in {} us to {why}: {present} present (first {:#x}, last {last_present:#x}); none written",
        time.now_us().saturating_sub(started),
        first_present.unwrap_or(0)
    ));
    let screen = lit.enabled().then(|| {
        let (width, height) = lit.dimensions();
        m2::ScreenPlane {
            ggtt: lit.ggtt_offset(),
            stride: lit.stride,
            ctl: lit.ctl,
            width,
            height,
        }
    });
    Ok((
        scanned,
        first_present.map(|_| last_present),
        entries * gtt::GTT_PAGE_SIZE,
        screen,
    ))
}

/// Stay, for the life of the machine.
fn stay(time: &Time) -> Result<core::convert::Infallible, Step> {
    loop {
        if time.idle.wait(Deadline::Never).is_err() {
            return Err(Step::Port);
        }
    }
}

/// M2 under every domain's forcewake: `intel_gt_init_hw` runs under
/// `FORCEWAKE_ALL`, every domain the fused engines have (a fused-off
/// VCS2's would never acknowledge).
fn run_m2(
    bar: &mut Bar0,
    device: &Device<Kernel>,
    time: &mut Time,
    firmware: m2::Firmware,
    steps: &Steps,
) -> Result<(), Step> {
    let all = uncore::fw_domains_init(firmware.engine_mask);
    steps.before(
        At::ForcewakeAll,
        format_args!(
            "forcewake every domain the fuses leave (engines {:#x}): media 0xa540.. and 0xa560.. too",
            firmware.engine_mask
        ),
    )?;
    let every = match uncore::fw_domains_get(bar, time, &all, !0) {
        Ok(()) => true,
        Err(error) => {
            say(format_args!(
                "intel: M2: forcewake of every domain failed ({error:?}); render and GT only"
            ));
            false
        }
    };
    match m2::run(bar, device, time, firmware, steps) {
        Err(Step::Stopped) => return Err(Step::Stopped),
        Err(step) => say(format_args!("intel: M2 stopped at {step:?}")),
        Ok(()) => {}
    }
    if every {
        uncore::fw_domains_put(bar, &all, !0);
    }
    Ok(())
}
