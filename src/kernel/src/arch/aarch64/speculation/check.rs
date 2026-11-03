//! The boot check of this architecture's speculation defences: what the
//! part tables decide for cores the machine may not have, what firmware's
//! answers mean, and that the entry loop runs exactly when a processor
//! recorded that it needs it.
//!
//! Verification, not the defences: a file of its own so that the manifest
//! counts it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`) and a `Verifies:` tag can go on it. A child of
//! `speculation`, so that it reads the tables and records that are private
//! there.

use core::sync::atomic::Ordering;

use ferrix_sched_class::MAX_CPUS;

use super::{
    Answer, BHB_LOOPS, Core, Defences, Exposure, Firmware, HARDENED, SEEN, Seen, applied_by,
    midr_of, plan_for,
};

/// What the boot check adds on this architecture: that the entry loop runs
/// exactly when some processor recorded that it needs it.
///
/// The count is one for the whole machine -- every vector entry reads the same
/// word -- so it is held to every processor's record, not to the boot
/// processor's plan, which on a machine of mixed cores says nothing about the
/// big ones.
///
/// # Errors
///
/// A processor that needs the loop and no count, or a count that no processor
/// asked for.
///
/// Verifies: L.aarch64.41
pub(crate) fn check() -> Result<(), &'static str> {
    check_part_tables()?;
    check_firmware_answers()?;
    check_midr_records()?;
    check_moving_midr()?;
    let needed = (0..crate::smp::count()).any(|logical| {
        applied_by(logical).is_some_and(|applied| applied.contains(Defences::BHB_LOOP))
    });
    if needed != (BHB_LOOPS.load(Ordering::Relaxed) != 0) {
        return Err("the branch history loop's count disagrees with what the processors recorded");
    }
    Ok(())
}

/// `midr_of`, which `/proc/cpuinfo` reads each processor's identity
/// through: for every processor that started, the `MIDR_EL1` its record
/// holds; `None` past the last processor, for a slot no processor recorded
/// in, and everywhere in a kernel built without its defences, which records
/// nothing. And the property the records serve: the entry loop in force is
/// at least what each recorded core needs, and what the core this check
/// runs on needs by the `MIDR_EL1` it reads now -- not that the register
/// still equals the record, which under a hypervisor it need not (FX-0307).
///
/// # Errors
///
/// A processor's answer that is not its record, an answer where nothing was
/// recorded, or a loop weaker than a core seen needs.
///
/// Verifies: L.aarch64.67
fn check_midr_records() -> Result<(), &'static str> {
    let online = |logical: usize| {
        crate::smp::topology()
            .and_then(|topology| topology.cpus().get(logical))
            .is_some_and(crate::smp::PerCpu::is_online)
    };
    let started = crate::smp::count();
    for logical in (0..started).filter(|&logical| online(logical)) {
        let recorded = SEEN
            .get(logical)
            .and_then(|slot| Seen::unpack(slot.load(Ordering::Acquire)))
            .and_then(|seen| u32::try_from(seen.core.midr).ok());
        if HARDENED && recorded.is_none() {
            return Err("a processor that started recorded no MIDR");
        }
        if midr_of(logical) != recorded.filter(|_| HARDENED) {
            return Err("midr_of does not answer a processor's recorded MIDR");
        }
        let needs = SEEN
            .get(logical)
            .and_then(|slot| Seen::unpack(slot.load(Ordering::Acquire)))
            .map_or(0, |seen| seen.core.bhb_loops());
        if BHB_LOOPS.load(Ordering::Relaxed) < needs {
            return Err("the entry loop is weaker than a recorded core needs");
        }
    }
    if HARDENED && BHB_LOOPS.load(Ordering::Relaxed) < Core::read().bhb_loops() {
        return Err("the entry loop is weaker than the core this processor reads now needs");
    }
    if (started..=MAX_CPUS).any(|logical| midr_of(logical).is_some()) {
        return Err("midr_of answers for a processor that recorded nothing");
    }
    Ok(())
}

/// The decision where a processor's `MIDR_EL1` may change under it
/// (FX-0307): one vCPU recorded a Cortex-A55, which needs no loop, and later
/// reads a Cortex-X1, which needs 32 branches. Under a hypervisor the count
/// decided from the A55's record must cover the X1, and Arm's Spectre v2
/// list must not excuse the A55; on a machine without one each core keeps
/// Linux's own figure, and `ECBHB` still needs no loop.
///
/// # Errors
///
/// A count decided from the first core that does not cover the later one,
/// or a decision that differs from the figures above.
///
/// Verifies: L.aarch64.41
fn check_moving_midr() -> Result<(), &'static str> {
    let core = |part: u64, ecbhb| Core {
        midr: 0x41 << 24 | part << 4,
        csv2: false,
        csv3: false,
        ssbs: 0,
        ecbhb,
    };
    let (a55, x1) = core::hint::black_box((core(0xD05, false), core(0xD44, false)));
    // The sequence one vCPU read: the record decides the count in force.
    let in_force = |unstable| {
        [a55]
            .iter()
            .map(|seen| seen.bhb_loops_needed(unstable))
            .max()
            .unwrap_or(0)
    };
    if in_force(true) < x1.bhb_loops() {
        return Err("a count decided under a hypervisor does not cover a core the vCPU moves to");
    }
    if in_force(false) != 0 || x1.bhb_loops_needed(false) != 32 {
        return Err("a core without a hypervisor was not given Linux's own figure");
    }
    if core(0xD44, true).bhb_loops_needed(true) != 0 {
        return Err("a core with ECBHB under a hypervisor was given the loop");
    }
    let firmware = Firmware {
        workaround_1: Answer::Required,
        workaround_2: Answer::NotSupported,
    };
    let plan = plan_for(a55, firmware, true);
    if !plan.contains(Defences::BHB_LOOP) || !plan.contains(Defences::SWITCH_BARRIER) {
        return Err("an A55 recorded under a hypervisor was excused by Arm's lists");
    }
    if plan_for(a55, firmware, false) != Defences::CLAMPED_INDICES {
        return Err("an A55 without a hypervisor was not excused by Arm's lists");
    }
    Ok(())
}

