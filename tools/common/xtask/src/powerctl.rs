//! `cargo xtask test-power`: the x86-64 power core end to end
//! (`docs/POWERCTL.md` §10), on boots QEMU can give, none of which touches
//! a register a processor implements.
//!
//! 1. Without either word: no power line, no node, no `power` driver.
//! 2. `ferrix.powerctl` on the gate host's processor, which the kernel's
//!    model table does not name: one refusal line, no node.
//! 3. `ferrix.powerctl.fake` (test only) with a plan and `hang@3`: the
//!    driver checks the claim's refusals, writes 27 then 29 A to the fake,
//!    stops renewing, and the kernel restores on the lease.
//! 4. The same with `exit@3`: the kernel restores when the holder goes,
//!    devmgr says the driver ended, and nothing starts it again.
//! 5. TCG with a processor that answers as Intel family 6 model 0xBE: the
//!    model gate passes, and the probe reads 0x601 as 0 (TCG answers an MSR
//!    it does not have with 0, not `#GP`), so no node.
//!
//! Each row is judged by [`judge`], a pure function host tests hold.
//!
//! The kernel's own init powers the machine off at `FERRIX-BOOT-OK`,
//! before a lease could lapse, so the rows boot with `--init` (or
//! `FERRIX_INIT`), the static busybox `test-shell` uses: its `sh -i`
//! waits at the console and the machine stays up:
//!
//! ```text
//! cargo xtask test-power --init '~/.local/share/ferrix/busybox/{arch}/bin/busybox.static'
//! ```

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-power",
    help_at: 206,
    help: &[
        "test-power    Boot x86_64 five times for the power core (docs/POWERCTL.md section 10): no word,",
        "              a refused processor, the fake's lease lapse and holder death, and TCG's 0x601 of 0;",
        "              needs --init (the static busybox test-shell uses) to keep the machine up",
    ],
    run: |_, args| test_power(args),
}];

/// The boot's last line, after which a row reads on for its own lines.
const BOOTED: &str = "FERRIX-BOOT-OK";
/// How long a row reads on after [`BOOTED`] for the lines it needs.
const PATIENCE: Duration = Duration::from_secs(30);

/// A row of the test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// No word.
    Off,
    /// `ferrix.powerctl` on a processor the table does not name.
    Refused,
    /// The fake, a plan, `hang@3`.
    Lease,
    /// The fake, a plan, `exit@3`.
    Gone,
    /// TCG answering as Intel 6/0xBE, `ferrix.powerctl`.
    Absent,
}

impl Row {
    /// Every row, in order.
    const ALL: [Row; 5] = [Row::Off, Row::Refused, Row::Lease, Row::Gone, Row::Absent];

    /// The kernel options the row boots with.
    fn options(self) -> &'static [&'static str] {
        match self {
            Row::Off => &[],
            Row::Refused | Row::Absent => &["ferrix.powerctl"],
            Row::Lease => &[
                "ferrix.powerctl.fake",
                "ferrix.power.vr=27,29@1",
                "ferrix.power.test=hang@3",
            ],
            Row::Gone => &[
                "ferrix.powerctl.fake",
                "ferrix.power.vr=27,29@1",
                "ferrix.power.test=exit@3",
            ],
        }
    }
}

/// The refusals the driver checks on the claim and the node (`L.device.52`,
/// `L.device.53`), as it says them.
const REFUSALS: &str = "power: refusals: second claim Some(AlreadyBound), copy Some(AccessDenied), \
                        pin Some(WrongType), mapping Some(AccessDenied), interrupt Some(InvalidArgs), \
                        quiesce Some(BadState)";
/// The fake's two writes.
const FIRST_WRITE: &str = "power (fake): set 0x601 0xd8 (was 0xc8)";
const SECOND_WRITE: &str = "power (fake): set 0x601 0xe8 (was 0xd8)";
/// The fake's restore, firmware's 25 A confirmed, and its cause.
const RESTORED: &str = "power (fake): restored Done { value: c8, taken: true, back: Some(c8) }: ";

