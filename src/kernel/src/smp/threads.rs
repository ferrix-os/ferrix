//! What the Arm architectures' firmware tables say about hardware threads
//! (`smt`, `docs/BUILD-FLAGS.md` §3.1 DF4 (b), `L.aarch64.65`,
//! `L.armv7a.27`).
//!
//! Read once, where the processors are described (`arch::describe_cpus`,
//! which has the tables), and kept for stage 4's check
//! (`threads_check::check_arm_smt`). Under `smt=off` a malformed table stops
//! the boot there; under `stibp` it is kept as malformed and said.

use ferrix_sync::Once;

/// What the tables said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// No topology table: no PPTT under ACPI, no `/cpus/cpu-map` in a tree.
    NoTable,
    /// The table's threads and cores (of the processors firmware enables).
    Map {
        /// Which table: `PPTT` or `cpu-map`.
        table: &'static str,
        /// Cores that list threads, or `cpu-map`'s cores.
        cores: usize,
        /// The most threads one core lists.
        most_a_core: usize,
    },
    /// A table that is there and malformed, with why.
    Malformed(&'static str),
}

/// What [`record`] was given, once.
static FOUND: Once<Found> = Once::new();

/// Keep what the tables said.
///
/// # Errors
///
/// Under `smt=off`, a malformed table: which threads share a core cannot be
/// known, and the boot stops rather than read it as none.
pub(crate) fn record(found: Found) -> Result<(), &'static str> {
    let _ = FOUND.call_once(|| found);
    if cfg!(ferrix_smt = "off")
        && let Found::Malformed(why) = found
    {
        return Err(why);
    }
    Ok(())
}

/// What the tables said, or [`Found::NoTable`] before [`record`].
pub(crate) fn found() -> Found {
    FOUND.get().copied().unwrap_or(Found::NoTable)
}
