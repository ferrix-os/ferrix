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
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce",
        help_at: 205,
        help: &[
            "test-mce      Boot x86_64, inject a machine check into every processor through QEMU's monitor after",
            "              stage 4, and require the kernel to stop as FX-9005 (F-73)",
        ],
        run: |_, args| test_mce(args),
    },
    Task {
        name: "test-mce-corrected",
        help_at: 207,
        help: &[
            "test-mce-corrected  Inject a corrected error into every x86_64 processor; each poll must log and count it once",
        ],
        run: |_, args| test_mode(args, Mode::Corrected),
    },
    Task {
        name: "test-mce-srar",
        help_at: 208,
        help: &[
            "test-mce-srar  Poison a page a ring-3 program reads (action required); it must get SIGBUS BUS_MCEERR_AR, the kernel go on",
        ],
        run: |_, args| test_mode(args, Mode::Srar),
    },
    Task {
        name: "test-mce-broadcast",
        help_at: 209,
        help: &[
            "test-mce-broadcast  test-mce-srar on two processors, the other told of it as a broadcast: it must not stop",
        ],
        run: |_, args| test_mode(args, Mode::Broadcast),
    },
    Task {
        name: "test-mce-srao",
        help_at: 211,
        help: &[
            "test-mce-srao  Poison a mapped page with no consumer (action optional): logged and retired, nobody signalled",
        ],
        run: |_, args| test_mode(args, Mode::Srao),
    },
    Task {
        name: "test-mce-left",
        help_at: 212,
        help: &[
            "test-mce-left  A corrected error on every processor while the loader runs: each must log it as left from before",
        ],
        run: |_, args| test_mode(args, Mode::Left),
    },
    Task {
        name: "test-mce-recover-off",
        help_at: 213,
        help: &[
            "test-mce-recover-off  test-mce-srar with ferrix.mce.recover=off: the kernel must stop as FX-9005",
        ],
        run: |_, args| test_mode(args, Mode::RecoverOff),
    },
    Task {
        name: "test-mce-kill",
        help_at: 214,
        help: &[
            "test-mce-kill  test-mce-srar with RIPV clear: the victim must be ended by SIGBUS without its handler, the kernel go on",
        ],
        run: |_, args| test_mode(args, Mode::Kill),
    },
    Task {
        name: "test-mce-amd-srar",
        help_at: 215,
        help: &[
            "test-mce-amd-srar  An AMD consumption (Poison) a ring-3 program reads: SIGBUS BUS_MCEERR_AR where SUCCOR is reported (KVM), the fail-closed stop where not (TCG)",
        ],
        run: |_, args| test_amd(args, "srar"),
    },
    Task {
        name: "test-mce-amd-kill",
        help_at: 216,
        help: &[
            "test-mce-amd-kill  test-mce-amd-srar with RIPV clear: the victim ended without its handler (or the fail-closed stop)",
        ],
        run: |_, args| test_amd(args, "kill"),
    },
    Task {
        name: "test-mce-amd-tcc",
        help_at: 217,
        help: &[
            "test-mce-amd-tcc  test-mce-amd-srar with TCC set: the victim ended without its handler (or the fail-closed stop)",
        ],
        run: |_, args| test_amd(args, "tcc"),
    },
    Task {
        name: "test-mce-amd-eipv",
        help_at: 218,
        help: &[
            "test-mce-amd-eipv  An AMD consumption with EIPV clear: the kernel stops (AMD has no bystander)",
        ],
        run: |_, args| test_amd(args, "eipv"),
    },
    Task {
        name: "test-mce-amd-pcc",
        help_at: 219,
        help: &["test-mce-amd-pcc  An AMD record with PCC: the kernel stops"],
        run: |_, args| test_amd(args, "pcc"),
    },
    Task {
        name: "test-mce-amd-nosuccor",
        help_at: 221,
        help: &[
            "test-mce-amd-nosuccor  An AMD consumption on a model without SUCCOR: the kernel stops",
        ],
        run: |_, args| test_amd(args, "nosuccor"),
    },
    Task {
        name: "test-mce-amd-over",
        help_at: 222,
        help: &[
            "test-mce-amd-over  An overflowed AMD consumption without OverflowRecov: the kernel stops",
        ],
        run: |_, args| test_amd(args, "over"),
    },
    Task {
        name: "test-mce-amd-deferred",
        help_at: 223,
        help: &[
            "test-mce-amd-deferred  An AMD deferred record: logged by the poll, its frame not retired",
        ],
        run: |_, args| test_amd(args, "deferred"),
    },
    Task {
        name: "test-mce-amd-ring0",
        help_at: 224,
        help: &["test-mce-amd-ring0  An AMD consumption in ring 0: the kernel stops"],
        run: |_, args| test_amd(args, "ring0"),
    },
    Task {
        name: "test-mce-amd-corrected",
        help_at: 225,
        help: &["test-mce-amd-corrected  An AMD corrected record: logged once by the poll"],
        run: |_, args| test_amd(args, "corrected"),
    },
    Task {
        name: "test-mce-amd-recover-off",
        help_at: 226,
        help: &[
            "test-mce-amd-recover-off  test-mce-amd-srar with ferrix.mce.recover=off: the kernel stops",
        ],
        run: |_, args| test_amd(args, "recover-off"),
    },
];

