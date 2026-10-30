//! `cargo xtask test-mce-copy` and its modes (`docs/MACHINE-CHECK-COPY.md`
//! §6): a machine check injected while the kernel's copy of a program's
//! buffer runs must fail the call and signal the program, the kernel going
//! on; every case outside the copy's narrow rules must still stop.
//!
//! The guest (`src/tests/mce-copy`) keeps the kernel copying: a victim writes
//! a 1 MiB buffer whose words name their page in a loop, or, in the environ
//! mode, a reader reads another process's 64 KiB environment. The gate stops
//! the machine until a processor's `rip` is the copy's `rep movsb`
//! (`ferrix_copy_mc_movs`, from the kernel's ELF: the gate boots `nokaslr`),
//! reads the 8 bytes at its `rsi` (`xp`) to know whose page the copy is
//! loading, translates `rsi` to the physical page, and injects there. QEMU's
//! poison is not in memory: what this shows is the decision, the fixup and
//! the caller's path.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{AMD, INTEL, gpa, monitor, refusal, stopped};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce-copy",
        help_at: 232,
        help: &[
            "test-mce-copy  Poison a page while a system call copies it (AMD's records): write must fail EFAULT, the",
            "               program get SIGBUS BUS_MCEERR_AR at the page, the frame retire and the kernel go on",
            "               where SUCCOR is reported (KVM); else the fail-closed stop (TCG)",
        ],
        run: |_, args| test_copy(args, Mode::Copy),
    },
    Task {
        name: "test-mce-copy-kill",
        help_at: 233,
        help: &[
            "test-mce-copy-kill  test-mce-copy with RIPV clear: the kernel must stop as FX-9005",
        ],
        run: |_, args| test_copy(args, Mode::Kill),
    },
    Task {
        name: "test-mce-copy-tcc",
        help_at: 234,
        help: &["test-mce-copy-tcc  test-mce-copy with TCC set: the kernel must stop as FX-9005"],
        run: |_, args| test_copy(args, Mode::Tcc),
    },
    Task {
        name: "test-mce-copy-page",
        help_at: 235,
        help: &[
            "test-mce-copy-page  test-mce-copy with the record at another page than the copy reads: the kernel must stop",
        ],
        run: |_, args| test_copy(args, Mode::OtherPage),
    },
    Task {
        name: "test-mce-copy-off",
        help_at: 236,
        help: &[
            "test-mce-copy-off  test-mce-copy with ferrix.mce.recover=off: the kernel must stop",
        ],
        run: |_, args| test_copy(args, Mode::RecoverOff),
    },
    Task {
        name: "test-mce-copy-outside",
        help_at: 237,
        help: &[
            "test-mce-copy-outside  The record injected in ring 0 away from the copy: the kernel must stop",
        ],
        run: |_, args| test_copy(args, Mode::Outside),
    },
    Task {
        name: "test-mce-copy-environ",
        help_at: 238,
        help: &[
            "test-mce-copy-environ  Poison another process's environment while /proc/<pid>/environ copies it: the",
            "                       read ends short, the frame retires, nobody is signalled, the kernel goes on",
        ],
        run: |_, args| test_copy(args, Mode::Environ),
    },
    Task {
        name: "test-mce-copy-file",
        help_at: 245,
        help: &[
            "test-mce-copy-file  test-mce-copy with the buffer a shared mapping of a clean file: the call fails, SIGBUS,",
            "                    and the cached page is dropped too (docs/MACHINE-CHECK-PAGECACHE.md §3.5)",
        ],
        run: |_, args| test_copy(args, Mode::File),
    },
    Task {
        name: "test-mce-copy-intel",
        help_at: 239,
        help: &[
            "test-mce-copy-intel  test-mce-copy with Intel's records, recovered where MCG_SER_P is reported, else the",
            "                     stop; and with EIPV clear (another processor's), nothing done where it is",
        ],
        run: |_, args| {
            test_copy(args, Mode::Intel)?;
            test_copy(args, Mode::Bystander)
        },
    },
];

