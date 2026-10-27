//! What this kernel was built as: its build flags, read back from the `cfg`s
//! it was compiled with (`docs/BUILD-FLAGS.md`, rule 6).
//!
//! Every word of the `build` boot line comes from a `cfg!` here: the same
//! `cfg(ferrix_<flag> = "<value>")` predicates that select the code, never
//! data the build tool wrote. The gate (`tools/common/check/check-boot-line.py`)
//! compares the line with the profile file the image claims to be, and ties
//! the image's hash to the run, so a resolver that set a wrong value makes a
//! kernel the gate refuses rather than a wrong claim. The same set is the
//! boot's configuration audit record (`FAU_GEN.1` b, `docs/certification/AUDIT.md`
//! §1), packed by [`audit_words`].
//!
//! Each flag must have exactly one value: a build that gave a flag none, or
//! two, fails to compile at the `const` assertions below, so no kernel can
//! print an incomplete line. The flags, their values and their order are the
//! flag file's (`tools/common/data/build-flags.toml`); the build script
//! declares each value to `--check-cfg`, so a value named here that the file
//! does not declare fails the build too.

use crate::console::println;

/// One flag: its name on the line, and its values.
struct Flag {
    name: &'static str,
    values: Values,
}

/// How a flag's value is known here.
enum Values {
    /// Each value with whether this build was compiled with it.
    Cfg(&'static [(bool, &'static str)]),
    /// As the scheduler selection's arm names it (`sched_*`): the arms file
    /// is the only one that reads those flags (SCHED-CLASSES.md §9a, D6).
    Selected(ferrix_kernel_sched_select::Value),
}

/// The profile, then the level, then every flag in the flag file's order.
const FLAGS: [Flag; 21] = [
    Flag {
        name: "profile",
        values: Values::Cfg(&[
            (cfg!(ferrix_profile = "present"), "present"),
            (cfg!(ferrix_profile = "desktop"), "desktop"),
            (cfg!(ferrix_profile = "fast"), "fast"),
            (cfg!(ferrix_profile = "max"), "max"),
            (cfg!(ferrix_profile = "none"), "none"),
        ]),
    },
    Flag {
        name: "level",
        values: Values::Cfg(&[
            (cfg!(ferrix_level = "none"), "none"),
            (cfg!(ferrix_level = "present"), "present"),
            (cfg!(ferrix_level = "sc2"), "sc2"),
            (cfg!(ferrix_level = "sc4"), "sc4"),
        ]),
    },
    Flag {
        name: "personality",
        values: Values::Cfg(&[
            (cfg!(ferrix_personality = "user"), "user"),
            (cfg!(ferrix_personality = "kernel"), "kernel"),
            (cfg!(ferrix_personality = "none"), "none"),
        ]),
    },
    Flag {
        name: "fs",
        values: Values::Cfg(&[
            (cfg!(ferrix_fs = "server"), "server"),
            (cfg!(ferrix_fs = "kernel"), "kernel"),
            (cfg!(ferrix_fs = "none"), "none"),
        ]),
    },
    Flag {
        name: "mitigations",
        values: Values::Cfg(&[
            (cfg!(ferrix_mitigations = "on"), "on"),
            (cfg!(ferrix_mitigations = "off"), "off"),
        ]),
    },
    Flag {
        name: "partition_speculation",
        values: Values::Cfg(&[
            (cfg!(ferrix_partition_speculation = "exit"), "exit"),
            (
                cfg!(ferrix_partition_speculation = "partition"),
                "partition",
            ),
            (
                cfg!(ferrix_partition_speculation = "not_applicable"),
                "not_applicable",
            ),
        ]),
    },
    Flag {
        name: "ipc_fastpath",
        values: Values::Cfg(&[
            (cfg!(ferrix_ipc_fastpath = "off"), "off"),
            (cfg!(ferrix_ipc_fastpath = "on"), "on"),
        ]),
    },
    Flag {
        name: "restricted_fastpath",
        values: Values::Cfg(&[
            (cfg!(ferrix_restricted_fastpath = "off"), "off"),
            (cfg!(ferrix_restricted_fastpath = "on"), "on"),
            (
                cfg!(ferrix_restricted_fastpath = "not_applicable"),
                "not_applicable",
            ),
        ]),
    },
    Flag {
        name: "kernel_memory",
        values: Values::Cfg(&[
            (cfg!(ferrix_kernel_memory = "retyped"), "retyped"),
            (cfg!(ferrix_kernel_memory = "heap"), "heap"),
        ]),
    },
    Flag {
        name: "sched_dispatch",
        values: Values::Selected(ferrix_kernel_sched_select::DISPATCH),
    },
    Flag {
        name: "sched_windows",
        values: Values::Selected(ferrix_kernel_sched_select::WINDOWS),
    },
    Flag {
        name: "sched_deadline",
        values: Values::Selected(ferrix_kernel_sched_select::DEADLINE),
    },
    Flag {
        name: "sched_realtime",
        values: Values::Selected(ferrix_kernel_sched_select::REALTIME),
    },
    Flag {
        name: "sched_fair",
        values: Values::Selected(ferrix_kernel_sched_select::FAIR),
    },
    Flag {
        name: "checks",
        values: Values::Cfg(&[
            (cfg!(ferrix_checks = "on"), "on"),
            (cfg!(ferrix_checks = "off"), "off"),
        ]),
    },
    Flag {
        name: "program_barrier",
        values: Values::Cfg(&[
            (cfg!(ferrix_program_barrier = "switch"), "switch"),
            (cfg!(ferrix_program_barrier = "session"), "session"),
        ]),
    },
    Flag {
        name: "ssbd",
        values: Values::Cfg(&[
            (cfg!(ferrix_ssbd = "always"), "always"),
            (cfg!(ferrix_ssbd = "on_request"), "on_request"),
        ]),
    },
    Flag {
        name: "smt",
        values: Values::Cfg(&[
            (cfg!(ferrix_smt = "off"), "off"),
            (cfg!(ferrix_smt = "stibp"), "stibp"),
        ]),
    },
    Flag {
        name: "runtime_trades",
        values: Values::Cfg(&[
            (cfg!(ferrix_runtime_trades = "none"), "none"),
            (cfg!(ferrix_runtime_trades = "game_mode"), "game_mode"),
        ]),
    },
    Flag {
        name: "linux_abi",
        values: Values::Cfg(&[
            (cfg!(ferrix_linux_abi = "on"), "on"),
            (cfg!(ferrix_linux_abi = "off"), "off"),
        ]),
    },
    Flag {
        name: "runtime_settings",
        values: Values::Cfg(&[
            (cfg!(ferrix_runtime_settings = "fixed"), "fixed"),
            (cfg!(ferrix_runtime_settings = "live"), "live"),
        ]),
    },
];

