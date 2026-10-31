//! `cargo xtask test-mce-cmci`: corrected machine check interrupts
//! (`docs/MACHINE-CHECK-CMCI.md` §5).
//!
//! No machine here offers CMCI, so the modes boot x86-64 under TCG on a QEMU
//! carrying `tools/common/data/qemu/0008-target-i386-CMCI-under-TCG.patch`,
//! Intel's model with `cmci=on`, two processors, and a poll period of an
//! hour, so that the period cannot be what finds a record. The guest is
//! `test-mce`'s (`src/tests/mce`): its victim reads a page for 20 seconds,
//! which gives the records a page and the gate time; the records come after
//! its line. QEMU's banks are per processor, so each processor claims its
//! own bank 1; the rule for a bank two processors share is the stage
//! check's (`arch/x86_64/mce/check.rs`, `cmci_claims`) and the N150's.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{INTEL, gpa, monitor, refusal, stopped};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce-cmci",
        help_at: 246,
        help: &[
            "test-mce-cmci  A corrected record on a CMCI QEMU (TCG, cmci=on, a poll every hour): the interrupt",
            "               must wake the poll and the line come at once",
        ],
        run: |_, args| test_cmci(args, Mode::Corrected),
    },
    Task {
        name: "test-mce-cmci-ucna",
        help_at: 247,
        help: &["test-mce-cmci-ucna  Intel's UCNA by CMCI: the frame retired at once"],
        run: |_, args| test_cmci(args, Mode::Ucna),
    },
    Task {
        name: "test-mce-cmci-storm",
        help_at: 248,
        help: &[
            "test-mce-cmci-storm  40 corrected records 20 ms apart: the storm rule masks the entry, every record logged",
        ],
        run: |_, args| test_cmci(args, Mode::Storm),
    },
    Task {
        name: "test-mce-cmci-window",
        help_at: 249,
        help: &[
            "test-mce-cmci-window  A record while the entry is masked (check build's held window): the look after",
            "                      the unmask finds it",
        ],
        run: |_, args| test_cmci(args, Mode::Window),
    },
    Task {
        name: "test-mce-cmci-parked",
        help_at: 251,
        help: &[
            "test-mce-cmci-parked  A record into a processor out of service: no line while it is parked, drained 0,",
            "                      the line at once after its return",
        ],
        run: |_, args| test_parked(args),
    },
    Task {
        name: "test-mce-cmci-off",
        help_at: 252,
        help: &[
            "test-mce-cmci-off  ferrix.mce.cmci=off: nothing claimed, no line before the poll's hour",
        ],
        run: |_, args| test_cmci(args, Mode::Off),
    },
    Task {
        name: "test-mce-cmci-unoffered",
        help_at: 253,
        help: &[
            "test-mce-cmci-unoffered  The same QEMU without cmci=on: cmci not offered, nothing claimed",
        ],
        run: |_, args| test_cmci(args, Mode::Unoffered),
    },
];

/// `docs/MACHINE-CHECK-CMCI.md` §5's modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// A corrected record into bank 1 of processor 1: its line at once.
    Corrected,
    /// Intel's UCNA at the victim's page: retired at once.
    Ucna,
    /// Forty corrected records, 20 ms apart: the storm and its end.
    Storm,
    /// A second record while the first round holds the masked window open.
    Window,
    /// `ferrix.mce.cmci=off`: no line within the patience.
    Off,
    /// No `cmci=on`: not offered.
    Unoffered,
}

impl Mode {
    /// The kernel options the mode boots with.
    const fn options(self) -> &'static str {
        match self {
            Mode::Off => "ferrix.mce.poll=3600 ferrix.mce.cmci=off nokaslr\n",
            Mode::Window => "ferrix.mce.poll=3600 ferrix.mce.cmci-window=3 nokaslr\n",
            _ => "ferrix.mce.poll=3600 nokaslr\n",
        }
    }

    /// Whether the processor model offers CMCI.
    const fn offered(self) -> bool {
        !matches!(self, Mode::Unoffered)
    }
}

