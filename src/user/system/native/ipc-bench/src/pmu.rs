//! MEASUREMENT ONLY (branch `os4b/b3-ferrix`, never lands): the board
//! bench's timing contract, `tools/common/bench/board/common/board-bench.h`,
//! in Rust, so Ferrix on the STM32MP157D-DK1 is timed exactly as Linux and
//! seL4 are there (`docs/BOARD-BENCH.md`).
//!
//! - Each sample reads the cycle counter (`PMCCNTR`), then the
//!   instructions-retired counter (event counter 0, event 0x08), then runs
//!   the operation, then reads instructions and cycles again, each read after
//!   an `isb` ([`time`]). The counters count every mode.
//! - [`WARMUP`] untimed operations, then [`SAMPLES`] timed ones, every sample
//!   kept ([`run`]). A percentile is `sorted[n * per_mille / 1000]`, clamped
//!   to the last.
//! - ns come from cycles at the clock measured against the system counter
//!   over a tenth of a second ([`cpu_hz`]).
//! - The lines, one set per series ([`Text::series`]):
//!   `ipc-bench clock cpu_hz=<hz> cntfrq=<hz>`, `ipc-bench <what> n= min=
//!   p50= p90= p99= mean=` in ns (the shape `bench-ipc --board-log` reads),
//!   `<what>.cycles`, `<what>.ins`, and `<what>.cycles.pct v=<p0>,...,<p100>`.
//! - [`touch`] is `bb_touch`'s loop, the same four instructions.
//!
//! ARMv7-A only, and only with `ipc-bench.pmu=1` (or `on`) on the kernel
//! command line, which also makes the kernel set `PMUSERENR.EN`; [`start`]
//! refuses to run without it. Elsewhere [`start`] refuses by name and the
//! other functions are never reached.

use core::fmt::Write as _;

use ferrix_rt::Kernel;
use ferrix_rt::linux::{self, numbers};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::vmo;

/// Untimed operations before a series (`BB_WARMUP`).
pub(crate) const WARMUP: u32 = 1_000;
/// Timed operations in a series (`BB_SAMPLES`).
pub(crate) const SAMPLES: usize = 20_000;
/// The after-the-call sweep: pages the caller touches, and the names of its
/// two series -- the touch alone, and the touch right after a call.
pub(crate) const SWEEP: [(u32, &str, &str); 4] = [
    (1, "base.w1", "domain-call.after.w1"),
    (16, "base.w16", "domain-call.after.w16"),
    (64, "base.w64", "domain-call.after.w64"),
    (256, "base.w256", "domain-call.after.w256"),
];
/// Bytes between two touched addresses: a page, so the sweep walks the TLB.
pub(crate) const STRIDE: u32 = 4096;
/// The touched buffer: the widest sweep's pages.
const TOUCH_BYTES: usize = 256 * 4096;

/// The kernel option that asks for this mode.
const OPTION: &[u8] = b"ipc-bench.pmu=";
/// `/proc/cmdline`.
const CMDLINE: &[u8] = b"/proc/cmdline\0";
/// `AT_FDCWD`.
const AT_FDCWD: usize = -100_isize as usize;
/// `O_RDONLY | O_CLOEXEC`.
const O_READ: usize = 0o2_000_000;