/// The guest program, `src/tests/mce-copy`.
const TEST_PROGRAM: &str = "mce-copy";
/// The victim's line, `mce-copy: victim pid=<pid> va=<hex> len=<hex>`.
const VICTIM: &str = "mce-copy: victim pid=";
/// The environ reader's line once the target runs.
const TARGET: &str = "mce-copy: target pid=";
/// The guest's last line.
const DONE: &str = "mce-copy: done";
/// The stop's catalogue code.
const FX_9005: &str = "FX-9005";
/// The victim's words' high half: `MCEC`.
const MAGIC: u64 = 0x4d43_4543;
/// Eight bytes of the environ target's padding.
const PAD: u64 = 0x5a5a_5a5a_5a5a_5a5a;

/// Intel's action-required data load: `VAL`, `UC`, `EN`, `MISCV`, `ADDRV`,
/// `S`, `AR`, MCA code 0x0134.
const SRAR_LOAD: u64 = 0xbd80_0000_0000_0134;
/// AMD's consumption: `VAL`, `UC`, `EN`, `MISCV`, `ADDRV`, `Poison`.
const AMD_AR: u64 = 0xbc00_0800_0000_0000;
/// AMD `MCA_STATUS.TCC`.
const TCC: u64 = 1 << 55;
/// `MCi_MISC`: a physical address recoverable to the page.
const MISC_PAGE: u64 = 0x8c;
/// `MCG_STATUS` `RIPV`, `EIPV`, `MCIP`.
const CONSUMER: u64 = 0x7;
/// `MCG_STATUS` `EIPV`, `MCIP`: not restartable.
const UNRESTARTABLE: u64 = 0x6;
/// `MCG_STATUS` `RIPV`, `MCIP`: another processor's consumption.
const BYSTANDER: u64 = 0x5;
/// The bank the modes inject into.
const BANK: u32 = 1;
/// Stops of the machine the gate makes looking for the copy.
const TRIES: usize = 400;
/// How long a mode waits for the guest's lines after it injects.
const PATIENCE: Duration = Duration::from_secs(90);

/// One mode. AMD's records but where said: under KVM on an AMD host the
/// guest has `SUCCOR`, so the recovery itself is shown; Intel's model there
/// has no `MCG_SER_P`, and TCG runs a `rep movsb` in one step, so no stop
/// catches it partway (ledger 5216 (8)'s rows read accordingly).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// Recovered: the call fails, the program is signalled.
    Copy,
    /// `RIPV` clear at the copy: stops.
    Kill,
    /// `TCC` set at the copy: stops.
    Tcc,
    /// The record names another page than `rsi`'s: stops.
    OtherPage,
    /// `ferrix.mce.recover=off`: stops.
    RecoverOff,
    /// Ring 0 away from the copy: stops.
    Outside,
    /// Another process's space: retired, nobody signalled.
    Environ,
    /// The buffer a clean file's shared mapping: as `Copy`, the page dropped.
    File,
    /// Intel's records: recovered where `MCG_SER_P` is reported, else stops.
    Intel,
    /// Intel's, `EIPV` clear with `RIPV` set at the copy: another
    /// processor's, nothing done.
    Bystander,
}

impl Mode {
    /// Whether the kernel must stop as FX-9005 whatever the processor reports.
    const fn stops(self) -> bool {
        matches!(
            self,
            Mode::Kill | Mode::Tcc | Mode::OtherPage | Mode::RecoverOff | Mode::Outside
        )
    }

    /// The guest's feature for the mode.
    const fn feature(self) -> Option<&'static str> {
        match self {
            Mode::Environ => Some("environ"),
            Mode::File => Some("file"),
            _ => None,
        }
    }

    /// Whether the mode's records are Intel's.
    const fn intel(self) -> bool {
        matches!(self, Mode::Intel | Mode::Bystander)
    }
}

