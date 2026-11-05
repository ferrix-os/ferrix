//! `test-reserve`: a time reservation as a Linux program asks for one, on a kernel of
//! `schedule=edf` (`src/tests/reserve/`, `docs/microkernel/K9.md`, ledger 7011 S.6).

use crate::args::Args;
use crate::sem::{boot, build_test, says, status};
use crate::task::Task;
use crate::{Error, Result, flags, paths};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-reserve",
    help_at: 588,
    help: &[
        "test-reserve  Boot reserve-test as init on a schedule=edf kernel and require sched_setattr's admission,",
        "              refusals by name, a budget held to its share and a child not held to it",
    ],
    run: |_, args| test_reserve(args),
}];

const STEPS: [&str; 8] = [
    "reserve: malformed ok",
    "reserve: admitted ok",
    "reserve: other ok",
    "reserve: affinity ok",
    "reserve: budget ok",
    "reserve: child ok",
    "reserve: unschedulable ok",
    "reserve: back ok",
];

fn test_reserve(args: &Args) -> Result<()> {
    if flags::this_run()?.value("schedule") != Some("edf") {
        return Err(Error::new("test-reserve is for schedule=edf"));
    }
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");
        let program = build_test("reserve", arch, None, false)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("reserve: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the reservation test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the reservation test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        let figure = lines.iter().find(|line| line.contains("reserve: budget "));
        println!(
            "  {arch}: reservations admitted and refused by name, {}",
            figure.map_or("", |line| line.trim())
        );
    }
    Ok(())
}
