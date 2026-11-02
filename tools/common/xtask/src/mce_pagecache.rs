//! `cargo xtask test-mce-pagecache` and its modes
//! (`docs/MACHINE-CHECK-PAGECACHE.md` §6): poison in a page of a file the page
//! cache holds must cost a disk read where the page is clean -- a read
//! refilled and answered, a mapper unsignalled, the poll dropping it first --
//! and never a dropped page where it is not.
//!
//! The guest (`src/tests/mce-pagecache`) writes a 64-page file on the writable
//! disk whose words name their page. The read modes stop the machine until a
//! processor's `rip` is the copy's `rep movsb` with the 8 bytes at its `rsi`
//! one of the file's words, as `test-mce-copy` does, and inject there; the
//! mapped modes find the processor running the guest in ring 3 and inject at
//! the page a mapped address translates to, as `test-mce-srar` does; the poll
//! mode injects Intel's UCNA record, which no machine check carries where the
//! processor reports `MCG_SER_P`, and the poll finds -- under TCG, the one
//! place here that reports it. AMD's records under KVM elsewhere, where the
//! guest has `SUCCOR`; the copy modes' TCG rows are fail-closed only (the
//! copy runs in one step there, and AMD's recovery is not reported).

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{AMD, INTEL, gpa, in_ring_3, monitor, stopped};
use crate::mce_copy::{in_ring_0, make, register, symbol, word_at};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce-pagecache",
        help_at: 241,
        help: &[
            "test-mce-pagecache  Poison a clean cached page while read(2) copies it (AMD's records): the read must be",
            "                    refilled from the disk and answered, the frame retired and the page dropped (KVM)",
        ],
        run: |_, args| test_pagecache(args, Mode::Read),
    },
    Task {
        name: "test-mce-pagecache-mapped",
        help_at: 242,
        help: &[
            "test-mce-pagecache-mapped  Poison a clean cached page a program maps and loads: no SIGBUS, the page",
            "                           dropped and faulted in again from the disk",
        ],
        run: |_, args| test_pagecache(args, Mode::Mapped),
    },
    Task {
        name: "test-mce-pagecache-dirty",
        help_at: 243,
        help: &[
            "test-mce-pagecache-dirty  The read mode on a page written and not written back: the read fails EIO, the",
            "                          page kept; and the mapped mode on one: SIGBUS",
        ],
        run: |_, args| {
            test_pagecache(args, Mode::DirtyRead)?;
            test_pagecache(args, Mode::DirtyMapped)
        },
    },
    Task {
        name: "test-mce-pagecache-poll",
        help_at: 244,
        help: &[
            "test-mce-pagecache-poll  Intel's UCNA record at a clean cached page the poll finds (TCG, where",
            "                         MCG_SER_P is reported): the page dropped before anyone consumes it",
        ],
        run: |_, args| test_pagecache(args, Mode::Poll),
    },
];

/// The guest program, `src/tests/mce-pagecache`.
const TEST_PROGRAM: &str = "mce-pagecache";
/// The guest's line once its file is in place.
const READY: &str = "mce-pc: ready ";
/// The guest's last line.
const DONE: &str = "mce-pc: done";
/// The stop's catalogue code.
const FX_9005: &str = "FX-9005";
/// The file's words' high half: `MCPC`.
const MAGIC: u64 = 0x4d43_5043;
/// The file's page the mapped modes inject at.
const TARGET_PAGE: u64 = 7;

/// AMD's consumption: `VAL`, `UC`, `EN`, `MISCV`, `ADDRV`, `Poison`.
const AMD_AR: u64 = 0xbc00_0800_0000_0000;
/// Intel's uncorrected record with no action required (UCNA): `VAL`, `UC`,
/// `EN`, `MISCV`, `ADDRV`, `S` and `AR` clear, MCA code 0xc0 (a patrol
/// scrub's). Where the processor reports `MCG_SER_P` (TCG here) QEMU raises
/// no machine check for it and the poll finds it; AMD's deferred record is
/// retired only by `PhysAddrV`, which no emulator's banks set.
const UCNA: u64 = 0xbc00_0000_0000_00c0;
/// `MCG_STATUS` `RIPV`, `EIPV`, `MCIP`.
const CONSUMER: u64 = 0x7;
/// `MCG_STATUS` `RIPV`, `MCIP`: the interrupted code restartable, the
/// record not its.
const RESTARTABLE: u64 = 0x5;
/// Stops the gate makes looking for its moment.
const TRIES: usize = 400;
/// How long a mode waits for the guest after it injects.
const PATIENCE: Duration = Duration::from_secs(90);

