//! What the trap path does after an `execve`, compared: an [`Outcome::Enter`]
//! is equal only to one with the same entry, stack and mode.
//!
//! Verification, not the trap path: a file of its own so that the manifest
//! counts it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). Every architecture's trap check runs it; x86-64's
//! adds the one mode only it has, `Abi::Compat`.

use core::hint::black_box;

use super::{Abi, Outcome};

/// Compare entries into a program that differ in one field each, and one that
/// does not differ at all.
///
/// # Errors
///
/// Two entries compared as equal that differ, or as different that do not.
pub fn outcomes() -> Result<(), &'static str> {
    // The dispatcher's callers compare outcomes, and an entry compared by
    // anything less than its entry and stack would restart a program at
    // another's first instruction, or on another's stack.
    let entered = Outcome::Enter {
        entry: 0x40_1000,
        stack: 0x7fff_f000,
        abi: Abi::Native,
    };
    let elsewhere = Outcome::Enter {
        entry: 0x40_1000,
        stack: 0x7fff_e000,
        abi: Abi::Native,
    };
    let other_entry = Outcome::Enter {
        entry: 0x40_2000,
        stack: 0x7fff_f000,
        abi: Abi::Native,
    };
    let again = Outcome::Enter {
        entry: 0x40_1000,
        stack: 0x7fff_f000,
        abi: Abi::Native,
    };
    // Through `black_box`, or the comparison is folded where it is written
    // and the trap path's own equality never runs.
    let entered = black_box(entered);
    if entered == black_box(elsewhere)
        || entered == black_box(other_entry)
        || entered != black_box(again)
        || entered == black_box(Outcome::Return(0x40_1000))
    {
        return Err("two entries into a program compared by something but entry and stack");
    }
    Ok(())
}
