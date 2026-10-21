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
//! 8. Unless the `WOPCM` is locked: forcewake of the domains M1 does not
//!    hold, then M2's checks ([`m2`]: the commands, the blits, a hang) only
//!    under `ferrix.intelgpu.m2`.
//! 9. M3: the GT brought up under `ferrix-intel-xelp`'s back end on the
//!    real platform ([`platform`]: BAR 0, page tables in a pinned pool, the
//!    driver's objects as pinned VMOs in its own GGTT window, the client's
//!    objects pinned at their first bind), and the render node published
//!    and served ([`serve`]) with the MSI on `serve::KEY_INTERRUPT` and
//!    the 1 ms poll beside it. The kernel refuses the node while 00:02.0's
//!    DMA is untranslated (K3); the driver then says so and stays.
//! 10. `card0` ([`display`]): where K4 lent the firmware framebuffer to
//!     00:02.0, the lit plane served as a copying display card, each flushed
//!     rectangle copied into its write-combining window, so a compositor runs
//!     under `ferrix.intelgpu`. Served beside the render node, or alone when
//!     the kernel refuses that.
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

mod arch;
mod display;
mod m2;
mod platform;
mod serve;
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
    /// M3: the back end did not bring the GT up (said).
    Backend = 12,
    /// M3: the render node was refused or serving stopped (said).
    Serve = 13,
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
        let now = ferrix_rt::linux::monotonic_nanos().unwrap_or(0);
        steps::reset_if_due(now);
        now / 1_000
    }

    fn delay_us(&mut self, us: u32) {
        let now = ferrix_rt::linux::monotonic_nanos().unwrap_or(0);
        steps::reset_if_due(now);
        let until = now.saturating_add(u64::from(us) * 1_000);
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
    let stopped = |step: Step| {
        if !matches!(step, Step::Stopped) {
            // Exiting would have devmgr quiesce the function, and a quiesce
            // turns its bus mastering off (`device_quiesce`), under the
            // plane the firmware lit and the display engine still scans
            // out of memory. i915 never does that with a lit plane: it
            // turns the display off before `pci_disable_device`.
            say(format_args!(
                "intel: stopped at {step:?}; the driver stays, so 00:02.0's bus mastering stays on under the lit screen"
            ));
        }
    };
    let done = match m1(&device, info.apertures, &platform, &steps, &mut time) {
        Ok(done) => done,
        Err(step) => {
            stopped(step);
            return stay(&time);
        }
    };
    if done.locked {
        say(format_args!(
            "intel: the WOPCM is locked: no M2 and no render node; the driver now waits and touches nothing"
        ));
        return stay(&time);
    }
    match serve_gpu(&device, info.location, info.device_id, done, &steps) {
        Ok(never) => match never {},
        Err(step) => {
            stopped(step);
            stay(&time)
        }
    }
}

/// What M1 leaves for the rest: BAR 0 under render and GT forcewake, and
/// the facts it read.
struct M1Done {
    bar: Bar0,
    firmware: m2::Firmware,
    locked: bool,
}