/// What a mode injected, for its judgement.
#[derive(Default, Debug)]
struct Injected {
    /// The physical page injected at.
    page: u64,
    /// The user page the copy read there (copy modes).
    user: u64,
    /// The victim's buffer (copy modes).
    base: u64,
    /// The processor injected into.
    cpu: usize,
    /// Stops it took to find the copy.
    tries: usize,
}

/// One mode on x86-64.
///
/// # Errors
///
/// A boot that did not reach the guest, no copy found within [`TRIES`], an
/// injection the monitor refused, or a line the mode requires missing.
///
/// Verifies: `H.FAIL.10`
/// Verifies: `L.x86_64.245`
/// Verifies: `L.x86_64.246`
/// Verifies: `L.syscall.25`
/// Verifies: `L.syscall.26`
fn test_copy(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-copy ({mode:?}): x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, mode.feature(), false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let movs = symbol(&kernel.elf, "ferrix_copy_mc_movs")?;
    if mode == Mode::Copy {
        pinned(&kernel.elf).map_err(|why| Error::new(format!("{arch}: test-mce-copy: {why}")))?;
    }
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &guest_files(mode, &program)?)?;
    let options = if mode == Mode::RecoverOff {
        "ferrix.mce.poll=1 ferrix.mce.recover=off nokaslr\n"
    } else {
        "ferrix.mce.poll=1 nokaslr\n"
    };
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.x86_cpu_extra = Some(if mode.intel() { INTEL } else { AMD }.to_owned());
    if !qemu_args.smp_given {
        qemu_args.smp = 1;
        qemu_args.smp_given = true;
    }
    let smp = qemu_args.smp.max(1) as usize;
    // TCG runs a `rep movsb` in one step, so no stop finds it partway, and
    // reports no recovery (it drops `SUCCOR`; Intel's model has no
    // `MCG_SER_P` there): every record stops, wherever in ring 0 it lands.
    let tcg = qemu_args
        .accel
        .as_deref()
        .is_none_or(|accel| accel == "tcg");
    let marker = if mode == Mode::Environ {
        TARGET
    } else {
        VICTIM
    };
    let mut injected = Injected::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let aim = if tcg && !mode.stops() {
            Mode::Outside
        } else {
            mode
        };
        injected = inject(&mut qmp, watching.lines(), aim, smp, movs)?;
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || (seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
                    && seen.iter().any(|line| line.contains("  code ")))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, marker, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    let recovery = !lines
        .iter()
        .any(|line| line.contains("recovery not reported"));
    if tcg && recovery {
        return Err(Error::new(format!(
            "{arch}: test-mce-copy ({mode:?}): TCG reported recovery, and this gate cannot place \
             an injection inside a rep movsb under TCG"
        )));
    }
    judge(mode, &lines, &injected, recovery).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-copy ({mode:?}): {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-copy ({mode:?}): as required (page {:#x} on cpu {}, found in {} \
         stop(s), {smp} processor(s){})",
        injected.page,
        injected.cpu,
        injected.tries,
        match (tcg, recovery) {
            (true, _) => "; TCG: a fail-closed row only, no recovery is shown under TCG",
            (false, true) => "",
            (false, false) => "; recovery not reported, so it stopped (a fail-closed row only)",
        }
    );
    Ok(())
}

/// The image's extra files for `mode`: the environ mode's target is this
/// program run again, and pid 1's own copy is gone once its inputs are
/// released, so it gets a copy of its own.
fn guest_files(mode: Mode, program: &std::path::Path) -> Result<Vec<crate::ports::File>> {
    if mode != Mode::Environ {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    Ok(vec![crate::ports::File {
        path: "bin/mce-copy".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(bytes),
    }])
}

/// The address of `name` in the kernel's ELF, by `nm`.
pub(crate) fn symbol(elf: &std::path::Path, name: &str) -> Result<u64> {
    let output = std::process::Command::new("nm")
        .arg(elf)
        .output()
        .map_err(|error| Error::new(format!("nm could not run: {error}")))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let _kind = fields.next()?;
            (fields.next()? == name).then(|| u64::from_str_radix(address, 16).ok())?
        })
        .ok_or_else(|| Error::new(format!("the kernel has no {name}")))
}