/// `test-mce` on x86-64; skipped on the other architectures, which have no
/// machine-check exception to enable.
///
/// # Errors
///
/// A boot that did not reach stage 4, a monitor that refused an injection,
/// or a kernel that did not stop as FX-9005.
///
/// Verifies: `L.x86_64.175`
/// Verifies: `L.x86_64.227`
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
        // Intel's rules, whatever the accelerator's model says
        // (`docs/MACHINE-CHECK-AMD.md` §6, the consultant's A8).
        qemu_args.x86_cpu_extra = Some(INTEL.to_owned());
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
            // secondary, which starts from a CR4 without the bit.
            for cpu in (0..processors).rev() {
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
        println!(
            "  {arch}: a machine check injected into each of {processors} processors stopped the \
             kernel as {MACHINE_CHECK}"
        );
    }
    Ok(())
}

/// `docs/MACHINE-CHECK.md` §5's recovery modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A corrected error in bank 1 of every processor: one `corrected` line
    /// from each processor's poll (`L.x86_64.223`).
    Corrected,
    /// An action-required error at the victim's page on the processor it
    /// runs on: `SIGBUS` `BUS_MCEERR_AR` at its address, the frame retired,
    /// the kernel on (`L.x86_64.228`).
    Srar,
    /// [`Mode::Srar`] on two processors, the other given the same record
    /// with `EIPV` clear, as a broadcast or a shared bank shows it: that one
    /// returns (`L.x86_64.225`).
    Broadcast,
    /// An action-optional error at the victim's page: logged and retired by
    /// the poll, the victim untouched (`L.x86_64.226`).
    Srao,
    /// A corrected error in bank 1 of every processor while the loader runs,
    /// before the kernel brings any bank up: each processor's poll logs it as
    /// left from before this boot (`L.x86_64.222`). What a warm reset keeps
    /// on hardware, injected where the gates' `-no-reboot` QEMU can show it.
    Left,
    /// [`Mode::Srar`] with `ferrix.mce.recover=off`: the kernel stops as
    /// FX-9005 (`L.x86_64.231`).
    RecoverOff,
    /// [`Mode::Srar`] with `RIPV` clear: the instruction cannot be
    /// restarted, so the victim is ended by `SIGBUS` without its handler and
    /// the kernel goes on (`L.x86_64.228`, the consultant's V2).
    Kill,
}

/// The guest program the modes boot as init (`src/tests/mce`).
const TEST_PROGRAM: &str = "mce";
/// The kernel options every mode boots with: a poll every second.
const POLL_EVERY_SECOND: &str = "ferrix.mce.poll=1\n";
/// The victim's line: `mce-test: victim pid=<pid> va=<address>`.
const VICTIM: &str = "mce-test: victim pid=";
/// The guest's last line.
const DONE: &str = "mce-test: done";
/// The loader's banner, printed before the kernel brings any bank up.
const LOADER: &str = "Ferrix loader ";
/// The machine check's catalogue code.
const FX_9005: &str = "FX-9005";