/// The guest program the modes boot as init (`src/tests/mce`).
const TEST_PROGRAM: &str = "mce";
/// The victim's line: `mce-test: victim pid=<pid> va=<address>`.
const VICTIM: &str = "mce-test: victim pid=";
/// The guest's last line.
const DONE: &str = "mce-test: done";
/// The processor the records go to: a secondary, which claims on its own.
const CPU: usize = 1;
/// The bank they go to.
const BANK: u32 = 1;
/// `MCi_STATUS` of a corrected memory read error: `VAL`, `EN`, `MISCV`,
/// `ADDRV`, MCA code 0x9f.
const CORRECTED: u64 = 0x9c00_0000_0000_009f;
/// `MCi_STATUS` of Intel's UCNA: `VAL`, `UC`, `EN`, `MISCV`, `ADDRV`, `S`,
/// `AR` and `PCC` clear, MCA code 0x9f.
const UCNA: u64 = 0xbc00_0000_0000_009f;
/// `MCi_MISC`: a physical address, recoverable to the page.
const MISC_PAGE: u64 = 0x8c;
/// How long a woken round is given to print: far short of the poll's hour,
/// long for TCG.
const AT_ONCE: Duration = Duration::from_secs(10);
/// How long the boot is given to reach the guest's last line.
const PATIENCE: Duration = Duration::from_secs(90);
/// The storm's records and their spacing.
const STORM_RECORDS: usize = 40;
/// Between two of the storm's records.
const STORM_GAP: Duration = Duration::from_millis(20);
/// The storm rule's most interrupts before it masks the entry: 15 rounds
/// and the one that begins it.
const STORM_MOST: u64 = 16;

/// One mode.
///
/// # Errors
///
/// A QEMU without the `cmci` property, a boot that did not reach the
/// victim, an injection the monitor refused, or a line the mode requires
/// missing.
///
/// Verifies: `L.x86_64.223`
/// Verifies: `L.x86_64.249`
/// Verifies: `L.x86_64.250`
/// Verifies: `L.x86_64.251`
/// Verifies: `L.x86_64.252`
/// Verifies: `L.x86_64.253`
fn test_cmci(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-cmci ({mode:?}): x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let image =
        crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(mode.options()))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.accel = Some("tcg".to_owned());
    qemu_args.x86_cpu_extra = Some(if mode.offered() {
        format!("{INTEL},cmci=on")
    } else {
        INTEL.to_owned()
    });
    qemu_args.smp = 2;
    qemu_args.smp_given = true;
    let mut said = String::new();
    let mut at_once = false;
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        (said, at_once) = inject(&mut qmp, watching, mode)?;
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = match qemu::watch_then(arch, &image, &kernel, &qemu_args, VICTIM, hook) {
        Err(why) if why.to_string().contains("'cmci' not found") => {
            return Err(Error::new(format!(
                "{arch}: test-mce-cmci ({mode:?}): this QEMU has no `cmci` property: rebuild it \
                 with tools/common/fetch/fetch-qemu-linux.sh (patch 0008)"
            )));
        }
        other => other?,
    };
    let log = crate::paths::build_dir(arch).join("serial.log");
    judge(mode, &lines, at_once).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-cmci ({mode:?}): {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!("  {arch}: test-mce-cmci ({mode:?}): as required ({said}; TCG, a patched QEMU)");
    println!(
        "  {arch}: test-mce-cmci ({mode:?}): QEMU {}",
        qemu_used(arch)
    );
    Ok(())
}

/// The QEMU the boot ran, as a path, its sha256 and its version, for the
/// row's log: the build carrying patch 0008 shares the shared one's version
/// string (ledger 5392 (d), (g)), so only the path and hash tell them apart.
fn qemu_used(arch: Arch) -> String {
    let Some(binary) = crate::window::qemu_for(arch.qemu_binary(), false) else {
        return "no QEMU found".to_owned();
    };
    let version = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.lines().next().map(str::to_owned))
        .unwrap_or_default();
    let hash = std::fs::read(&binary)
        .map(|bytes| crate::sha256::hex(&bytes))
        .unwrap_or_default();
    format!("{} sha256 {hash}, {version}", binary.display())
}

/// The line a woken round prints for a corrected record in [`BANK`] of
/// [`CPU`].
fn corrected_line() -> String {
    format!("mce: cpu {CPU} bank {BANK} corrected")
}