/// M3's driver: every forcewake domain for the bring-up, M2's checks if
/// `ferrix.intelgpu.m2` asks, then the GT under `ferrix-intel-xelp`'s back
/// end on the real platform, and the render node served through
/// [`serve`] until the kernel takes it back.
fn serve_gpu(
    device: &Device<Kernel>,
    location: u32,
    device_id: u16,
    done: M1Done,
    steps: &Steps,
) -> Result<core::convert::Infallible, Step> {
    use ferrix_intel_xelp::backend::XelpBackend;
    use ferrix_intel_xelp::frontend::Frontend;
    let M1Done {
        mut bar, firmware, ..
    } = done;
    let mut time = Time {
        idle: port::create(Kernel).map_err(|_| Step::Port)?,
    };
    // intel_gt_init_hw runs under FORCEWAKE_ALL. Render and GT are held
    // since M1, so only the others are asked for: asking a held domain
    // again waits for an ack that cannot clear (the box, 2026-10-20:
    // "AckNotCleared { domain: 1, ack: 1 }", i915's
    // fw_domain_wait_ack_clear refusing GT).
    let all = uncore::fw_domains_init(firmware.engine_mask);
    let rest = !(uncore::FORCEWAKE_RENDER | uncore::FORCEWAKE_GT);
    steps.before(
        At::ForcewakeAll,
        format_args!(
            "forcewake the domains M1 does not hold (engines {:#x}): media",
            firmware.engine_mask
        ),
    )?;
    let others = match uncore::fw_domains_get(&mut bar, &mut time, &all, rest) {
        Ok(()) => true,
        Err(error) => {
            say(format_args!(
                "intel: forcewake of the media domains failed ({error:?}); render and GT only"
            ));
            false
        }
    };
    if steps.selftest {
        match m2::run(&mut bar, device, &mut time, firmware, steps) {
            Err(Step::Stopped) => return Err(Step::Stopped),
            Err(step) => say(format_args!("intel: M2 stopped at {step:?}")),
            Ok(()) => {}
        }
    }
    steps.before(
        At::Serve,
        format_args!("bring the GT up under the back end and serve the render node"),
    )?;
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let interrupt = bind_interrupt(device, &port);
    // Above everything the firmware left and above M2's window, if it ran.
    let start = (firmware
        .scanned
        .max(firmware.last_present.map_or(0, |l| l + 1))
        * 4096)
        .max(SERVE_GGTT_FLOOR);
    let ggtt = ferrix_intel_xelp::ggtt_space::GgttSpace::new(start, firmware.total);
    let identity = identity(device, device_id, firmware.total);
    let (fused, count) = fused_engines(firmware.engine_mask);
    let real = platform::Real::new(
        bar,
        Time {
            idle: port::create(Kernel).map_err(|_| Step::Port)?,
        },
        device,
        ggtt,
        interrupt,
    )?;
    let started = time.now_us();
    let mut backend = XelpBackend::new(
        real,
        firmware.subslice_mask,
        fused.get(..count).unwrap_or_default(),
        2,
        identity,
    )
    .map_err(|error| {
        say(format_args!(
            "intel: the back end did not come up: {error:?}"
        ));
        Step::Backend
    })?;
    let facts = *backend.facts();
    say(format_args!(
        "intel: back end up in {} ms: GGTT window {start:#x}, {} engines, {} EUs, {} MiB of system memory",
        time.now_us().saturating_sub(started) / 1_000,
        facts.engines.len(),
        facts.sseu.eu_total,
        facts.system_memory_bytes >> 20
    ));
    power(&mut backend, steps);
    if others {
        uncore::fw_domains_put(backend.platform().bar(), &all, rest);
    }
    let mut frontend = Frontend::new(backend);
    frontend.set_trace(steps.uapitrace);
    if steps.uapitrace {
        say(format_args!(
            "intel: uapitrace: held waits, their answers, failed ioctls, and a wait unanswered for 2 s (ferrix.intelgpu.uapitrace)"
        ));
    }
    serve_nodes(device, location, firmware.plane, &port, &mut frontend)
}

/// The MSI, bound to the serving loop's port under
/// [`serve::KEY_INTERRUPT`]; none, said, and the back end polls. (On the box
/// every M2 completion was seen by the poll, 2026-10-20.)
fn bind_interrupt(
    device: &Device<Kernel>,
    port: &Port<Kernel>,
) -> Option<ferrix_rt::native::device::Interrupt<Kernel>> {
    match device.interrupt(0) {
        Ok(interrupt) => match interrupt.bind(port, serve::KEY_INTERRUPT) {
            Ok(()) => Some(interrupt),
            Err(error) => {
                say(format_args!(
                    "intel: the interrupt could not be bound ({error:?}); polling only"
                ));
                None
            }
        },
        Err(error) => {
            say(format_args!(
                "intel: no interrupt ({error:?}); polling only"
            ));
            None
        }
    }
}