/// `MCi_STATUS` of a corrected memory read error: `VAL`, `EN`, `MISCV`,
/// `ADDRV`, MCA code 0x9f.
const CORRECTED: u64 = 0x9c00_0000_0000_009f;
/// `MCi_STATUS` of an action-required data load: `VAL`, `UC`, `EN`,
/// `MISCV`, `ADDRV`, `S`, `AR`, MCA code 0x134 (SDM table 16-28).
const SRAR: u64 = 0xbd80_0000_0000_0134;
/// `MCi_STATUS` of an action-optional patrol-scrub find: `VAL`, `UC`,
/// `EN`, `MISCV`, `ADDRV`, `S`, MCA code 0xc0.
const SRAO: u64 = 0xbd00_0000_0000_00c0;
/// `MCi_MISC`: a physical address, recoverable to the page.
const MISC_PAGE: u64 = 0x8c;
/// `MCG_STATUS`: `RIPV`, `EIPV` and `MCIP` -- the consumer's.
const CONSUMER: u64 = 0x7;
/// `MCG_STATUS`: `RIPV` and `MCIP` -- a processor that did not consume.
const BYSTANDER: u64 = 0x5;
/// `MCG_STATUS`: `EIPV` and `MCIP`, `RIPV` clear -- a consumer that cannot
/// restart its instruction.
const UNRESTARTABLE: u64 = 0x6;
/// The bank the modes inject into.
const BANK: u32 = 1;

/// How long a mode waits for the guest's lines after it injects.
const MODE_PATIENCE: Duration = Duration::from_secs(60);

