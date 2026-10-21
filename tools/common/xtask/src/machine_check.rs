//! `cargo xtask test-mce`: a machine check injected into a running x86-64
//! kernel must stop it as FX-9005 (F-73, `L.x86_64.175`).
//!
//! The kernel sets `CR4.MCE` on every processor (`arch/x86_64/cpu.rs`,
//! `enable_machine_check`), and stage 4 reads it set on each. That says the
//! bit is there; this says what the bit is for. Once stage 4 has passed,
//! QEMU's monitor injects an uncorrected error into every processor, the
//! last one first, and the kernel must report it through the paranoid entry
//! as FX-9005. QEMU refuses `mce -b`, the broadcast, on the processor models
//! the gates boot ("Guest CPU does not support MCA broadcast"), so each
//! processor is named in turn. One that took it with `CR4.MCE` clear would
//! shut down instead, which QEMU makes a triple fault and says so in the
//! monitor's answer: the machine resets or stops, and no FX-9005 is printed.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

/// The line stage 4 prints once every processor is online and its checks,
/// the `mce` line's among them, have passed: `  stage 4  <n> processors
/// online, ...`.
const STAGE_4: &str = "  stage 4  ";

/// The catalogue code of the machine check's report.
const MACHINE_CHECK: &str = "FX-9005";

/// The human monitor's injection after the processor's index: bank 0, with
/// `MCi_STATUS` valid, uncorrected, enabled and processor context corrupt
/// (bits 63, 61, 60 and 57), and `MCG_STATUS` restart-IP valid and
/// machine-check in progress (bits 0 and 2). No address, no misc.
const ERROR: &str = "0 0xb200000000000000 0x5 0 0";

/// How long the kernel is given to report the machine check.
const PATIENCE: Duration = Duration::from_secs(30);

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-mce",
    help_at: 205,
    help: &[
        "test-mce      Boot x86_64, inject a machine check into every processor through QEMU's monitor after",
        "              stage 4, and require the kernel to stop as FX-9005 (F-73)",
    ],
    run: |_, args| test_mce(args),
}];

/// `test-mce` on x86-64; skipped on the other architectures, which have no
/// machine-check exception to enable.
///
/// # Errors
///
/// A boot that did not reach stage 4, a monitor that refused an injection,
/// or a kernel that did not stop as FX-9005.
///
/// Verifies: `L.x86_64.175`
pub(crate) fn test_mce(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch != Arch::X86_64 {
            println!("  {arch}: no machine-check exception to inject; skipped");
            continue;
        }
        let (image, kernel) = crate::build_image(arch, args)?;
        let port = free_port()?;
        let mut qemu_args = args.clone();
        qemu_args.qmp_port = Some(port);
        let mut replies = Vec::new();
        let mut processors = 0;
        let mut reported = false;
        let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
            processors = online(watching.lines()).ok_or_else(|| {
                Error::new(format!(
                    "{arch}: stage 4's line named no count of processors"
                ))
            })?;
            let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
            // The last processor first, so that the report comes from a
            // secondary, which starts from a CR4 without the bit. With
            // `--smp-threads`, the last of QEMU's processors alone: the last
            // thread of the last core, which `smt=off` parks, so the report
            // comes from a parked thread (the consultant's ledger 2563, (2)).
            let targets: Vec<usize> = if qemu_args.smp_threads > 1 {
                vec![
                    usize::try_from(qemu_args.smp)
                        .unwrap_or(1)
                        .saturating_sub(1),
                ]
            } else {
                (0..processors).rev().collect()
            };
            for cpu in targets {
                let command = format!("mce {cpu} {ERROR}");
                let arguments = format!(
                    "{{\"command-line\":{}}}",
                    crate::display::json_string(&command)
                );
                let reply = qmp.execute("human-monitor-command", Some(&arguments))?;
                println!(
                    "  {arch}: injected `{command}`; the monitor said {}",
                    reply.trim()
                );
                replies.push((command, reply));
            }
            reported = watching.read_more(Instant::now() + PATIENCE, |seen| {
                seen.iter().any(|line| line.contains(MACHINE_CHECK))
                    && seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
            })?;
            watching.stop_when_done();
            Ok(())
        };
        let _ = qemu::watch_then(arch, &image, &kernel, &qemu_args, STAGE_4, hook)?;
        // The monitor answers an injection it would not make -- no MCA in
        // the processor model, a bank whose reporting is disabled, a
        // processor that would shut down -- with a sentence; one it made,
        // with nothing.
        for (command, reply) in &replies {
            if let Some(problem) = refusal(reply) {
                return Err(Error::new(format!(
                    "{arch}: the monitor did not inject `{command}` as asked: {problem}"
                )));
            }
        }
        if !reported {
            return Err(Error::new(format!(
                "{arch}: a machine check injected into each of {processors} processors after \
                 stage 4 was not reported as {MACHINE_CHECK} within {}s: a processor that takes \
                 one with CR4.MCE clear shuts down unreported",
                PATIENCE.as_secs()
            )));
        }
        if args.smp_threads > 1 {
            println!(
                "  {arch}: a machine check injected into QEMU's last processor, the last thread \
                 of the last core, stopped the kernel as {MACHINE_CHECK}"
            );
        } else {
            println!(
                "  {arch}: a machine check injected into each of {processors} processors stopped \
                 the kernel as {MACHINE_CHECK}"
            );
        }
    }
    Ok(())
}

/// The processors stage 4's line says are online.
fn online(lines: &[String]) -> Option<usize> {
    let line = lines.iter().rev().find(|line| line.contains(STAGE_4))?;
    let at = line.find(STAGE_4)? + STAGE_4.len();
    line.get(at..)?.split_whitespace().next()?.parse().ok()
}

/// What the monitor's reply to an injection says went wrong, if anything:
/// its `return` string, which is empty when the injection was made.
fn refusal(reply: &str) -> Option<String> {
    let text = reply
        .split_once("\"return\":")
        .map_or(reply, |(_, rest)| rest)
        .trim()
        .trim_end_matches('}')
        .trim()
        .trim_matches('"')
        .replace("\\r\\n", " ")
        .replace("\\n", " ");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{online, refusal};

    #[test]
    fn an_empty_reply_is_an_injection_made() {
        assert_eq!(refusal("{\"return\": \"\"}"), None);
    }

    #[test]
    fn a_sentence_is_a_refusal() {
        assert_eq!(
            refusal("{\"return\": \"MCE injection not supported\\r\\n\"}").as_deref(),
            Some("MCE injection not supported")
        );
    }

    #[test]
    fn stage_4_names_its_processors() {
        let lines = [
            "    2.38 |   stage 4  4 processors online, a contended counter came to 1 of 1"
                .to_owned(),
        ];
        assert_eq!(online(&lines), Some(4));
        assert_eq!(online(&["nothing".to_owned()]), None);
    }
}