/// `copy_mc` as built (`L.x86_64.244`, ledger 5216 (12)): exactly `mov
/// rcx, rdx`, the `rep movsb`, `xor edx, edx`, `mov rax, rcx` and `ret` --
/// so from `ferrix_copy_mc_movs` to `ferrix_copy_mc_out` nothing touches
/// `rcx` or memory but the movs, and nothing is called.
fn pinned(elf: &std::path::Path) -> std::result::Result<(), String> {
    let output = std::process::Command::new("objdump")
        .args(["-d", "--no-show-raw-insn", "-M", "intel"])
        .arg(elf)
        .output()
        .map_err(|error| format!("objdump could not run: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let start = text
        .find("<ferrix_copy_mc>:")
        .ok_or("the kernel has no ferrix_copy_mc")?;
    let body: Vec<String> = text
        .get(start..)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (_, instruction) = line.split_once(":\t")?;
            Some(instruction.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .take(5)
        .collect();
    let want = [
        "mov rcx,rdx",
        "rep movs BYTE PTR [rdi],BYTE PTR [rsi]",
        "xor edx,edx",
        "mov rax,rcx",
        "ret",
    ];
    if body != want {
        return Err(format!(
            "ferrix_copy_mc is not one rep movsb and its exit: {body:?}"
        ));
    }
    Ok(())
}

/// A register's value in the monitor's `info registers` answer: `RIP=...`.
pub(crate) fn register(registers: &str, name: &str) -> Option<u64> {
    let at = registers.find(&format!("{name}="))? + name.len() + 1;
    u64::from_str_radix(registers.get(at..at + 16)?, 16).ok()
}

/// Whether `info registers` shows privilege level 0: `CS =<selector>`'s low
/// two bits.
pub(crate) fn in_ring_0(registers: &str) -> bool {
    registers
        .split("CS =")
        .nth(1)
        .and_then(|rest| u16::from_str_radix(rest.get(..4)?, 16).ok())
        .is_some_and(|selector| selector & 3 == 0)
}

/// The eight bytes at the physical address `at`, by the monitor's `xp`.
pub(crate) fn word_at(qmp: &mut Qmp, at: u64) -> Result<Option<u64>> {
    let reply = monitor(qmp, &format!("xp /1gx {at:#x}"), None)?;
    Ok(reply
        .split(": 0x")
        .nth(1)
        .and_then(|rest| u64::from_str_radix(rest.get(..16)?, 16).ok()))
}

/// Find a processor where `mode` wants it, with the machine stopped, and
/// inject its record there; resume either way between tries.
fn inject(qmp: &mut Qmp, lines: &[String], mode: Mode, smp: usize, movs: u64) -> Result<Injected> {
    let base = if mode == Mode::Environ {
        0
    } else {
        lines
            .iter()
            .rev()
            .find_map(|line| {
                let at = line.find(" va=0x")? + " va=0x".len();
                u64::from_str_radix(line.get(at..)?.split_whitespace().next()?, 16).ok()
            })
            .ok_or_else(|| Error::new("the victim printed no address"))?
    };
    // What the stops saw, for a failure's message: in ring 0, at the movs,
    // with bytes left, over the guest's buffer; and the last `rip`.
    let mut seen = [0usize; 4];
    let mut rips = std::collections::BTreeMap::<u64, usize>::new();
    for tries in 1..=TRIES {
        stopped(qmp, true)?;
        let mut found = None;
        for cpu in 0..smp {
            let registers = monitor(qmp, "info registers", Some(cpu))?;
            let (Some(rip), Some(rsi)) = (register(&registers, "RIP"), register(&registers, "RSI"))
            else {
                continue;
            };
            *rips.entry(rip).or_default() += 1;
            if !in_ring_0(&registers) {
                continue;
            }
            seen[0] += 1;
            if mode == Mode::Outside {
                if rip != movs {
                    found = Some((cpu, 0x1000, 0));
                    break;
                }
                continue;
            }
            // At the movs with bytes left: at 0 it loaded nothing, and the
            // kernel would rightly stop.
            if rip != movs {
                continue;
            }
            seen[1] += 1;
            if register(&registers, "RCX").is_none_or(|rcx| rcx == 0) {
                continue;
            }
            seen[2] += 1;
            let Some(physical) = gpa(&monitor(qmp, &format!("gva2gpa {rsi:#x}"), Some(cpu))?)
            else {
                continue;
            };
            let Some(word) = word_at(qmp, physical & !7)? else {
                continue;
            };
            let page = physical & !0xfff;
            seen[3] += usize::from(word == PAD || word >> 32 == MAGIC);
            let user = match mode {
                Mode::Environ if word == PAD => 0,
                Mode::Environ => continue,
                _ if word >> 32 == MAGIC => base + (word & 0xffff_ffff) * 0x1000,
                _ => continue,
            };
            found = Some((cpu, page, user));
            break;
        }
        let made = found.map(|(cpu, page, user)| {
            let (status, mcg, address) = record(mode, page);
            make(qmp, cpu, status, mcg, address).map(|()| Injected {
                page: address & !0xfff,
                user,
                base,
                cpu,
                tries,
            })
        });
        stopped(qmp, false)?;
        if let Some(made) = made {
            return made;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(Error::new(format!(
        "no processor was at the copy's rep movsb over the guest's buffer within {TRIES} stops \
         ({} in ring 0, {} at {movs:#x}, {} with bytes left, {} over the buffer; most seen: {})",
        seen[0],
        seen[1],
        seen[2],
        seen[3],
        most_seen(&rips)
    )))
}

/// The five instruction pointers the stops found most often, with counts.
fn most_seen(rips: &std::collections::BTreeMap<u64, usize>) -> String {
    let mut counted: Vec<(u64, usize)> = rips.iter().map(|(&rip, &count)| (rip, count)).collect();
    counted.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    counted
        .iter()
        .take(5)
        .map(|(rip, count)| format!("{rip:#x} x{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `mode`'s record for the copy reading `page`: status, `MCG_STATUS`, address.
const fn record(mode: Mode, page: u64) -> (u64, u64, u64) {
    match mode {
        Mode::Copy | Mode::RecoverOff | Mode::Outside | Mode::Environ | Mode::File => {
            (AMD_AR, CONSUMER, page)
        }
        Mode::Kill => (AMD_AR, UNRESTARTABLE, page),
        Mode::Tcc => (AMD_AR | TCC, CONSUMER, page),
        Mode::OtherPage => (AMD_AR, CONSUMER, page ^ 0x1000),
        Mode::Intel => (SRAR_LOAD, CONSUMER, page),
        Mode::Bystander => (SRAR_LOAD, BYSTANDER, page),
    }
}

/// One injection, refused by the monitor or made.
pub(crate) fn make(qmp: &mut Qmp, cpu: usize, status: u64, mcg: u64, address: u64) -> Result<()> {
    let command = format!("mce {cpu} {BANK} {status:#x} {mcg:#x} {address:#x} {MISC_PAGE:#x}");
    let reply = monitor(qmp, &command, None)?;
    if let Some(problem) = refusal(&reply) {
        return Err(Error::new(format!(
            "the monitor did not inject `{command}`: {problem}"
        )));
    }
    println!("  x86_64: injected `{command}`");
    Ok(())
}

/// What `mode` requires of the boot's `lines`.
fn judge(
    mode: Mode,
    lines: &[String],
    injected: &Injected,
    recovery: bool,
) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let stopped = has(qemu::PANIC_MARKER)
        && has(FX_9005)
        && !has("mce-copy: write failed")
        && !has("mce-copy: write ended short");
    // Where the processor reports no recovery (Intel's model without
    // `MCG_SER_P`, AMD's without `SUCCOR`), every uncorrected record stops:
    // the fail-closed answer, as test-mce-srar accepts it.
    if mode.stops() || !recovery {
        if !stopped {
            return Err("the kernel went on where it must stop as FX-9005".to_owned());
        }
        // The machine check's own stop, not a later one: the copy's caller
        // re-checks the page and stops too (`uaccess::poisoned`), which
        // would hide a handler that recovered where it must not.
        return has("FERRIX-PANIC machine check")
            .then_some(())
            .ok_or_else(|| "the stop was not the machine check's own".to_owned());
    }
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    let retired = format!("mce: frame {:#x} retired", injected.page);
    let copies = format!("cpu{} ", injected.cpu);
    let counted = lines.iter().any(|line| {
        line.contains("/proc/ferrix-mce: ") && line.contains(&copies) && line.ends_with(" copies 1")
    });
    match mode {
        Mode::Copy | Mode::Intel | Mode::File => {
            let signal = format!("mce-copy: SIGBUS code=4 addr={:#x} lsb=12", injected.user);
            // The write fails, or ends short exactly at the poisoned page:
            // the bytes before it stored, as for a fault there.
            let short = format!(
                "mce-copy: write ended short at {:#x}",
                injected.user - injected.base
            );
            if !has("mce-copy: write failed errno=14") && !has(&short) {
                return Err(format!(
                    "the write neither failed with EFAULT nor ended short at the poisoned page ({short})"
                ));
            }
            all(
                lines,
                &[
                    &signal,
                    &retired,
                    "mce-copy: a fresh write of 1048576 bytes",
                    "mce-copy: victim exited 0",
                    DONE,
                ],
            )?;
            let dropped = format!(
                "mce: frame {:#x} was a clean cached page, dropped",
                injected.page
            );
            if mode == Mode::File && !has(&dropped) {
                return Err(format!("the boot never said `{dropped}`"));
            }
            counted.then_some(()).ok_or_else(|| {
                format!(
                    "/proc/ferrix-mce did not count the copy on cpu{}",
                    injected.cpu
                )
            })
        }
        Mode::Bystander => {
            all(
                lines,
                &[
                    "mce-copy: write never failed",
                    "mce-copy: victim exited 3",
                    DONE,
                ],
            )?;
            if has("mce: frame ") || has("mce-copy: SIGBUS") {
                return Err("another processor's record was acted on".to_owned());
            }
            Ok(())
        }
        Mode::Environ => {
            all(
                lines,
                &[
                    "mce-copy: environ short ",
                    "mce-copy: target alive",
                    &retired,
                    DONE,
                ],
            )?;
            if has("mce-copy: reader SIGBUS") || has("mce-copy: target SIGBUS") {
                return Err("a process was signalled for poison in another's space".to_owned());
            }
            counted.then_some(()).ok_or_else(|| {
                format!(
                    "/proc/ferrix-mce did not count the copy on cpu{}",
                    injected.cpu
                )
            })
        }
        Mode::Kill | Mode::Tcc | Mode::OtherPage | Mode::RecoverOff | Mode::Outside => Ok(()),
    }
}

/// Every line of `needs` somewhere in `lines`, or the first missing.
fn all(lines: &[String], needs: &[&str]) -> std::result::Result<(), String> {
    for want in needs {
        if !lines.iter().any(|line| line.contains(want)) {
            return Err(format!("the boot never said `{want}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{in_ring_0, register};

    #[test]
    fn registers_are_read_from_the_monitors_answer() {
        let answer = "RAX=0000000000000001 RBX=0000000000000002 RCX=0000000000000010 RDX=0000000000000000\n\
                      RSI=ffff800000123456 RDI=ffff8000000a0000 RBP=0 RSP=0\n\
                      RIP=ffffffff80123450 RFL=00000246\n\
                      CS =0008 0000000000000000 ffffffff 00af9b00 DPL=0 CS64 [-RA]\n";
        assert_eq!(register(answer, "RIP"), Some(0xffff_ffff_8012_3450));
        assert_eq!(register(answer, "RSI"), Some(0xffff_8000_0012_3456));
        assert!(in_ring_0(answer));
        assert!(!in_ring_0("CS =0033 0000000000000000"));
    }
}