/// One recovery mode on x86-64.
///
/// # Errors
///
/// A boot that did not reach the victim, an injection the monitor refused,
/// or a line the mode requires missing, or a stop.
///
/// Verifies: `H.FAIL.8`
/// Verifies: `L.x86_64.221`
/// Verifies: `L.x86_64.222`
/// Verifies: `L.x86_64.223`
/// Verifies: `L.x86_64.225`
/// Verifies: `L.x86_64.226`
/// Verifies: `L.x86_64.228`
/// Verifies: `L.x86_64.230`
/// Verifies: `L.x86_64.231`
/// Verifies: `L.mm.91`
fn test_mode(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-{mode:?}: x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let options = if mode == Mode::RecoverOff {
        "ferrix.mce.poll=1 ferrix.mce.recover=off\n"
    } else {
        POLL_EVERY_SECOND
    };
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    // Intel's rules: these modes' records are Intel's (A8).
    qemu_args.x86_cpu_extra = Some(INTEL.to_owned());
    qemu_args.smp = match mode {
        Mode::Srar | Mode::Srao | Mode::RecoverOff | Mode::Kill => 1,
        Mode::Broadcast => 2,
        Mode::Corrected | Mode::Left => args.smp,
    };
    qemu_args.smp_given = true;
    let mut injected = Injected::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        injected = inject(&mut qmp, watching.lines(), mode, qemu_args.smp)?;
        let deadline = Instant::now() + MODE_PATIENCE;
        // To the guest's last line, or through a stop's whole report: its
        // catalogue code comes after the banks.
        let _ = watching.read_more(deadline, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || (seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
                    && seen.iter().any(|line| line.contains("  code ")))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let marker = if mode == Mode::Left { LOADER } else { VICTIM };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, marker, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    judge(mode, &lines, &injected).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-{mode:?}: {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    let closed = !lines
        .iter()
        .any(|line| line.contains("recovery not reported"))
        || matches!(mode, Mode::Corrected | Mode::Left | Mode::RecoverOff);
    println!(
        "  {arch}: test-mce-{mode:?}: as required ({}){}",
        injected.said,
        if closed {
            ""
        } else {
            "; no MCG_SER_P here, so it stopped as FX-9005"
        }
    );
    Ok(())
}

/// What a mode injected, for its judgement.
#[derive(Default, Debug)]
struct Injected {
    /// The victim's virtual address, from its line.
    virt: u64,
    /// The physical page the monitor gave for it.
    page: u64,
    /// The processor the victim ran on.
    consumer: usize,
    /// Processors injected into.
    processors: usize,
    /// A sentence for the success line.
    said: String,
}

/// Make `mode`'s injections, and say what was made.
fn inject(qmp: &mut Qmp, lines: &[String], mode: Mode, smp: u32) -> Result<Injected> {
    let processors = smp.max(1) as usize;
    if mode == Mode::Left {
        for cpu in (0..processors).rev() {
            let command = format!("mce {cpu} {BANK} {CORRECTED:#x} 0 0x1000 {MISC_PAGE:#x}");
            let reply = monitor(qmp, &command, None)?;
            if let Some(problem) = refusal(&reply) {
                return Err(Error::new(format!(
                    "the monitor did not inject `{command}`: {problem}"
                )));
            }
            println!("  x86_64: injected `{command}` while the loader ran");
        }
        return Ok(Injected {
            processors,
            said: format!("a corrected error on each of {processors} processors before the kernel"),
            ..Injected::default()
        });
    }
    let virt = lines
        .iter()
        .rev()
        .find_map(|line| {
            let at = line.find(" va=0x")? + " va=0x".len();
            u64::from_str_radix(line.get(at..)?.trim(), 16).ok()
        })
        .ok_or_else(|| Error::new("the victim printed no address"))?;
    // The consumer is the processor running the victim in ring 3: its `CS`
    // has privilege level 3 and `gva2gpa` walks its tables (an idle
    // processor may keep the victim's tables loaded, so the translation
    // alone does not say which one runs it). The machine is stopped while
    // that is read and the records are injected, then resumed: a machine
    // check outranks every pending interrupt, so it is taken on the
    // program's own instruction. Injected into a running machine it lands
    // wherever the processor is when QEMU gets to it, often the kernel.
    let mut done = None;
    for _ in 0..200 {
        stopped(qmp, true)?;
        let mut found = None;
        for cpu in 0..processors {
            if !in_ring_3(&monitor(qmp, "info registers", Some(cpu))?) {
                continue;
            }
            let reply = monitor(qmp, &format!("gva2gpa {virt:#x}"), Some(cpu))?;
            if let Some(physical) = gpa(&reply) {
                found = Some((cpu, physical & !0xfff));
                break;
            }
        }
        let made = found.map(|(consumer, page)| make_all(qmp, mode, processors, consumer, page));
        stopped(qmp, false)?;
        if let Some(made) = made {
            done = Some(made?);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let (consumer, page, said) = done.ok_or_else(|| {
        Error::new(format!(
            "no processor ran the victim in ring 3 with {virt:#x} mapped within 200 tries"
        ))
    })?;
    Ok(Injected {
        virt,
        page,
        consumer,
        processors,
        said,
    })
}

/// Stop the machine, or let it go on.
pub(crate) fn stopped(qmp: &mut Qmp, stop: bool) -> Result<()> {
    let _ = qmp.execute(if stop { "stop" } else { "cont" }, None)?;
    Ok(())
}

/// One injection, refused by the monitor or made.
fn make(qmp: &mut Qmp, cpu: usize, status: u64, mcg: u64, address: u64) -> Result<()> {
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

/// `mode`'s records, with the victim's `page` and the processor running it:
/// the consumer, the page and a sentence.
fn make_all(
    qmp: &mut Qmp,
    mode: Mode,
    processors: usize,
    consumer: usize,
    page: u64,
) -> Result<(usize, u64, String)> {
    let said = match mode {
        Mode::Corrected => {
            for cpu in (0..processors).rev() {
                make(qmp, cpu, CORRECTED, 0, page)?;
            }
            format!("a corrected error on each of {processors} processors")
        }
        Mode::Srar | Mode::RecoverOff => {
            make(qmp, consumer, SRAR, CONSUMER, page)?;
            format!("action required at {page:#x} on processor {consumer}")
        }
        Mode::Broadcast => {
            for cpu in (0..processors).filter(|&cpu| cpu != consumer) {
                make(qmp, cpu, SRAR, BYSTANDER, page)?;
            }
            make(qmp, consumer, SRAR, CONSUMER, page)?;
            format!("action required at {page:#x} on processor {consumer}, told to the other")
        }
        Mode::Kill => {
            make(qmp, consumer, SRAR, UNRESTARTABLE, page)?;
            format!("action required at {page:#x} on processor {consumer}, RIPV clear")
        }
        Mode::Srao => {
            make(qmp, consumer, SRAO, BYSTANDER, page)?;
            format!("action optional at {page:#x}")
        }
        // Injected before the victim (`inject`).
        Mode::Left => String::new(),
    };
    Ok((consumer, page, said))
}

/// Whether the monitor's `info registers` shows privilege level 3: its
/// `CS =<selector>` line's low two bits.
fn in_ring_3(registers: &str) -> bool {
    registers
        .split("CS =")
        .nth(1)
        .and_then(|rest| u16::from_str_radix(rest.get(..4)?, 16).ok())
        .is_some_and(|selector| selector & 3 == 3)
}

/// The physical address in the monitor's `gva2gpa` answer, `gpa: 0x...`.
pub(crate) fn gpa(reply: &str) -> Option<u64> {
    let at = reply.find("gpa: 0x")? + "gpa: 0x".len();
    let digits: String = reply
        .get(at..)?
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    u64::from_str_radix(&digits, 16).ok()
}

/// One human-monitor command, on processor `cpu` where it matters.
pub(crate) fn monitor(qmp: &mut Qmp, command: &str, cpu: Option<usize>) -> Result<String> {
    let mut arguments = format!(
        "{{\"command-line\":{}",
        crate::display::json_string(command)
    );
    if let Some(cpu) = cpu {
        arguments.push_str(&format!(",\"cpu-index\":{cpu}"));
    }
    arguments.push('}');
    qmp.execute("human-monitor-command", Some(&arguments))
}

/// Whether `lines` show what `mode` requires.
fn judge(mode: Mode, lines: &[String], injected: &Injected) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let stopped = has(qemu::PANIC_MARKER) && has(FX_9005) && !has("mce-test: SIGBUS");
    // A processor model without `MCG_SER_P` -- KVM on an AMD host offers
    // none -- reports no recovery, and the kernel must then stop on every
    // uncorrected record (`L.x86_64.224`): the recovery modes require that
    // stop there instead.
    let uncorrected = matches!(mode, Mode::Srar | Mode::Broadcast | Mode::Srao | Mode::Kill);
    if uncorrected && has("recovery not reported") {
        return stopped.then_some(()).ok_or_else(|| {
            "a processor reporting no recovery went on after an uncorrected error".to_owned()
        });
    }
    if mode == Mode::RecoverOff {
        return stopped.then_some(()).ok_or_else(|| {
            "with ferrix.mce.recover=off the machine check did not stop as FX-9005".to_owned()
        });
    }
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    let page = format!("{:#x}", injected.page);
    let retired = format!("mce-test: /proc/ferrix-mce-retired: {page}");
    match mode {
        Mode::Corrected | Mode::Left => {
            let what = if mode == Mode::Left {
                "left from before this boot"
            } else {
                "corrected"
            };
            for cpu in 0..injected.processors {
                once(lines, &format!("mce: cpu {cpu} bank {BANK} {what}"))?;
            }
            if mode == Mode::Corrected && has(&retired) {
                return Err("a corrected error retired its frame".to_owned());
            }
            if mode == Mode::Left && has(&format!("bank {BANK} corrected")) {
                return Err("a record left from before was also counted as new".to_owned());
            }
        }
        Mode::Srar | Mode::Broadcast => {
            let want = format!(
                "mce-test: SIGBUS code=4 addr={:#x} lsb=12 err=0x0",
                injected.virt
            );
            all(
                lines,
                &[&want, "mce-test: victim exited 42", &retired, DONE],
            )?;
            if mode == Mode::Broadcast {
                let other = 1 - injected.consumer.min(1);
                let bystander = format!("mce-test: /proc/ferrix-mce: cpu{other} ");
                let spurious = lines
                    .iter()
                    .rfind(|line| line.contains(&bystander))
                    .is_some_and(|line| line.contains("spurious 1"));
                if !spurious {
                    return Err(format!(
                        "processor {other} did not count its check as spurious"
                    ));
                }
            }
        }
        Mode::Srao => {
            let optional = format!("mce: cpu {} bank {BANK} action-optional", injected.consumer);
            let frame = format!("mce: frame {page} retired");
            all(
                lines,
                &[
                    &optional,
                    &frame,
                    "mce-test: victim exited 0",
                    &retired,
                    DONE,
                ],
            )?;
        }
        Mode::Kill => {
            all(
                lines,
                &[
                    "mce-test: victim killed by signal 7",
                    &retired,
                    "mce-test: a second program ran",
                    DONE,
                ],
            )?;
            if has("mce-test: SIGBUS") {
                return Err(
                    "the victim's handler ran for an unrestartable machine check".to_owned(),
                );
            }
        }
        Mode::RecoverOff => {}
    }
    Ok(())
}

/// Every one of `needs` is on some line.
fn all(lines: &[String], needs: &[&str]) -> std::result::Result<(), String> {
    match needs
        .iter()
        .find(|need| !lines.iter().any(|line| line.contains(**need)))
    {
        Some(need) => Err(format!("no `{need}`")),
        None => Ok(()),
    }
}

/// `want` is on exactly one line.
fn once(lines: &[String], want: &str) -> std::result::Result<(), String> {
    match lines.iter().filter(|line| line.contains(want)).count() {
        1 => Ok(()),
        times => Err(format!("`{want}` was printed {times} times, not once")),
    }
}

/// The processors stage 4's line says are online.
fn online(lines: &[String]) -> Option<usize> {
    let line = lines.iter().rev().find(|line| line.contains(STAGE_4))?;
    let at = line.find(STAGE_4)? + STAGE_4.len();
    line.get(at..)?.split_whitespace().next()?.parse().ok()
}

/// What the monitor's reply to an injection says went wrong, if anything:
/// its `return` string, which is empty when the injection was made.
pub(crate) fn refusal(reply: &str) -> Option<String> {
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

    /// Every machine-check register access in the kernel's bring-up is the
    /// checked one, and the bank cap is 32.
    ///
    /// Verifies: `L.x86_64.232`
    #[test]
    fn bring_up_uses_only_checked_accesses() {
        let source = include_str!("../../../../src/kernel/src/arch/x86_64/mce.rs");
        // `bring_up` and the two it calls, `keep_left` and `enable`, up to
        // the vendor test that follows them.
        let start = source.find("pub(super) fn bring_up()").unwrap_or(0);
        let body = source.get(start..).unwrap_or("");
        let end = body
            .find("/// Whether `CPUID` names Intel.")
            .unwrap_or(body.len());
        let body = body.get(..end).unwrap_or("");
        assert!(body.contains("fn keep_left(") && body.contains("fn enable("));
        assert!(body.contains("read_msr_checked"), "bring_up was not found");
        for unchecked in [
            "cpu::read_msr(",
            "cpu::write_msr(",
            "read_bank(",
            "clear_bank(",
        ] {
            assert!(
                !body.contains(unchecked),
                "bring_up makes an unchecked access: {unchecked}"
            );
        }
        assert!(source.contains("pub(super) const MAX_BANKS: usize = 32;"));
    }

    /// AMD's SMCA registers are reached only in `smca_bank_up` (`MCA_CONFIG`
    /// read and written, `MCA_IPID` read) and `read_synd` (`MCA_SYND` read,
    /// only with `SyndV`), each through the checked accesses alone.
    ///
    /// Verifies: `L.x86_64.239`
    #[test]
    fn smca_registers_only_in_their_two_functions() {
        let source = include_str!("../../../../src/kernel/src/arch/x86_64/mce.rs");
        let mut function = "";
        let mut seen = Vec::new();
        for line in source.lines() {
            if let Some(name) = line
                .trim_start()
                .strip_prefix("fn ")
                .and_then(|rest| rest.split('(').next())
            {
                function = name;
            }
            if ["smca_bank_up", "read_synd"].contains(&function) {
                assert!(
                    !line.contains("cpu::read_msr(") && !line.contains("cpu::write_msr("),
                    "{function} makes an unchecked access: {line}"
                );
            }
            if line.contains("smca_msr(") && !line.contains("fn smca_msr") {
                let register = ["SMCA_CONFIG", "SMCA_IPID", "SMCA_SYND"]
                    .into_iter()
                    .find(|register| line.contains(register))
                    .unwrap_or("unnamed");
                seen.push((function, register));
            }
        }
        for (function, register) in &seen {
            let allowed = match *function {
                "smca_bank_up" => ["SMCA_CONFIG", "SMCA_IPID"].contains(register),
                "read_synd" => *register == "SMCA_SYND",
                _ => false,
            };
            assert!(allowed, "{register} reached in {function}");
        }
        assert!(seen.contains(&("read_synd", "SMCA_SYND")));
        assert!(source.contains("if status & decode::SYNDV == 0 || proc.ipid(bank).is_none() {"));
    }

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

// ---------------------------------------------------------------------------
// AMD's rules (`docs/MACHINE-CHECK-AMD.md` §6, the consultant's A8)
// ---------------------------------------------------------------------------

/// The `-cpu` properties of Intel's modes.
pub(crate) const INTEL: &str = "vendor=GenuineIntel";
/// AMD's, with both recovery bits asked for. QEMU's TCG drops both
/// ("TCG doesn't support requested feature"), KVM on an AMD host passes
/// them: under TCG the guest reports no `SUCCOR`, and the kernel's rules
/// then stop on every uncorrected record.
pub(crate) const AMD: &str = "vendor=AuthenticAMD,+succor,+overflow-recov";
/// AMD's, without `SUCCOR`.
const AMD_NO_SUCCOR: &str = "vendor=AuthenticAMD,-succor,+overflow-recov";
/// AMD's, without `OverflowRecov`.
const AMD_NO_OVERFLOW_RECOV: &str = "vendor=AuthenticAMD,+succor,-overflow-recov";

/// An AMD consumption: `VAL`, `UC`, `EN`, `MISCV`, `ADDRV` and `Poison`.
const AMD_AR: u64 = 0xbc00_0800_0000_0000;
/// AMD `MCA_STATUS.TCC`.
const TCC: u64 = 1 << 55;
/// `MCi_STATUS.PCC`.
const PCC: u64 = 1 << 57;
/// `MCi_STATUS.OVER`.
const OVER: u64 = 1 << 62;
/// An AMD deferred record: `VAL`, `MISCV`, `ADDRV`, `Deferred`, `Poison`,
/// `UC` clear, so no machine check is raised.
const AMD_DEFERRED: u64 = 0x8c00_1800_0000_0000;

/// What an AMD mode requires of the boot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    /// `SIGBUS` to the handler where `SUCCOR` is reported, else the stop.
    Signal,
    /// The victim ended without its handler where `SUCCOR` is reported,
    /// else the stop.
    Kill,
    /// The kernel stops as FX-9005.
    Stop,
    /// The poll logs the record once and retires nothing.
    Deferred,
    /// The poll logs the record once.
    Corrected,
}

/// Where an AMD mode's record goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// The processor running the victim in ring 3.
    Consumer,
    /// Another processor, in ring 0 when the machine is stopped.
    Ring0,
}

/// One AMD mode.
#[derive(Clone, Copy, Debug)]
struct AmdMode {
    /// The task's name after `test-mce-amd-`.
    name: &'static str,
    /// The `-cpu` properties.
    cpu: &'static str,
    /// The kernel options.
    options: &'static str,
    /// The record's `MCi_STATUS`.
    status: u64,
    /// The record's `MCG_STATUS`.
    mcg: u64,
    /// Where it goes.
    target: Target,
    /// What the boot must show.
    expect: Expect,
}

/// The AMD modes (the consultant's A8, ledger 4820).
const AMD_MODES: &[AmdMode] = &[
    AmdMode {
        name: "srar",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Signal,
    },
    AmdMode {
        name: "kill",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR,
        mcg: UNRESTARTABLE,
        target: Target::Consumer,
        expect: Expect::Kill,
    },
    AmdMode {
        name: "tcc",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR | TCC,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Kill,
    },
    AmdMode {
        name: "eipv",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR,
        mcg: BYSTANDER,
        target: Target::Consumer,
        expect: Expect::Stop,
    },
    AmdMode {
        name: "pcc",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR | PCC,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Stop,
    },
    AmdMode {
        name: "nosuccor",
        cpu: AMD_NO_SUCCOR,
        options: POLL_EVERY_SECOND,
        status: AMD_AR,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Stop,
    },
    AmdMode {
        name: "over",
        cpu: AMD_NO_OVERFLOW_RECOV,
        options: POLL_EVERY_SECOND,
        status: AMD_AR | OVER,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Stop,
    },
    AmdMode {
        name: "deferred",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_DEFERRED,
        mcg: 0,
        target: Target::Consumer,
        expect: Expect::Deferred,
    },
    AmdMode {
        name: "ring0",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: AMD_AR,
        mcg: CONSUMER,
        target: Target::Ring0,
        expect: Expect::Stop,
    },
    AmdMode {
        name: "corrected",
        cpu: AMD,
        options: POLL_EVERY_SECOND,
        status: CORRECTED,
        mcg: 0,
        target: Target::Consumer,
        expect: Expect::Corrected,
    },
    AmdMode {
        name: "recover-off",
        cpu: AMD,
        options: "ferrix.mce.poll=1 ferrix.mce.recover=off\n",
        status: AMD_AR,
        mcg: CONSUMER,
        target: Target::Consumer,
        expect: Expect::Stop,
    },
];

/// One AMD mode on x86-64.
///
/// # Errors
///
/// A boot that did not reach the victim, an injection the monitor refused,
/// or a line the mode requires missing.
///
/// Verifies: `L.x86_64.233`
/// Verifies: `L.x86_64.236`
/// Verifies: `L.x86_64.237`
/// Verifies: `L.x86_64.238`
/// Verifies: `L.x86_64.242`
fn test_amd(args: &Args, name: &str) -> Result<()> {
    let arch = Arch::X86_64;
    let mode = AMD_MODES
        .iter()
        .find(|mode| mode.name == name)
        .copied()
        .ok_or_else(|| Error::new(format!("no AMD mode {name}")))?;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-amd-{name}: x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(mode.options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.x86_cpu_extra = Some(mode.cpu.to_owned());
    qemu_args.smp = if mode.target == Target::Ring0 { 2 } else { 1 };
    qemu_args.smp_given = true;
    let mut injected = Injected::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        injected = inject_amd(&mut qmp, watching.lines(), mode, qemu_args.smp)?;
        let deadline = Instant::now() + MODE_PATIENCE;
        let _ = watching.read_more(deadline, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || (seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
                    && seen.iter().any(|line| line.contains("  code ")))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, VICTIM, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    let recovery = !lines
        .iter()
        .any(|line| line.contains("recovery not reported"));
    judge_amd(mode, &lines, &injected, recovery).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-amd-{name}: {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    let ebx = lines
        .iter()
        .find_map(|line| line.split("cpuid 80000007 ebx=").nth(1)?.split(',').next())
        .unwrap_or("?");
    println!(
        "  {arch}: test-mce-amd-{name}: as required ({}; cpuid 80000007 ebx={ebx}, recovery {})",
        injected.said,
        if recovery { "reported" } else { "not reported" }
    );
    Ok(())
}

/// Make an AMD mode's injection with the machine stopped, as [`inject`]
/// does: into the processor running the victim in ring 3, or, for
/// [`Target::Ring0`], into another processor found in ring 0.
fn inject_amd(qmp: &mut Qmp, lines: &[String], mode: AmdMode, smp: u32) -> Result<Injected> {
    let processors = smp.max(1) as usize;
    let virt = lines
        .iter()
        .rev()
        .find_map(|line| {
            let at = line.find(" va=0x")? + " va=0x".len();
            u64::from_str_radix(line.get(at..)?.trim(), 16).ok()
        })
        .ok_or_else(|| Error::new("the victim printed no address"))?;
    for _ in 0..200 {
        stopped(qmp, true)?;
        let mut consumer = None;
        let mut idle = None;
        for cpu in 0..processors {
            if in_ring_3(&monitor(qmp, "info registers", Some(cpu))?) {
                let reply = monitor(qmp, &format!("gva2gpa {virt:#x}"), Some(cpu))?;
                if let Some(physical) = gpa(&reply) {
                    consumer = Some((cpu, physical & !0xfff));
                }
            } else {
                idle = Some(cpu);
            }
        }
        let target = match (mode.target, consumer, idle) {
            (Target::Consumer, Some((cpu, page)), _) => Some((cpu, page)),
            (Target::Ring0, Some((_, page)), Some(cpu)) => Some((cpu, page)),
            _ => None,
        };
        let made = target
            .map(|(cpu, page)| make(qmp, cpu, mode.status, mode.mcg, page).map(|()| (cpu, page)));
        stopped(qmp, false)?;
        if let Some(made) = made {
            let (cpu, page) = made?;
            return Ok(Injected {
                virt,
                page,
                consumer: cpu,
                processors,
                said: format!("{:#x} at {page:#x} on processor {cpu}", mode.status),
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(Error::new(format!(
        "no processor was where the {} mode needs it within 200 tries",
        mode.name
    )))
}

/// Whether `lines` show what `mode` requires, `recovery` being whether the
/// boot reported `SUCCOR`.
fn judge_amd(
    mode: AmdMode,
    lines: &[String],
    injected: &Injected,
    recovery: bool,
) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let stopped = has(qemu::PANIC_MARKER) && has(FX_9005) && !has("mce-test: SIGBUS");
    let retired = format!("mce-test: /proc/ferrix-mce-retired: {:#x}", injected.page);
    let expect = match mode.expect {
        Expect::Signal | Expect::Kill if !recovery => Expect::Stop,
        other => other,
    };
    if expect == Expect::Stop {
        return stopped
            .then_some(())
            .ok_or_else(|| "the kernel went on where it must stop as FX-9005".to_owned());
    }
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    match expect {
        Expect::Signal => {
            let want = format!(
                "mce-test: SIGBUS code=4 addr={:#x} lsb=12 err=0x0",
                injected.virt
            );
            all(
                lines,
                &[&want, "mce-test: victim exited 42", &retired, DONE],
            )
        }
        Expect::Kill => {
            all(
                lines,
                &[
                    "mce-test: victim killed by signal 7",
                    &retired,
                    "mce-test: a second program ran",
                    DONE,
                ],
            )?;
            if has("mce-test: SIGBUS") {
                return Err("the victim's handler ran for a machine check that ends it".to_owned());
            }
            Ok(())
        }
        Expect::Deferred | Expect::Corrected => {
            let class = if expect == Expect::Deferred {
                "deferred"
            } else {
                "corrected"
            };
            once(
                lines,
                &format!("mce: cpu {} bank {BANK} {class}", injected.consumer),
            )?;
            if has("mce: frame ") {
                return Err(format!("a {class} record's frame was retired"));
            }
            all(lines, &["mce-test: victim exited 0", DONE])
        }
        Expect::Stop => Ok(()),
    }
}
