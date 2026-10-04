//! init's own self-check: pid 1's inputs judged as `set_inputs` says
//! (`L.init.2`, `L.init.3`; the `inputs` line, FX-1503). A child of `init`
//! so that it can drive `judge` and read the inputs without either being
//! visible beyond init.

use super::{
    COMMANDS_INPUT, InputEntry, Inputs, PROGRAM_INPUT, SCRIPT_INPUT, inputs, judge, set_inputs,
};

/// A case of [`check`]: the entries, the program, script and commands they
/// should give, and how many of them should be refused.
type Case<'a> = (&'a [InputEntry], &'a [u8], &'a [u8], &'a [u8], u32);

/// What [`check`] found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckReport {
    /// Sets of entries judged.
    pub(crate) cases: u32,
    /// Entries refused across them, each as the case expected.
    pub(crate) refusals: u32,
}

/// An entry for [`check`].
const fn made_up(
    name: &'static [u8],
    regular: bool,
    directory: bool,
    links: u32,
    data: &'static [u8],
) -> InputEntry {
    InputEntry {
        name,
        regular,
        directory,
        links,
        data,
    }
}

/// [`judge`] over `entries`, and how many it refused.
fn judged(entries: &[InputEntry]) -> (Inputs, u32) {
    let mut refused = 0_u32;
    let inputs = judge(entries.iter().copied(), &mut |_, _| {
        refused = refused.saturating_add(1);
    });
    (inputs, refused)
}

/// Judge sets of entries made up here, as an archive could carry them, and
/// see that each input is taken or refused as `set_inputs` says, then that a
/// second `set_inputs` changes nothing. Prints nothing of its own: a refusal
/// here is counted, not said.
///
/// Verifies: L.init.2, L.init.3, H.BOOT.15
pub(crate) fn check() -> Result<CheckReport, &'static str> {
    const DIRS: [InputEntry; 2] = [
        made_up(b".ferrix", false, true, 2, b""),
        made_up(b".ferrix/init", false, true, 2, b""),
    ];
    let program = made_up(PROGRAM_INPUT, true, false, 1, b"P");
    let script = made_up(SCRIPT_INPUT, true, false, 1, b"S");
    let commands = made_up(COMMANDS_INPUT, true, false, 1, b"a\0\0");
    // (entries, program, script, commands, refusals) expected.
    let cases: [Case<'_>; 9] = [
        (
            &[DIRS[0], DIRS[1], program, script, commands],
            b"P",
            b"S",
            b"a\0\0",
            0,
        ),
        (&[], b"", b"", b"", 0),
        (
            &[program, made_up(PROGRAM_INPUT, true, false, 1, b"Q")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(PROGRAM_INPUT, false, true, 2, b"")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(PROGRAM_INPUT, true, false, 2, b"P")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[
                made_up(b".ferrix/init/other", true, false, 1, b"x"),
                program,
            ],
            b"P",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(b".ferrix/init", true, false, 1, b"x"), script],
            b"",
            b"S",
            b"",
            1,
        ),
        (
            &[made_up(SCRIPT_INPUT, true, false, 1, b"a\0b")],
            b"",
            b"",
            b"",
            1,
        ),
        (
            &[made_up(COMMANDS_INPUT, true, false, 1, b"a\0")],
            b"",
            b"",
            b"",
            1,
        ),
    ];
    let mut refusals = 0_u32;
    for (entries, program, script, commands, expected) in cases {
        let (inputs, refused) = judged(entries);
        if inputs.program != program || inputs.script != script || inputs.commands != commands {
            return Err("a set of entries gave other inputs than set_inputs says");
        }
        if refused != expected {
            return Err("a set of entries was refused other than set_inputs says");
        }
        refusals = refusals.saturating_add(refused);
    }
    let before = inputs();
    if set_inputs(core::iter::once(program)) {
        return Err("a second set_inputs was taken");
    }
    let after = inputs();
    if !core::ptr::eq(before.program, after.program)
        || !core::ptr::eq(before.script, after.script)
        || !core::ptr::eq(before.commands, after.commands)
    {
        return Err("a second set_inputs changed pid 1's inputs");
    }
    Ok(CheckReport {
        cases: 10,
        refusals,
    })
}