/// What the uAPI says of the device and the machine.
fn identity(
    device: &Device<Kernel>,
    device_id: u16,
    ggtt_bytes: u64,
) -> ferrix_intel_xelp::backend::Identity {
    ferrix_intel_xelp::backend::Identity {
        device_id,
        revision: device.config_read(0x08, 1).map_or(0, |value| value as u8),
        ggtt_bytes,
        system_memory_bytes: steps::system_memory_bytes().unwrap_or(SYSTEM_MEMORY_FALLBACK),
    }
}

/// The engines the media fuses leave, in id order, and how many.
fn fused_engines(mask: u32) -> ([ferrix_intel_xelp::engine::EngineInfo; 5], usize) {
    let mut fused = [ENGINES[0]; 5];
    let mut count = 0;
    for e in ENGINES.iter().filter(|e| mask & engine::bit(e.id) != 0) {
        if let Some(slot) = fused.get_mut(count) {
            *slot = *e;
            count += 1;
        }
    }
    (fused, count)
}

/// The lowest GGTT offset the serving back end binds at: past BAR 2's
/// aperture and M2's window, which `ferrix.intelgpu.m2` fills first.
const SERVE_GGTT_FLOOR: u64 = 1 << 30;

/// System memory to report when `/proc/meminfo` cannot be read: the box's.
const SYSTEM_MEMORY_FALLBACK: u64 = 16 << 30;

/// M1, each step said first. Render and GT stay forcewoken for what
/// follows.
fn m1(
    device: &Device<Kernel>,
    apertures: u32,
    platform: &Platform,
    steps: &Steps,
    time: &mut Time,
) -> Result<M1Done, Step> {
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

    Ok(M1Done {
        bar,
        firmware: m2::Firmware {
            scanned,
            last_present,
            total,
            subslice_mask,
            engine_mask,
            plane,
        },
        locked,
    })
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
        let deadline = steps::reset_at_nanos().map_or(Deadline::Never, Deadline::At);
        match time.idle.wait(deadline) {
            Err(ferrix_rt::native::error::Error::TimedOut) => {
                steps::reset_if_due(ferrix_rt::linux::monotonic_nanos().unwrap_or(0));
            }
            Err(_) => return Err(Step::Port),
            Ok(_) => {}
        }
    }
}

/// RPS and RC6 by the host (`ferrix_intel_xelp::power`): the first request
/// at RP1, or `ferrix.intelgpu.freq=MHz`; under `ferrix.intelgpu.freqtest`
/// the box's checkpoint (RP1, then RP0, each held and sampled, then back);
/// under `ferrix.intelgpu.freqlog` a line each second from then on.
fn power<P: ferrix_intel_xelp::backend::Platform>(
    backend: &mut ferrix_intel_xelp::backend::XelpBackend<P>,
    steps: &Steps,
) where
    P::Memory: serve::AsVmo,
{
    use ferrix_intel_xelp::rps::intel_gpu_freq;
    let rps = backend.power_up(steps.freq_mhz, true);
    say(format_args!(
        "intel: RPS on: RP0 {} MHz, RP1 {} MHz, RPn {} MHz; request {} MHz{}; RC6 enabled (residency stays low while the driver holds forcewake)",
        intel_gpu_freq(rps.caps.rp0_freq),
        intel_gpu_freq(rps.caps.rp1_freq),
        intel_gpu_freq(rps.caps.min_freq),
        intel_gpu_freq(rps.cur),
        if steps.freq_mhz.is_some() {
            " (fixed: ferrix.intelgpu.freq)"
        } else {
            " (RP1)"
        }
    ));
    if steps.freqtest {
        let mut time = Time {
            idle: match port::create(Kernel) {
                Ok(port) => port,
                Err(_) => return,
            },
        };
        for (name, val) in [("RP1", rps.caps.rp1_freq), ("RP0", rps.caps.rp0_freq)] {
            let asked = backend.set_frequency_mhz(intel_gpu_freq(val)).unwrap_or(0);
            // The GT under load: an idle GT runs below its request.
            let busy = busy_start(backend);
            say(format_args!(
                "intel: freqtest {name}: {}",
                if busy.is_some() {
                    "a spinning batch keeps RCS0 busy while sampled"
                } else {
                    "no spinning batch (refused); sampled idle"
                }
            ));
            for n in 0..5 {
                time.delay_us(100_000);
                if let Some(sample) = backend.frequency_sample() {
                    say(format_args!(
                        "intel: freqtest {name}: asked {asked} MHz, sample {n}: RPSTAT1 {:#010x}, CAGF {} MHz, RC6 count {}, package energy {:#x} (unit 1/2^{} J)",
                        sample.rpstat,
                        sample.actual_mhz,
                        sample.rc6_count,
                        sample.energy_raw,
                        sample.energy_unit
                    ));
                }
            }
            if let Some(busy) = busy {
                let ended = busy_end(backend, busy, &mut time);
                say(format_args!(
                    "intel: freqtest {name}: the spinning batch {}",
                    match ended {
                        BusyEnd::Ended => "ran until ended",
                        BusyEnd::Early =>
                            "had completed before it was ended: the GT was NOT kept busy",
                        BusyEnd::Stuck => "did not complete within 1 s of being ended",
                    }
                ));
            }
        }
        let back = backend
            .set_frequency_mhz(intel_gpu_freq(rps.cur))
            .unwrap_or(0);
        say(format_args!(
            "intel: freqtest done; the request is {back} MHz again"
        ));
    }
    if steps.freqlog {
        backend.set_freqlog(true, ferrix_rt::linux::monotonic_nanos().unwrap_or(0));
        say(format_args!(
            "intel: freqlog: one line a second while serving (ferrix.intelgpu.freqlog)"
        ));
    }
}