/// How many of `values` this build was compiled with.
const fn chosen_count(values: &[(bool, &str)]) -> usize {
    let mut count = 0;
    let mut rest = values;
    while let [(chosen, _), tail @ ..] = rest {
        count += *chosen as usize;
        rest = tail;
    }
    count
}

/// Whether every flag has exactly one value.
const fn each_has_one(flags: &[Flag]) -> bool {
    let mut rest = flags;
    while let [flag, tail @ ..] = rest {
        // A selected value is the arm that compiled: none, or two, does not.
        if let Values::Cfg(values) = flag.values
            && chosen_count(values) != 1
        {
            return false;
        }
        rest = tail;
    }
    true
}

const _: () = assert!(
    each_has_one(&FLAGS),
    "a build flag has no value, or more than one: build the kernel through cargo xtask"
);

/// The value this build was compiled with, and its place in the flag's list.
const fn chosen(values: &Values) -> (&'static str, u32) {
    let values = match values {
        Values::Cfg(values) => *values,
        Values::Selected(value) => return (value.name, value.place),
    };
    let mut at = 0;
    let mut rest = values;
    while let [(set, value), tail @ ..] = rest {
        if *set {
            return (value, at);
        }
        at += 1;
        rest = tail;
    }
    ("?", at)
}

/// Whether this build carries no claim: an ad-hoc one (rule 5).
const AD_HOC: bool = cfg!(ferrix_profile = "none");

/// Print the `build` line: `profile=`, `level=`, then each flag, and for an
/// ad-hoc build `no claim`.
pub fn report() {
    println!(
        "  build    {Words}{}",
        if AD_HOC { " ; no claim" } else { "" }
    );
}

/// The line's `name=value` words.
struct Words;

impl core::fmt::Display for Words {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, flag) in FLAGS.iter().enumerate() {
            if index > 0 {
                out.write_str(" ")?;
            }
            write!(out, "{}={}", flag.name, chosen(&flag.values).0)?;
        }
        Ok(())
    }
}

/// The set packed for the boot's configuration audit record: in the first
/// word each flag's place in its list, two bits a flag in the flag file's
/// order from bit 0, the scheduler's flags left out; in the second the
/// profile's place in bits 0 to 3, the level's in bits 4 to 7, then the
/// scheduler's, `sched_dispatch` in bits 8 and 9 and each tier in three
/// bits from bit 10, in the flag file's order (SCHED-CLASSES.md §9a, D8). `docs/certification/AUDIT.md` §1 gives the
/// lists a reader decodes them with.
pub fn audit_words() -> (u32, u32) {
    let place = |index: usize| FLAGS.get(index).map_or(0, |flag| chosen(&flag.values).1);
    let (mut flags, mut at) = (0_u32, 0);
    let mut second = (place(0) & 0xf) | ((place(1) & 0xf) << 4);
    let mut tier = 0;
    for (index, flag) in FLAGS.iter().enumerate().skip(2) {
        match flag.values {
            Values::Cfg(_) => {
                flags |= (place(index) & 0b11) << at;
                at += 2;
            }
            Values::Selected(value) if tier == 0 => {
                second |= (value.place & 0b11) << 8;
                tier += 1;
            }
            Values::Selected(value) => {
                second |= (value.place & 0b111) << (10 + (tier - 1) * 3);
                tier += 1;
            }
        }
    }
    (flags, second)
}