/// Whether `ipc-bench.pmu=1` (or `on`) is on the kernel command line, read
/// from `/proc/cmdline`, which is mounted here if it is not. False when it
/// cannot be read: then nothing of the PMU is touched.
pub(crate) fn asked() -> bool {
    let open = || {
        // SAFETY: `CMDLINE` is NUL-terminated and borrowed for the call.
        unsafe {
            linux::call(
                numbers::OPENAT,
                [AT_FDCWD, CMDLINE.as_ptr().addr(), O_READ, 0, 0, 0],
            )
        }
    };
    let fd = match open() {
        Ok(fd) => fd,
        Err(_) => {
            let proc_dir = b"/proc\0";
            // SAFETY: the strings are NUL-terminated and borrowed for the
            // calls; no data argument.
            let _ = unsafe {
                linux::call(
                    numbers::MKDIRAT,
                    [AT_FDCWD, proc_dir.as_ptr().addr(), 0o555, 0, 0, 0],
                )
            };
            // SAFETY: as above.
            let _ = unsafe {
                linux::call(
                    numbers::MOUNT,
                    [
                        c"proc".as_ptr().addr(),
                        proc_dir.as_ptr().addr(),
                        c"proc".as_ptr().addr(),
                        0,
                        0,
                        0,
                    ],
                )
            };
            match open() {
                Ok(fd) => fd,
                Err(_) => return false,
            }
        }
    };
    let mut line = [0_u8; 4096];
    let got = linux::read(fd, &mut line).unwrap_or(0);
    let _ = linux::close(fd);
    line.get(..got)
        .unwrap_or_default()
        .split(u8::is_ascii_whitespace)
        .filter_map(|word| word.strip_prefix(OPTION))
        .any(|value| value == b"1" || value == b"on")
}

#[cfg(target_arch = "arm")]
mod counters {
    //! The ARMv7-A PMU and generic timer, as `board-bench.h` reads them.

    use core::arch::asm;

    /// Instructions architecturally executed (`ARMv7` PMU event 0x08).
    const EVENT_INST_RETIRED: u32 = 0x08;