/// How many lines contain `want`.
fn count(lines: &[String], want: &str) -> usize {
    lines.iter().filter(|line| line.contains(want)).count()
}

/// One injection into [`BANK`] of [`CPU`], refused by the monitor or made.
fn make(qmp: &mut Qmp, status: u64, address: u64) -> Result<()> {
    let command = format!("mce {CPU} {BANK} {status:#x} 0 {address:#x} {MISC_PAGE:#x}");
    let reply = monitor(qmp, &command, None)?;
    if let Some(problem) = refusal(&reply) {
        return Err(Error::new(format!(
            "the monitor did not inject `{command}`: {problem}"
        )));
    }
    Ok(())
}

/// The victim's physical page: its address from its line, translated by
/// whichever processor maps it, with the machine stopped for the walk.
fn victim_page(qmp: &mut Qmp, lines: &[String]) -> Result<u64> {
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
        let mut found = None;
        for cpu in 0..2 {
            if let Some(physical) = gpa(&monitor(qmp, &format!("gva2gpa {virt:#x}"), Some(cpu))?) {
                found = Some(physical & !0xfff);
                break;
            }
        }
        stopped(qmp, false)?;
        if let Some(page) = found {
            return Ok(page);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(Error::new(format!(
        "no processor mapped the victim's {virt:#x} within 200 tries"
    )))
}

/// Make `mode`'s records and wait for what they should bring: a sentence,
/// and whether the line came within [`AT_ONCE`] (for `Off`, whether it
/// stayed away that long).
fn inject(qmp: &mut Qmp, watching: &mut qemu::Watching<'_>, mode: Mode) -> Result<(String, bool)> {
    let line = corrected_line();
    match mode {
        Mode::Corrected | Mode::Off | Mode::Unoffered => {
            make(qmp, CORRECTED, 0x1000)?;
            let came = watching.read_more(Instant::now() + AT_ONCE, |seen| {
                seen.iter().any(|seen| seen.contains(&line))
            })?;
            let said = format!("a corrected record into bank {BANK} of cpu {CPU}");
            Ok((said, if mode == Mode::Corrected { came } else { !came }))
        }
        Mode::Ucna => {
            let page = victim_page(qmp, watching.lines())?;
            make(qmp, UCNA, page)?;
            let retired = format!("mce: frame {page:#x} retired");
            let came = watching.read_more(Instant::now() + AT_ONCE, |seen| {
                seen.iter().any(|seen| seen.contains(&retired))
            })?;
            Ok((format!("UCNA at {page:#x} on cpu {CPU}"), came))
        }
        Mode::Storm => {
            for _ in 0..STORM_RECORDS {
                make(qmp, CORRECTED, 0x1000)?;
                std::thread::sleep(STORM_GAP);
            }
            let over = format!("mce: cpu {CPU} cmci storm over");
            let came = watching.read_more(Instant::now() + AT_ONCE, |seen| {
                seen.iter().any(|seen| seen.contains(&over))
            })?;
            Ok((
                format!("{STORM_RECORDS} corrected records {STORM_GAP:?} apart"),
                came,
            ))
        }
        Mode::Window => {
            make(qmp, CORRECTED, 0x1000)?;
            let held = format!("mce: cpu {CPU} cmci window held");
            if !watching.read_more(Instant::now() + AT_ONCE, |seen| {
                seen.iter().any(|seen| seen.contains(&held))
            })? {
                return Ok(("the first round never held its window".to_owned(), false));
            }
            make(qmp, CORRECTED, 0x2000)?;
            let came =
                watching.read_more(Instant::now() + AT_ONCE, |seen| count(seen, &line) >= 2)?;
            Ok((
                "a second record while the masked window was held".to_owned(),
                came,
            ))
        }
    }
}

/// The banks processor `cpu`'s claim line names, if it printed one.
fn claimed(lines: &[String], cpu: usize) -> Option<u64> {
    let want = format!("mce: cpu {cpu} cmci on banks 0x");
    lines.iter().find_map(|line| {
        let at = line.find(&want)? + want.len();
        let digits: String = line
            .get(at..)?
            .chars()
            .take_while(char::is_ascii_hexdigit)
            .collect();
        u64::from_str_radix(&digits, 16).ok()
    })
}

/// A field of `/proc/ferrix-mce`'s last `cmci<cpu>` line.
fn cmci_field(lines: &[String], cpu: usize, field: &str) -> Option<u64> {
    let want = format!("/proc/ferrix-mce: cmci{cpu} ");
    let line = lines.iter().rfind(|line| line.contains(&want))?;
    let mut words = line.split_whitespace();
    let _ = words.find(|&word| word == field)?;
    let value = words.next()?;
    value.strip_prefix("0x").map_or_else(
        || value.parse().ok(),
        |hex| u64::from_str_radix(hex, 16).ok(),
    )
}

/// Whether `lines` show what `mode` requires.
fn judge(mode: Mode, lines: &[String], at_once: bool) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    if !has(DONE) {
        return Err(format!("the guest never said `{DONE}`"));
    }
    let interrupts = cmci_field(lines, CPU, "interrupts").unwrap_or(0);
    match mode {
        Mode::Off | Mode::Unoffered => {
            let why = if mode == Mode::Off {
                "mce: ferrix.mce.cmci=off"
            } else {
                "mce: cmci not offered"
            };
            if !has(why) {
                return Err(format!("the boot never said `{why}`"));
            }
            if (0..2).any(|cpu| claimed(lines, cpu).is_some()) || interrupts != 0 {
                return Err("a bank was claimed, or a CMCI counted".to_owned());
            }
            if !at_once {
                return Err(format!(
                    "`{}` came within {}s with CMCI not in use: something other than the \
                     period's round printed it",
                    corrected_line(),
                    AT_ONCE.as_secs()
                ));
            }
            return Ok(());
        }
        _ => {}
    }
    if !has("mce: cmci offered") {
        return Err("the boot never said `mce: cmci offered`".to_owned());
    }
    // Per-processor banks: each claims its own bank 1.
    for cpu in 0..2 {
        if claimed(lines, cpu).is_none_or(|banks| banks & (1 << BANK) == 0) {
            return Err(format!("cpu {cpu} did not claim its bank {BANK}"));
        }
    }
    if !at_once {
        return Err(match mode {
            Mode::Ucna => format!(
                "the UCNA's frame was not retired within {}s",
                AT_ONCE.as_secs()
            ),
            Mode::Storm => format!("the storm was not over within {}s", AT_ONCE.as_secs()),
            _ => format!(
                "`{}` did not come within {}s of the record: the CMCI did not wake the poll",
                corrected_line(),
                AT_ONCE.as_secs()
            ),
        });
    }
    match mode {
        Mode::Corrected | Mode::Window => {
            let records = if mode == Mode::Window { 2 } else { 1 };
            if count(lines, &corrected_line()) != records {
                return Err(format!(
                    "`{}` printed other than {records} time(s)",
                    corrected_line()
                ));
            }
            if interrupts < 1 {
                return Err(format!("/proc/ferrix-mce counted no CMCI on cpu {CPU}"));
            }
        }
        Mode::Ucna => {
            if !has(&format!("mce: cpu {CPU} bank {BANK} uncorrected-no-action")) {
                return Err("the UCNA was not printed".to_owned());
            }
        }
        Mode::Storm => {
            if !has(&format!("mce: cpu {CPU} cmci storm: polling every 1 s")) {
                return Err("no storm was said".to_owned());
            }
            if interrupts > STORM_MOST {
                return Err(format!(
                    "{interrupts} CMCIs counted, past the storm rule's {STORM_MOST}"
                ));
            }
        }
        Mode::Off | Mode::Unoffered => {}
    }
    Ok(())
}

