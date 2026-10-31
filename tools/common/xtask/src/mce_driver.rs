//! `cargo xtask test-mce-driver`: poison a ring-3 driver consumes ends that
//! driver alone, its frame retired and left unread by the pin quarantine,
//! and devmgr starts it again (`docs/MACHINE-CHECK-DRIVER.md` §3).
//!
//! The image carries `mce-victim` (`src/user/system/native/mce-victim`)
//! under the name `intel` on the QEMU stand-in for ADL-N's GPU, as
//! `test-restart --boot crashloop` carries `crash-at-start`. The victim pins
//! and maps one page, fills it with words naming its pid, says its pid and
//! the page's address, sends the chardev HELLO, and then reads the page in
//! ring 3 and writes it through a channel of its own, round after round.
//! The gate stops the machine until a processor is at the victim's page --
//! in ring 3 for Own, Kill and Bystander, at the kernel's `copy_mc` over it
//! for Copy -- injects there, and lets the machine go on. One injection a
//! boot: a second death within 10 s would be devmgr's crash-loop streak.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{AMD, INTEL, gpa, in_ring_3, monitor, stopped};
use crate::mce_copy::{in_ring_0, make, register, symbol, word_at};
use crate::native::{self, Program};
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, intel_uapi, qemu, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce-driver",
        help_at: 254,
        help: &[
            "test-mce-driver  Poison a ring-3 driver's own page as it loads it: the driver alone ends (status",
            "                 135), the frame retired and left unread by the pin quarantine, devmgr starts it again",
        ],
        run: |_, args| test(args, Mode::Own),
    },
    Task {
        name: "test-mce-driver-kill",
        help_at: 255,
        help: &["test-mce-driver-kill  test-mce-driver with RIPV clear: ended without a handler"],
        run: |_, args| test(args, Mode::Kill),
    },
    Task {
        name: "test-mce-driver-copy",
        help_at: 256,
        help: &[
            "test-mce-driver-copy  Poison the driver's page as the kernel copies it in channel_write (KVM, AMD's records)",
        ],
        run: |_, args| test(args, Mode::Copy),
    },
    Task {
        name: "test-mce-driver-bystander",
        help_at: 257,
        help: &[
            "test-mce-driver-bystander  test-mce-driver on two processors, the other told the record with EIPV clear (TCG)",
        ],
        run: |_, args| test(args, Mode::Bystander),
    },
];

/// `docs/MACHINE-CHECK-DRIVER.md` §3's modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A ring-3 load of the victim's page, restartable.
    Own,
    /// The same with `RIPV` clear.
    Kill,
    /// The kernel's copy of the page in `channel_write`.
    Copy,
    /// Own on two processors, the other a bystander (`EIPV` clear).
    Bystander,
}

/// The victim, carried under `intel`.
const VICTIM_PROGRAM: Program = Program {
    package: "ferrix-mce-victim",
    binary: "mce-victim",
    directory: native::DRIVERS,
};

/// The victim's line at each start: `mce-victim: pid <p> va <hex>`.
const STARTED: &str = "mce-victim: pid ";
/// The stand-in's location, as devmgr and the kernel name it.
const LOCATION: &str = "0x00000010";
/// The init's lines.
const ALIVE: &str = "mce-driver-gate: alive ";
const DONE: &str = "mce-driver-gate: done";
const RETIRED_FILE: &str = "mce-driver-gate: ";

/// The init: say it is alive every 2 s for 80 s, then the retired frames.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
for i in $(busybox seq 40); do
    busybox sleep 2
    echo "mce-driver-gate: alive $i"
done
busybox sed 's/^/mce-driver-gate: /' /proc/ferrix-mce-retired
echo "mce-driver-gate: done"
exit 17
"#;

/// The high half of every word of the victim's page: `MCEV`.
const MAGIC: u64 = 0x4d43_4556;
/// Intel's action-required data load, MCA code 0x134 (`VAL UC EN MISCV
/// ADDRV S AR`).
const SRAR: u64 = 0xbd80_0000_0000_0134;
/// AMD's action-required record (`VAL UC EN MISCV ADDRV` and the poison
/// bit's code).
const AMD_AR: u64 = 0xbc00_0800_0000_0000;
/// `MCG_STATUS` of the consumer: `RIPV EIPV MCIP`.
const CONSUMER: u64 = 0x7;
/// Of a consumer that cannot restart: `EIPV MCIP`.
const UNRESTARTABLE: u64 = 0x6;
/// Of a processor that did not consume: `RIPV MCIP`.
const BYSTANDER: u64 = 0x5;
/// Stops the gate makes looking for the victim at its page.
const TRIES: usize = 2000;
/// How long the boot is given after the injection.
const PATIENCE: Duration = Duration::from_secs(150);