/// The index of the first line holding `text`.
fn find(lines: &[String], text: &str) -> Option<usize> {
    lines.iter().position(|line| line.contains(text))
}

/// Whether the row's lines are what it must show.
///
/// # Errors
///
/// What was missing or out of place.
pub(crate) fn judge(row: Row, lines: &[String]) -> std::result::Result<(), String> {
    let kernel_power =
        |line: &&String| line.contains("| power: ") || line.contains("| power (fake)");
    let need = |text: &str| find(lines, text).ok_or_else(|| format!("no line `{text}`"));
    let started = lines
        .iter()
        .filter(|line| line.contains("devmgr   power "))
        .count();
    match row {
        Row::Off => {
            if let Some(line) = lines.iter().find(kernel_power) {
                return Err(format!("a power line without the word: {line}"));
            }
            if started != 0 {
                return Err("devmgr started a power driver without the word".to_owned());
            }
        }
        Row::Refused | Row::Absent => {
            let why = if row == Row::Refused {
                "power: refused: not an authorised processor"
            } else {
                "power: refused: VR_CURRENT_CONFIG locked or 0 (Intel true, signature 0xb06e"
            };
            let _ = need(why)?;
            if find(lines, "power: node:").is_some() || started != 0 {
                return Err("a refused processor got a power node or driver".to_owned());
            }
        }
        Row::Lease | Row::Gone => {
            let (said, cause) = if row == Row::Lease {
                ("power: test: renewing no more", "Lease")
            } else {
                ("power: test: exiting", "Gone")
            };
            let _ = need("power (fake): node:")?;
            let _ = need(REFUSALS)?;
            let first = need(FIRST_WRITE)?;
            let second = need(SECOND_WRITE)?;
            let test = need(said)?;
            let restored = need(&format!("{RESTORED}{cause}"))?;
            if !(first < second && second < test && test < restored) {
                return Err(
                    "the writes, the test's stop and the restore came out of order".to_owned(),
                );
            }
            if lines
                .iter()
                .skip(restored)
                .any(|line| line.contains("power (fake): set"))
            {
                return Err("a write after the restore".to_owned());
            }
            // The kernel's own lines: its node, writes, restores and latch.
            let kernel_said = [
                "| power: node",
                "| power: set",
                "| power: restored",
                "| power: latched",
            ];
            if let Some(line) = lines
                .iter()
                .find(|line| kernel_said.iter().any(|said| line.contains(said)))
            {
                return Err(format!(
                    "a kernel power line under the fake without (fake): {line}"
                ));
            }
            if row == Row::Gone {
                let _ = need("devmgr   the driver of 0xffffffff ended with status 4")?;
            }
            if started != 1 {
                return Err(format!(
                    "devmgr started the power driver {started} times, not once"
                ));
            }
        }
    }
    Ok(())
}

