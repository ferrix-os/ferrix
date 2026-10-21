//! Which hardware threads share a core, from `CPUID`'s topology leaves
//! (`smt=off`, `docs/BUILD-FLAGS.md` §3.1 DF4 (a), `L.x86_64.183`).
//!
//! An x2APIC identifier is a core's number shifted left by the bits that
//! name a thread within it. Leaf `0x1F`, where the processor reports it, and
//! leaf `0xB` otherwise, give that shift as the SMT level's `EAX[4:0]`, and
//! each thread's own identifier as `EDX`. A processor with neither leaf has
//! one thread a core only if `CPUID.1:EDX.HTT` is clear; one that sets `HTT`
//! and gives no leaf cannot be shown to run one thread a core, and the boot
//! stops rather than guess.

use core::sync::atomic::{AtomicU32, Ordering};

/// `CPUID.1:EDX.HTT`: more than one logical processor in the package may
/// share it.
const HTT: u32 = 1 << 28;
/// A topology leaf's level type, `ECX[15:8]`, for the SMT level.
const LEVEL_SMT: u32 = 1;
/// Levels a topology leaf may list before the walk gives up: the SDM names
/// six types, so a seventh subleaf that is not the end is a broken leaf.
const MAX_LEVELS: u32 = 8;

/// What the boot processor read: the shift, plus one so that zero means
/// "not read yet".
static SHIFT: AtomicU32 = AtomicU32::new(0);

/// Where the topology came from, for the boot line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Leaf `0x1F`.
    Leaf1F,
    /// Leaf `0xB`.
    LeafB,
    /// Neither leaf, and `HTT` clear: one thread a core.
    NoHtt,
}

/// The topology leaf this processor reports, if any.
fn leaf() -> Option<(u32, Source)> {
    let max = core::arch::x86_64::__cpuid(0).eax;
    // A leaf is reported when its subleaf 0 names a level (`EBX[15:0]`, the
    // processors at that level, is not zero).
    let reports =
        |leaf: u32| max >= leaf && core::arch::x86_64::__cpuid_count(leaf, 0).ebx & 0xFFFF != 0;
    if reports(0x1F) {
        Some((0x1F, Source::Leaf1F))
    } else if reports(0xB) {
        Some((0xB, Source::LeafB))
    } else {
        None
    }
}

/// The SMT shift and where it came from, read on this processor.
///
/// # Errors
///
/// A processor that sets `HTT` and reports no topology leaf, or a leaf whose
/// levels never end.
pub(crate) fn read() -> Result<(u32, Source), &'static str> {
    let Some((leaf, source)) = leaf() else {
        if core::arch::x86_64::__cpuid(1).edx & HTT == 0 {
            return Ok((0, Source::NoHtt));
        }
        return Err(
            "smt: CPUID reports more than one thread a package (HTT) and no topology leaf, so \
             which threads share a core cannot be known",
        );
    };
    for subleaf in 0..MAX_LEVELS {
        let level = core::arch::x86_64::__cpuid_count(leaf, subleaf);
        let kind = (level.ecx >> 8) & 0xFF;
        if kind == 0 {
            // No SMT level: one thread a core.
            return Ok((0, source));
        }
        if kind == LEVEL_SMT {
            return Ok((level.eax & 0x1F, source));
        }
    }
    Err("smt: CPUID's topology leaf lists more levels than there are")
}

/// Read the topology on the boot processor and keep its shift.
///
/// # Errors
///
/// As [`read`].
pub(crate) fn discover() -> Result<(u32, Source), &'static str> {
    let (shift, source) = read()?;
    SHIFT.store(shift + 1, Ordering::Release);
    Ok((shift, source))
}

/// The boot processor's SMT shift, or zero before [`discover`].
pub(crate) fn shift() -> u32 {
    SHIFT.load(Ordering::Acquire).saturating_sub(1)
}

/// The thread number within its core of the processor named `id`.
pub(crate) const fn thread_of(id: u64, shift: u32) -> u64 {
    id & ((1 << shift) - 1)
}

/// The core number of the processor named `id`.
pub(crate) const fn core_of(id: u64, shift: u32) -> u64 {
    id >> shift
}

/// This processor's x2APIC identifier as its own topology leaf gives it, or
/// its initial APIC identifier (`CPUID.1:EBX[31:24]`) with no leaf (P3: each
/// thread reads its own).
pub(crate) fn own_id() -> u64 {
    match leaf() {
        Some((leaf, _)) => u64::from(core::arch::x86_64::__cpuid_count(leaf, 0).edx),
        None => u64::from(core::arch::x86_64::__cpuid(1).ebx >> 24),
    }
}