/// The parked mode's guest half, run by init's unit once lode is up
/// (`crate::quarantine::image_files`): processor 1 out of service, a pause
/// for the record, the service line, processor 1 back, a pause for the
/// woken round, the counts.
const PARKED_SCRIPT: &str = r#"
say() { echo "cmci-parked: $*"; }
cpu=/sys/devices/system/cpu
echo 0 > $cpu/cpu1/online
say "out $? online $(busybox cat $cpu/online)"
say parked
busybox sleep 8
say "service $(busybox grep '^cpu1 ' /proc/ferrix-service)"
echo 1 > $cpu/cpu1/online
say "back $?"
busybox sleep 10
busybox sed 's/^/cmci-parked: /' /proc/ferrix-mce
say done
"#;

/// The parked mode's line once processor 1 is out of service.
const PARKED: &str = "cmci-parked: parked";
/// Less than the parked mode's guest sleeps between `parked` and the
/// return, in seconds.
const PARKED_SLEEP: f64 = 7.0;
/// The parked mode's line once processor 1 is back.
const BACK: &str = "cmci-parked: back";
/// The parked mode's last line.
const PARKED_DONE: &str = "cmci-parked: done";

/// A corrected record into processor 1 while it is out of service: its
/// CMCI is held, nothing runs or is drained there, and its return wakes
/// the poll (`docs/MACHINE-CHECK-CMCI.md` §3.3; ledger 5348 (3)).
///
/// # Errors
///
/// As [`test_cmci`].
///
/// Verifies: `L.x86_64.250`
fn test_parked(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-cmci (Parked): x86_64 only; skipped");
        return Ok(());
    }
    let files = crate::quarantine::image_files(arch, PARKED_SCRIPT.to_owned())?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel(arch, args.release)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &files)?;
    let options = format!(
        "{} ferrix.mce.poll=3600\n",
        crate::init::command_line().trim_end()
    );
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.accel = Some("tcg".to_owned());
    qemu_args.x86_cpu_extra = Some(format!("{INTEL},cmci=on"));
    qemu_args.smp = 2;
    qemu_args.smp_given = true;
    if !args.timeout_given {
        qemu_args.timeout = 300;
    }
    let mut at_once = false;
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        make(&mut qmp, CORRECTED, 0x1000)?;
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(BACK))
                || seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        let line = corrected_line();
        at_once = watching.read_more(Instant::now() + AT_ONCE, |seen| {
            seen.iter().any(|seen| seen.contains(&line))
        })?;
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(PARKED_DONE))
                || seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, PARKED, hook)?;
    judge_parked(&lines, at_once).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-cmci (Parked): {why}.\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-cmci (Parked): as required (a corrected record into cpu {CPU} out of \
         service: held, nothing drained, the line at once after its return; TCG, a patched QEMU)"
    );
    println!("  {arch}: test-mce-cmci (Parked): QEMU {}", qemu_used(arch));
    Ok(())
}

