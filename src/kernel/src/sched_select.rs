//! The scheduler stack this kernel is built with, chosen by the build flags
//! before the core (docs/SCHED-CLASSES.md §5 and §9a; the consultant's design
//! review of step 3, ledger line 20, D1-D13).
//!
//! This file is the arms file: the one file that selects code by a `sched_*`
//! build flag (D6, rule 6e's one exception, S2). Each value of each tier flag
//! has one hand-written arm, giving the tier as a generic alias, `absent` the
//! identity, so the stack is only the tiers present: the fair-only stack is
//! exactly `Eevdf`, and `Over` stands only between two present tiers (D3).
//! Each arm also names its value and its place in the flag file's list
//! ([`Value`]), which the `build` line and the audit record read instead of a
//! `cfg!` of their own. A value not built yet is a `compile_error!` naming it
//! (D7). check-build-flags.py holds the arms one to one with the flag file.
//!
//! It is also where every kernel crate's flag word is made
//! ([`build_flag_word!`], [`flag_word`], ledger line 3282 (K4)): the lowest
//! of the kernel's crates, so the core re-exports both.

#![no_std]
#![forbid(unsafe_code)]
// The fixture class crate is a dependency no file names (SCHED-CLASSES.md
// §9b, G2): naming it, even `as _`, would load it and hand its rlib to the
// linker, the opposite of what it shows. check-item-boundary's rule (d)
// refuses any name of it, so this lint is the only thing it is excused from.
#![expect(
    unused_crate_dependencies,
    reason = "ferrix_sched_fixture, the standing negative case no build links (SCHED-CLASSES.md §9b)"
)]

use ferrix_sched_class::SchedClass;
// Renamed: check-item-boundary.py reads every kernel crate root as one
// module, where a bare `Mode` is the load ring's btrfs_powerfail one.
use ferrix_sched_class::Mode as SchedMode;

/// The build flags the crate expanding it was compiled with, for
/// [`flag_word`] to make one word of: one `bool` for each value of the
/// profile, the level and every flag, in the flag file's order, each the
/// `cfg!` of its value where the macro is expanded.
/// Each of the kernel's crates expands it into a constant of its own, and the
/// binary requires them equal at compile time (ledger line 3282, (K4)):
/// the build line is computed from the item's `cfg`s, so a crate compiled
/// with others would run code the line does not name. `check-build-flags.py`
/// holds this list to the flag file.
#[macro_export]
macro_rules! build_flag_word {
    () => {
        [
            cfg!(ferrix_profile = "present"),
            cfg!(ferrix_profile = "desktop"),
            cfg!(ferrix_profile = "fast"),
            cfg!(ferrix_profile = "max"),
            cfg!(ferrix_profile = "none"),
            cfg!(ferrix_profile = "max_plus"),
            cfg!(ferrix_level = "none"),
            cfg!(ferrix_level = "present"),
            cfg!(ferrix_level = "sc2"),
            cfg!(ferrix_level = "sc4"),
            cfg!(ferrix_level = "sc4_plus"),
            cfg!(ferrix_personality = "user"),
            cfg!(ferrix_personality = "kernel"),
            cfg!(ferrix_personality = "none"),
            cfg!(ferrix_fs = "server"),
            cfg!(ferrix_fs = "kernel"),
            cfg!(ferrix_fs = "none"),
            cfg!(ferrix_mitigations = "on"),
            cfg!(ferrix_mitigations = "off"),
            cfg!(ferrix_partition_speculation = "exit"),
            cfg!(ferrix_partition_speculation = "partition"),
            cfg!(ferrix_partition_speculation = "not_applicable"),
            cfg!(ferrix_ipc_fastpath = "off"),
            cfg!(ferrix_ipc_fastpath = "on"),
            cfg!(ferrix_restricted_fastpath = "off"),
            cfg!(ferrix_restricted_fastpath = "on"),
            cfg!(ferrix_restricted_fastpath = "not_applicable"),
            cfg!(ferrix_kernel_memory = "retyped"),
            cfg!(ferrix_kernel_memory = "heap"),
            cfg!(ferrix_sched_dispatch = "fixed"),
            cfg!(ferrix_sched_dispatch = "switchable"),
            cfg!(ferrix_sched_windows = "arinc"),
            cfg!(ferrix_sched_windows = "absent"),
            cfg!(ferrix_sched_windows = "arinc_table"),
            cfg!(ferrix_sched_deadline = "edf_cbs"),
            cfg!(ferrix_sched_deadline = "absent"),
            cfg!(ferrix_sched_realtime = "fifo_rr"),
            cfg!(ferrix_sched_realtime = "absent"),
            cfg!(ferrix_sched_fair = "eevdf"),
            cfg!(ferrix_sched_fair = "absent"),
            cfg!(ferrix_checks = "on"),
            cfg!(ferrix_checks = "off"),
            cfg!(ferrix_program_barrier = "switch"),
            cfg!(ferrix_program_barrier = "session"),
            cfg!(ferrix_ssbd = "always"),
            cfg!(ferrix_ssbd = "on_request"),
            cfg!(ferrix_smt = "off"),
            cfg!(ferrix_smt = "stibp"),
            cfg!(ferrix_runtime_trades = "none"),
            cfg!(ferrix_runtime_trades = "game_mode"),
            cfg!(ferrix_linux_abi = "on"),
            cfg!(ferrix_linux_abi = "off"),
            cfg!(ferrix_runtime_settings = "fixed"),
            cfg!(ferrix_runtime_settings = "live"),
        ]
    };
}