/// What the injection found and made.
#[derive(Default, Debug)]
struct Injected {
    /// The frame injected at.
    page: u64,
    /// The victim's pid.
    pid: u64,
    /// Stops it took.
    tries: usize,
}

/// One mode.
///
/// # Errors
///
/// An unpatched QEMU, a build or boot failure, no stop at the victim's page
/// within [`TRIES`], or a line the judge wants missing.
///
/// Verifies: `H.FAIL.8`
/// Verifies: `L.x86_64.228`
/// Verifies: `L.object.262`
/// Verifies: `L.mm.91`
fn test(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-driver ({mode:?}): x86_64 only; skipped");
        return Ok(());
    }
    intel_uapi::refuse_unpatched_qemu()?;
    let mut qemu_args = args.clone();
    qemu_args.intel_gpu_standin = true;
    if !args.timeout_given {
        qemu_args.timeout = 300;
    }
    let tcg = match mode {
        Mode::Copy => false,
        Mode::Bystander => true,
        Mode::Own | Mode::Kill => qemu_args
            .accel
            .as_deref()
            .is_none_or(|accel| accel == "tcg"),
    };
    qemu_args.accel = Some(if tcg { "tcg" } else { "kvm" }.to_owned());
    // TCG's Intel model reports MCG_SER_P; KVM's AMD model reports SUCCOR.
    qemu_args.x86_cpu_extra = Some(if tcg { INTEL } else { AMD }.to_owned());
    qemu_args.smp = if mode == Mode::Bystander { 2 } else { 1 };
    qemu_args.smp_given = true;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let movs = symbol(&kernel.elf, "ferrix_copy_mc_movs")?;
    let mut natives = native::build(arch, args.release)?;
    let victim = native::build_one(arch, args.release, &paths::target_dir(), &VICTIM_PROGRAM)?;
    natives
        .iter_mut()
        .find(|built| built.name == "intel")
        .ok_or_else(|| Error::new("the native programs carry no intel driver to replace"))?
        .bytes = victim.bytes;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &[])?;
    let options = format!("{} nokaslr\n", intel_uapi::OPTION);
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let port = free_port()?;
    qemu_args.qmp_port = Some(port);
    let smp = qemu_args.smp as usize;
    let mut injected = Injected::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        injected = inject(&mut qmp, watching.lines(), mode, tcg, smp, movs)?;
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, STARTED, hook)?;
    let log = paths::build_dir(arch).join("serial.log");
    judge(mode, &lines, &injected).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-driver ({mode:?}): {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-driver ({mode:?}): as required (frame {:#x} of pid {} found in {} \
         stop(s), {}, {smp} processor(s))",
        injected.page,
        injected.pid,
        injected.tries,
        if tcg {
            "TCG, Intel's records"
        } else {
            "KVM, AMD's records"
        }
    );
    Ok(())
}

/// The victim's pid and page address from the first start's line.
fn victim(lines: &[String]) -> Option<(u64, u64)> {
    lines.iter().find_map(|line| {
        let rest = line.split(STARTED).nth(1)?;
        let mut words = rest.split_whitespace();
        let pid = words.next()?.parse().ok()?;
        let _ = words.next()?;
        let va = u64::from_str_radix(words.next()?.trim_start_matches("0x"), 16).ok()?;
        Some((pid, va))
    })
}

/// Whether the word at `physical` is the victim `pid`'s.
fn is_victims(qmp: &mut Qmp, physical: u64, pid: u64) -> Result<bool> {
    Ok(word_at(qmp, physical & !7)?
        .is_some_and(|word| word >> 32 == MAGIC && (word >> 16) & 0xffff == pid & 0xffff))
}