/// Whether the parked mode's lines show what it requires.
fn judge_parked(lines: &[String], at_once: bool) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    for want in ["cmci-parked: out 0", "cmci-parked: back 0", PARKED_DONE] {
        if !has(want) {
            return Err(format!("the guest never said `{want}`"));
        }
    }
    let line = corrected_line();
    // The guest sleeps 8 s after `parked` and only then brings cpu 1 back;
    // the record is made at `parked`. Init relays the guest's own lines
    // through its log, so their place on the port may trail the kernel's:
    // the kernel's timestamps say when each was printed.
    let at = |want: &str| {
        lines
            .iter()
            .find(|seen| seen.contains(want))
            .and_then(|seen| seen.split('|').next()?.trim().parse::<f64>().ok())
    };
    if let (Some(seen), Some(parked)) = (at(&line), at(PARKED))
        && seen - parked < PARKED_SLEEP
    {
        return Err(format!(
            "`{line}` came {:.2}s after cpu {CPU} was parked, while it was out of service",
            seen - parked
        ));
    }
    if !lines
        .iter()
        .any(|seen| seen.contains("cmci-parked: service cpu1 ") && seen.contains(" drained 0 "))
    {
        return Err(format!(
            "cpu {CPU} drained a task while it was out of service"
        ));
    }
    if !at_once {
        return Err(format!(
            "`{line}` did not come within {}s of cpu {CPU}'s return: the return did not wake the poll",
            AT_ONCE.as_secs()
        ));
    }
    let want = format!("cmci-parked: cmci{CPU} interrupts 1 held 1 ");
    if !has(&want) {
        return Err(format!(
            "/proc/ferrix-mce did not count one CMCI on cpu {CPU}, held for its return"
        ));
    }
    Ok(())
}