/// Where the spinning batch sits in its own VM, and the request id it runs
/// as (above any the front end gives, which count up from 1).
const BUSY_AT: u64 = 0x1_0000_0000;
const BUSY_REQUEST: u64 = u64::MAX - 1;

/// The spinning batch of [`busy_start`].
struct Busy<M> {
    memory: M,
    vm: ferrix_intel_xelp::frontend::VmId,
    context: ferrix_intel_xelp::frontend::HwContext,
}

/// How [`busy_end`] found the batch.
enum BusyEnd {
    /// It completed once ended.
    Ended,
    /// It had completed before: it never spun.
    Early,
    /// It did not complete within a second of being ended.
    Stuck,
}

/// `igt_spinner` through the back end: an arbitration point and a jump
/// back to itself (PPGTT), on the first engine (RCS0) in a VM of its own,
/// so the GT is busy while freqtest samples. `None` if any step is refused.
fn busy_start<P: ferrix_intel_xelp::backend::Platform>(
    backend: &mut ferrix_intel_xelp::backend::XelpBackend<P>,
) -> Option<Busy<P::Memory>>
where
    P::Memory: serve::AsVmo,
{
    use ferrix_intel_xelp::commands::gpu;
    use ferrix_intel_xelp::frontend::{Backend as Fb, ContextParams};
    use serve::AsVmo as _;
    let memory = Fb::create_memory(backend, 4096).ok()?;
    let words = [
        gpu::MI_ARB_CHECK,
        gpu::MI_BATCH_BUFFER_START_GEN8 | (1 << 8),
        BUSY_AT as u32,
        (BUSY_AT >> 32) as u32,
    ];
    let mut bytes = [0_u8; 16];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    memory.vmo().write(&bytes, 0).ok()?;
    let engine = *Fb::facts(backend).engines.first()?;
    let vm = Fb::vm_create(backend).ok()?;
    if Fb::bind(backend, vm, BUSY_AT, &memory, 4096, 0).is_err() {
        Fb::vm_destroy(backend, vm);
        return None;
    }
    let Ok(context) = Fb::context_create(backend, vm, engine, ContextParams::DEFAULT) else {
        Fb::unbind(backend, vm, BUSY_AT, 4096);
        Fb::vm_destroy(backend, vm);
        return None;
    };
    if Fb::submit(backend, context, BUSY_AT, 16, BUSY_REQUEST).is_err() {
        Fb::context_destroy(backend, context);
        Fb::unbind(backend, vm, BUSY_AT, 4096);
        Fb::vm_destroy(backend, vm);
        return None;
    }
    Fb::service(
        backend,
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0),
        false,
    );
    Some(Busy {
        memory,
        vm,
        context,
    })
}