    /// `PMUSERENR`, which user mode may always read where there is a PMU.
    fn pmuserenr() -> u32 {
        let value: u32;
        // SAFETY: a read of a register user mode may read; no memory.
        unsafe {
            asm!("mrc p15, 0, {}, c9, c14, 0", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// `bb_pmu_start`: the cycle counter and event counter 0 (instructions)
    /// started, both reset, no divider.
    pub(crate) fn start() -> Result<(), &'static str> {
        if pmuserenr() & 1 == 0 {
            return Err("the kernel left PMUSERENR.EN clear, so user mode may not use the PMU");
        }
        // SAFETY: `PMUSERENR.EN` is set, so each of these is defined from
        // user mode; they program only this processor's counters, which
        // nothing else in the system uses.
        unsafe {
            asm!(
                "mcr p15, 0, {sel}, c9, c12, 5",  // PMSELR = 0
                "mcr p15, 0, {event}, c9, c13, 1", // PMXEVTYPER = INST_RETIRED
                "mcr p15, 0, {overflow}, c9, c12, 3", // PMOVSR: clear overflows
                "mcr p15, 0, {control}, c9, c12, 0", // PMCR = E | P | C, D = 0
                "mcr p15, 0, {enable}, c9, c12, 1", // PMCNTENSET = cycles | counter 0
                "isb",
                sel = in(reg) 0_u32,
                event = in(reg) EVENT_INST_RETIRED,
                overflow = in(reg) 0xFFFF_FFFF_u32,
                control = in(reg) 0x7_u32,
                enable = in(reg) (1_u32 << 31) | 1,
                options(nostack, preserves_flags),
            );
        }
        Ok(())
    }

    /// `bb_cycles`: `isb`, then `PMCCNTR`.
    #[inline(always)]
    pub(crate) fn cycles() -> u32 {
        let value: u32;
        // SAFETY: `start` found `PMUSERENR.EN` set; a read, ordered with
        // the memory accesses around it as the C header's "memory" clobber
        // orders it.
        unsafe {
            asm!("isb", "mrc p15, 0, {}, c9, c13, 0", out(reg) value, options(nostack, preserves_flags));
        }
        value
    }

    /// `bb_insns`: `isb`, then event counter 0, which `start` selected.
    #[inline(always)]
    pub(crate) fn insns() -> u32 {
        let value: u32;
        // SAFETY: as `cycles`.
        unsafe {
            asm!("isb", "mrc p15, 0, {}, c9, c13, 2", out(reg) value, options(nostack, preserves_flags));
        }
        value
    }

    /// `bb_cntvct`: `isb`, then the virtual counter.
    pub(crate) fn cntvct() -> u64 {
        let (low, high): (u32, u32);
        // SAFETY: the kernel sets `CNTKCTL.PL0VCTEN` on every processor; a
        // read.
        unsafe {
            asm!("isb", "mrrc p15, 1, {}, {}, c14", out(reg) low, out(reg) high, options(nostack, preserves_flags));
        }
        (u64::from(high) << 32) | u64::from(low)
    }

    /// `bb_cntfrq`: the system counter's rate.
    pub(crate) fn cntfrq() -> u32 {
        let value: u32;
        // SAFETY: readable from user mode with `PL0VCTEN`; a read.
        unsafe {
            asm!("mrc p15, 0, {}, c14, c0, 0", out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// `bb_touch`: one load from each of `n` (more than 0) addresses
    /// `stride` bytes apart from `base`, the same four instructions on every
    /// kernel.
    #[inline(always)]
    pub(crate) fn touch(base: usize, n: u32, stride: u32) {
        // SAFETY: `base` is the start of a mapping of at least `n * stride`
        // readable bytes (`touch_buffer`), so every load is of this
        // program's own memory; nothing is written.
        unsafe {
            asm!(
                "1:",
                "ldr r12, [{p}]",
                "add {p}, {p}, {stride}",
                "subs {n}, {n}, #1",
                "bne 1b",
                p = inout(reg) base => _,
                n = inout(reg) n => _,
                stride = in(reg) stride,
                out("r12") _,
                options(nostack),
            );
        }
    }
}

#[cfg(not(target_arch = "arm"))]
mod counters {
    //! No PMU bench here: [`start`] refuses, so nothing else is called.

    /// Refused by name.
    pub(crate) fn start() -> Result<(), &'static str> {
        Err("the PMU bench is ARMv7-A only")
    }

    /// Never called.
    pub(crate) fn cycles() -> u32 {
        0
    }

    /// Never called.
    pub(crate) fn insns() -> u32 {
        0
    }

    /// Never called.
    pub(crate) fn cntvct() -> u64 {
        0
    }

    /// Never called.
    pub(crate) fn cntfrq() -> u32 {
        0
    }

    /// Never called.
    pub(crate) fn touch(_base: usize, _n: u32, _stride: u32) {}
}

pub(crate) use counters::{start, touch};

/// `bb_cpu_hz`: cycles over a tenth of a second of the system counter.
pub(crate) fn cpu_hz() -> u64 {
    let frequency = u64::from(counters::cntfrq());
    let v0 = counters::cntvct();
    let c0 = counters::cycles();
    let mut v1;
    loop {
        v1 = counters::cntvct();
        if v1.wrapping_sub(v0) >= frequency / 10 {
            break;
        }
    }
    let c1 = counters::cycles();
    u64::from(c1.wrapping_sub(c0)).saturating_mul(frequency) / v1.wrapping_sub(v0).max(1)
}

/// One series: every sample kept, cycles and instructions apart, in memory
/// mapped for them (a native program has no heap).
pub(crate) struct Series {
    /// Cycles per sample, the first `n` filled.
    cycles: &'static mut [u32],
    /// Instructions per sample, the first `n` filled.
    instructions: &'static mut [u32],
    /// Samples taken.
    n: usize,
    /// Their cycles' sum.
    cycle_sum: u64,
    /// Their instructions' sum.
    instruction_sum: u64,
}

impl Series {
    /// Room for [`SAMPLES`], or `None` when it cannot be mapped.
    pub(crate) fn new() -> Option<Series> {
        let bytes = (2 * SAMPLES * size_of::<u32>()).next_multiple_of(4096);
        let at = map(bytes)?;
        // SAFETY: `map` answered `bytes` of fresh, zeroed, writable memory
        // at `at`, page-aligned, which nothing else in this process names and
        // which stays mapped for the process's life; `u32` is valid for any
        // bytes, and the two halves do not overlap.
        let all = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::with_exposed_provenance_mut::<u32>(at),
                2 * SAMPLES,
            )
        };
        let (cycles, instructions) = all.split_at_mut(SAMPLES);
        Some(Series {
            cycles,
            instructions,
            n: 0,
            cycle_sum: 0,
            instruction_sum: 0,
        })
    }

    /// `bb_reset`.
    fn reset(&mut self) {
        self.n = 0;
        self.cycle_sum = 0;
        self.instruction_sum = 0;
    }

    /// `bb_add`.
    fn add(&mut self, cycles: u32, instructions: u32) {
        if let (Some(c), Some(i)) = (
            self.cycles.get_mut(self.n),
            self.instructions.get_mut(self.n),
        ) {
            *c = cycles;
            *i = instructions;
            self.cycle_sum += u64::from(cycles);
            self.instruction_sum += u64::from(instructions);
            self.n += 1;
        }
    }
}

/// `BB_TIME`: cycles, instructions, `op`, instructions, cycles. A sample is
/// kept only when `op` succeeded.
#[inline(always)]
pub(crate) fn time<E>(series: &mut Series, op: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
    let c0 = counters::cycles();
    let i0 = counters::insns();
    let result = op();
    let i1 = counters::insns();
    let c1 = counters::cycles();
    result?;
    series.add(c1.wrapping_sub(c0), i1.wrapping_sub(i0));
    Ok(())
}

/// `BB_RUN`: [`WARMUP`] untimed runs of `op`, then [`SAMPLES`] timed ones
/// into `series`, reset first.
pub(crate) fn run<E>(series: &mut Series, mut op: impl FnMut() -> Result<(), E>) -> Result<(), E> {
    for _ in 0..WARMUP {
        op()?;
    }
    series.reset();
    for _ in 0..SAMPLES {
        time(series, &mut op)?;
    }
    Ok(())
}

/// The after-the-call series: [`WARMUP`] untimed runs of `call` then `op`,
/// then [`SAMPLES`] of `call` untimed and `op` timed ([`time`]) into
/// `series`, reset first. What `op` costs more than in a [`run`] of it alone
/// is what the call left behind.
pub(crate) fn run_after<E>(
    series: &mut Series,
    mut call: impl FnMut() -> Result<(), E>,
    mut op: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    for _ in 0..WARMUP {
        call()?;
        op()?;
    }
    series.reset();
    for _ in 0..SAMPLES {
        call()?;
        time(series, &mut op)?;
    }
    Ok(())
}

/// A page-aligned mapping of `bytes` fresh writable memory, its address.
fn map(bytes: usize) -> Option<usize> {
    vmo::create(Kernel, bytes)
        .and_then(|room| room.map(None, bytes, Protection::ReadWrite, 0))
        .ok()
}

/// The buffer the sweep touches: 256 pages, each written once here so each
/// has a frame of its own before the first timed load.
pub(crate) fn touch_buffer() -> Option<usize> {
    let at = map(TOUCH_BYTES)?;
    for page in (0..TOUCH_BYTES).step_by(4096) {
        // SAFETY: `at` is the start of `TOUCH_BYTES` of this program's own
        // writable memory, and `page` is inside it.
        unsafe {
            core::ptr::with_exposed_provenance_mut::<u8>(at.wrapping_add(page)).write_volatile(1);
        }
    }
    Some(at)
}

/// Lines held until every series is measured, so that printing them -- the
/// console draining at its own pace -- never overlaps a timed sample.
pub(crate) struct Text {
    /// The bytes, the first `len` written.
    bytes: &'static mut [u8],
    /// How many are written.
    len: usize,
}

/// Room for every line of one process's series.
const TEXT_BYTES: usize = 64 * 1024;

impl Text {
    /// Empty, in memory of its own; `None` when it cannot be mapped.
    pub(crate) fn new() -> Option<Text> {
        let at = map(TEXT_BYTES)?;
        // SAFETY: as `Series::new`'s, for bytes.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::with_exposed_provenance_mut::<u8>(at),
                TEXT_BYTES,
            )
        };
        Some(Text { bytes, len: 0 })
    }