/// What the tables say about cores the machine may not have: the plan for a
/// Pixel 7's Cortex-A78 or a server's Neoverse N1 is decided from them, and a
/// QEMU boot runs one part. Held to Linux's figures
/// (`arch/arm64/kernel/proton-pack.c`, `cpufeature.c`) for one part of each
/// row, and for a core another company designed, which no Arm row names.
///
/// Verifies: L.aarch64.39
fn check_part_tables() -> Result<(), &'static str> {
    let core = |implementer: u64, part: u64, ecbhb| Core {
        midr: implementer << 24 | part << 4,
        csv2: false,
        csv3: false,
        ssbs: 0,
        ecbhb,
    };
    // A Cortex-A76 whose branch history is not shared needs no loop.
    let ecbhb = core(0x41, 0xD0B, true);
    // (core, branches the BHB loop takes, listed safe from Spectre v2,
    // safe from Meltdown, in order)
    let rows = [
        (core(0x41, 0xD4E, false), 132, false, true, false), // Cortex-X3
        (core(0x41, 0xD4D, false), 38, false, true, false),  // Cortex-A715
        (core(0x41, 0xD41, false), 32, false, true, false),  // Cortex-A78
        (core(0x41, 0xD44, false), 32, false, true, false),  // Cortex-X1
        (core(0x41, 0xD0B, false), 24, false, true, false),  // Cortex-A76
        (core(0x41, 0xD0C, false), 24, false, true, false),  // Neoverse N1
        (core(0x41, 0xD08, false), 8, false, true, false),   // Cortex-A72
        (core(0x41, 0xD0A, false), 0, false, false, false),  // Cortex-A75
        (core(0x41, 0xD05, false), 0, true, true, true),     // Cortex-A55
        (core(0x41, 0xD03, false), 0, true, true, true),     // Cortex-A53
        (ecbhb, 0, false, true, false),
        // Qualcomm's: on none of Arm's lists.
        (core(0x51, 0xD0B, false), 0, false, false, false),
    ];
    for (core, loops, v2_safe, meltdown_safe, in_order) in rows {
        let core = core::hint::black_box(core);
        if core.bhb_loops() != loops
            || core.v2_listed_safe() != v2_safe
            || core.meltdown_safe() != meltdown_safe
            || core.in_order() != in_order
        {
            return Err("a core was not given the defences Linux's tables give it");
        }
    }
    let seen = Seen {
        core: ecbhb,
        firmware: Firmware::default(),
    };
    let exposure = core::hint::black_box(Exposure(seen, Defences::CLAMPED_INDICES));
    // Into a buffer on the stack: the item allocates only fallibly, and a
    // report line fits.
    let mut text = Line::default();
    if core::fmt::write(&mut text, format_args!("{exposure}")).is_err()
        || !text.holds("Spectre-BHB not affected (ECBHB)")
    {
        return Err("a core with ECBHB was not reported as unaffected by Spectre-BHB");
    }
    Ok(())
}

/// One report line, written into a fixed buffer; what does not fit is an
/// error, not a truncation.
struct Line {
    bytes: [u8; 256],
    len: usize,
}

impl Default for Line {
    fn default() -> Line {
        Line {
            bytes: [0; 256],
            len: 0,
        }
    }
}

impl Line {
    /// Whether the line contains `text`.
    fn holds(&self, text: &str) -> bool {
        self.bytes.get(..self.len).is_some_and(|line| {
            line.windows(text.len())
                .any(|window| window == text.as_bytes())
        })
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len.checked_add(text.len()).ok_or(core::fmt::Error)?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// What firmware's `SMCCC_ARCH_FEATURES` status means (ARM DEN 0028D,
/// §7.5.2 and §7.6.2), every status it can return, and that each answer
/// survives the record it is packed into. Firmware that answers at all is
/// the Pixel 7's, not QEMU's.
///
/// Verifies: L.aarch64.40
fn check_firmware_answers() -> Result<(), &'static str> {
    let statuses = [
        (0, Answer::Required),
        (1, Answer::NotRequired),
        (-2, Answer::AlwaysOn),
        (-1, Answer::NotSupported),
        (-3, Answer::NotSupported),
    ];
    for (status, answer) in statuses {
        let status = core::hint::black_box(status);
        if Answer::from_status(status) != answer || Answer::from_bits(answer.bits()) != answer {
            return Err("an SMCCC answer about a workaround was read as another");
        }
    }
    Ok(())
}