/// `test-power` on x86-64; skipped elsewhere, which publishes no node.
///
/// # Errors
///
/// A boot that did not come up, or a row whose lines [`judge`] refuses.
///
/// Verifies: `H.DEV.5`
/// Verifies: `H.DEV.6`
/// Verifies: `L.device.51`
/// Verifies: `L.device.52`
/// Verifies: `L.device.53`
/// Verifies: `L.x86_64.207`
fn test_power(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch != Arch::X86_64 {
            println!("  {arch}: no power core; skipped");
            continue;
        }
        if crate::optional_program(arch, args)?.is_none() {
            return Err(Error::new(
                "test-power needs --init (or FERRIX_INIT): a program whose shell keeps the \
                 machine up past FERRIX-BOOT-OK, such as the static busybox test-shell uses",
            ));
        }
        for row in Row::ALL {
            let mut row_args = args.clone();
            row_args.kernel_options = row.options().iter().map(|&word| word.to_owned()).collect();
            if row == Row::Absent {
                row_args.accel = Some("tcg".to_owned());
                row_args.x86_cpu = Some(format!(
                    "{},vendor=GenuineIntel,family=6,model=190",
                    qemu::x86_cpu("tcg")
                ));
            }
            let (image, kernel) = crate::build_image(arch, &row_args)?;
            let mut lines = Vec::new();
            let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
                let before = watching.lines().to_vec();
                let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
                    let mut all = before.clone();
                    all.extend_from_slice(seen);
                    judge(row, &all).is_ok()
                })?;
                lines = before;
                lines.extend_from_slice(watching.after());
                watching.stop_when_done();
                Ok(())
            };
            let _ = qemu::watch_then(arch, &image, &kernel, &row_args, BOOTED, hook)?;
            judge(row, &lines)
                .map_err(|why| Error::new(format!("{arch}: test-power row {row:?}: {why}")))?;
            println!("  {arch}: test-power row {row:?} passed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter()
            .map(|line| format!("   1.00 | {line}"))
            .collect()
    }

    fn lease() -> Vec<String> {
        lines(&[
            "power (fake): node: signature 0xb06e0, 0x601 0xc8, ceiling 0x100",
            "devmgr   power 0xffffffff 0000:0007: started",
            REFUSALS,
            FIRST_WRITE,
            SECOND_WRITE,
            "power: test: renewing no more",
            &format!("{RESTORED}Lease"),
        ])
    }

    #[test]
    fn a_row_off_passes_without_power_lines_and_fails_with_one() {
        assert_eq!(
            judge(Row::Off, &lines(&["  hooks    3 commits before power-off"])),
            Ok(())
        );
        assert!(judge(Row::Off, &lines(&["power: refused: x"])).is_err());
        assert!(
            judge(
                Row::Off,
                &lines(&["devmgr   power 0xffffffff 0000:0007: started"])
            )
            .is_err()
        );
    }

    #[test]
    fn a_refused_row_needs_its_line_and_no_node() {
        let refused = lines(&["power: refused: not an authorised processor (Intel false)"]);
        assert_eq!(judge(Row::Refused, &refused), Ok(()));
        assert!(judge(Row::Absent, &refused).is_err());
        let zero = lines(&[
            "power: refused: VR_CURRENT_CONFIG locked or 0 (Intel true, signature 0xb06e1, 0x601 Some(0)); no node",
        ]);
        assert_eq!(judge(Row::Absent, &zero), Ok(()));
        let mut noded = refused;
        noded.extend(lines(&["power: node: signature 0x1"]));
        assert!(judge(Row::Refused, &noded).is_err());
    }

    #[test]
    fn the_lease_row_needs_every_line_in_order() {
        assert_eq!(judge(Row::Lease, &lease()), Ok(()));
        let mut swapped = lease();
        swapped.swap(3, 4);
        assert!(judge(Row::Lease, &swapped).is_err());
        let mut late = lease();
        late.extend(lines(&[FIRST_WRITE]));
        assert!(judge(Row::Lease, &late).is_err());
        let mut unmarked = lease();
        unmarked.extend(lines(&["power: set 0x601 0xd8 (was 0xc8)"]));
        assert!(judge(Row::Lease, &unmarked).is_err());
        for missing in 0..7 {
            let mut short = lease();
            let _ = short.remove(missing);
            assert!(
                judge(Row::Lease, &short).is_err(),
                "line {missing} missing passed"
            );
        }
    }

    #[test]
    fn the_gone_row_needs_the_death_and_one_start() {
        let mut gone = lease();
        gone[5] = "   1.00 | power: test: exiting".to_owned();
        gone[6] = format!("   1.00 | {RESTORED}Gone");
        assert!(judge(Row::Gone, &gone).is_err());
        gone.extend(lines(&[
            "  devmgr   the driver of 0xffffffff ended with status 4; the device is quiesced",
        ]));
        assert_eq!(judge(Row::Gone, &gone), Ok(()));
        gone.extend(lines(&["devmgr   power 0xffffffff 0000:0007: started"]));
        assert!(judge(Row::Gone, &gone).is_err());
    }
}