/// End [`busy_start`]'s batch (`MI_BATCH_BUFFER_END` over its first dword,
/// as `igt_spinner_end` does), wait up to a second for it, and free it.
fn busy_end<P: ferrix_intel_xelp::backend::Platform>(
    backend: &mut ferrix_intel_xelp::backend::XelpBackend<P>,
    busy: Busy<P::Memory>,
    time: &mut Time,
) -> BusyEnd
where
    P::Memory: serve::AsVmo,
{
    use ferrix_intel_xelp::commands::gpu;
    use ferrix_intel_xelp::frontend::Backend as Fb;
    use serve::AsVmo as _;
    let mut done = false;
    let take = |backend: &mut ferrix_intel_xelp::backend::XelpBackend<P>| {
        Fb::service(
            backend,
            ferrix_rt::linux::monotonic_nanos().unwrap_or(0),
            false,
        );
        let mut seen = false;
        Fb::completed(backend, &mut |id, _| {
            seen |= id == BUSY_REQUEST;
        });
        seen
    };
    let early = take(backend);
    done |= early;
    let _ = busy
        .memory
        .vmo()
        .write(&gpu::MI_BATCH_BUFFER_END.to_le_bytes(), 0);
    for _ in 0..1000 {
        if done {
            break;
        }
        time.delay_us(1_000);
        done |= take(backend);
    }
    if done {
        Fb::context_destroy(backend, busy.context);
        Fb::unbind(backend, busy.vm, BUSY_AT, 4096);
        Fb::vm_destroy(backend, busy.vm);
    }
    // A batch still running keeps its memory, VM and context: leaked, for
    // the box's checkpoint alone, rather than freed under the GPU.
    match (early, done) {
        (true, _) => BusyEnd::Early,
        (false, true) => BusyEnd::Ended,
        (false, false) => {
            let _kept = core::mem::ManuallyDrop::new(busy.memory);
            BusyEnd::Stuck
        }
    }
}

/// `card0` over the lit plane where K4 lent it ([`display`]), then the
/// render node, both served on `port`. A refused render node (K3, while
/// 00:02.0's DMA is untranslated) leaves `card0` served alone.
fn serve_nodes<B: ferrix_intel_xelp::frontend::Backend>(
    device: &Device<Kernel>,
    location: u32,
    plane: Option<m2::ScreenPlane>,
    port: &Port<Kernel>,
    frontend: &mut ferrix_intel_xelp::frontend::Frontend<B>,
) -> Result<core::convert::Infallible, Step>
where
    B::Memory: serve::AsVmo,
{
    let apertures = device.info().map_or(0, |info| info.apertures);
    let mut card = display::Display::publish(device, apertures, location, plane, port);
    let mut on_display = |key: u64, port: &Port<Kernel>| {
        if key == display::KEY_DISPLAY
            && let Some(display) = card.as_mut()
            && !display.on_packet(port)
        {
            card = None;
        }
    };
    let mut host = match serve::publish(device, location) {
        Ok(host) => host,
        Err(stopped) => {
            say(format_args!(
                "intel: no render node ({stopped:?}); card0 alone is served"
            ));
            loop {
                match port.wait(Deadline::Never) {
                    Ok(packet) => on_display(packet.key, port),
                    Err(_) => return Err(Step::Serve),
                }
            }
        }
    };
    match serve::serve(&mut host, port, frontend, &mut on_display) {
        Ok(never) => match never {},
        Err(stopped) => {
            say(format_args!("intel: serving stopped: {stopped:?}"));
            Err(Step::Serve)
        }
    }
}