/// One mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A clean page's read: refilled, answered.
    Read,
    /// A clean page a program loads from: dropped, no signal.
    Mapped,
    /// A dirty page's read: `EIO`.
    DirtyRead,
    /// A dirty page a program loads from: `SIGBUS`.
    DirtyMapped,
    /// Intel's UCNA record at a clean page: the poll drops it.
    Poll,
}

impl Mode {
    /// The guest's features.
    const fn features(self) -> Option<&'static str> {
        match self {
            Mode::Read => None,
            Mode::Mapped => Some("mapped"),
            Mode::DirtyRead => Some("dirty"),
            Mode::DirtyMapped => Some("mapped,dirty"),
            Mode::Poll => Some("poll"),
        }
    }

    /// Whether the record goes where the copy reads (else at a mapped page).
    const fn at_copy(self) -> bool {
        matches!(self, Mode::Read | Mode::DirtyRead)
    }
}

/// What a mode injected.
#[derive(Default, Debug)]
struct Injected {
    /// The physical page.
    page: u64,
    /// The processor.
    cpu: usize,
    /// Stops it took.
    tries: usize,
    /// The file's page the copy was loading, from its words (read modes).
    file_page: u64,
}

/// One mode on x86-64.
///
/// # Errors
///
/// A boot that did not reach the guest, no moment found within [`TRIES`], an
/// injection the monitor refused, or a line the mode requires missing.
///
/// Verifies: `H.FAIL.11`
/// Verifies: `L.object.260`
/// Verifies: `L.object.261`
/// Verifies: `L.x86_64.247`
/// Verifies: `L.x86_64.248`
fn test_pagecache(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-pagecache ({mode:?}): x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, mode.features(), false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let movs = symbol(&kernel.elf, "ferrix_copy_mc_movs")?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let image = crate::fat::write_image_with(
        arch,
        &loader,
        &kernel,
        &archive,
        Some("ferrix.mce.poll=1 nokaslr\n"),
    )?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.x86_cpu_extra = Some(if mode == Mode::Poll { INTEL } else { AMD }.to_owned());
    if !qemu_args.smp_given {
        qemu_args.smp = 1;
        qemu_args.smp_given = true;
    }
    let smp = qemu_args.smp.max(1) as usize;
    let tcg = qemu_args
        .accel
        .as_deref()
        .is_none_or(|accel| accel == "tcg");
    let mut injected = Injected::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        injected = inject(&mut qmp, watching.lines(), mode, smp, movs, tcg)?;
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || (seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
                    && seen.iter().any(|line| line.contains("  code ")))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, READY, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    let recovery = !lines
        .iter()
        .any(|line| line.contains("recovery not reported"));
    judge(mode, &lines, &injected, recovery).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-pagecache ({mode:?}): {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-pagecache ({mode:?}): as required (page {:#x} on cpu {}, found in {} \
         stop(s){})",
        injected.page,
        injected.cpu,
        injected.tries,
        match (tcg, recovery) {
            (true, false) => "; TCG: a fail-closed row only, no recovery is shown under TCG",
            (false, false) => "; recovery not reported, so it stopped (a fail-closed row only)",
            (_, true) => "",
        }
    );
    Ok(())
}

/// The mapped address the guest printed, `va=0x...`.
fn mapped_address(lines: &[String]) -> Option<u64> {
    lines.iter().rev().find_map(|line| {
        let at = line.find(" va=0x")? + " va=0x".len();
        u64::from_str_radix(line.get(at..)?.split_whitespace().next()?, 16).ok()
    })
}

/// Find the moment `mode` wants with the machine stopped, and inject there.
fn inject(
    qmp: &mut Qmp,
    lines: &[String],
    mode: Mode,
    smp: usize,
    movs: u64,
    tcg: bool,
) -> Result<Injected> {
    let virt = if mode.at_copy() {
        0
    } else {
        mapped_address(lines).ok_or_else(|| Error::new("the guest printed no mapped address"))?
            + TARGET_PAGE * 0x1000
    };
    for tries in 1..=TRIES {
        stopped(qmp, true)?;
        let mut found = None;
        for cpu in 0..smp {
            let registers = monitor(qmp, "info registers", Some(cpu))?;
            if mode.at_copy() {
                // TCG runs the movs in one step: inject at the first stop in
                // ring 0, which must stop (no recovery is reported there).
                if tcg && in_ring_0(&registers) {
                    found = Some((cpu, 0x1000, 0));
                    break;
                }
                let (Some(rip), Some(rsi), Some(rcx)) = (
                    register(&registers, "RIP"),
                    register(&registers, "RSI"),
                    register(&registers, "RCX"),
                ) else {
                    continue;
                };
                if rip != movs || rcx == 0 || !in_ring_0(&registers) {
                    continue;
                }
                let Some(physical) = gpa(&monitor(qmp, &format!("gva2gpa {rsi:#x}"), Some(cpu))?)
                else {
                    continue;
                };
                if let Some(word) = word_at(qmp, physical & !7)?.filter(|word| word >> 32 == MAGIC)
                {
                    found = Some((cpu, physical & !0xfff, word & 0xffff_ffff));
                    break;
                }
            } else {
                // A consumption is taken on the program's own instruction, in
                // ring 3; the poll's deferred record needs no moment. Either
                // way the page must be the file's target page, by its words.
                if mode != Mode::Poll && !in_ring_3(&registers) {
                    continue;
                }
                let Some(physical) = gpa(&monitor(qmp, &format!("gva2gpa {virt:#x}"), Some(cpu))?)
                else {
                    continue;
                };
                let page = physical & !0xfff;
                if word_at(qmp, page)? == Some((MAGIC << 32) | TARGET_PAGE) {
                    found = Some((cpu, page, TARGET_PAGE));
                    break;
                }
            }
        }
        let made = found.map(|(cpu, page, file_page)| {
            // QEMU raises a machine check for the UCNA record too; with
            // `RIPV` set the kernel counts it spurious and leaves the bank to
            // the poll (L.x86_64.225), which retires and drops the page.
            let (status, mcg) = if mode == Mode::Poll {
                (UCNA, RESTARTABLE)
            } else {
                (AMD_AR, CONSUMER)
            };
            make(qmp, cpu, status, mcg, page).map(|()| Injected {
                page,
                cpu,
                tries,
                file_page,
            })
        });
        stopped(qmp, false)?;
        if let Some(made) = made {
            return made;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(Error::new(format!(
        "no processor was where {mode:?} injects within {TRIES} stops"
    )))
}

/// What `mode` requires of the boot's `lines`.
fn judge(
    mode: Mode,
    lines: &[String],
    injected: &Injected,
    recovery: bool,
) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if !recovery {
        return (has(qemu::PANIC_MARKER) && has(FX_9005))
            .then_some(())
            .ok_or_else(|| "the kernel went on where it must stop as FX-9005".to_owned());
    }
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    let retired = format!("mce: frame {:#x} retired", injected.page);
    let dropped = format!(
        "mce: frame {:#x} was a clean cached page, dropped",
        injected.page
    );
    let kept = format!(
        "mce: frame {:#x} was a cached page that is not clean, kept",
        injected.page
    );
    let needs: Vec<&str> = match mode {
        Mode::Read => vec![
            &retired,
            &dropped,
            "mce-pc: reads all right",
            "mce-pc: child exited 0",
        ],
        Mode::Mapped => vec![
            &retired,
            &dropped,
            "mce-pc: mapped reads all right",
            "mce-pc: child exited 0",
        ],
        Mode::DirtyRead => vec![&retired, &kept],
        Mode::DirtyMapped => vec![
            &retired,
            &kept,
            "mce-pc: SIGBUS code=4",
            "mce-pc: child exited 42",
        ],
        Mode::Poll => vec![
            &retired,
            &dropped,
            "mce-pc: polled reads all right",
            "mce-pc: child exited 0",
        ],
    };
    // A dirty page's read fails, or answers the bytes before the poisoned
    // page and no more (a read that fails part-way does).
    let short = format!(
        "mce-pc: read ended short at {:#x}",
        injected.file_page * 0x1000
    );
    if mode == Mode::DirtyRead && !has("mce-pc: read failed errno=5") && !has(&short) {
        return Err(format!(
            "the read neither failed EIO nor ended short at the poisoned page ({short})"
        ));
    }
    for want in needs.iter().chain([&DONE]) {
        if !has(want) {
            return Err(format!("the boot never said `{want}`"));
        }
    }
    if matches!(mode, Mode::Read | Mode::Mapped | Mode::Poll) && has("mce-pc: SIGBUS") {
        return Err("a program was signalled for a clean cached page".to_owned());
    }
    if matches!(mode, Mode::DirtyRead | Mode::DirtyMapped) && has(&dropped) {
        return Err("a page that is not clean was dropped".to_owned());
    }
    Ok(())
}
