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

/// One flag: its name on the line, and each value with whether this build
/// was compiled with it.
struct Flag {
    name: &'static str,
    values: &'static [(bool, &'static str)],
}

/// The profile, then the level, then every flag in the flag file's order.
const FLAGS: [Flag; 16] = [
    Flag {
        name: "profile",
        values: &[
            (cfg!(ferrix_profile = "present"), "present"),
            (cfg!(ferrix_profile = "desktop"), "desktop"),
            (cfg!(ferrix_profile = "fast"), "fast"),
            (cfg!(ferrix_profile = "max"), "max"),
            (cfg!(ferrix_profile = "none"), "none"),
        ],
    },
    Flag {
        name: "level",
        values: &[
            (cfg!(ferrix_level = "none"), "none"),
            (cfg!(ferrix_level = "present"), "present"),
            (cfg!(ferrix_level = "sc2"), "sc2"),
            (cfg!(ferrix_level = "sc4"), "sc4"),
        ],
    },
    Flag {
        name: "personality",
        values: &[
            (cfg!(ferrix_personality = "user"), "user"),
            (cfg!(ferrix_personality = "kernel"), "kernel"),
            (cfg!(ferrix_personality = "none"), "none"),
        ],
    },
    Flag {
        name: "fs",
        values: &[
            (cfg!(ferrix_fs = "server"), "server"),
            (cfg!(ferrix_fs = "kernel"), "kernel"),
            (cfg!(ferrix_fs = "none"), "none"),
        ],
    },
    Flag {
        name: "mitigations",
        values: &[
            (cfg!(ferrix_mitigations = "on"), "on"),
            (cfg!(ferrix_mitigations = "off"), "off"),
        ],
    },
    Flag {
        name: "partition_speculation",
        values: &[
            (cfg!(ferrix_partition_speculation = "exit"), "exit"),
            (
                cfg!(ferrix_partition_speculation = "partition"),
                "partition",
            ),
            (
                cfg!(ferrix_partition_speculation = "not_applicable"),
                "not_applicable",
            ),
        ],
    },
    Flag {
        name: "ipc_fastpath",
        values: &[
            (cfg!(ferrix_ipc_fastpath = "off"), "off"),
            (cfg!(ferrix_ipc_fastpath = "on"), "on"),
        ],
    },
    Flag {
        name: "restricted_fastpath",
        values: &[
            (cfg!(ferrix_restricted_fastpath = "off"), "off"),
            (cfg!(ferrix_restricted_fastpath = "on"), "on"),
            (
                cfg!(ferrix_restricted_fastpath = "not_applicable"),
                "not_applicable",
            ),
        ],
    },
    Flag {
        name: "kernel_memory",
        values: &[
            (cfg!(ferrix_kernel_memory = "retyped"), "retyped"),
            (cfg!(ferrix_kernel_memory = "heap"), "heap"),
        ],
    },
    Flag {
        name: "schedule",
        values: &[
            (cfg!(ferrix_schedule = "windows"), "windows"),
            (cfg!(ferrix_schedule = "edf"), "edf"),
            (cfg!(ferrix_schedule = "fair"), "fair"),
        ],
    },
    Flag {
        name: "checks",
        values: &[
            (cfg!(ferrix_checks = "on"), "on"),
            (cfg!(ferrix_checks = "off"), "off"),
        ],
    },
    Flag {
        name: "program_barrier",
        values: &[
            (cfg!(ferrix_program_barrier = "switch"), "switch"),
            (cfg!(ferrix_program_barrier = "session"), "session"),
        ],
    },
    Flag {
        name: "ssbd",
        values: &[
            (cfg!(ferrix_ssbd = "always"), "always"),
            (cfg!(ferrix_ssbd = "on_request"), "on_request"),
        ],
    },
    Flag {
        name: "smt",
        values: &[
            (cfg!(ferrix_smt = "off"), "off"),
            (cfg!(ferrix_smt = "stibp"), "stibp"),
        ],
    },
    Flag {
        name: "runtime_trades",
        values: &[
            (cfg!(ferrix_runtime_trades = "none"), "none"),
            (cfg!(ferrix_runtime_trades = "game_mode"), "game_mode"),
        ],
    },
    Flag {
        name: "linux_abi",
        values: &[
            (cfg!(ferrix_linux_abi = "on"), "on"),
            (cfg!(ferrix_linux_abi = "off"), "off"),
        ],
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
        if chosen_count(flag.values) != 1 {
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
const fn chosen(values: &'static [(bool, &'static str)]) -> (&'static str, u32) {
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
            write!(out, "{}={}", flag.name, chosen(flag.values).0)?;
        }
        Ok(())
    }
}

/// The set packed for the boot's configuration audit record: in the first
/// word each flag's place in its list, two bits a flag in the flag file's
/// order from bit 0; in the second the profile's place in bits 0 to 3 and
/// the level's in bits 4 to 7. `docs/certification/AUDIT.md` §1 gives the
/// lists a reader decodes them with.
pub fn audit_words() -> (u32, u32) {
    let place = |index: usize| FLAGS.get(index).map_or(0, |flag| chosen(flag.values).1);
    let mut flags = 0_u32;
    for index in 2..FLAGS.len() {
        flags |= (place(index) & 0b11) << ((index - 2) * 2);
    }
    (flags, (place(0) & 0xf) | ((place(1) & 0xf) << 4))
}