/// Stop until a processor is at the victim's page as `mode` wants, inject
/// there, and go on.
fn inject(
    qmp: &mut Qmp,
    lines: &[String],
    mode: Mode,
    tcg: bool,
    smp: usize,
    movs: u64,
) -> Result<Injected> {
    let (pid, va) =
        victim(lines).ok_or_else(|| Error::new("the victim printed no pid and page"))?;
    for tries in 1..=TRIES {
        stopped(qmp, true)?;
        let mut found = None;
        for cpu in 0..smp {
            let registers = monitor(qmp, "info registers", Some(cpu))?;
            let at = if mode == Mode::Copy {
                let (Some(rip), Some(rsi)) =
                    (register(&registers, "RIP"), register(&registers, "RSI"))
                else {
                    continue;
                };
                if !in_ring_0(&registers)
                    || rip != movs
                    || register(&registers, "RCX").is_none_or(|rcx| rcx == 0)
                {
                    continue;
                }
                rsi
            } else {
                if !in_ring_3(&registers) {
                    continue;
                }
                va
            };
            let Some(physical) = gpa(&monitor(qmp, &format!("gva2gpa {at:#x}"), Some(cpu))?) else {
                continue;
            };
            if is_victims(qmp, physical, pid)? {
                found = Some((cpu, physical & !0xfff));
                break;
            }
        }
        let made = found.map(|(cpu, page)| {
            let status = if tcg { SRAR } else { AMD_AR };
            let consumer = if mode == Mode::Kill {
                UNRESTARTABLE
            } else {
                CONSUMER
            };
            if mode == Mode::Bystander {
                for other in (0..smp).filter(|&other| other != cpu) {
                    make(qmp, other, status, BYSTANDER, page)?;
                }
            }
            make(qmp, cpu, status, consumer, page).map(|()| Injected { page, pid, tries })
        });
        stopped(qmp, false)?;
        if let Some(made) = made {
            return made;
        }
        let jitter = ((tries as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 32) % 21;
        std::thread::sleep(Duration::from_millis(5 + jitter));
    }
    Err(Error::new(format!(
        "no processor was at the victim's page (pid {pid}, {va:#x}) within {TRIES} stops"
    )))
}

/// Whether `lines` show what `mode` requires. A ring-3 consumption's end
/// prints `ended by signal 7`; a system call's copy raises its `SIGBUS` at
/// the call's way out (`trap::copy_poisoned`), which says only who the
/// process was, so Copy wants that line -- devmgr's status 135 is required
/// in every mode.
fn judge(mode: Mode, lines: &[String], injected: &Injected) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    let first = lines
        .iter()
        .position(|line| line.contains(STARTED))
        .ok_or("the victim never started")?;
    let after = lines.get(first..).unwrap_or_default();
    let retired = format!("mce: frame {:#x} retired", injected.page);
    let ended = if mode == Mode::Copy {
        format!("signal   pid {} is ", injected.pid)
    } else {
        format!("signal   pid {} ended by signal 7", injected.pid)
    };
    let died = format!("devmgr   the driver of {LOCATION} ended with status 135");
    let restarted = format!("devmgr   the driver of {LOCATION} was started again");
    for (want, missing) in [
        (retired.as_str(), "the frame was not retired"),
        (ended.as_str(), "the victim was not ended by SIGBUS"),
        (
            died.as_str(),
            "devmgr did not report the death with status 135",
        ),
        (restarted.as_str(), "devmgr did not start the driver again"),
        (DONE, "the init never finished"),
    ] {
        if !after.iter().any(|line| line.contains(want)) {
            return Err(format!("{missing}: no `{want}`"));
        }
    }
    let others: Vec<&String> = after
        .iter()
        .filter(|line| {
            line.contains("ended by signal")
                && !line.contains(&format!("signal   pid {} ", injected.pid))
        })
        .collect();
    if let Some(line) = others.first() {
        return Err(format!("another program ended: `{}`", line.trim()));
    }
    let second = after
        .iter()
        .skip(1)
        .position(|line| line.contains(STARTED))
        .map(|at| at + 1)
        .ok_or("no second start of the driver")?;
    let restarted_after = after.get(second..).unwrap_or_default();
    if !restarted_after.iter().any(|line| line.contains(ALIVE)) {
        return Err("the init said nothing after the second start".to_owned());
    }
    let page = format!("{RETIRED_FILE}{:#x}", injected.page);
    if !restarted_after.iter().any(|line| line.contains(&page)) {
        return Err(format!(
            "/proc/ferrix-mce-retired does not hold {:#x} at the end",
            injected.page
        ));
    }
    if !restarted_after.iter().any(|line| {
        line.contains("went back once its next driver had reset it")
            && line.contains("; 1 retired, not read")
    }) {
        return Err(
            "no release of the dead driver's pins saying `1 retired, not read` after the second start"
                .to_owned(),
        );
    }
    Ok(())
}
