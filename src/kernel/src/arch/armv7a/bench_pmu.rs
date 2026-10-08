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

use ferrix_bootinfo::{BootView, option_in};
use ferrix_fdt::Fdt;

use super::cpu;
use crate::console::println;

/// The command-line option.
const OPTION: &str = "ipc-bench.pmu";

/// Whether every processor gives user mode the PMU. Written once, by
/// [`read_option`] on the boot processor, before [`allow_user_pmu`] first
/// runs.
static ON: AtomicBool = AtomicBool::new(false);

/// Read `ipc-bench.pmu`, once, on the boot processor, before `timer::init`,
/// and say what it decided.
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
    let version = cpu::pmu_version();
    if version == 0 || version == 0xF {
        println!(
            "  pmu      {OPTION} asked, but ID_DFR0.PerfMon is {version:#x}: no PMU, user mode keeps no access"
        );
        return;
    }
    ON.store(true, Ordering::Relaxed);
    println!(
        "  pmu      {OPTION} on (MEASUREMENT ONLY): PMUSERENR.EN set on every processor as it starts, \
         ID_DFR0.PerfMon {version:#x}, PMCR {:#010x}",
        cpu::read_pmcr()
    );
}

/// Give user mode this processor's PMU when [`read_option`] said so; nothing
/// otherwise. A processor's own register: every processor runs this beside
/// `cpu::allow_user_counter`.
pub(super) fn allow_user_pmu() {
    if ON.load(Ordering::Relaxed) {
        cpu::allow_user_pmu();
    }
}