/// [`build_flag_word!`]'s values as bits, the first lowest. More than 64
/// values do not compile (D5): a value past the word would be one two
/// crates could differ in unseen. With the scheduler's flags the word holds
/// 51 (docs/BUILD-FLAGS.md).
#[must_use]
pub const fn flag_word(chosen: &[bool]) -> u64 {
    assert!(
        chosen.len() <= 64,
        "build_flag_word! names more values than a word holds"
    );
    let mut word = 0_u64;
    let mut at = 0;
    let mut rest = chosen;
    while let [set, tail @ ..] = rest {
        if *set {
            word |= 1 << at;
        }
        at += 1;
        rest = tail;
    }
    word
}

/// The build flags this crate was compiled with ([`build_flag_word!`]).
pub const SELECT_FLAG_WORD: u64 = flag_word(&build_flag_word!());

/// A scheduler flag's value as its arm names it: for the `build` line and
/// the audit record, which read it instead of a `cfg!` (D6).
#[derive(Clone, Copy, Debug)]
pub struct Value {
    /// The value's word, as the flag file and the profile name it.
    pub name: &'static str,
    /// Its place in the flag's list in the flag file, from 0.
    pub place: u32,
    /// Whether its tier holds a class (every value but `absent`).
    pub present: bool,
}

// sched_dispatch: how the stack is dispatched.
/// `sched_dispatch` as this build has it.
#[cfg(ferrix_sched_dispatch = "fixed")]
pub const DISPATCH: Value = Value {
    name: "fixed",
    place: 0,
    present: true,
};
#[cfg(ferrix_sched_dispatch = "switchable")]
compile_error!("sched_dispatch=switchable is not built yet (SCHED-CLASSES.md §5a, step 5)");

// sched_windows: the static time windows, outermost.
#[cfg(ferrix_sched_windows = "arinc")]
compile_error!("sched_windows=arinc is not built yet (ferrix-sched-windows, M3)");
/// `sched_windows` as this build has it.
#[cfg(ferrix_sched_windows = "absent")]
pub const WINDOWS: Value = Value {
    name: "absent",
    place: 1,
    present: false,
};
/// The windows tier over `L`: `L` itself while it is absent.
#[cfg(ferrix_sched_windows = "absent")]
pub type Windows<L> = L;
#[cfg(ferrix_sched_windows = "arinc_table")]
compile_error!(
    "sched_windows=arinc_table is not built yet (ferrix-sched-windows-table, M3; SCHED-CLASSES.md section 9c)"
);

// sched_deadline: HardRt's EDF with CBS.
#[cfg(ferrix_sched_deadline = "edf_cbs")]
compile_error!("sched_deadline=edf_cbs is not built yet (ferrix-sched-edf, M3)");
/// `sched_deadline` as this build has it.
#[cfg(ferrix_sched_deadline = "absent")]
pub const DEADLINE: Value = Value {
    name: "absent",
    place: 1,
    present: false,
};
/// The deadline tier over `L`: `L` itself while it is absent.
#[cfg(ferrix_sched_deadline = "absent")]
pub type Deadline<L> = L;

// sched_realtime: SoftRt's fixed priorities.
#[cfg(ferrix_sched_realtime = "fifo_rr")]
compile_error!("sched_realtime=fifo_rr is not built yet (ferrix-sched-fifo, stage 14)");
/// `sched_realtime` as this build has it.
#[cfg(ferrix_sched_realtime = "absent")]
pub const REALTIME: Value = Value {
    name: "absent",
    place: 1,
    present: false,
};
/// The realtime tier over `L`: `L` itself while it is absent.
#[cfg(ferrix_sched_realtime = "absent")]
pub type Realtime<L> = L;

// sched_fair: EEVDF.
/// `sched_fair` as this build has it.
#[cfg(ferrix_sched_fair = "eevdf")]
pub const FAIR: Value = Value {
    name: "eevdf",
    place: 0,
    present: true,
};
/// The fair tier.
#[cfg(ferrix_sched_fair = "eevdf")]
pub type Fair = ferrix_sched_fair::Eevdf;
/// `sched_fair` as this build has it.
#[cfg(ferrix_sched_fair = "absent")]
pub const FAIR: Value = Value {
    name: "absent",
    place: 1,
    present: false,
};
/// The fair tier, absent: a class nothing can be queued on.
#[cfg(ferrix_sched_fair = "absent")]
pub type Fair = ferrix_sched_class::Empty;

/// The stack: the tiers present, outermost first.
#[cfg(ferrix_sched_dispatch = "fixed")]
pub type Stack = Windows<Deadline<Realtime<Fair>>>;

/// What the stack is asked about its name with: a class's name depends on
/// no payload.
struct Probe;

/// A stack with no tier present has nothing to run and no name to print
/// (line 15's C1).
const _: () = assert!(
    !<Stack as SchedClass<Probe>>::NAME.is_empty(),
    "the build flags leave every scheduler tier absent"
);

/// Whether `mode`'s tiers are compiled in (D9): Throughput needs the fair
/// tier, SoftRt the realtime and fair tiers, HardRt the deadline tier or the
/// windows tier, whose crate schedules by fixed priority inside its windows
/// (SCHED-CLASSES.md C6). A domain
/// asked for a mode this answers no for is refused `ModeUnavailable`.
#[must_use]
pub const fn mode_available(mode: SchedMode) -> bool {
    match mode {
        SchedMode::Throughput => FAIR.present,
        SchedMode::SoftRt => REALTIME.present && FAIR.present,
        SchedMode::HardRt => DEADLINE.present || WINDOWS.present,
    }
}