    /// `bb_print_clock`.
    pub(crate) fn clock(&mut self, hz: u64) {
        let _ = writeln!(
            self,
            "ipc-bench clock cpu_hz={hz} cntfrq={}",
            counters::cntfrq()
        );
    }

    /// `bb_print`: the series' four lines (and the series sorted).
    pub(crate) fn series(&mut self, what: &str, series: &mut Series, hz: u64) {
        let n = series.n;
        if n == 0 {
            let _ = writeln!(self, "ipc-bench {what} n=0");
            return;
        }
        let cycles = series.cycles.get_mut(..n).unwrap_or_default();
        cycles.sort_unstable();
        let instructions = series.instructions.get_mut(..n).unwrap_or_default();
        instructions.sort_unstable();
        let at = |sorted: &[u32], per_mille: usize| -> u64 {
            let i = (n * per_mille / 1000).min(n - 1);
            u64::from(sorted.get(i).copied().unwrap_or(0))
        };
        let ns = |cycles: u64| cycles.saturating_mul(1_000_000_000) / hz.max(1);
        let count = n as u64;
        let cycle_mean = series.cycle_sum / count;
        let instruction_mean = series.instruction_sum / count;
        let _ = writeln!(
            self,
            "ipc-bench {what} n={n} min={} p50={} p90={} p99={} mean={}",
            ns(at(cycles, 0)),
            ns(at(cycles, 500)),
            ns(at(cycles, 900)),
            ns(at(cycles, 990)),
            ns(cycle_mean),
        );
        let _ = writeln!(
            self,
            "ipc-bench {what}.cycles n={n} min={} p50={} p90={} p99={} mean={cycle_mean}",
            at(cycles, 0),
            at(cycles, 500),
            at(cycles, 900),
            at(cycles, 990),
        );
        let _ = writeln!(
            self,
            "ipc-bench {what}.ins n={n} min={} p50={} p90={} p99={} mean={instruction_mean}",
            at(instructions, 0),
            at(instructions, 500),
            at(instructions, 900),
            at(instructions, 990),
        );
        let _ = write!(self, "ipc-bench {what}.cycles.pct v=");
        for percent in 0..=100 {
            let separator = if percent == 0 { "" } else { "," };
            let _ = write!(self, "{separator}{}", at(cycles, percent * 10));
        }
        let _ = writeln!(self);
    }

    /// Each line, its newline included.
    pub(crate) fn lines(&self) -> impl Iterator<Item = &[u8]> {
        self.bytes
            .get(..self.len)
            .unwrap_or_default()
            .split_inclusive(|&byte| byte == b'\n')
    }

    /// Every line on standard output.
    pub(crate) fn print(&self) {
        for line in self.lines() {
            put(line);
        }
    }
}

impl core::fmt::Write for Text {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len.saturating_add(text.len()).min(self.bytes.len());
        let room = end - self.len;
        if let (Some(into), Some(from)) = (
            self.bytes.get_mut(self.len..end),
            text.as_bytes().get(..room),
        ) {
            into.copy_from_slice(from);
        }
        self.len = end;
        Ok(())
    }
}

/// `bytes` on standard output, all of them unless a write fails.
pub(crate) fn put(mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match linux::write(1, bytes) {
            Ok(0) | Err(_) => return,
            Ok(written) => bytes = bytes.get(written..).unwrap_or_default(),
        }
    }
}
