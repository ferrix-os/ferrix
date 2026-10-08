//! MEASUREMENT ONLY (branch `os4b/b3-ferrix`, never lands): `ipc-bench.pmu`,
//! the switch that gives user mode the performance monitors, so `ipc-bench`
//! times the round trip on the STM32MP157D-DK1 exactly as the Linux and seL4
//! benches do (`docs/BOARD-BENCH.md`, "Counters";
//! `tools/common/bench/board/common/board-bench.h`).
//!
//! **Off unless asked for.** With `ipc-bench.pmu=1` (or `on`) on the command
//! line -- the loader's first, then the device tree's `/chosen/bootargs`,
//! read as `ferrix.fastpath` is -- every processor sets `PMUSERENR.EN` beside
//! `cpu::allow_user_counter`, before it can run a program: the boot processor
//! from `timer::init`, each secondary in `smp::secondary_start`. A program
//! may then program and read the cycle counter and the event counters
//! (`PMCR`, `PMSELR`, `PMXEVTYPER`, `PMCNTENSET`, `PMCCNTR`, `PMXEVCNTR`),
//! which also lets it read every mode's cycles and reset counters another
//! program set: a side channel and a covert channel, which is why this is a
//! measurement switch and not a configuration. `PMINTENSET`, `PMINTENCLR` and
//! `PMUSERENR` itself stay the kernel's.
//!
//! A core whose `ID_DFR0.PerfMon` names no PMU is left alone, and the line
//! says so: there the registers would be undefined instructions.

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::{BootView, flag_in, option_in};
use ferrix_fdt::Fdt;

use super::cpu;
use crate::console::println;

/// The command-line option.
const OPTION: &str = "ipc-bench.pmu";

/// Whether every processor gives user mode the PMU. Written once, by
/// [`read_option`] on the boot processor, before [`allow_user_pmu`] first
/// runs.
static ON: AtomicBool = AtomicBool::new(false);

/// Whether [`allow_user_pmu`] has run once: the boot processor's call, the
/// first, reads the register back and says what it found.
static REPORTED: AtomicBool = AtomicBool::new(false);

/// The commit the kernel was built from, as `cargo xtask` read it
/// (`FERRIX_COMMIT`, with `-dirty` for a tree with changes), or `unknown`
/// for a kernel built by cargo alone.
const COMMIT: &str = match option_env!("FERRIX_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

/// Read `ipc-bench.pmu`, once, on the boot processor, before `timer::init`,
/// and say what it decided. With it on, also the record every board-bench
/// boot starts with (`docs/BOARD-BENCH.md`): the kernel's commit, and the
/// boot processor's `SCTLR` and `ACTLR` (`noactlr` leaves `ACTLR` unread).
pub(super) fn read_option(view: &BootView<'_>, tree: &Fdt<'_>) {
    let value = view
        .option(OPTION)
        .or_else(|| option_in(tree.bootargs()?, OPTION));
    let asked = match value {
        None => return,
        Some("1" | "on") => true,
        Some("0" | "off") => false,
        Some(other) => {
            println!(
                "  pmu      {OPTION}={other} is not understood; user mode keeps no PMU access"
            );
            return;
        }
    };
    if !asked {
        println!("  pmu      {OPTION} off: user mode keeps no PMU access");
        return;
    }
    let args = tree.bootargs().unwrap_or("");
    let sctlr = cpu::read_sctlr();
    if view.flag("noactlr") || flag_in(args, "noactlr") {
        println!("  bench    kernel {COMMIT}, SCTLR {sctlr:#010x}, ACTLR not read (noactlr)");
    } else {
        println!(
            "  bench    kernel {COMMIT}, SCTLR {sctlr:#010x}, ACTLR {:#010x} (boot processor)",
            cpu::read_actlr()
        );
    }
    let version = cpu::pmu_version();
    if version == 0 || version == 0xF {
        println!(
            "  pmu      {OPTION} asked, but ID_DFR0.PerfMon is {version:#x}: no PMU, user mode keeps no access"
        );
        return;
    }
    ON.store(true, Ordering::Relaxed);
    println!(
        "  pmu      {OPTION} on (MEASUREMENT ONLY): PMUSERENR.EN set on every processor as it \
         starts, ID_DFR0.PerfMon {version:#x}, PMCR {:#010x}",
        cpu::read_pmcr()
    );
}

/// Give user mode this processor's PMU when [`read_option`] said so; nothing
/// otherwise. A processor's own register: every processor runs this beside
/// `cpu::allow_user_counter`. The first call, the boot processor's from
/// `timer::init`, reads `PMUSERENR` back and prints it.
pub(super) fn allow_user_pmu() {
    if !ON.load(Ordering::Relaxed) {
        return;
    }
    cpu::allow_user_pmu();
    if !REPORTED.swap(true, Ordering::Relaxed) {
        let read = cpu::read_pmuserenr();
        let en = if read & 1 == 0 { "clear" } else { "set" };
        println!("  pmu      PMUSERENR {read:#010x} on the boot processor: EN {en}");
    }
}
