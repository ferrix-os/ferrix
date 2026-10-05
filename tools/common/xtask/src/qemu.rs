//! Booting the image under QEMU.
//!
//! `run` attaches the guest's serial port to this terminal. `test-boot` does
//! the same thing headless, watches for the kernel's report, and turns it into
//! an exit status — which makes it the only check in this repository that can
//! tell us the operating system runs, as opposed to compiling.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::btrfs_disk;
use crate::console::{self, Console};
use crate::dma_faults;
use crate::paths::{self, Arch, Firmware};
use crate::symbolize::Symbolizer;
use crate::test_disk;
use crate::{Error, Result};

/// What the kernel prints when it has finished its self-checks.
pub(crate) const SUCCESS_MARKER: &str = "FERRIX-BOOT-OK";
/// What the panic handler prints. Seeing this ends the test immediately: the
/// kernel will not recover, and waiting out the timeout only hides the reason.
pub(crate) const PANIC_MARKER: &str = "FERRIX-PANIC";
/// What the kernel prints instead of [`SUCCESS_MARKER`] when
/// `ferrix.checks=skip` had it bring every stage up without checking it. Never
/// a pass: a boot waiting for the success marker that sees this one fails at
/// once, and says why, rather than waiting out its timeout.
pub(crate) const UNCHECKED_MARKER: &str = "FERRIX-BOOT-UNCHECKED";

/// The option `--reset` puts in the image's `CMDLINE.TXT`.
pub(crate) const RESET_OPTION: &str = "ferrix.onexit=reset";

/// The option that has the kernel start pid 1 from the file at `path` in the
/// image, rather than from the program built into it (`src/kernel/src/init.rs`):
/// what `--init-path` puts in `CMDLINE.TXT`, and what a test that boots a
/// program from a file puts there itself.
pub(crate) fn init_option(path: &str) -> String {
    format!("ferrix.init={path}")
}

/// What the kernel says once it has read that option.
const RESET_ARMED: &str = "power    ferrix.onexit=reset: the machine resets when boot ends";

/// What it says as it acts on it.
const RESETTING: &str = "power    resetting, as ferrix.onexit=reset asks";

/// The loader's first line, which only a machine that really reset prints twice.
const LOADER_BANNER: &str = "Ferrix loader ";
/// How long to keep reading after the panic marker. The marker line names the
/// failure; the lines after it say where and on which processor, and a log
/// that stops at the marker loses them.
const PANIC_REPORT_GRACE: Duration = Duration::from_secs(2);

/// The exit status QEMU reports when the x86-64 kernel writes 0x10 to the
/// `isa-debug-exit` port: `(value << 1) | 1`.
const DEBUG_EXIT_SUCCESS: i32 = 33;

/// Boot the image with the serial port attached to this terminal.
///
/// Which terminal that is depends on the host: QEMU's own on a POSIX one,
/// and a socket this program carries by hand on Windows, where
/// [`crate::console`] says what the difference is for.
pub(crate) fn run(arch: Arch, image: &Path, args: &Args) -> Result<()> {
    // The host's clipboard, bridged by xtask for as long as the boot runs:
    // `crate::clipboard::host` says why QEMU's own peer is not enough.
    #[cfg(unix)]
    let bridge = crate::clipboard::host::start(arch, args);
    #[cfg(unix)]
    let bridged;
    #[cfg(unix)]
    let args = match &bridge {
        Some(bridge) => {
            bridged = Args {
                clipboard_socket: Some(bridge.socket().to_path_buf()),
                ..args.clone()
            };
            &bridged
        }
        None => args,
    };
    let console = console::open()?;
    crate::builds::refuse_boot(arch)?;
    let (mut command, network) = qemu_command(arch, image, None, args, &console)?;
    if args.gdb {
        let _ = command.args(["-s", "-S"]);
        println!("  waiting for a debugger on localhost:1234");
    }
    println!("  {arch}: booting (quit with Ctrl-A x)\n");
    let booted = console.attach(command);
    report_network(&network);
    booted
}

/// Boot the image headless and require the kernel to report success.
pub(crate) fn test_boot(arch: Arch, image: &Path, kernel: &Path, args: &Args) -> Result<()> {
    test_boot_lines(arch, image, kernel, args).map(|_| ())
}

/// [`test_boot`], returning what the guest printed.
/// Verifies: `L.x86_64.97`
pub(crate) fn test_boot_lines(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
) -> Result<Vec<String>> {
    println!("  {arch}: booting under QEMU (timeout {}s)", args.timeout);
    let watched = watch(arch, image, kernel, args, SUCCESS_MARKER)?;
    match watched.verdict {
        Verdict::Reached => {
            let code = watched.ended.status.code().unwrap_or(0);
            if code != 0 && code != DEBUG_EXIT_SUCCESS {
                return Err(Error::new(format!(
                    "the {arch} kernel reported success but QEMU exited {code}"
                )));
            }
            let off = crate::cargo::mitigations_off();
            if let Some(problem) =
                crate::kaslr::problem(&watched.lines, off, crate::kaslr::declined(args))
            {
                return Err(Error::new(format!(
                    "{arch}: {problem}.\n  Serial output is in {}",
                    watched.log.display()
                )));
            }
            let own_model = std::env::var_os("FERRIX_X86_CPU").is_none();
            // In the order each is reported: the first that fails is said.
            let problems = [
                entropy_problem(&watched.lines),
                devmgr_problem(&watched.lines),
                msi_problem(arch, &watched.lines),
                cleaning_problem(arch, &watched.lines),
                remap_problem(arch, &watched.lines),
                queue_problem(arch, &watched.lines),
                config_problem(arch, &watched.lines),
                iommu_problem(&watched.lines),
                namespace_problem(&watched.lines),
                xstate_problem(arch, own_model, &watched.lines),
                fault_problem(arch, &watched.lines),
            ];
            if let Some(problem) = problems.into_iter().flatten().next() {
                return Err(Error::new(format!(
                    "{arch}: {problem}.\n  Serial output is in {}",
                    watched.log.display()
                )));
            }
            if args.reset {
                if let Some(problem) = reset_problem(&watched) {
                    return Err(Error::new(format!(
                        "{arch}: {problem}.\n  Serial output is in {}",
                        watched.log.display()
                    )));
                }
                println!("  {arch}: the machine reset, as ferrix.onexit=reset asked");
            }
            println!("  {arch}: boot ok");
            Ok(watched.lines)
        }
        Verdict::Panicked => Err(panicked(arch, &watched.log)),
        Verdict::Silent => Err(Error::new(format!(
            "the {arch} kernel never printed `{SUCCESS_MARKER}` within {}s.\n  \
             Serial output is in {}",
            args.timeout,
            watched.log.display()
        ))),
    }
}

/// Why a `--reset` boot did not show a reset, if it did not.
///
/// The kernel has to have read the option from the image's `CMDLINE.TXT` and
/// said it was resetting, and the loader then has to have started again. A
/// power-off cannot do that: under `-action shutdown=pause` it only pauses
/// QEMU, the debug-exit write on x86-64 included.
/// Verifies: `L.x86_64.99`, H.BOOT.7
fn reset_problem(watched: &Watched) -> Option<String> {
    let first = |text: &str| watched.lines.iter().position(|line| line.contains(text));
    if first(RESET_ARMED).is_none() {
        return Some(format!(
            "--reset: the kernel never said `{RESET_ARMED}`, so the image's CMDLINE.TXT did not reach it"
        ));
    }
    let Some(at) = first(RESETTING) else {
        return Some(format!("--reset: the kernel never said `{RESETTING}`"));
    };
    if watched
        .lines
        .iter()
        .skip(at)
        .any(|line| line.contains(LOADER_BANNER))
    {
        None
    } else {
        Some(format!(
            "--reset: the loader did not start again after `{RESETTING}`, so the machine did not reset"
        ))
    }
}

/// What stage 10's PCI check prints after the number of bytes a virtio-rng
/// device wrote into memory the kernel gave it.
const ENTROPY_READ: &str = " entropy bytes read by DMA";

/// What it prints after the number of those requests that completed by MSI-X.
const BY_MSIX: &str = " completions by MSI-X";

/// The number written just before `suffix` in `line`.
fn count_before(line: &str, suffix: &str) -> Option<u32> {
    let before = line.split(suffix).next()?;
    before
        .rsplit(' ')
        .next()
        .and_then(|count| count.parse::<u32>().ok())
}

/// Why a boot that reached the marker still failed the DMA check, if it did.
///
/// The kernel skips a virtio-rng device that refuses or stalls rather than
/// halting, because on a hypervisor somebody else configured — libvirt adds
/// one to every guest — that is not the kernel's fault. Every machine this
/// tool boots has one it configured itself, so here a boot that read no
/// entropy has lost DMA. A device may legitimately write fewer bytes than it
/// was asked for, so any positive count passes.
/// Verifies: `L.x86_64.113`
fn entropy_problem(lines: &[String]) -> Option<String> {
    let Some(line) = lines.iter().find(|line| line.contains(ENTROPY_READ)) else {
        return Some("the kernel never reported reading entropy by DMA".to_owned());
    };
    if !count_before(line, ENTROPY_READ).is_some_and(|count| count > 0) {
        return Some(format!(
            "the kernel read no entropy by DMA: `{}`",
            line.trim()
        ));
    }
    // Every machine this tool boots has an interrupt controller that takes
    // messages, so a completion that had to be polled is a lost interrupt.
    if !count_before(line, BY_MSIX).is_some_and(|count| count > 0) {
        return Some(format!(
            "no entropy request completed by MSI-X: `{}`",
            line.trim()
        ));
    }
    None
}

/// What stage 10's devices line says when the MSI check minted `edu`'s one
/// vector and saw both of its unmasked deliveries.
const MSI_DELIVERED: &str = "MSI capabilities (1 vectors minted, 2 deliveries)";

/// Why an x86-64 boot did not run stage 10's MSI check whole, if it did not.
///
/// `device::check::check_msi` passes on a machine without QEMU's `edu`, and
/// does nothing when `edu`'s vector cannot be minted; every x86-64 machine
/// this tool boots has `edu` behind a root port (`attach_rng`), so here the
/// check must have minted its vector and seen both deliveries.
///
/// Verifies: L.device.22
fn msi_problem(arch: Arch, lines: &[String]) -> Option<String> {
    if arch != Arch::X86_64 {
        return None;
    }
    let Some(line) = lines.iter().find(|line| line.contains("  devices  ")) else {
        return Some("the kernel never printed its devices line".to_owned());
    };
    if !line.contains(MSI_DELIVERED) {
        return Some(format!(
            "the MSI check did not mint edu's vector and see both deliveries: `{}`",
            line.trim()
        ));
    }
    None
}

/// What stage 10's IOMMU check prints when a VT-d unit whose walk does not
/// snoop had its table writes cleaned to memory before each was published.
const CLEANED: &str = "fresh tables noted and cleaned to memory on ";

/// Why an x86-64 boot did not show its VT-d table writes cleaned, if it did
/// not.
///
/// QEMU's `intel-iommu` reports `ECAP.C` clear, so on every x86-64 machine
/// this tool boots the kernel takes the cleaning path (finding F-58), and
/// stage 10's check must have printed what it cleaned; a boot without the
/// line ran the path unchecked, or not at all.
///
/// Verifies: L.iommu.56
fn cleaning_problem(arch: Arch, lines: &[String]) -> Option<String> {
    if arch != Arch::X86_64 {
        return None;
    }
    if lines.iter().any(|line| line.contains(CLEANED)) {
        return None;
    }
    Some("the kernel never said it cleaned its VT-d table writes to memory".to_owned())
}

/// What stage 10's IOMMU check prints when every VT-d invalidation of the
/// boot went through a unit's queue (`docs/NVIDIA.md` §12.3, check R6).
const QUEUED: &str = "invalidations queued and each waited for, none by register";

/// What it prints after the number of invalidations check R7 made fail.
const R7_FAILED: &str = " failed as check R7 made it";

/// Why an x86-64 boot did not show its VT-d invalidations queued, if it did
/// not.
///
/// Every x86-64 machine this tool boots has a VT-d unit, and the kernel
/// refuses one without an invalidation queue, so stage 10's check must have
/// said every invalidation went through the queue and none by register, and
/// that the one invalidation check R7 makes fail did fail.
///
/// Verifies: L.iommu.47, L.iommu.48
fn queue_problem(arch: Arch, lines: &[String]) -> Option<String> {
    if arch != Arch::X86_64 {
        return None;
    }
    let Some(line) = lines.iter().find(|line| line.contains(QUEUED)) else {
        return Some("the kernel never said its VT-d invalidations were queued".to_owned());
    };
    if count_before(line, R7_FAILED) != Some(1) {
        return Some(format!(
            "check R7 did not make exactly one invalidation fail: `{}`",
            line.trim()
        ));
    }
    None
}

/// What every x86-64 boot must print of interrupt remapping
/// (`docs/NVIDIA.md` §12.3, N0g): the unit's bring-up with `CFIS` read back
/// clear, check R9, R1 to R3, `L.device.28`, R10, R5, the stray deliveries,
/// and R8, each by the words that say it held.
const REMAP_LINES: [(&str, &str); 9] = [
    (
        "remapping on (256 entries, xAPIC format), CFIS=0 read back",
        "the VT-d unit never said it remaps with compatibility format blocked",
    ),
    (
        "all the console's; 1 I/O APIC inputs converted",
        "check R9 never said no vector was minted in compatibility format before remapping",
    ),
    (
        "edu's forged messages refused",
        "checks R1 to R3 never refused edu's forged messages",
    ),
    (
        "is given no message route, so no vector",
        "a requester no unit places was not shown to get no route",
    ),
    (
        "the isolated-interrupts mark set on",
        "check R10 never showed the isolated-interrupts mark",
    ),
    (
        "delivered a looped-back byte on vector",
        "check R5 never saw the console's line delivered through its entry",
    ),
    (
        "  0 deliveries of the check vector 0xfc outside its window, 0 on the console's retired vector",
        "the check vector or the console's retired vector was delivered, or never counted",
    ),
    (
        "processors looked at their local APIC's mode before using its window; 1 put in x2APIC mode",
        "check R8 never put a processor in x2APIC mode and saw it switched back",
    ),
    (
        "byte received while it was masked read by the service after its conversion",
        "the console's line was never serviced once after its conversion",
    ),
];

/// What the kernel prints for each interrupt request a check makes its unit
/// refuse, before the reason.
const PROVOKED_INTERRUPT: &str = " is made to send an interrupt its unit refuses with reason ";

/// Why an x86-64 boot did not show interrupt remapping whole, if it did not.
///
/// Every x86-64 machine this tool boots has a VT-d unit that remaps
/// (`INTEL_IOMMU`) on the patched QEMU, an `edu` and a console on the I/O
/// APIC, so every check of `docs/NVIDIA.md` §12.3 must have run and said it
/// held, and the interrupt requests the checks made the unit refuse must be
/// exactly R1's and R2's 0x25 and R3's 0x26: any other reason fails by name.
///
/// Verifies: L.iommu.50, L.iommu.54, `L.x86_64.129`, `L.x86_64.130`, `L.x86_64.131`
fn remap_problem(arch: Arch, lines: &[String]) -> Option<String> {
    if arch != Arch::X86_64 {
        return None;
    }
    if let Some(line) = lines
        .iter()
        .find(|line| line.contains("remapping not enabled"))
    {
        return Some(format!(
            "interrupt remapping was not enabled: `{}`",
            line.trim()
        ));
    }
    for (text, problem) in REMAP_LINES {
        if !lines.iter().any(|line| line.contains(text)) {
            return Some(problem.to_owned());
        }
    }
    let mut reasons: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.split(PROVOKED_INTERRUPT).nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect();
    reasons.sort_unstable();
    if reasons != ["0x25", "0x25", "0x26"] {
        return Some(format!(
            "the checks provoked interrupt faults with reasons {reasons:?}, where R1, R2 and R3 \
             provoke 0x25, 0x25 and 0x26"
        ));
    }
    None
}

/// What stage 10's `config` line says when `device_aperture` reported
/// `pci-testdev`'s 8 GiB BAR whole (`docs/NVIDIA.md` §12.1, check W1).
const ABOVE_4_GIB: &str = " 1 above 4 GiB and longer than it";

/// What the last `config` line says when the breach check (W7) found the
/// command register it rewrote.
const BREACH_FOUND: &str = "a COMMAND rewritten behind the kernel was found and its node refused";

/// Why a boot did not run stage 10's configuration window checks whole, if
/// it did not.
///
/// The kernel prints them on every boot that runs its checks. Every x86-64
/// machine this tool boots carries `pci-testdev,membar=8G` (`attach_rng`), so
/// there `device_aperture` must have reported one aperture above 4 GiB and
/// longer than it; and on every machine the breach check must have found
/// the command register it rewrote.
///
/// Verifies: L.device.24, L.device.25
fn config_problem(arch: Arch, lines: &[String]) -> Option<String> {
    let Some(apertures) = lines.iter().find(|line| {
        line.contains("  config   ") && line.contains("reported whole by device_aperture")
    }) else {
        return Some("the kernel never printed its configuration window line".to_owned());
    };
    if arch == Arch::X86_64 && !apertures.contains(ABOVE_4_GIB) {
        return Some(format!(
            "device_aperture did not report pci-testdev's BAR above 4 GiB whole: `{}`",
            apertures.trim()
        ));
    }
    if !lines.iter().any(|line| line.contains(BREACH_FOUND)) {
        return Some("the configuration breach check did not find what it rewrote".to_owned());
    }
    None
}

/// What stage 10's IOMMU discovery prints after the number of PCI functions
/// firmware puts behind a unit.
const BEHIND_IOMMU: &str = " PCI functions behind one";

/// What it prints after the number of functions whose description it could
/// not follow.
const UNRESOLVED: &str = " unresolved";

/// What it prints after the number of functions firmware puts behind no
/// unit.
const BYPASSING: &str = " bypassing";

/// The `virt` machine the Arm architectures boot, with an `SMMUv3` for stage
/// 10's IOMMU domains, and stage 2 on it, which those domains are made of.
///
/// `virt` turns stage 2 on by default only from QEMU 9.2 ("Default to
/// two-stage SMMU from virt-9.2"); from 8.1, when the device gained it, to 9.1
/// it offers stage 1 alone unless asked. 9.2's machine sets "nested" itself,
/// after the global, so asking changes nothing there. Before 8.1 the property
/// does not exist and QEMU refuses it.
pub(crate) const VIRT_MACHINE: [&str; 4] = [
    "-global",
    "arm-smmuv3.stage=2",
    "-machine",
    "virt,iommu=smmuv3",
];

/// Why a boot that reached the marker still failed IOMMU discovery, if it did.
///
/// Every machine this tool boots has an IOMMU it configured — `intel-iommu` on
/// `q35`, `iommu=smmuv3` on `virt` — and firmware that describes it. A boot that
/// places no PCI function behind one has lost that description, and one that
/// leaves a function unresolved reads it differently from the firmware that
/// wrote it.
///
/// Verifies: L.iommu.2, L.iommu.46
fn iommu_problem(lines: &[String]) -> Option<String> {
    let Some(line) = lines.iter().find(|line| line.contains(BEHIND_IOMMU)) else {
        return Some("the kernel never reported where its IOMMUs are".to_owned());
    };
    if !count_before(line, BEHIND_IOMMU).is_some_and(|count| count > 0) {
        return Some(format!(
            "no PCI function was placed behind an IOMMU: `{}`",
            line.trim()
        ));
    }
    if count_before(line, UNRESOLVED) != Some(0) {
        return Some(format!(
            "an IOMMU description could not be followed: `{}`",
            line.trim()
        ));
    }
    // Every function on these machines is behind their unit, the ones below
    // a root port included (`docs/NVIDIA.md` §2.3): one counted bypassing
    // is a description the kernel misread as naming nothing.
    if count_before(line, BYPASSING) != Some(0) {
        return Some(format!(
            "a PCI function was placed behind no IOMMU: `{}`",
            line.trim()
        ));
    }
    None
}

/// What stage 10's PCI check prints after the number of writes outside a
/// translated domain that its unit faulted.
const FAULTED: &str = " out-of-domain writes faulted";

/// What the kernel prints when it leaves an `SMMUv3` alone for having no stage 2.
const NO_STAGE_2: &str = "left alone: it has no AArch64 stage 2";

/// What stage 10's PCI check prints when the host bridges came from the device
/// tree rather than ACPI's MCFG.
const DEVICE_TREE_HOSTS: &str = " device-tree hosts";

/// What the stage 12 check prints when it wrote the third disk.
const STAGE12_WROTE: &str = "btrfs-rw vdc written";

/// What the mount namespace line ends with when it saw the detach's
/// write-out reach that disk.
const MNTNS_COMMITTED: &str = "on the disk after the detach";

/// Why a boot whose stage 12 check wrote the third disk did not show the
/// mount namespace check's write-out reaching it, if it did not.
///
/// The `mntns` line (FX-0887) passes on a machine with no third disk and
/// says so; on one with it, the write-out of a btrfs inside a detached
/// subtree is the evidence the N2 review asked N3 for
/// (`docs/NAMESPACES.md` §12), and it must not go missing quietly.
fn namespace_problem(lines: &[String]) -> Option<String> {
    if !lines.iter().any(|line| line.contains(STAGE12_WROTE)) {
        return None;
    }
    match lines.iter().find(|line| line.contains("  mntns   ")) {
        None => Some(
            "the stage 12 disk was written, and the kernel never reported on mount namespaces"
                .to_owned(),
        ),
        Some(line) if line.contains(MNTNS_COMMITTED) => None,
        Some(line) => Some(format!(
            "the stage 12 disk was written, and the mount namespace check did not show a \
             detached subtree's write-out reach it: `{}`",
            line.trim()
        )),
    }
}

/// What the `xstate` line ends with when a signal frame's poisoned `XSAVE`
/// header came back through `rt_sigreturn` without a fault.
const XSTATE_SURVIVED: &str = "without a fault";

/// Why an x86-64 boot on this tool's own model did not show a poisoned
/// `XSAVE` header come back safely, if it did not.
///
/// The `xstate` line has a quiet variant, for a processor with no `XSAVE`
/// area in its frames, which the coverage suite's `qemu64` is meant to be
/// and names with `FERRIX_X86_CPU`. [`x86_cpu`]'s model has AVX, so there
/// the line with the survived return is the evidence for `L.x86_64.122` the
/// certification review asked be kept, and it must not go missing quietly.
fn xstate_problem(arch: Arch, own_model: bool, lines: &[String]) -> Option<String> {
    if arch != Arch::X86_64 || !own_model {
        return None;
    }
    match lines.iter().find(|line| line.contains("  xstate   ")) {
        None => Some("the kernel never reported on its signal frames' XSAVE header".to_owned()),
        Some(line) if line.contains(XSTATE_SURVIVED) => None,
        Some(line) => Some(format!(
            "on a model with AVX, the XSAVE signal-frame check did not show a poisoned header \
             come back without a fault: `{}`",
            line.trim()
        )),
    }
}

/// What stage 10's devmgr line ends with.
const DEVMGR_FAILED: &str = " failed";

/// Why a boot whose image carries `devmgr` failed to start its drivers, if
/// it did: the devmgr line must say at least one started and none failed.
/// Every machine this tool boots has a virtio-blk disk for it. An image
/// without `devmgr` prints that it was not started, which is not a failure.
fn devmgr_problem(lines: &[String]) -> Option<String> {
    let Some(line) = lines.iter().find(|line| line.contains("  devmgr   ")) else {
        return Some("the kernel never reported on devmgr".to_owned());
    };
    if line.contains("not started") {
        return None;
    }
    let started = line
        .split(" started")
        .next()
        .and_then(|before| before.rsplit(' ').next())
        .and_then(|count| count.parse::<u32>().ok());
    let failed = count_before(line, DEVMGR_FAILED);
    match (started, failed) {
        (Some(started), Some(0)) if started > 0 => None,
        _ => Some(format!(
            "devmgr started no driver, or one failed: `{}`",
            line.trim()
        )),
    }
}

/// Why a boot on a machine whose IOMMU translates still failed the stage 10
/// exit criterion's out-of-domain check, if it did.
///
/// On the machines this tool boots, x86-64 and AArch64 send the entropy
/// device's DMA through a translated domain. ARMv7-A does not — U-Boot keeps
/// its virtio devices from offering the platform's translation, which the exit
/// criterion states as degraded trusted mode — so it is not asked.
///
/// Nor is an AArch64 boot that found its PCI hosts in the device tree, which
/// `FERRIX_ARM_MACHINE=acpi=off` asks for: the kernel brings an `SMMUv3` up
/// from ACPI's IORT alone (`src/kernel/src/iommu.rs`), so on that path it is
/// ARMv7-A's case, and says so in the same degraded-trusted-mode line. The
/// coverage suite boots it for the Pixel 7's path, which has no ACPI.
///
/// Verifies: L.iommu.7, L.iommu.10, L.iommu.35, L.iommu.36, L.iommu.46, L.iommu.58, H.DMA.2
fn fault_problem(arch: Arch, lines: &[String]) -> Option<String> {
    if arch == Arch::Armv7a {
        return None;
    }
    if let Some(line) = lines.iter().find(|line| line.contains(DEVICE_TREE_HOSTS)) {
        println!(
            "  {arch}: the out-of-domain write check was skipped: PCI came from the device \
             tree, where no SMMUv3 is brought up (`{}`)",
            line.trim()
        );
        return None;
    }
    // A QEMU whose SMMUv3 offers no stage 2 leaves the unit alone, and the
    // kernel says so and why: there is no translated domain for a write to
    // fault in. That is the QEMU's, not the kernel's, and the check says what
    // it needs rather than failing.
    if let Some(line) = lines.iter().find(|line| line.contains(NO_STAGE_2)) {
        println!(
            "  {arch}: the out-of-domain write check was skipped: the SMMUv3 offers no stage 2 \
             (`{}`), which needs QEMU 8.1 or later",
            line.trim()
        );
        return None;
    }
    let Some(line) = lines.iter().find(|line| line.contains(FAULTED)) else {
        return Some("the kernel never reported an out-of-domain write".to_owned());
    };
    if count_before(line, FAULTED).is_some_and(|count| count > 0) {
        None
    } else {
        Some(format!(
            "no write outside a translated domain faulted: `{}`",
            line.trim()
        ))
    }
}

/// Boot an image with a program and a script built in, and require the
/// script's output, in order, and its exit status.
///
/// The lines are looked for *after* the boot marker, so a kernel that happened
/// to print one of them during its self-checks cannot satisfy the test.
pub(crate) fn test_shell(arch: Arch, image: &Path, kernel: &Path, args: &Args) -> Result<()> {
    println!(
        "  {arch}: running the built-in script under QEMU (timeout {}s)",
        args.timeout
    );
    let watched = watch(arch, image, kernel, args, crate::shell::EXITED)?;
    let log = watched.log.display();
    let after_boot = watched
        .lines
        .iter()
        .position(|line| line.contains(SUCCESS_MARKER))
        .and_then(|at| watched.lines.get(at..))
        .unwrap_or_default();

    if let Some(line) = after_boot
        .iter()
        .find(|line| line.contains(crate::shell::NOT_STARTED))
    {
        return Err(Error::new(format!(
            "{arch}: {}\n  Serial output is in {log}",
            line.trim()
        )));
    }
    match watched.verdict {
        Verdict::Reached => {}
        Verdict::Panicked => return Err(panicked(arch, &watched.log)),
        Verdict::Silent => {
            return Err(Error::new(format!(
                "{arch}: the shell never exited within {}s.\n  Serial output is in {log}",
                args.timeout
            )));
        }
    }

    let mut remaining = after_boot.iter();
    for want in crate::shell::EXPECTED {
        if !remaining.any(|line| line.trim_end() == *want) {
            return Err(Error::new(format!(
                "{arch}: the script's output is missing `{want}`, or it came out of order.\n  \
                 Serial output is in {log}"
            )));
        }
    }
    let status = format!("{} {}", crate::shell::EXITED, crate::shell::STATUS);
    if !after_boot.iter().any(|line| line.trim() == status) {
        return Err(Error::new(format!(
            "{arch}: the shell did not exit with {}.\n  Serial output is in {log}",
            crate::shell::STATUS
        )));
    }
    println!(
        "  {arch}: the shell ran the script and exited with {}",
        crate::shell::STATUS
    );
    Ok(())
}

/// What crossed the seam over the whole run, as the kernel counted it: the
/// second "seam measured" row's line (`docs/OPAQUE-KERNEL.md`, S0), from the
/// command that read it rather than the echo of the command.
fn print_seam(arch: Arch, after_boot: &[String]) {
    if let Some(line) = after_boot
        .iter()
        .find(|line| line.contains("seam syscalls ") && !line.contains("cat "))
    {
        let counters = line
            .get(line.find("seam syscalls").unwrap_or(0)..)
            .unwrap_or(line);
        println!("  {arch}: {}", counters.trim_end());
    }
}

/// Boot an image whose kernel runs `vfs::commands(carried)`, and judge each
/// command by its status and its output.
///
/// As with [`test_shell`], only what follows the boot marker counts.
pub(crate) fn test_vfs(
    arch: Arch,
    carried: crate::vfs::Utilities,
    image: &Path,
    kernel: &Path,
    args: &Args,
) -> Result<()> {
    let commands = crate::vfs::COMMANDS;
    let applets = &crate::vfs::applets(carried)[..];
    let shell = crate::vfs::SHELL;
    let utilities = crate::vfs::utilities(carried);
    let seam = crate::vfs::SEAM;
    println!(
        "  {arch}: running {} programs, {} applets, {} shell and {} uutils commands \
         under QEMU (timeout {}s)",
        commands.len(),
        applets.len(),
        shell.len(),
        utilities.len(),
        args.timeout
    );
    let watched = watch(arch, image, kernel, args, crate::vfs::DONE)?;
    let log = watched.log.display();
    let after_boot = watched
        .lines
        .iter()
        .position(|line| line.contains(SUCCESS_MARKER))
        .and_then(|at| watched.lines.get(at..))
        .unwrap_or_default();
    let ending = match watched.verdict {
        Verdict::Reached => None,
        Verdict::Panicked => Some("the kernel panicked".to_owned()),
        Verdict::Silent => Some(format!(
            "the programs did not all finish within {}s",
            args.timeout
        )),
    };

    // The exit criterion and the applets are judged and reported apart, so
    // that the criterion's line means what it did before the applets were
    // added, whatever they do.
    let groups = [
        (
            commands,
            0,
            "programs",
            "stage 8's exit programs all passed",
        ),
        (
            applets,
            commands.len(),
            "applets",
            "stage 8's applets all passed",
        ),
        (
            shell,
            commands.len() + applets.len(),
            "shell commands",
            "/bin/sh is zinc",
        ),
        (
            utilities,
            commands.len() + applets.len() + shell.len(),
            "uutils commands",
            "the uutils family ran on Ferrix",
        ),
        (
            seam,
            commands.len() + applets.len() + shell.len() + utilities.len(),
            "seam counters",
            "the seam's counters were read",
        ),
    ];
    let mut failures = Vec::new();
    for (group, first, what, all_passed) in groups {
        // A group the image does not run, uutils' in an image without it,
        // passes nothing and says nothing.
        if group.is_empty() {
            continue;
        }
        match (crate::vfs::judge(group, first, after_boot), &ending) {
            (Ok(passed), None) => {
                for line in passed {
                    println!("  {arch}: {line}");
                }
                println!("  {arch}: {all_passed}");
            }
            (Ok(_), Some(_)) => {}
            (Err(failed), _) => failures.push(format!(
                "{} of {} {what} failed:\n    {}",
                failed.len(),
                group.len(),
                failed.join("\n    ")
            )),
        }
    }
    print_seam(arch, after_boot);
    if ending.is_none() && failures.is_empty() {
        return Ok(());
    }
    let why = match (ending, failures.is_empty()) {
        (Some(ending), true) => format!("{ending}."),
        (Some(ending), false) => format!("{ending}; {}", failures.join("\n  ")),
        (None, _) => failures.join("\n  "),
    };
    Err(Error::new(format!(
        "{arch}: {why}\n  Serial output is in {log}"
    )))
}

/// Boot with a network device and require the guest's networking programs to
/// pass, one report for the lot.
///
/// The programs come from the caller rather than from a constant, because
/// their arguments hold the ports the host's servers were given a moment ago.
pub(crate) fn test_net(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    programs: &[crate::vfs::Command],
    args: &Args,
) -> Result<()> {
    println!(
        "  {arch}: running {} networking programs under QEMU (timeout {}s)",
        programs.len(),
        args.timeout
    );
    let watched = watch(arch, image, kernel, args, crate::vfs::DONE)?;
    let log = watched.log.display();
    let after_boot = watched
        .lines
        .iter()
        .position(|line| line.contains(SUCCESS_MARKER))
        .and_then(|at| watched.lines.get(at..))
        .unwrap_or_default();
    let ending = match watched.verdict {
        Verdict::Reached => None,
        Verdict::Panicked => Some("the kernel panicked".to_owned()),
        Verdict::Silent => Some(format!(
            "the programs did not all finish within {}s",
            args.timeout
        )),
    };
    let judged = crate::vfs::judge(programs, 0, after_boot);
    match (judged, ending) {
        (Ok(passed), None) => {
            for line in passed {
                println!("  {arch}: {line}");
            }
            println!("  {arch}: the networking programs all passed");
            Ok(())
        }
        (Ok(_), Some(ending)) => Err(Error::new(format!(
            "{arch}: {ending}.\n  Serial output is in {log}"
        ))),
        (Err(failed), ending) => {
            let why = failed.join("\n    ");
            let ending = ending.map_or(String::new(), |ending| format!("{ending}; "));
            Err(Error::new(format!(
                "{arch}: {ending}{} of {} networking programs failed:\n    {why}\n  \
                 Serial output is in {log}",
                failed.len(),
                programs.len()
            )))
        }
    }
}

/// How a watched boot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The line being waited for arrived.
    Reached,
    /// The kernel panicked first.
    Panicked,
    /// Neither, before the timeout or before QEMU closed the port.
    Silent,
}

/// What watching a boot produced.
#[derive(Debug)]
struct Watched {
    /// Every line the guest printed, in order.
    lines: Vec<String>,
    /// How it ended.
    verdict: Verdict,
    /// How QEMU ended.
    ended: Ended,
    /// Where the serial output was saved.
    log: PathBuf,
}

/// How QEMU ended: [`finish`]'s answer.
#[derive(Debug, Clone, Copy)]
struct Ended {
    /// QEMU's exit status.
    status: std::process::ExitStatus,
    /// Whether the guest powered the machine off itself, rather than QEMU
    /// being stopped from outside once the verdict was in.
    powered_off: bool,
}

/// Stage 12's exit, the boot half: boot as [`test_boot`] does, and say
/// whether the guest wrote the writable btrfs disk and read it back. The
/// caller then points host `btrfs check` at the same image.
///
/// # Errors
///
/// A boot that panicked, timed out or never reached the marker, exactly as
/// [`test_boot`] judges one, and a boot whose output says nothing about the
/// disk at all.
pub(crate) fn test_btrfs_write(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
) -> Result<bool> {
    let watched = watch(arch, image, kernel, args, SUCCESS_MARKER)?;
    if watched.verdict != Verdict::Reached {
        return Err(Error::new(format!(
            "{arch}: the boot did not reach the marker.\n  Serial output is in {}",
            watched.log.display()
        )));
    }
    crate::btrfs_check::guest_wrote(&watched.lines)
}

/// Boot headless, echo and save the serial port, and stop at the first line
/// containing `until`, at a panic, or at the timeout.
///
/// `kernel` is the ELF the image was built from, which is what a panic
/// report's backtrace addresses are resolved against.
fn watch(arch: Arch, image: &Path, kernel: &Path, args: &Args, until: &str) -> Result<Watched> {
    watch_hooked(arch, image, kernel, args, until, None)
}

/// Boot, and answer every line up to the first holding `until`. A boot that
/// panicked or never printed it is an error.
pub(crate) fn watch_lines(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
    until: &str,
) -> Result<Vec<String>> {
    let watched = watch(arch, image, kernel, args, until)?;
    match watched.verdict {
        Verdict::Reached => Ok(watched.lines),
        Verdict::Panicked => Err(panicked(arch, &watched.log)),
        Verdict::Silent => Err(Error::new(format!(
            "{arch}: `{until}` never came.\n  Serial output is in {}",
            watched.log.display()
        ))),
    }
}

/// [`watch`], calling `at_marker` once `until` has been printed, while QEMU
/// is still running, and returning the lines. The hook sees the lines so far
/// and may read the ones that follow ([`Watching`]). A boot that panicked or
/// never printed `until` is an error, as in `test_boot`.
pub(crate) fn watch_then(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
    until: &str,
    mut at_marker: impl FnMut(&mut Watching<'_>) -> Result<()>,
) -> Result<Vec<String>> {
    let watched = watch_hooked(arch, image, kernel, args, until, Some(&mut at_marker))?;
    match watched.verdict {
        Verdict::Reached => Ok(watched.lines),
        Verdict::Panicked => Err(panicked(arch, &watched.log)),
        Verdict::Silent => Err(Error::new(format!(
            "{arch}: `{until}` was not printed within {}s.\n  Serial output is in {}",
            args.timeout,
            watched.log.display()
        ))),
    }
}

/// [`watch_then`], and after `until` wait for the guest to power itself off,
/// however long that takes inside the timeout.
///
/// For a test that reads back a disk the guest wrote. The kernel commits
/// `/data` on its way to the power-off (`power::finish`), and the few
/// seconds [`watch_then`] gives a guest before stopping QEMU are not enough
/// to commit a build. A QEMU stopped from outside rather than powered off by
/// its guest is an error, since its last commit may be half written.
///
/// # Errors
///
/// As [`watch_then`], and when QEMU did not exit by itself with the status a
/// clean power-off gives.
pub(crate) fn watch_to_power_off(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
    until: &str,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    // Nothing to wait for but the end: `read_more` returns when the serial
    // port closes, which is QEMU exiting.
    let mut wait =
        |watching: &mut Watching<'_>| watching.read_more(deadline, |_| false).map(|_| ());
    let watched = watch_hooked(arch, image, kernel, args, until, Some(&mut wait))?;
    match watched.verdict {
        Verdict::Reached => {}
        Verdict::Panicked => return Err(panicked(arch, &watched.log)),
        Verdict::Silent => {
            return Err(Error::new(format!(
                "{arch}: `{until}` was not printed within {}s.\n  Serial output is in {}",
                args.timeout,
                watched.log.display()
            )));
        }
    }
    let code = watched.ended.status.code();
    // A QEMU asked to stop exits 0 as well, so the status alone would pass a
    // guest that never powered off.
    if !watched.ended.powered_off || (code != Some(0) && code != Some(DEBUG_EXIT_SUCCESS)) {
        return Err(Error::new(format!(
            "{arch}: the guest did not power itself off ({}), so what it wrote to /data may \
             not be committed.\n  Serial output is in {}",
            watched.ended.status,
            watched.log.display()
        )));
    }
    Ok(watched.lines)
}

/// The hook [`watch_then`] runs at the marker.
type AtMarker<'a> = &'a mut dyn FnMut(&mut Watching<'_>) -> Result<()>;

/// What a hook sees while QEMU is still running.
///
/// A test that has to *do* something at the marker -- take a screendump,
/// send an input event -- usually has to see what the guest said in answer,
/// and the guest says it after the marker. [`Watching::read_more`] reads
/// those lines from the same channel every other line comes from, and prints
/// and logs them the same way, so a transcript read afterwards is whole.
pub(crate) struct Watching<'a> {
    lines: &'a [String],
    receiver: &'a mpsc::Receiver<String>,
    log: &'a mut std::fs::File,
    started: Instant,
    after: Vec<String>,
    /// QEMU's standard input, which `-serial stdio` gives to the guest's
    /// console: what a person at the terminal would type goes here.
    keyboard: Option<&'a mut std::process::ChildStdin>,
    /// How QEMU is ended when the hook returns: given the moment to power
    /// off that a finished boot gets, unless the hook said otherwise.
    ending: Ending,
    /// What no line read from here on may show ([`Watching::redact`]).
    hidden: Vec<String>,
}

/// How [`finish`] ends QEMU once the hook has returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Give the guest [`POWER_OFF_GRACE`] to power itself off, then ask QEMU
    /// to stop: a guest on its way down gets to finish.
    Grace,
    /// Ask QEMU to stop at once: the check is done with a guest that will
    /// not power itself off ([`Watching::stop_when_done`]).
    Stop,
    /// Kill QEMU at once, with no chance for anything to finish
    /// ([`Watching::cut_power`]).
    Cut,
}

impl std::fmt::Debug for Watching<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Watching")
            .field("lines", &self.lines.len())
            .field("after", &self.after.len())
            .finish_non_exhaustive()
    }
}

impl Watching<'_> {
    /// Every line up to and including the one the boot waited for.
    pub(crate) fn lines(&self) -> &[String] {
        self.lines
    }

    /// Kill QEMU the moment the hook returns, with no chance for the guest to
    /// finish anything: the power failure `test-powerfail` needs.
    pub(crate) fn cut_power(&mut self) {
        self.ending = Ending::Cut;
    }

    /// Stop QEMU as soon as the hook returns, rather than give the guest
    /// [`POWER_OFF_GRACE`] to power itself off first: for a guest that never
    /// will -- a desktop, a shell left at its prompt -- once the check is done
    /// with it, where the grace is five seconds of waiting for nothing.
    ///
    /// Nothing the gate checks is lost. The grace reads no lines, since the
    /// transcript ends when the hook returns, and a guest that stays up
    /// powers nothing off in it. QEMU is still asked to stop before it is
    /// killed, as after the grace, so a coverage run's trace is written the
    /// same way. A guest that powers off after its hook -- one that was
    /// told `poweroff`, or whose init exits -- must not ask for this, or its
    /// power-off would go unrun.
    pub(crate) fn stop_when_done(&mut self) {
        if self.ending == Ending::Grace {
            self.ending = Ending::Stop;
        }
    }

    /// The lines read since, by [`Watching::read_more`].
    pub(crate) fn after(&self) -> &[String] {
        &self.after
    }

    /// Type `keys` at the guest's console, as a person at the terminal
    /// would: the bytes reach the serial port, and the kernel's line
    /// discipline and whatever is reading it do the rest.
    ///
    /// The transcript records what was typed, so a log read afterwards says
    /// what the guest was answering.
    ///
    /// # Errors
    ///
    /// When there is no console to type at -- a run whose QEMU was not given
    /// one -- or when the bytes cannot be written, which is QEMU having gone.
    pub(crate) fn type_in(&mut self, keys: &[u8]) -> Result<()> {
        let at = self.started.elapsed().as_secs_f64();
        let shown = String::from_utf8_lossy(keys)
            .replace('\n', "\\n")
            .replace('\x03', "^C")
            .replace('\x1a', "^Z")
            .replace('\x04', "^D");
        println!("  {at:6.2} > {shown}");
        writeln!(self.log, "{at:6.2} > {shown}")?;
        let Some(keyboard) = self.keyboard.as_mut() else {
            return Err(Error::new(
                "this boot has no console to type at: QEMU was started without one",
            ));
        };
        keyboard.write_all(keys)?;
        keyboard.flush()?;
        Ok(())
    }

    /// Read lines until `enough` is true of them all, or until `deadline`.
    ///
    /// Says whether `enough` was ever true: a caller that waited for an
    /// answer and did not get one reports that itself, since only it knows
    /// what it was waiting for.
    ///
    /// **The stamp on a line read here is when it was read, not when it
    /// arrived.** The reader thread buffers into a channel the whole time,
    /// and a hook that sleeps -- for a screen to settle, say -- comes back to
    /// find everything the guest said in the meantime and stamps it now. Two
    /// lines the guest printed a millisecond apart can therefore show seconds
    /// apart in the transcript, and that is the hook's pause and not the
    /// guest's.
    ///
    /// # Errors
    ///
    /// Only a log that could not be written.
    pub(crate) fn read_more(
        &mut self,
        deadline: Instant,
        mut enough: impl FnMut(&[String]) -> bool,
    ) -> Result<bool> {
        if enough(&self.after) {
            return Ok(true);
        }
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            let Ok(line) = self.receiver.recv_timeout(remaining) else {
                return Ok(false);
            };
            self.keep(line)?;
            if enough(&self.after) {
                return Ok(true);
            }
        }
    }

    /// Read what the guest has said so far: every line until it has been
    /// quiet for [`QUIET`], or until `most` has passed.
    ///
    /// For a hook that has what it waited for and wants the rest of the
    /// transcript before it is judged, where a fixed wait of `most` used to
    /// stand. A line the guest has printed is in the pipe within
    /// microseconds, so a quiet stretch of this length means it has nothing
    /// more to say for now; a guest that goes on talking is read for `most`,
    /// as before.
    ///
    /// # Errors
    ///
    /// Only a log that could not be written.
    pub(crate) fn read_what_was_said(&mut self, most: Duration) -> Result<()> {
        let deadline = Instant::now() + most;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(());
            };
            let Ok(line) = self.receiver.recv_timeout(remaining.min(QUIET)) else {
                return Ok(());
            };
            self.keep(line)?;
        }
    }

    /// Wait until a shell reads the console: type a line only a running
    /// shell answers, and type it again every [`RETYPE`] until one is
    /// answered or `deadline` passes. Says whether one was.
    ///
    /// Where a gate used to wait a fixed while before its first keystroke,
    /// for the shell to have started and printed its prompt: a prompt ends
    /// no line, so it never reaches [`Watching::read_more`], and the answer
    /// to a line typed at it is the first thing that does. A line typed
    /// before the shell reads is not lost -- the terminal holds it, and the
    /// shell reads it first -- and one that is lost anyway is typed again.
    /// Every step after this looks only at lines after its own keystrokes,
    /// so an answer to a line typed twice is answered twice and harms none.
    ///
    /// The line has no quote, so a part of it read on its own opens nothing
    /// the next line would be read into, and its answer, `42`, is not in
    /// what the console echoes of it.
    ///
    /// # Errors
    ///
    /// When there is no console to type at, or a log that could not be
    /// written.
    pub(crate) fn wait_for_shell(&mut self, deadline: Instant) -> Result<bool> {
        const ASK: &[u8] = b"echo xtask-shell-$((6 * 7))\n";
        const ANSWER: &str = "xtask-shell-42";
        let before = self.after.len();
        loop {
            self.type_in(ASK)?;
            let retype = (Instant::now() + RETYPE).min(deadline);
            let answered = self.read_more(retype, |lines| {
                lines
                    .get(before..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(ANSWER))
            })?;
            if answered {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }

    /// Keep `secret` out of every line read from here on: each is
    /// [`redacted`] before it is printed, logged or kept. For a gate that
    /// types a credential into the guest, whose programs may say it back.
    pub(crate) fn redact(&mut self, secret: &str) {
        if !secret.is_empty() {
            self.hidden.push(secret.to_owned());
        }
    }

    /// Print, log and keep one line read after the marker.
    fn keep(&mut self, line: String) -> Result<()> {
        let line = if self.hidden.is_empty() {
            line
        } else {
            redacted(&line, &self.hidden)
        };
        let at = self.started.elapsed().as_secs_f64();
        println!("  {at:6.2} | {line}");
        writeln!(self.log, "{at:6.2} | {line}")?;
        self.after.push(line);
        Ok(())
    }
}

/// `line` with each of `secrets` in it, in any case, replaced by
/// `<redacted>`: an account name comes back from a program in the case it
/// likes.
pub(crate) fn redacted(line: &str, secrets: &[String]) -> String {
    let mut line = line.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        let wanted = secret.to_ascii_lowercase();
        let mut out = String::with_capacity(line.len());
        let mut rest = line.as_str();
        while let Some(at) = rest.to_ascii_lowercase().find(&wanted) {
            // Lowering ASCII moves no byte, so `at` is a boundary of `rest`.
            out.push_str(rest.get(..at).unwrap_or_default());
            out.push_str("<redacted>");
            rest = rest.get(at + wanted.len()..).unwrap_or_default();
        }
        out.push_str(rest);
        line = out;
    }
    line
}

fn watch_hooked(
    arch: Arch,
    image: &Path,
    kernel: &Path,
    args: &Args,
    until: &str,
    at_marker: Option<AtMarker<'_>>,
) -> Result<Watched> {
    let symbols = Symbolizer::open(kernel);
    crate::builds::refuse_boot(arch)?;
    let (mut command, network) = qemu_command(arch, image, Some(kernel), args, &Console::Owned)?;
    // Every DMA fault the machine's IOMMU records, which the run is judged by
    // below; `crate::dma_faults` says why QEMU's own remarks are not enough.
    let _ = command.args(dma_faults::trace_args(arch));
    // Stderr is piped rather than inherited so that one message can be taken
    // out of it; everything else QEMU says there still reaches the terminal,
    // which the error below about a boot that never started depends on.
    // `crate::noise` says which message and why.
    let _ = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| Error::new(format!("could not start QEMU: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::new("QEMU produced no stdout to read"))?;
    let noise = child.stderr.take().map(crate::noise::Filter::start_judged);
    // The guest's keyboard. Held for the whole boot rather than dropped,
    // because `-serial stdio` gives it to the guest's console: closing it
    // would be a person walking away from the terminal, and a shell reading
    // it would see end of input. A gate that types uses `Watching::type_in`.
    let mut keyboard = child.stdin.take();

    // A reader thread and a channel, rather than a non-blocking read: the guest
    // may say nothing for seconds at a time, and the timeout has to apply to
    // the boot as a whole rather than to each line.
    let (receiver, reader) = read_lines(stdout);

    let log_path = paths::build_dir(arch).join("serial.log");
    let mut log = std::fs::File::create(&log_path)?;
    // Every line is stamped with the seconds since QEMU was started, on the
    // screen and in the log. A boot that stops says *where* it stopped either
    // way; only a stamp says whether it was slow getting there or hung, which
    // on a loaded host are two different problems with the same last line.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.timeout);
    let mut lines = Vec::new();
    let mut verdict = Verdict::Silent;

    // Under `--reset` the marker is not the end: the kernel still has to say it
    // is resetting, and the loader to start again because it did.
    let mut resetting = false;
    let mut restarted = false;
    let mut closed = false;
    let mut unchecked = false;
    while verdict == Verdict::Silent || (args.reset && verdict == Verdict::Reached && !restarted) {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(remaining) {
            Ok(line) => {
                let at = started.elapsed().as_secs_f64();
                println!("  {at:6.2} | {line}");
                writeln!(log, "{at:6.2} | {line}")?;
                if verdict == Verdict::Silent && line.contains(until) {
                    verdict = Verdict::Reached;
                    // A kernel that has not said it will reset by its marker
                    // never will, and a power-off only pauses QEMU: waiting
                    // for the loader again would be waiting for the timeout.
                    if args.reset && !lines.iter().any(|seen: &String| seen.contains(RESET_ARMED)) {
                        restarted = true;
                    }
                } else if line.contains(PANIC_MARKER) {
                    verdict = Verdict::Panicked;
                } else if until == SUCCESS_MARKER && line.contains(UNCHECKED_MARKER) {
                    unchecked = true;
                }
                if args.reset && verdict == Verdict::Reached {
                    if line.contains(RESETTING) {
                        resetting = true;
                    } else if resetting && line.contains(LOADER_BANNER) {
                        restarted = true;
                    }
                }
                lines.push(line);
                if unchecked {
                    break;
                }
            }
            // The guest closed the serial port: QEMU is on its way out, so stop
            // reading and judge on the exit status below.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                closed = true;
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    if verdict == Verdict::Panicked {
        take_panic_report(&receiver, &mut log, symbols.as_ref())?;
    }
    // While QEMU still runs, so the hook can ask it things; a hook that fails
    // still lets QEMU be stopped and the log be kept.
    let (hooked, ending) = run_hook(
        at_marker,
        verdict,
        &mut lines,
        &receiver,
        &mut log,
        started,
        keyboard.as_mut(),
    );
    let ended = finish(&mut child, verdict != Verdict::Silent, ending)?;
    drop(receiver);
    let _ = reader.join();
    // Before the error below says QEMU's own is "above": joining the sieve's
    // thread is what makes that true, since it ends when stderr does.
    let sieved = noise.map(crate::noise::Filter::finish).unwrap_or_default();
    crate::noise::report(sieved.hidden);
    log.flush()?;
    // After QEMU has gone, so that the numbers are final and the gateway's
    // thread is not still being fed while they are read.
    report_network(&network);

    if unchecked {
        return Err(skipped_its_checks(arch, &log_path));
    }
    // QEMU gone before the guest said what it was waited for is not a timeout,
    // and every caller would otherwise report one: an argument QEMU refuses
    // ends it at once, with the reason on the stderr above, and "did not
    // finish within 600s" sends whoever reads it to look at the guest.
    if closed && verdict == Verdict::Silent {
        return Err(exited_early(arch, ended.status, started, until, &log_path));
    }

    hooked?;
    sieved
        .dma
        .judge(arch, verdict == Verdict::Reached, &lines, &log_path)?;
    finish_watching(lines, verdict, ended, log_path)
}

/// What [`watch_hooked`] saw, once QEMU is gone; and, for a coverage run, the
/// boot's slide written beside its trace, since the trace records the
/// addresses the kernel ran at, which KASLR moved.
fn finish_watching(
    lines: Vec<String>,
    verdict: Verdict,
    ended: Ended,
    log: PathBuf,
) -> Result<Watched> {
    crate::kaslr::record_coverage_slide(&lines)?;
    Ok(Watched {
        lines,
        verdict,
        ended,
        log,
    })
}

/// Read `stdout` a line at a time on a thread of its own, into a channel.
///
/// **Bytes, not UTF-8.** A line is read up to its newline as bytes and
/// decoded lossily, a byte that is not UTF-8 shown as U+FFFD. `lines()`
/// answers such a line with an error, which ended this thread, and the
/// watch took the closed channel for the guest closing its serial port and
/// stopped QEMU: an x86-64 `test-init` boot of main 3c08d5657 ended that
/// way at 7.5 s, its last line the I/O APIC's conversion just before the
/// console's own interrupt line is converted (2026-10-05,
/// `batch-20261005T114127Z-b0-3`). Only the end of QEMU's output, or a read
/// that fails, ends it now.
fn read_lines(
    stdout: std::process::ChildStdout,
) -> (mpsc::Receiver<String>, std::thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        send_lines(BufReader::new(stdout), &sender);
    });
    (receiver, reader)
}

/// [`read_lines`]'s loop: each line of `input`, its newline and a carriage
/// return before it taken off, decoded lossily, sent until the input ends,
/// a read fails or nobody listens.
fn send_lines(mut input: impl BufRead, sender: &mpsc::Sender<String>) {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match input.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if sender
            .send(String::from_utf8_lossy(line).into_owned())
            .is_err()
        {
            break;
        }
    }
}

/// The error for a boot that was waited on for [`SUCCESS_MARKER`] and printed
/// [`UNCHECKED_MARKER`] instead.
fn skipped_its_checks(arch: Arch, log: &Path) -> Error {
    Error::new(format!(
        "{arch}: the kernel skipped its self-checks (`{UNCHECKED_MARKER}`): the command \
         line asked for ferrix.checks=skip, and a boot that was waited on for \
         `{SUCCESS_MARKER}` needs them run.\n  Serial output is in {}",
        log.display()
    ))
}

/// The error for a QEMU that exited before the guest printed `until`.
fn exited_early(
    arch: Arch,
    status: std::process::ExitStatus,
    started: Instant,
    until: &str,
    log: &Path,
) -> Error {
    Error::new(format!(
        "{arch}: QEMU exited ({status}) {:.1}s after it started, before the guest printed \
         `{until}`; QEMU's own error, if it gave one, is above.\n  \
         Serial output is in {}",
        started.elapsed().as_secs_f64(),
        log.display()
    ))
}

/// Run the marker hook, if there is one and the marker was reached, and
/// keep whatever lines it read in the transcript. Answers what the hook
/// answered, and how it asked for QEMU to be ended.
fn run_hook(
    at_marker: Option<AtMarker<'_>>,
    verdict: Verdict,
    lines: &mut Vec<String>,
    receiver: &mpsc::Receiver<String>,
    log: &mut std::fs::File,
    started: Instant,
    keyboard: Option<&mut std::process::ChildStdin>,
) -> (Result<()>, Ending) {
    let Some(hook) = at_marker else {
        return (Ok(()), Ending::Grace);
    };
    if verdict != Verdict::Reached {
        return (Ok(()), Ending::Grace);
    }
    let mut watching = Watching {
        lines,
        receiver,
        log,
        started,
        after: Vec::new(),
        keyboard,
        ending: Ending::Grace,
        hidden: Vec::new(),
    };
    let answered = hook(&mut watching);
    let ending = watching.ending;
    let after = watching.after;
    lines.extend(after);
    (answered, ending)
}

/// The error for a boot that panicked.
fn panicked(arch: Arch, log: &Path) -> Error {
    Error::new(format!(
        "the {arch} kernel panicked during boot; see {}",
        log.display()
    ))
}

/// Copy the rest of a panic report to the terminal and the log, naming the
/// function beside each backtrace address when `symbols` can.
///
/// Stops when the guest goes quiet for [`PANIC_REPORT_GRACE`] or closes the
/// port. The verdict is already decided; this is only so that it arrives with
/// its reasons.
pub(crate) fn take_panic_report(
    receiver: &mpsc::Receiver<String>,
    log: &mut impl Write,
    symbols: Option<&Symbolizer>,
) -> Result<()> {
    while let Ok(line) = receiver.recv_timeout(PANIC_REPORT_GRACE) {
        let line = symbols
            .and_then(|symbols| symbols.annotate(&line))
            .unwrap_or(line);
        println!("       | {line}");
        writeln!(log, "       | {line}")?;
        if line.contains(SUCCESS_MARKER) || line.contains(PANIC_MARKER) {
            // Another processor's report, or something worse; either way the
            // first one is what failed the boot, and waiting for more of them
            // could go on for as long as the guest keeps printing.
            break;
        }
    }
    log.flush()?;
    Ok(())
}

/// How long a guest that has reached its verdict is given to power itself off.
const POWER_OFF_GRACE: Duration = Duration::from_secs(5);

/// How long a guest has to have said nothing for
/// [`Watching::read_what_was_said`] to take it that it has said what it had
/// to: many times what a line takes to cross the serial port, even under
/// emulation on a loaded host.
const QUIET: Duration = Duration::from_millis(500);

/// How long [`Watching::wait_for_shell`] waits for an answer before it types
/// its line again.
const RETYPE: Duration = Duration::from_secs(2);

/// How long QEMU is given to exit once it has been asked to.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Wait for QEMU to exit, and end it if the guest will not.
///
/// Answers QEMU's status and whether the guest powered the machine off
/// itself, which [`watch_to_power_off`] requires and nothing else does.
///
/// A guest that does not power off -- an interactive shell still waiting at
/// its prompt, a kernel halted by the panic a test asked for -- is *asked* to
/// stop before it is killed. QEMU treats SIGTERM as a host shutdown: it stops
/// the machine and exits normally, and a normal exit is the only one that
/// runs the TCG plugins' exit hooks. The coverage plugin writes its whole
/// table from that hook, so a killed QEMU leaves an empty trace, and every
/// gate that ended by killing QEMU used to contribute nothing to the coverage
/// measurement (`docs/certification/VERIFICATION.md` §3). The guest sees no
/// difference: it is stopped either way, after its verdict was read.
///
/// [`Ending::Stop`] skips the grace: the hook said the guest will not power
/// itself off, so QEMU is asked to stop at once. [`Ending::Cut`] is
/// `test-powerfail`'s power failure, which is killed at once and never
/// asked: the point of it is that nothing, QEMU's own block layer included,
/// gets to finish anything.
fn finish(child: &mut std::process::Child, decided: bool, ending: Ending) -> Result<Ended> {
    if ending != Ending::Cut {
        // Give the guest a moment to shut itself down cleanly, so a working
        // power-off path is exercised rather than always being papered over;
        // a guest that will not power off is only looked at, not waited for.
        let grace = match ending {
            Ending::Grace => POWER_OFF_GRACE,
            Ending::Stop | Ending::Cut => Duration::ZERO,
        };
        if decided && let Some(status) = exited_within(child, grace)? {
            return Ok(Ended {
                status,
                powered_off: true,
            });
        }
        if ask_to_stop(child)
            && let Some(status) = exited_within(child, STOP_GRACE)?
        {
            return Ok(Ended {
                status,
                powered_off: false,
            });
        }
    }
    let _ = child.kill();
    Ok(Ended {
        status: child.wait()?,
        powered_off: false,
    })
}

/// QEMU's status if it exits within `grace`; with no grace, if it already
/// has.
fn exited_within(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Send QEMU SIGTERM, through `kill(1)` since this program links no libc
/// crate. Says whether the signal was sent.
#[cfg(unix)]
fn ask_to_stop(child: &std::process::Child) -> bool {
    Command::new("kill")
        .args(["-s", "TERM", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Windows has no SIGTERM to send, so QEMU is killed there as it always was.
#[cfg(not(unix))]
fn ask_to_stop(_child: &std::process::Child) -> bool {
    false
}

/// A VNC server on the loopback for a judged boot that needs a viewer's view
/// of the card -- its cursor plane, which a screendump cannot see -- beside
/// the boot's headless display; nothing for every other boot.
fn judged_vnc(args: &Args, card: Option<&str>) -> Vec<String> {
    match (args.judge_vnc, card) {
        (Some(port), Some(card)) => vec![
            "-vnc".to_owned(),
            format!(
                "127.0.0.1:{},display={card},head=0",
                port.saturating_sub(5900)
            ),
        ],
        _ => Vec::new(),
    }
}

/// `-accel`, and the `-plugin` a structural-coverage run adds beside it.
///
/// A TCG plugin counts the basic blocks a boot actually executes, which is the
/// only way to measure the kernel's coverage at all: `docs/sysml/
/// 11-assurance.sysml` records that a fuzzer cannot drive a page-fault handler
/// and Miri cannot interpret a privileged instruction, so QEMU is the only
/// thing that reaches ring 0. `tools/common/gen/coverage-report.py` turns what the
/// plugin writes into statement coverage per certification ring; see
/// `docs/certification/VERIFICATION.md`.
///
/// The two belong in one function because they constrain each other: a plugin
/// observes only blocks TCG translates, so an accelerator that does not
/// translate is refused here rather than producing a boot's worth of coverage
/// that reads as zero. A silent zero is indistinguishable from a kernel that
/// nothing exercised, which is the worst of the available failures.
///
/// `kernel` is the ELF the boot runs, which a coverage run keeps beside its
/// trace: gates build different kernels, and a trace means nothing read
/// against another one's addresses ([`crate::coverage::plugin_for_boot`]).
pub(crate) fn accelerator_arguments(
    accelerator: &str,
    kernel: Option<&Path>,
) -> Result<Vec<String>> {
    let mut arguments = vec!["-accel".to_owned(), accelerator.to_owned()];

    if let Ok(plugin) = std::env::var("FERRIX_QEMU_PLUGIN") {
        if accelerator != "tcg" {
            return Err(Error::new(format!(
                "FERRIX_QEMU_PLUGIN needs `--accel tcg`: a TCG plugin observes \
                 translated blocks, and under {accelerator} the guest never \
                 translates any, so coverage would read as zero rather than as \
                 an error."
            )));
        }
        arguments.push("-plugin".to_owned());
        arguments.push(crate::coverage::plugin_for_boot(&plugin, kernel)?);
    }

    Ok(arguments)
}

/// The CPU model `q35` emulates, under `accelerator`.
///
/// Under a hypervisor the guest's speculation controls are the host
/// processor's, and `qemu64` passes on none of them unless asked: the kernel's
/// side-channel defences would find nothing to turn on, and the code that turns
/// them on would never run. So the hypervisor is asked for them --
/// `IA32_SPEC_CTRL` with IBRS, STIBP and SSBD, `IA32_ARCH_CAPABILITIES`, and
/// AMD's automatic IBRS -- and passes on whichever the host has, warning about
/// the rest. Not under TCG, which emulates no speculation to control and
/// would warn about every one. `docs/certification/SPECULATION.md`.
///
/// And under KVM and WHPX an invariant TSC, which the host's is and `qemu64`
/// does not say: without the bit the kernel's clock is the HPET
/// (`src/kernel/src/arch/x86_64/clock.rs`), and every reading of an emulated HPET
/// is an exit to QEMU. A browser asking the time six thousand times a second,
/// and the scheduler asking at every switch, spent processors on nothing
/// else. Under WHPX, on the one processor it runs with, Chrome on the desktop
/// answered a click half a minute late: `bench-chrome --accel whpx` found the
/// machine 100% busy at 16 to 36 frames a second with the HPET, and 19 to 25%
/// busy at 60 with the TSC (2026-09-29). TCG has no invariant TSC to give
/// and says so.
///
/// UMIP under both, which TCG emulates: without it a program's `SIDT` reads
/// the address of the IDT inside the kernel image, and KASLR is undone by one
/// instruction (`SPECULATION.md` §6).
///
/// And x86-64-v3's instructions under every accelerator -- SSSE3 to SSE4.2,
/// `XSAVE`, AVX and AVX2, BMI, FMA -- which every x86-64 processor sold in the
/// last decade has and `qemu64` lacks. A program built for them with no
/// fallback cannot run without them: Claude Code's Bun runtime, on a
/// `qemu64` without AVX, found no string routine it was allowed and spun
/// forever in its first conversion (`docs/CLAUDE-CODE.md` §3). The kernel
/// saves AVX with `XSAVE` when the processor has it
/// (`src/kernel/src/arch/x86_64/cpu.rs`). TCG emulates all of them since
/// QEMU 7.2. `XSAVEOPT` with them, as every processor with AVX has it:
/// QEMU's TCG takes `CR4.OSXSAVE` for a reserved bit unless the model has
/// one of leaf 0xD's sub-leaf 1 features, and answers the kernel's write of
/// it by running the write again, forever (QEMU 9.2 and 10.2, 2026-09-30).
///
/// `FERRIX_X86_CPU` replaces the model, as `FERRIX_ARM_CPU` does on Arm: the
/// coverage suite boots a processor with `RDRAND` and no `RDSEED` that way,
/// which is the only one that takes `cpu::hardware_random`'s other
/// instruction.
pub(crate) fn x86_cpu(accelerator: &str) -> String {
    if let Ok(model) = std::env::var("FERRIX_X86_CPU") {
        return model;
    }
    let base = "qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand,+rdseed,+ssse3,+sse4.1,+sse4.2,+popcnt,\
                +cx16,+movbe,+xsave,+xsaveopt,+avx,+avx2,+f16c,+fma,+bmi1,+bmi2,+abm,+pclmulqdq,+aes,\
                +x2apic";
    if accelerator == "tcg" {
        return base.to_owned();
    }
    let clock = if matches!(accelerator, "kvm" | "whpx") {
        ",+invtsc"
    } else {
        ""
    };
    format!("{base},+spec-ctrl,+stibp,+ssbd,+arch-capabilities,+auto-ibrs{clock}")
}

/// [`x86_cpu`] for the QEMU at `binary`: under TCG before QEMU 9.1, without
/// SMAP.
///
/// Those QEMUs read the stack of a far return or `iret` from user mode to user
/// mode as ring 0 -- "target/i386/tcg: Allow IRET from user mode to user mode
/// with SMAP" fixed it in 9.1 -- so with SMAP on, the boot check that enters
/// compatibility mode with `lretq` took a supervisor page fault in ring 3 and
/// panicked the kernel. Ubuntu 24.04's QEMU is 8.2, which is what GitHub's
/// runners install, and why CI's boot job failed from 2026-09-24. The
/// kernel runs without SMAP, and says so; KVM uses the processor, which
/// has no such fault, and `FERRIX_X86_CPU` still names any model at all.
pub(crate) fn x86_cpu_for(binary: &Path, accelerator: &str) -> String {
    let model = x86_cpu(accelerator);
    if accelerator != "tcg" || std::env::var_os("FERRIX_X86_CPU").is_some() {
        return model;
    }
    match qemu_version(binary) {
        Some(version) if version < (9, 1) => {
            println!(
                "  x86_64: SMAP left off: QEMU {}.{} reads a user-mode far return's stack as                  ring 0 under TCG, which 9.1 fixed",
                version.0, version.1
            );
            model.replace(",+smap", "")
        }
        _ => model,
    }
}

/// The major and minor version `binary --version` prints: "QEMU emulator
/// version 8.2.2 (Debian ...)".
fn qemu_version(binary: &Path) -> Option<(u32, u32)> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_qemu_version(&text)
}

/// What `tools/common/fetch/fetch-qemu-linux.sh` builds QEMU with
/// (`--with-pkgversion`), and so what its `--version` says in brackets.
const PATCHED_QEMU: &str = "(ferrix-cfi";

/// Refuse an x86-64 boot on a QEMU that does not block compatibility-format
/// interrupts (F-57, `docs/NVIDIA.md` §12.3).
///
/// Released QEMU ignores the VT-d `GCMD.CFI` bit and passes every
/// compatibility-format message through even with interrupt remapping on,
/// so on it the kernel's own check of that block cannot pass, and a
/// `CFIS`=0 read back proves nothing. The patched build says so in its
/// `--version`; anything else is refused before the boot, by name.
///
/// # Errors
///
/// `binary` does not run, or is not the patched build.
fn require_compatibility_block(binary: &Path) -> Result<()> {
    let output = Command::new(binary).arg("--version").output();
    let text = output
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    if blocks_compatibility_format(&text) {
        return Ok(());
    }
    let banner = text.lines().next().unwrap_or("no --version");
    Err(Error::new(format!(
        "x86-64 boots need a QEMU that blocks compatibility-format interrupts (F-57); \
         run tools/common/fetch/fetch-qemu-linux.sh\n  {} says: {banner}",
        binary.display()
    )))
}

/// Whether a QEMU `--version` banner is the patched build's.
fn blocks_compatibility_format(version: &str) -> bool {
    version
        .lines()
        .next()
        .is_some_and(|banner| banner.contains(PATCHED_QEMU))
}

/// [`qemu_version`]'s parse, apart for its tests.
fn parse_qemu_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.split("version ").nth(1)?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The PC QEMU emulates under `accelerator`: `q35`, with KVM's interrupt
/// controller split so that the I/O APIC is QEMU's (QEMU refuses
/// `intremap=on` beside a whole in-kernel irqchip, `x86-iommu.c`), and with
/// `FERRIX_X86_MACHINE` added as `FERRIX_ARM_MACHINE` is added to `virt` --
/// `hpet=off` for a PC without an HPET, whose clock is then the TSC measured
/// against the PIT.
pub(crate) fn x86_machine(accelerator: &str) -> String {
    let base = if accelerator == "kvm" {
        "q35,kernel-irqchip=split"
    } else {
        "q35"
    };
    match std::env::var("FERRIX_X86_MACHINE") {
        Ok(extra) => format!("{base},{extra}"),
        Err(_) => base.to_owned(),
    }
}

/// The VT-d unit every x86-64 machine this tool boots has: interrupt
/// remapping on, as the kernel drives it (`docs/NVIDIA.md` §12.3, N0g), and
/// extended interrupt mode off, since the kernel runs xAPIC and programs
/// `IRTA.EIME` = 0 -- QEMU's `auto` would offer it under the split irqchip.
pub(crate) const INTEL_IOMMU: &str = "intel-iommu,intremap=on,eim=off";

/// [`INTEL_IOMMU`] without interrupt remapping: the unit translates DMA and
/// remaps no interrupt, so `device_isolation`'s bit 1 is clear for every
/// device. Only `test-nvrm`'s refused hand-over boots it
/// (`Args::unisolated_interrupts`).
pub(crate) const INTEL_IOMMU_UNISOLATED: &str = "intel-iommu,intremap=off";

/// Which CPU model the `virt` machine emulates for an Arm architecture.
///
/// `cortex-a72` is ARMv8.0 and so has no PAN -- the feature the kernel uses to
/// keep EL1 out of user pages -- and QEMU exposes no property to add one to
/// this model the way x86-64's `+smap` adds SMAP to `qemu64`. `FERRIX_ARM_CPU`
/// swaps the model, which is how `docs/certification/FINDINGS.md` F-32
/// demonstrates that path on `max` without changing what every Arm test runs
/// on. ARMv7-A's Cortex-A7 cannot have PAN at all: it is an ARMv8.1 feature.
pub(crate) fn arm_cpu(arch: Arch) -> String {
    if let Ok(model) = std::env::var("FERRIX_ARM_CPU") {
        return model;
    }
    if arch == Arch::AArch64 {
        "cortex-a72"
    } else {
        "cortex-a7"
    }
    .to_owned()
}

/// Assemble the QEMU command line for `arch`.
fn qemu_command(
    arch: Arch,
    image: &Path,
    kernel: Option<&Path>,
    args: &Args,
    console: &Console,
) -> Result<(Command, Network)> {
    // A boot that wants the 3D card runs a QEMU that has it, where this
    // machine has one; `window::qemu_for` says so when that is not the
    // first on `PATH`.
    let binary = crate::window::qemu_for(arch.qemu_binary(), args.gl).ok_or_else(|| {
        Error::new(format!(
            "{} is not on PATH.\n  Install QEMU (Debian/Ubuntu: `qemu-system-x86` and \
             `qemu-system-arm`; Windows: `winget install SoftwareFreedomConservancy.QEMU`).",
            arch.qemu_binary()
        ))
    })?;

    // Linux hosts only for now: the Windows build (fetch-qemu-windows.sh)
    // does not carry the patch yet (docs/BACKLOG.md).
    if arch == Arch::X86_64 && cfg!(target_os = "linux") {
        require_compatibility_block(&binary)?;
    }

    let firmware = paths::find_firmware(arch)?;
    let accelerator = accelerator(arch, &binary, args.accel.as_deref())?;
    let processors = processors(&accelerator, args);
    // Every boot names its accelerator, so a log read later says whether
    // the guest was emulated without anybody having to guess.
    println!("  qemu: {arch} under {accelerator}, {processors} processors");

    let mut command = Command::new(&binary);
    let _ = command.current_dir(paths::workspace_root());

    let _ = command.args(accelerator_arguments(&accelerator, kernel)?);
    let _ = command.args([
        "-m",
        &args.memory.to_string(),
        "-smp",
        &processors.to_string(),
        "-monitor",
        "none",
    ]);
    // Where the screen goes: nowhere for a test, which reads it with a
    // screendump; a window or a VNC server for a boot somebody watches.
    // `window` chooses by asking this QEMU what it was built with, because
    // `-display default` fails outright on a build with no local backend
    // rather than falling back to anything.
    let window = crate::window::choose(&binary, args)?;
    // Which console the person means: the card, on a machine that has one and
    // a boot that asked for it. `attach_display` puts it there.
    let card = args.display.then(|| crate::display::device_id(0));
    // Whether the card on the bus will be the 3D one, which the backend has
    // to know: `attach_display` asks the same question of the same QEMU and
    // gets the same answer.
    let (_, gl) = crate::window::card(&binary, args.gl && card.is_some());
    let _ = command.args(window.arguments_with(card.as_deref(), gl, args.rendernode.as_deref()));
    let _ = command.args(judged_vnc(args, card.as_deref()));
    let _ = command.args(crate::window::keymap_arguments(&window, args)?);
    window.announce(card.as_deref());
    // The serial port, which is this machine's whole console.
    let _ = command.args(console.arguments()?);
    // A guest that reboots on a triple fault turns a crash into an endless
    // loop, which in CI is a timeout with no cause in the log. Not under
    // `--reset`, whose point is that the machine starts again: there a reset
    // restarts firmware and the loader, and a power-off only pauses QEMU, so
    // the loader can appear a second time only if the kernel reset it.
    if args.reset {
        let _ = command.args(["-action", "shutdown=pause"]);
    } else {
        let _ = command.arg("-no-reboot");
    }

    match arch {
        Arch::X86_64 => {
            let _ = command.args([
                "-machine",
                &x86_machine(&accelerator),
                // SMEP and SMAP are the two features the kernel relies on to
                // keep ring 0 out of user pages, so emulate a CPU that has them.
                // RDRAND and RDSEED too, which the kernel seeds its random
                // generator from beside firmware's bytes, so that path runs.
                // Under a hypervisor, the speculation controls as well, and
                // under KVM an invariant TSC; see `x86_cpu`.
                "-cpu",
                &x86_cpu_for(&binary, &accelerator),
                // A controlled way for the guest to end the test: writing 0x10
                // to port 0xF4 exits QEMU with status 33.
                "-device",
                "isa-debug-exit,iobase=0xf4,iosize=0x04",
                // A VT-d unit for stage 10's IOMMU domains, remapping
                // interrupts (`INTEL_IOMMU`), unless the boot is
                // `test-nvrm`'s without.
                "-device",
                if args.unisolated_interrupts {
                    INTEL_IOMMU_UNISOLATED
                } else {
                    INTEL_IOMMU
                },
            ]);
            // The machine's own VGA, which `q35` adds unasked, is QEMU's
            // first console and the card is the second, so a window opens on
            // firmware's head and the compositor draws out of sight. A boot
            // that is watched takes the VGA away and leaves the card as the
            // only console; a boot that is judged keeps it, because the
            // loader's framebuffer is then VGA's rather than the card the
            // driver takes over, which is `docs/DISPLAY.md` §2.4's hazard.
            if crate::window::sole_screen(args) {
                let _ = command.args(["-vga", "none"]);
            }
            // The image on the first port of q35's AHCI controller, where a
            // bare `-drive` puts it, spelled out only to give it `bootindex`.
            // Without one OVMF tries the virtio test disk first, because its
            // slot comes before the controller's, and says so:
            //
            //     BdsDxe: failed to load Boot0002 "UEFI Misc Device" from
            //     PciRoot(0x0)/Pci(0x3,0x0): Not Found
            //
            // which is harmless and the kind of noise that trains people to
            // skim the boot log.
            if !args.boot_virtio {
                let _ = command.args([
                    "-drive",
                    &format!("format=raw,file={},if=none,id=disk", display(image)),
                    "-device",
                    "ide-hd,drive=disk,bus=ide.0,bootindex=0",
                ]);
            }
        }
        Arch::AArch64 | Arch::Armv7a => {
            // The same `virt` machine for both: a GICv2, a PL011 at the same
            // address, the architected timer, a virtio disk. Only the CPU
            // differs, and with it the width of everything the CPU does.
            let cpu = arm_cpu(arch);
            let _ = command.args(VIRT_MACHINE);
            // QEMU merges a second `-machine` into the first, so this adds to
            // `virt` rather than replacing it: `gic-version=3` for the GICv3
            // driver, `acpi=off` for the device-tree path the Pixel 7 takes.
            if let Ok(extra) = std::env::var("FERRIX_ARM_MACHINE") {
                let _ = command.args(["-machine", &extra]);
            }
            let _ = command.args([
                "-cpu",
                &cpu,
                "-drive",
                &format!("format=raw,file={},if=none,id=disk", display(image)),
                "-device",
                "virtio-blk-device,drive=disk",
            ]);
            if arch == Arch::AArch64 && !crate::window::sole_screen(args) {
                // A framebuffer for the panic screen. `virt` has no display
                // device, and firmware offers graphics output only when there
                // is one; ramfb is the simplest one edk2 drives, and it needs
                // no display attached. ARMv7-A boots through U-Boot, which
                // this has not been tried with. Left off a boot that is
                // watched, for the reason `q35`'s VGA is: it would be the
                // console the window opens on.
                let _ = command.args(["-device", "ramfb"]);
            }
        }
    }

    attach_rng(&mut command, arch);
    attach_display(&mut command, arch, args, &binary);
    attach_asked_for(&mut command, arch, args, &binary);
    attach_test_disk(&mut command, arch)?;
    attach_btrfs_disk(&mut command, arch)?;
    attach_install_disks(&mut command, arch, image, args);
    attach_root_disk(&mut command, arch, args)?;
    attach_data_image(&mut command, arch, args)?;
    if let Some(home) = &args.home_image {
        attach_home(&mut command, arch, home, "kept for the next boot");
    }
    let network = attach_network(&mut command, arch, args)?;

    attach_firmware(&mut command, arch, &firmware)?;

    Ok((command, network))
}

/// The firmware's flash or ROM: EDK2's code and a fresh variable store, or
/// U-Boot and its environment.
fn attach_firmware(command: &mut Command, arch: Arch, firmware: &Firmware) -> Result<()> {
    match firmware {
        Firmware::Pflash { code, vars } => {
            let vars = prepare_vars(arch, code, vars.as_deref())?;
            let _ = command.args([
                "-drive",
                &format!("if=pflash,format=raw,readonly=on,file={}", display(code)),
            ]);
            let _ = command.args([
                "-drive",
                &format!("if=pflash,format=raw,file={}", display(&vars)),
            ]);
        }
        // U-Boot runs from RAM and keeps no variables: there is no store to
        // prepare, and so none for a previous run to have poisoned. Its
        // environment is given it, rewritten every boot, with no autoboot
        // delay (`crate::uboot_env`).
        Firmware::Bios(uboot) => {
            let _ = command.args(["-bios", &display(uboot)]);
            match std::fs::read(uboot)
                .ok()
                .and_then(|bytes| crate::uboot_env::bank(&bytes))
            {
                Some(bank) => {
                    let target = paths::build_dir(arch).join("uboot-env.fd");
                    std::fs::create_dir_all(paths::build_dir(arch))?;
                    std::fs::write(&target, bank)?;
                    let _ = command.args([
                        "-drive",
                        &format!(
                            "if=pflash,unit=1,format=raw,readonly=on,file={}",
                            display(&target)
                        ),
                    ]);
                }
                None => println!(
                    "  {arch}: no default environment found in {}; U-Boot counts down to boot",
                    display(uboot)
                ),
            }
        }
    }
    Ok(())
}

/// A virtio device on PCI, on every machine, for stage 10's enumeration to
/// find: a 64-bit BAR to size, MSI-X and virtio's vendor capabilities to
/// walk. An entropy source because it needs no backend and nothing on the
/// guest side depends on it, so it changes what firmware and the kernel see
/// on the bus and nothing else.
///
/// `disable-legacy=on,iommu_platform=on` sends the device's DMA through the
/// machine's IOMMU, which QEMU otherwise lets virtio bypass, and which the
/// out-of-domain fault stage 10 exits on needs. Not on ARMv7-A: U-Boot
/// 2025.10's virtio-pci driver fails a heap assertion
/// (`do_check_inuse_chunk`) and resets when a device offers
/// `VIRTIO_F_ACCESS_PLATFORM`, with or without an SMMU, while the loader is
/// still running on its boot services.
///
/// On x86-64 it sits behind a PCIe root port, where libvirt puts every
/// device it is given and where a passed-through GPU wants to be
/// (`docs/NVIDIA.md` §2.3): the DMAR names a root port by a sub-hierarchy
/// scope, and stage 10's out-of-domain fault then proves that a function
/// below a bridge gets a translated domain. A second root port holds QEMU's
/// `edu` device, which has MSI and no MSI-X, for stage 10's MSI check
/// (`device::check::check_msi`), and the root bus a `pci-testdev` with an
/// 8 GiB BAR for `device_aperture`'s (`device::config_check`).
fn attach_rng(command: &mut Command, arch: Arch) {
    let rng = match arch {
        Arch::Armv7a => "virtio-rng-pci,disable-legacy=on",
        Arch::X86_64 => "virtio-rng-pci,disable-legacy=on,iommu_platform=on,bus=ferrix.port0",
        Arch::AArch64 => "virtio-rng-pci,disable-legacy=on,iommu_platform=on",
    };
    if arch == Arch::X86_64 {
        let _ = command.args([
            "-device",
            "pcie-root-port,id=ferrix.port0,chassis=1,slot=1",
            "-device",
            "pcie-root-port,id=ferrix.port1,chassis=2,slot=2",
            "-device",
            "edu,bus=ferrix.port1",
            // A 64-bit prefetchable BAR of 8 GiB, which firmware can only
            // place above 4 GiB: what `device_aperture` must report whole
            // (`docs/NVIDIA.md` §12.1, check W1).
            "-device",
            "pci-testdev,membar=8G",
        ]);
    }
    let _ = command.args(["-device", rng]);
}

/// The clipboard of `--clipboard` (`docs/CLIPBOARD.md` §3.1): a
/// `virtio-serial` device with the one port SPICE's agent protocol has always
/// used, and QEMU's own half of that protocol behind it.
///
/// `qemu-vdagent` is a character device that speaks the host end of vdagent
/// with no SPICE server anywhere: it is a peer of whatever clipboard QEMU's
/// UI has -- a window's, or a VNC client's through the RFB extended clipboard
/// -- so the guest reaches the clipboard of whoever is watching it, on
/// whichever machine that is. `clipboard=on` is what makes it that peer;
/// without it the chardev exists and carries nothing. A window's clipboard
/// is only a peer in a QEMU built with `gtk_clipboard`, which neither of this
/// host's is; so on a Wayland host a watched boot's port goes to a socket
/// instead, and xtask is the peer (`crate::clipboard::host`).
///
/// `mouse` is left off: the agent announces no mouse capability, QEMU sends
/// no mouse state to an agent that has not asked for it, and the machine
/// already has a virtio tablet for that.
///
/// ARMv7-A is left out for the reason every other PCI virtio device here
/// leaves it out: U-Boot 2025.10's virtio-pci driver fails a heap assertion
/// on the bus this would add a device to.
fn attach_clipboard(command: &mut Command, arch: Arch, args: &Args) {
    if !args.clipboard || arch == Arch::Armv7a {
        return;
    }
    // `test-clipboard`, or on a Wayland host the bridge a watched boot runs
    // (`crate::clipboard::host`), is the host itself, over a socket it
    // listens on.
    let chardev = match &args.clipboard_socket {
        Some(path) => format!("socket,id=vdagent,path={}", path.display()),
        None => "qemu-vdagent,id=vdagent,name=vdagent,clipboard=on,mouse=off".to_owned(),
    };
    let _ = command.args(["-chardev", &chardev]);
    let _ = command.args([
        "-device",
        "virtio-serial-pci,id=vdagent-bus,disable-legacy=on,iommu_platform=on",
    ]);
    // The port's name is how the guest finds it; its number is QEMU's to
    // choose, and the guest matches on the name (`docs/CLIPBOARD.md` §3.3).
    let _ = command.args([
        "-device",
        "virtserialport,bus=vdagent-bus.0,chardev=vdagent,name=com.redhat.spice.0",
    ]);
    println!("  {arch}: clipboard over virtio-serial, port com.redhat.spice.0");
}

/// The devices a flag asks for and nothing else brings: the clipboard's
/// virtio-serial port and the sound card.
fn attach_asked_for(command: &mut Command, arch: Arch, args: &Args, binary: &Path) {
    attach_clipboard(command, arch, args);
    attach_audio(command, arch, args, binary);
}

/// A virtio-snd card (`docs/AUDIO.md` §4), when `--audio` names a backend,
/// or when `--everything` asks for a desktop with all of it, which gets the
/// host's own sound server as this QEMU knows it
/// ([`crate::window::audio_backend`]).
///
/// `wav:PATH` writes what the guest plays to `PATH` with QEMU's mixing
/// engine off, so the file holds the stream's own frames, neither resampled
/// nor scaled; any other backend is passed to `-audiodev` as named, for a
/// person to hear. Through the IOMMU but on ARMv7-A, as every virtio device.
fn attach_audio(command: &mut Command, arch: Arch, args: &Args, binary: &Path) {
    let chosen = if args.everything && args.audio.is_none() {
        let found = crate::window::audio_backend(binary).map(str::to_owned);
        if found.is_none() {
            println!(
                "  {arch}: no sound: {} has none of pipewire, pa, coreaudio or dsound \
                 (`-audiodev help`); name one with --audio",
                binary.display()
            );
        }
        found
    } else {
        args.audio.clone()
    };
    let Some(backend) = &chosen else {
        return;
    };
    // A sound server's stream at the card's own rate, so QEMU's mixing
    // engine does not resample it, and with 100 ms of the server's buffer
    // rather than 46: a host busy enough to starve QEMU's main loop for
    // longer than that was heard as crackle.
    let audiodev = match backend.strip_prefix("wav:") {
        Some(path) => format!("wav,id=snd0,path={path},out.mixing-engine=off"),
        None if matches!(backend.as_str(), "pipewire" | "pa") => {
            format!("{backend},id=snd0,out.frequency=48000,out.latency=100000")
        }
        None => format!("{backend},id=snd0,out.frequency=48000"),
    };
    let flags = if arch == Arch::Armv7a {
        "disable-legacy=on"
    } else {
        "disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-audiodev", &audiodev]);
    let _ = command.args([
        "-device",
        &format!("virtio-sound-pci,audiodev=snd0,{flags}"),
    ]);
    println!("  {arch}: sound over virtio-snd, backend {backend}");
}

/// The display of iteration 1 (`docs/DISPLAY.md` §3), when `--display` or
/// `test-display` asks for it: a virtio-gpu device beside the firmware's own
/// head, so the panic screen keeps the head it has, through the IOMMU like
/// every other PCI virtio device except on ARMv7-A, where U-Boot resets when
/// a device offers `VIRTIO_F_ACCESS_PLATFORM` (see [`attach_rng`]); and QMP,
/// when a port was picked for it.
fn attach_display(command: &mut Command, arch: Arch, args: &Args, binary: &Path) {
    let flags = if arch == Arch::Armv7a {
        "disable-legacy=on"
    } else {
        "disable-legacy=on,iommu_platform=on"
    };
    if args.display {
        // One device a screen. QEMU gives each its own console, which is
        // what a screendump names and what makes the guest's second card a
        // second monitor; a second *output* of one device stays disabled
        // until a host window manager resizes it, which a headless test has
        // nothing to do.
        // Which card, and whether it turned out to be the 3D one: a display
        // backend without GL will not have it, and the answer decides both.
        let (gl_card, _) = crate::window::card(binary, args.gl);
        for index in 0..args.screens.max(1) {
            // A watched boot pins the cards high on the bus, for the reason
            // `WATCHED_CARD_SLOT` gives. A judged one says nothing and lets
            // QEMU assign, because the bus a test enumerates is the bus it
            // has always enumerated.
            let slot = if crate::window::sole_screen(args) {
                format!(",addr=0x{:x}", WATCHED_CARD_SLOT + index)
            } else {
                String::new()
            };
            // Venus keeps its rings and every host-visible allocation in blob
            // resources, which the host maps into a window of its own:
            // `hostmem` is that window's size, and a gigabyte is room for a
            // Vulkan program's allocations many times over.
            let venus = if args.venus && gl_card == crate::window::GL_CARD {
                ",venus=on,blob=on,hostmem=1G"
            } else {
                ""
            };
            // `ioeventfd=on`, which QEMU's virtio-gpu devices alone default
            // off: without it the driver's doorbell is an exit to QEMU that
            // handles the queue -- and on the 3D card runs the frame's GL --
            // before the guest's processor comes back, which billed a
            // compositor's frames to the driver as a fifth of a processor.
            // With it the doorbell is an eventfd, and QEMU takes the queue up
            // in its own loop.
            //
            // `edid=on`, QEMU's default for these devices, said rather than
            // assumed: the EDID `GET_EDID` answers with is the one place QEMU
            // tells the guest the refresh of the host monitor its window is
            // on (75 Hz where no window says, as under VNC), and the driver
            // reports it for the kernel to list the card's modes at.
            let _ = command.args([
                "-device",
                // 1024x768 is what every judged boot's pictures are of;
                // `run-compositor` says another.
                &format!(
                    "{card},id={id}{slot},{flags}{venus},ioeventfd=on,edid=on,\
                     xres={wide},yres={tall}",
                    card = gl_card,
                    id = crate::display::device_id(index),
                    wide = args.size.map_or(1024, |size| size.0),
                    tall = args.size.map_or(768, |size| size.1),
                ),
            ]);
        }
    }
    // A keyboard and a tablet, which is what QEMU's own HID devices are and
    // what `input-send-event` over QMP drives. The tablet rather than the
    // mouse: it reports absolute positions, so a monitor command names a
    // pixel rather than a movement, which is what a test can check.
    // `--display` brings them too: a screen with no keyboard and no pointer
    // is not a machine anybody drives, and the compositor `run --display`
    // starts wants a seat.
    if args.input || args.display {
        for kind in ["virtio-keyboard-pci", "virtio-tablet-pci"] {
            let _ = command.args(["-device", &format!("{kind},{flags}")]);
        }
    }
    // The head firmware draws on, created after the cards so that QEMU's
    // first console is a card and a window opens on the compositor. On
    // `q35` that is a VGA in place of the machine's own, which
    // `qemu_command` took away with `-vga none`; on `virt` it is the `ramfb`
    // the arch arm leaves to this.
    if crate::window::sole_screen(args) {
        match arch {
            Arch::X86_64 => {
                let _ = command.args(["-device", &format!("VGA,addr=0x{FIRMWARE_SLOT:x}")]);
            }
            Arch::AArch64 => {
                let _ = command.args(["-device", "ramfb"]);
            }
            Arch::Armv7a => {}
        }
    }
    if let Some(port) = args.qmp_port {
        let _ = command.args(["-qmp", &format!("tcp:127.0.0.1:{port},server=on,wait=off")]);
    }
}

/// Where a watched boot's first card goes on the bus, the next one a slot
/// along.
///
/// Two orders decide two different things, and a window needs them to
/// disagree. QEMU's consoles are in device *creation* order, and it shows the
/// first one; firmware picks its graphics output in PCI *address* order. So a
/// watched boot creates the cards first and puts them high on the bus, and
/// creates the head firmware should use last and puts it low: the window
/// opens on the compositor, and the loader's framebuffer is still not the
/// card the ring-3 driver takes over.
///
/// Both are pinned rather than one, because QEMU fills the low slots as it
/// realizes devices and this machine's count changes with `--net` and the
/// test disks: a slot that is free today is the network's tomorrow.
const WATCHED_CARD_SLOT: u32 = 0x10;

/// Where a watched boot's firmware head goes: below the cards, and clear of
/// the slots QEMU assigns from the bottom.
const FIRMWARE_SLOT: u32 = 0x0a;

/// The MAC address the guest's virtio-net device carries.
///
/// QEMU's own default for the first NIC, kept so that a guest driver, a DHCP
/// lease and a packet capture all name the guest the same way whether the
/// frames went through this gateway or through anything else.
pub(crate) const GUEST_MAC: &str = "52:54:00:12:34:56";

/// What `--net` leaves behind for the caller to hold: the gateway serving the
/// guest's wire.
type Network = Option<crate::gateway::Gateway>;

/// Give the guest a network device, or deliberately no network at all.
///
/// Without `--net` this is the argument it always was, and the comment is the
/// one that was on it, because the reason has not changed: QEMU adds a network
/// device by default, and on AArch64 firmware then finds a PCI option ROM built
/// for x86 and says so —
///
/// ```text
/// Image type X64 can't be loaded on AARCH64 UEFI system.
/// ```
///
/// — which is alarming, unrelated to us, and exactly the kind of noise that
/// trains people to skim the boot log.
///
/// With `--net` the device is a virtio-net on PCI whose backend is a UDP socket
/// on the loopback, with `xtask`'s own gateway on the other end of it;
/// `gateway` says why that rather than `-netdev user`. `-netdev` is enough on
/// its own to stop QEMU adding its default device, so `-net none` is not also
/// passed: QEMU warns about mixing the two families.
///
/// The same virtio flags as every other device this tool attaches, and the same
/// ARMv7-A exception: U-Boot 2025.10's virtio-pci driver fails a heap assertion
/// and resets when a device offers `VIRTIO_F_ACCESS_PLATFORM`.
fn attach_network(command: &mut Command, arch: Arch, args: &Args) -> Result<Network> {
    if !args.net {
        let _ = command.args(["-net", "none"]);
        return Ok(None);
    }
    let gateway = crate::gateway::Gateway::start(args.resolver, &args.forwards)?;
    println!(
        "  {arch}: network through xtask's gateway: guest {}, gateway {}, DNS {}",
        crate::gateway::GUEST_IP,
        crate::gateway::GATEWAY_IP,
        crate::gateway::DNS_IP
    );
    for forward in &args.forwards {
        println!(
            "  {arch}: 127.0.0.1:{} forwards to the guest's port {}",
            forward.host, forward.guest
        );
    }
    let _ = command.args([
        "-netdev",
        &format!(
            concat!(
                "dgram,id=net0,local.type=inet,local.host=127.0.0.1,local.port=0,",
                "remote.type=inet,remote.host={},remote.port={}"
            ),
            gateway.address().ip(),
            gateway.address().port()
        ),
    ]);
    let flags = if arch == Arch::Armv7a {
        "disable-legacy=on"
    } else {
        "disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args([
        "-device",
        &format!("virtio-net-pci,netdev=net0,mac={GUEST_MAC},{flags}"),
    ]);
    Ok(Some(gateway))
}

/// Say what the gateway saw, once the guest has stopped talking to it.
///
/// A network test that fails says the guest never got an address, or never
/// resolved a name. Which of those is a driver that never transmitted and which
/// is a gateway that dropped what it was given is not visible from the guest's
/// side at all, and is exactly what these counters answer.
fn report_network(network: &Network) {
    if let Some(gateway) = network {
        println!("  gateway: {}", gateway.counters().report());
        if gateway.counters().frames_in() == 0 {
            println!("  gateway: the guest never transmitted a frame");
        }
    }
}

/// Attach the test disk as a second virtio device on PCI: a block device, for
/// stage 10's ring-3 driver to read sectors from.
///
/// `test_disk` says what each sector holds, and writes the image on demand, so
/// nothing has to run before QEMU does; the layout goes to this command's
/// output, since the serial log holds only what the guest printed. Read-only,
/// because a driver test that could write would change what the next one reads.
///
/// Added after the entropy device, so that one keeps its slot, and with the
/// same flags for the same reasons: DMA through the IOMMU, except on ARMv7-A,
/// where U-Boot resets when a device offers `VIRTIO_F_ACCESS_PLATFORM`. No
/// `bootindex`: the first sector holds no partition table and no filesystem,
/// so firmware that looks at the disk finds nothing to boot and goes on to the
/// image.
fn attach_test_disk(command: &mut Command, arch: Arch) -> Result<()> {
    let disk = test_disk::ensure()?;
    println!(
        "  {arch}: test disk {} as virtio-blk-pci: {}",
        display(&disk),
        test_disk::describe()
    );
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=testdisk,readonly=on",
            display(&disk)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=testdisk,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=testdisk,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
    Ok(())
}

/// Attach the btrfs fixture as a third virtio device on PCI, after the
/// pattern disk so that it is `vdb`: the disk stage 11's exit mounts. The
/// same flags as the pattern disk, for the same reasons, and read-only, since
/// the mount is.
fn attach_btrfs_disk(command: &mut Command, arch: Arch) -> Result<()> {
    let disk = btrfs_disk::ensure()?;
    println!(
        "  {arch}: btrfs disk {} as virtio-blk-pci: {}",
        display(&disk),
        btrfs_disk::describe()
    );
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=btrfsdisk,readonly=on",
            display(&disk)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=btrfsdisk,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=btrfsdisk,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
    attach_btrfs_write_disk(command, arch)
}

/// Attach the btrfs compiler volume for `test-rustc`, `test-selfhost` or a
/// default boot. It carries no root label, so the kernel mounts it at
/// `/data`. Under `snapshot=on` unless [`Args::data_image_kept`]: what the
/// guest writes goes to a file QEMU throws away, so the next run reads the
/// volume as this one did. Under `--persistent`, `run` and `run-compositor`
/// attach the machine's own copy of the volume instead, and keep what is
/// written to it (`crate::persistent`).
fn attach_data_image(command: &mut Command, arch: Arch, args: &Args) -> Result<()> {
    let Some(volume) = &args.data_image else {
        return Ok(());
    };
    let persistent =
        args.persistent && matches!(args.command.as_deref(), Some("run" | "run-compositor"));
    let copy;
    let volume = if persistent {
        copy = crate::persistent::data_volume(volume, args.reset_flash)?;
        &copy
    } else {
        volume
    };
    let (snapshot, said) = if args.data_image_kept || persistent {
        ("", "kept")
    } else {
        (",snapshot=on", "snapshot")
    };
    println!(
        "  {arch}: btrfs volume {} at /data, {said}",
        display(volume)
    );
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=btrfsdata{snapshot}",
            display(volume)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=btrfsdata,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=btrfsdata,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
    Ok(())
}

/// `test-install`'s disks, after the three every boot has: the live disk and
/// the target as `vdd` and `vde`, or the installed disk alone as `vdd`,
/// booted from (`tools/common/xtask/src/installer.rs`).
fn attach_install_disks(command: &mut Command, arch: Arch, image: &Path, args: &Args) {
    let virtio = |id: &str, boot: bool| {
        let boot = if boot { ",bootindex=0" } else { "" };
        format!("virtio-blk-pci,drive={id},disable-legacy=on,iommu_platform=on{boot}")
    };
    if args.boot_virtio && arch == Arch::X86_64 {
        println!("  {arch}: {} as a virtio disk, booted from", display(image));
        let _ = command.args([
            "-drive",
            &format!("file={},if=none,format=raw,id=installed", display(image)),
            "-device",
            &virtio("installed", true),
        ]);
    }
    if let Some((live, target)) = &args.install_disks {
        println!(
            "  {arch}: live disk {} and target {} as vdd and vde",
            display(live),
            display(target)
        );
        let _ = command.args([
            "-drive",
            &format!(
                "file={},if=none,format=raw,id=live,readonly=on",
                display(live)
            ),
            "-device",
            &virtio("live", false),
            "-drive",
            &format!("file={},if=none,format=raw,id=target", display(target)),
            "-device",
            &virtio("target", false),
        ]);
    }
}

/// Attach the root disk as the fifth virtio device, so it is `vdd` and the
/// kernel puts `/` on it — for the commands someone sits at, `run` and
/// `run-compositor`, unless `--tmpfs-root` says otherwise; and, made fresh
/// each time so the boot depends on nothing an earlier one left, for
/// `test-init`, whose pid 1 the switch moves onto it (`docs/INIT.md` §7.3),
/// `test-clipboard`, whose driver starts before that switch and whose agent
/// after it, and `test-chrome-window` and `test-restart` under
/// `--btrfs-root`.
fn attach_root_disk(command: &mut Command, arch: Arch, args: &Args) -> Result<()> {
    // A test's root is made fresh for it, so its boot still depends on
    // nothing an earlier one left.
    let test = (args.btrfs_root
        && matches!(
            args.command.as_deref(),
            Some("test-chrome-window" | "test-restart")
        ))
        || matches!(
            args.command.as_deref(),
            Some("test-init" | "test-clipboard")
        );
    if !test && !matches!(args.command.as_deref(), Some("run" | "run-compositor")) {
        return Ok(());
    }
    if args.tmpfs_root {
        println!("  {arch}: / in memory, as --tmpfs-root asks; the btrfs root is left off");
        return Ok(());
    }
    let (disk, made) = if test {
        (btrfs_disk::ensure_test_root(arch)?, true)
    } else {
        btrfs_disk::ensure_root(args.reset_root || args.reset_flash)?
    };
    println!(
        "  {arch}: btrfs root {}, {}",
        display(&disk),
        if made {
            "made empty; the kernel installs the system on it at this boot"
        } else {
            "as the last boot left it (--reset-root starts it over, --tmpfs-root leaves it off)"
        }
    );
    let home = if test {
        None
    } else {
        Some(btrfs_disk::ensure_home(args.reset_flash)?)
    };
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=btrfsroot,cache=writeback",
            display(&disk)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=btrfsroot,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=btrfsroot,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
    // The users' files, beside the root and kept when it starts over: the
    // kernel mounts the disk at `/home` by its label.
    if let Some((home, made)) = home {
        let said = if made {
            "made empty; init makes each account's home on it"
        } else {
            "as the last boot left it (--reset-flash starts it over)"
        };
        attach_home(command, arch, &home, said);
    }
    Ok(())
}

/// Attach `home`, a btrfs volume labelled `ferrix-home`, written through:
/// the kernel mounts it at `/home` by its label, wherever it is on the bus.
fn attach_home(command: &mut Command, arch: Arch, home: &Path, said: &str) {
    println!("  {arch}: btrfs home {}, {said}", display(home));
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=btrfshome,cache=writeback",
            display(home)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=btrfshome,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=btrfshome,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
}

/// Attach a fresh blank btrfs volume as a fourth virtio device, so it is
/// `vdc`: the disk stage 12's check writes on and host `btrfs check` reads
/// afterwards.
///
/// Writable, of course, and rewritten from the fixture for every boot, so no
/// run ever starts from what the last one left. `cache=writeback` is QEMU's
/// default and is what makes the guest's flush mean something: the guest's
/// flush becomes a host `fsync`, which is the ordering the commit rests on.
fn attach_btrfs_write_disk(command: &mut Command, arch: Arch) -> Result<()> {
    let disk = btrfs_disk::ensure_blank(arch)?;
    println!(
        "  {arch}: writable btrfs disk {} as virtio-blk-pci: {}",
        display(&disk),
        btrfs_disk::describe_blank()
    );
    let _ = command.args([
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=btrfswrite,cache=writeback",
            display(&disk)
        ),
    ]);
    let device = if arch == Arch::Armv7a {
        "virtio-blk-pci,drive=btrfswrite,disable-legacy=on"
    } else {
        "virtio-blk-pci,drive=btrfswrite,disable-legacy=on,iommu_platform=on"
    };
    let _ = command.args(["-device", device]);
    Ok(())
}

/// The hardware accelerator this host's QEMU would use, if it has one.
///
/// Named per platform rather than probed, because the name is the only thing
/// that varies: each of these is the one interface its operating system
/// exposes for running guest instructions on the processor directly.
const fn host_accelerator() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        // The Windows Hypervisor Platform. Present on Windows 10 and 11, but
        // only once the optional feature is turned on, which is why `auto`
        // asks QEMU rather than assuming.
        Some("whpx")
    } else if cfg!(target_os = "linux") {
        Some("kvm")
    } else if cfg!(target_os = "macos") {
        Some("hvf")
    } else {
        None
    }
}

/// Decide which accelerator to boot under.
///
/// # Why this is a choice and not a default
///
/// `tcg` emulates the processor, including its `MMU`, and an emulated `MMU`
/// has no `TLB` to speak of: it resolves every access through the page tables
/// as it finds them. That makes it wonderfully reproducible and it makes it
/// blind to an entire class of bug, because a stale translation cannot be
/// stale in a cache that does not exist. A hardware accelerator runs on the
/// real `MMU`, with the real `TLB`, and sees them.
///
/// **This is not an abstract preference.** `arch::flush_tlb` on x86-64 spared
/// global entries — which is nearly every mapping the kernel makes — and every
/// boot test passed anyway for as long as every boot test ran under `tcg`. The
/// first run under `whpx` failed three different self-checks. So the default
/// stays `tcg`, because reproducibility is what a boot test is for and `CI`
/// has no hypervisor to offer; `auto` is for the machine in front of you,
/// which usually does.
///
/// # Except an x86-64 guest on an x86-64 Linux host
///
/// There, with nothing asked for, the default is KVM ([`default_accelerator`]):
/// the long guests -- the compositor's boots, a browser, a game's store --
/// spend most of their time emulated otherwise, and KVM sees the real `TLB`
/// that `tcg` cannot. `tcg` stays the default in CI, which keeps its emulated
/// boots, and for a coverage run, whose plugin sees only translated blocks
/// (`docs/TEST-TIME.md`, C3).
pub(crate) fn accelerator(arch: Arch, binary: &Path, requested: Option<&str>) -> Result<String> {
    let Some(requested) = requested else {
        return Ok(default_accelerator(arch, binary).to_owned());
    };
    if requested == "tcg" {
        return Ok("tcg".to_owned());
    }

    let available = available_accelerators(binary);
    let supported = |name: &str| available.iter().any(|found| found == name);

    if requested != "auto" {
        // Asked for by name: refuse rather than quietly emulating. Somebody
        // who typed `--accel kvm` wants to know it did not happen.
        if !supported(requested) {
            return Err(Error::new(format!(
                "this QEMU has no `{requested}` accelerator; it offers {}",
                available.join(", ")
            )));
        }
        return Ok(requested.to_owned());
    }

    // `auto`, which never fails: it falls back to emulation, since the whole
    // point is that it works on whatever machine it is run on. A guest of a
    // different architecture than the host has nothing to accelerate — there
    // are no Arm instructions for an x86 processor to run directly.
    if arch != Arch::host() {
        return Ok("tcg".to_owned());
    }
    match host_accelerator() {
        Some(name) if supported(name) && usable(arch, binary, name) => Ok(name.to_owned()),
        _ => Ok("tcg".to_owned()),
    }
}

/// What a boot runs under when `--accel` is not given: `kvm` for an x86-64
/// guest on an x86-64 Linux host whose QEMU has it and whose `/dev/kvm` this
/// user may open, outside CI (`$CI` set) and outside a coverage run
/// (`FERRIX_QEMU_PLUGIN` set); `tcg` everywhere else.
///
/// GitHub's Linux runners have a `/dev/kvm` these days, which is why CI is
/// asked about by name rather than left to the device: its boots stay
/// emulated, so a bug only `tcg` shows still has a gate. The device is opened
/// rather than probed with a halted QEMU, because that probe costs half a
/// second and this is asked once a boot.
fn default_accelerator(arch: Arch, binary: &Path) -> &'static str {
    let kvm = cfg!(all(target_os = "linux", target_arch = "x86_64"))
        && arch == Arch::X86_64
        && std::env::var_os("CI").is_none()
        && std::env::var_os("FERRIX_QEMU_PLUGIN").is_none()
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
        && available_accelerators(binary)
            .iter()
            .any(|name| name == "kvm");
    if kvm {
        println!(
            "  qemu: kvm, the default for an x86-64 guest on this host; --accel tcg emulates \
             (docs/TEST-TIME.md, C3)"
        );
        "kvm"
    } else {
        "tcg"
    }
}

/// How many processors to give the guest under `accelerator`.
///
/// # One under WHPX
///
/// QEMU 11.1's Windows Hypervisor Platform backend emulates a guest's
/// memory-mapped I/O with QEMU's own x86 emulator, which walks the guest's
/// page tables itself (`target/i386/emulate/x86_mmu.c`). With more than one
/// processor that walk intermittently answers "not mapped" for a mapping that
/// is there, and the fault it raises reaches the guest as error code 12 at
/// address 0: `/sbin/blk` dies reading its virtio registers, and stage 10's
/// driver check panics. Ferrix's own page tables and `CR3` were checked at the
/// fault and are right; `docs/BACKLOG.md` has the analysis. At one processor
/// it has not happened, and WHPX is still the fast path for the display, so
/// that is the default under WHPX. A count given with `--smp` is kept, with a
/// warning, for whoever is looking into it.
pub(crate) fn processors(accelerator: &str, args: &Args) -> u32 {
    if accelerator != "whpx" {
        return args.smp;
    }
    if !args.smp_given {
        println!(
            "  qemu: one processor under whpx: QEMU 11.1's WHPX MMIO emulation faults ring-3 \
             drivers with more than one (docs/BACKLOG.md); --smp N overrides"
        );
        return 1;
    }
    if args.smp > 1 {
        println!(
            "  qemu: warning: --smp {} under whpx: QEMU 11.1's WHPX MMIO emulation faults ring-3 \
             drivers with more than one processor, and /sbin/blk may die (docs/BACKLOG.md)",
            args.smp
        );
    }
    args.smp
}

/// Whether this QEMU can actually *initialise* `name` on this machine.
///
/// Being built with an accelerator and being allowed to use it are different
/// questions, and only the first is answerable from a list. `/dev/kvm` is
/// `root:kvm` on most distributions, so a developer who has never been added
/// to that group has a QEMU that offers `kvm` and cannot open it; the
/// Windows Hypervisor Platform is an optional feature that may be off. In
/// both cases QEMU exits immediately with an error, which for `auto` — which
/// promises to work on whatever machine it is run on — must mean "fall back to
/// emulation", not "fail the boot test".
///
/// There is no way to ask the question without answering it: QEMU initialises
/// an accelerator only when it starts a machine. So this starts one with no
/// devices and its CPU halted, and watches. Failure is prompt and is an exit;
/// success is QEMU sitting there waiting, which is what the deadline is for.
/// The asymmetry is the signal.
///
/// The machine is the one a boot of `arch` uses, not `-M none`: the Windows
/// QEMU xtask boots with (`docs/GPU.md` §3.12) aborts under WHPX on a
/// machine that is not an x86 one -- "`X86_MACHINE`: Object ... is not an
/// instance of type `x86-machine`" -- well inside the deadline, and `auto` then
/// chose `tcg`: `run-compositor --everything` ran emulated on four processors
/// with the HPET for a clock, and Chrome took half a minute to answer a click.
fn usable(arch: Arch, binary: &Path, name: &str) -> bool {
    let machine = if arch == Arch::X86_64 { "q35" } else { "none" };
    let Ok(mut child) = Command::new(binary)
        .args(["-accel", name])
        .args([
            "-M",
            machine,
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "none",
            "-nodefaults",
            "-no-user-config",
            // Halted, so a successful probe runs no guest instructions.
            "-S",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match child.try_wait() {
            // Exited on its own inside the window: the accelerator did not
            // come up. QEMU has no other reason to leave this quickly.
            Ok(Some(_)) => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }

    // Still running, so the accelerator initialised. Nothing to wait for.
    let _ = child.kill();
    let _ = child.wait();
    true
}

/// The accelerators this QEMU binary was built with.
///
/// Asked of the binary rather than assumed, because whether one is *usable* is
/// a property of the machine — Hyper-V switched on, `/dev/kvm` readable — and
/// a list QEMU itself prints is the closest thing to an answer that does not
/// involve starting a guest. An empty list on error, which sends `auto` to
/// `tcg` and gives a named request the error it deserves.
fn available_accelerators(binary: &Path) -> Vec<String> {
    let Ok(output) = Command::new(binary).args(["-accel", "help"]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        // The first line is a heading, and every other line is one name.
        .filter(|line| !line.is_empty() && !line.contains(' '))
        .map(str::to_owned)
        .collect()
}

/// A path as QEMU wants it: forward slashes, even on Windows, because a
/// backslash inside a `-drive` argument is taken as an escape.
fn display(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Produce the writable UEFI variable store QEMU needs beside the firmware.
///
/// It has to be writable and it has to survive between runs, so it is copied
/// into `build/` rather than used from the read-only system location. On the
/// Arm `virt` machine both pflash images additionally have to be exactly the
/// same size, which is why this pads.
fn prepare_vars(arch: Arch, code: &Path, template: Option<&Path>) -> Result<PathBuf> {
    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let target = directory.join("uefi-vars.fd");

    // **Always rewritten, never reused.** UEFI variables persist across boots by
    // design -- that is what they are for -- and firmware records its boot
    // options in them. A store carried over from a previous run can therefore
    // hold entries describing an image that is no longer there, and the symptom
    // is firmware skipping our disk entirely and dropping to the EFI shell,
    // with nothing in the log to say why.
    //
    // That is not hypothetical: it happened here, after a run that deliberately
    // booted the wrong architecture's image to isolate a firmware message. A
    // boot test whose result depends on what the previous boot test left behind
    // is not a test, so this starts from a known state every time.

    let mut contents = match template {
        Some(source) => std::fs::read(source)
            .map_err(|error| Error::new(format!("reading {}: {error}", source.display())))?,
        // No template: an all-zero store is not a valid variable store, and
        // EDK2 responds by formatting one, which is what we want anyway.
        None => Vec::new(),
    };

    if arch != Arch::X86_64 {
        let code_size = std::fs::metadata(code)?.len() as usize;
        if contents.is_empty() && arch == Arch::AArch64 {
            contents = crate::uefi_vars::formatted(code_size);
        }
        contents.resize(code_size, 0);
    }
    // Without the boot menu's five seconds (`crate::uefi_vars`).
    if arch == Arch::AArch64 && !crate::uefi_vars::without_boot_menu_wait(&mut contents) {
        println!(
            "  {arch}: the UEFI variable store is not one xtask knows; firmware waits at its menu"
        );
    }

    std::fs::write(&target, contents)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::{
        Arch, REMAP_LINES, SUCCESS_MARKER, UNCHECKED_MARKER, blocks_compatibility_format,
        cleaning_problem, config_problem, devmgr_problem, entropy_problem, fault_problem,
        iommu_problem, msi_problem, namespace_problem, parse_qemu_version, queue_problem,
        remap_problem, send_lines, xstate_problem,
    };

    /// A byte that is not UTF-8 is shown, not the end of the guest's
    /// output: the lines after it still arrive (`batch-20261005T114127Z-b0-3`).
    #[test]
    fn a_line_that_is_not_utf8_does_not_end_the_watch() {
        let (sender, receiver) = std::sync::mpsc::channel();
        send_lines(
            &b"before\r\nbad \xff\xfe here\nafter\nlast, no newline"[..],
            &sender,
        );
        drop(sender);
        let lines: Vec<String> = receiver.iter().collect();
        assert_eq!(
            lines,
            [
                "before",
                "bad \u{fffd}\u{fffd} here",
                "after",
                "last, no newline"
            ]
        );
    }

    /// Only the build `fetch-qemu-linux.sh` makes is taken for x86-64.
    #[test]
    fn only_the_patched_qemu_blocks_compatibility_format() {
        assert!(blocks_compatibility_format(
            "QEMU emulator version 10.2.1 (ferrix-cfi)\nCopyright (c) 2003-2025\n"
        ));
        assert!(!blocks_compatibility_format(
            "QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)\n"
        ));
        assert!(!blocks_compatibility_format(
            "QEMU emulator version 9.2.4 (v9.2.4-dirty)\n"
        ));
        assert!(!blocks_compatibility_format(""));
    }

    /// A secret goes in any case and every time, and nothing else does.
    #[test]
    fn a_redacted_line_shows_no_secret() {
        let secrets = vec!["Tester".to_owned(), "p4ss!".to_owned(), String::new()];
        assert_eq!(
            super::redacted("logged in as tester (TESTER), pw p4ss!", &secrets),
            "logged in as <redacted> (<redacted>), pw <redacted>"
        );
        assert_eq!(super::redacted("nothing here", &secrets), "nothing here");
        assert_eq!(super::redacted("", &secrets), "");
    }

    /// What QEMU prints, from the two versions CI and this host have.
    #[test]
    fn the_qemu_version_is_read_from_its_banner() {
        assert_eq!(
            parse_qemu_version("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.18)\n"),
            Some((8, 2))
        );
        assert_eq!(
            parse_qemu_version("QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)\n"),
            Some((10, 2))
        );
        assert!(parse_qemu_version("QEMU emulator version 8.2.2").is_some_and(|v| v < (9, 1)));
        assert_eq!(parse_qemu_version("not qemu"), None);
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_devmgr_line_must_say_one_started_and_none_failed() {
        let good = lines(&["  devmgr   8 devices, 1 drivers, 2 started, 0 failed"]);
        assert_eq!(devmgr_problem(&good), None, "two started");
        let none = lines(&["  devmgr   8 devices, 1 drivers, 0 started, 0 failed"]);
        assert!(devmgr_problem(&none).is_some(), "nothing started");
        let failed = lines(&["  devmgr   8 devices, 1 drivers, 1 started, 1 failed"]);
        assert!(devmgr_problem(&failed).is_some(), "one failed");
        let absent = lines(&["  devmgr   not started: the image carries no /sbin/devmgr"]);
        assert_eq!(devmgr_problem(&absent), None, "an image without devmgr");
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(devmgr_problem(&silent).is_some(), "no line at all");
    }

    #[test]
    fn an_x86_64_boot_on_the_own_model_must_show_the_xsave_header_survive() {
        let survived = "  xstate   a signal frame's XSAVE header, every bit set, came back through \
                        rt_sigreturn masked to XCR0, without a fault";
        let quiet = "  xstate   no XSAVE area in signal frames on this processor: FXSAVE alone";
        let x86 = Arch::X86_64;
        assert_eq!(xstate_problem(x86, true, &lines(&[survived])), None);
        assert!(
            xstate_problem(x86, true, &lines(&[quiet])).is_some(),
            "the quiet variant"
        );
        assert!(
            xstate_problem(x86, true, &lines(&["FERRIX-BOOT-OK stages 1-12"])).is_some(),
            "no line at all"
        );
        assert_eq!(
            xstate_problem(x86, false, &lines(&[quiet])),
            None,
            "a model FERRIX_X86_CPU named"
        );
        assert_eq!(
            xstate_problem(Arch::AArch64, true, &lines(&["FERRIX-BOOT-OK"])),
            None,
            "another architecture"
        );
    }

    #[test]
    fn a_boot_that_wrote_the_stage_12_disk_must_show_the_detach_reach_it() {
        let wrote = "  btrfs-rw vdc written and remounted: 8 files";
        let seen = "  mntns    55 calls ...; a btrfs write inside a detached subtree on the disk \
                    after the detach";
        let unseen = "  mntns    55 calls ...; no stage 12 disk for the detach's write-out";
        assert_eq!(namespace_problem(&lines(&[wrote, seen])), None);
        assert!(
            namespace_problem(&lines(&[wrote, unseen])).is_some(),
            "the quiet variant"
        );
        assert!(
            namespace_problem(&lines(&[wrote])).is_some(),
            "no line at all"
        );
        let no_disk = "  btrfs-rw not checked: no third disk is served";
        assert_eq!(
            namespace_problem(&lines(&[no_disk, unseen])),
            None,
            "no third disk"
        );
    }

    #[test]
    fn a_boot_that_read_entropy_passes_however_little_it_read() {
        for count in ["64", "1"] {
            let boot = lines(&[
                &format!(
                    "  pci      2 functions, 1 virtio transports, {count} entropy bytes read by DMA, 1 completions by MSI-X"
                ),
                "FERRIX-BOOT-OK stages 1-12",
            ]);
            assert_eq!(entropy_problem(&boot), None, "{count} bytes");
        }
    }

    #[test]
    fn a_boot_that_read_none_or_never_said_fails() {
        let none = lines(&["  pci      1 virtio transports, 0 entropy bytes read by DMA"]);
        assert!(entropy_problem(&none).is_some(), "zero bytes");
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(entropy_problem(&silent).is_some(), "no line at all");
    }

    #[test]
    fn a_boot_whose_completion_was_polled_fails() {
        let polled = lines(&["  pci      64 entropy bytes read by DMA, 0 completions by MSI-X"]);
        assert!(entropy_problem(&polled).is_some(), "no MSI-X");
        let old = lines(&["  pci      64 entropy bytes read by DMA"]);
        assert!(
            entropy_problem(&old).is_some(),
            "a kernel that does not say"
        );
    }

    #[test]
    fn an_x86_64_boot_without_its_table_writes_cleaned_fails() {
        let cleaned = lines(&[
            "  iommu    26 entry writes and 10 fresh tables noted and cleaned to memory on 1 VT-d units that do not snoop; 12 changes each found them cleaned before its own publish point",
        ]);
        assert_eq!(cleaning_problem(Arch::X86_64, &cleaned), None);
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(cleaning_problem(Arch::X86_64, &silent).is_some());
        assert_eq!(
            cleaning_problem(Arch::AArch64, &silent),
            None,
            "an SMMU snoops"
        );
    }

    #[test]
    fn an_x86_64_boot_without_interrupt_remapping_whole_fails() {
        let mut whole: Vec<String> = REMAP_LINES
            .iter()
            .map(|(text, _)| format!("    6.0 |   remap    {text}"))
            .collect();
        for reason in [
            "0x25 (compatibility format blocked)",
            "0x25 (compatibility format blocked)",
            "0x26 (source ID check)",
        ] {
            whole.push(format!(
                "  iommu    stream 0x200 is made to send an interrupt its unit refuses with reason {reason}, on purpose"
            ));
        }
        assert_eq!(remap_problem(Arch::X86_64, &whole), None);
        assert_eq!(remap_problem(Arch::AArch64, &[]), None, "no VT-d");
        for missing in 0..whole.len() {
            let mut cut = whole.clone();
            let _ = cut.remove(missing);
            assert!(
                remap_problem(Arch::X86_64, &cut).is_some(),
                "line {missing} missing"
            );
        }
        let mut stray = whole.clone();
        stray.push(
            "  iommu    stream 0xff00 is made to send an interrupt its unit refuses with reason 0x22 (entry not present), on purpose"
                .to_owned(),
        );
        assert!(
            remap_problem(Arch::X86_64, &stray).is_some(),
            "another reason"
        );
        let mut off = whole;
        off.push("  iommu    vt-d unit 0xfed90000: remapping not enabled: x".to_owned());
        assert!(remap_problem(Arch::X86_64, &off).is_some());
    }

    #[test]
    fn an_x86_64_boot_without_its_invalidations_queued_fails() {
        let queued = lines(&[
            "  iommu    4 context-cache and 16 IOTLB invalidations queued and each waited for, none by register on 1 VT-d units; 1 failed as check R7 made it, its 2 pages kept; a queue left on as firmware leaves it turned off on 1",
        ]);
        assert_eq!(queue_problem(Arch::X86_64, &queued), None);
        let none_failed = lines(&[
            "  iommu    4 context-cache and 16 IOTLB invalidations queued and each waited for, none by register on 1 VT-d units; 0 failed as check R7 made it, its 0 pages kept; a queue left on as firmware leaves it turned off on 1",
        ]);
        assert!(queue_problem(Arch::X86_64, &none_failed).is_some());
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(queue_problem(Arch::X86_64, &silent).is_some());
        assert_eq!(queue_problem(Arch::AArch64, &silent), None, "no VT-d");
    }

    #[test]
    fn an_x86_64_boot_without_both_msi_deliveries_fails() {
        let devices = |msi: &str| {
            lines(&[&format!(
                "  devices  12 nodes (0 from the device tree), 6 MSI-X tables (1 vectors minted), {msi}, 59 refusals as specified; 12 published"
            )])
        };
        let whole = devices("2 MSI capabilities (1 vectors minted, 2 deliveries)");
        assert_eq!(msi_problem(Arch::X86_64, &whole), None);
        for unexercised in [
            "1 MSI capabilities (0 vectors minted, 0 deliveries)",
            "2 MSI capabilities (1 vectors minted, 1 deliveries)",
        ] {
            assert!(
                msi_problem(Arch::X86_64, &devices(unexercised)).is_some(),
                "{unexercised}"
            );
        }
        assert!(msi_problem(Arch::X86_64, &lines(&["FERRIX-BOOT-OK"])).is_some());
        let arm = devices("0 MSI capabilities (0 vectors minted, 0 deliveries)");
        assert_eq!(msi_problem(Arch::AArch64, &arm), None, "no edu on Arm");
    }

    #[test]
    fn a_boot_without_its_configuration_window_checks_fails() {
        let config = |above: &str| {
            lines(&[
                &format!(
                    "  config   23 apertures reported whole by device_aperture, {above} above 4 GiB and longer than it; 120 reads of 14 functions answered as enumerated"
                ),
                "  config   a COMMAND rewritten behind the kernel was found and its node refused, then restored",
            ])
        };
        assert_eq!(config_problem(Arch::X86_64, &config("1")), None);
        assert!(
            config_problem(Arch::X86_64, &config("0")).is_some(),
            "W1 unexercised"
        );
        assert_eq!(
            config_problem(Arch::AArch64, &config("0")),
            None,
            "no testdev on Arm"
        );
        assert!(config_problem(Arch::AArch64, &lines(&["FERRIX-BOOT-OK"])).is_some());
        let unbreached = lines(&[
            "  config   4 apertures reported whole by device_aperture, 0 above 4 GiB and longer than it; 9 reads of 2 functions answered as enumerated",
        ]);
        assert!(
            config_problem(Arch::Armv7a, &unbreached).is_some(),
            "W7 not run"
        );
    }

    #[test]
    fn a_boot_that_placed_functions_behind_an_iommu_passes() {
        let boot = lines(&[
            "  iommu    1 VT-d units, 0 SMMUv3s; 6 PCI functions behind one, 0 bypassing, 0 unresolved",
            "  iommu    pci 0000:00:02.0 behind the VtD unit at 0xfed90000 as stream 0x10",
        ]);
        assert_eq!(iommu_problem(&boot), None);
    }

    #[test]
    fn a_boot_with_nothing_behind_an_iommu_or_anything_unresolved_fails() {
        let nothing = lines(&[
            "  iommu    0 VT-d units, 0 SMMUv3s; 0 PCI functions behind one, 2 bypassing, 0 unresolved",
        ]);
        assert!(iommu_problem(&nothing).is_some(), "nothing placed");
        let unresolved = lines(&[
            "  iommu    0 VT-d units, 1 SMMUv3s; 1 PCI functions behind one, 0 bypassing, 1 unresolved",
        ]);
        assert!(iommu_problem(&unresolved).is_some(), "one unresolved");
        let bypassing = lines(&[
            "  iommu    1 VT-d units, 0 SMMUv3s; 10 PCI functions behind one, 2 bypassing, 0 unresolved",
        ]);
        assert!(iommu_problem(&bypassing).is_some(), "two bypassing");
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(iommu_problem(&silent).is_some(), "no line at all");
    }

    #[test]
    fn a_translating_machine_must_show_its_out_of_domain_fault() {
        let faulted = lines(&[
            "  pci      2 functions, 64 entropy bytes read by DMA, 1 completions by MSI-X, 1 out-of-domain writes faulted",
        ]);
        assert_eq!(fault_problem(Arch::X86_64, &faulted), None);
        let none = lines(&[
            "  pci      2 functions, 64 entropy bytes read by DMA, 1 completions by MSI-X, 0 out-of-domain writes faulted",
        ]);
        assert!(
            fault_problem(Arch::AArch64, &none).is_some(),
            "nothing faulted"
        );
        let silent = lines(&["FERRIX-BOOT-OK stages 1-12"]);
        assert!(
            fault_problem(Arch::X86_64, &silent).is_some(),
            "no line at all"
        );
        assert_eq!(
            fault_problem(Arch::Armv7a, &none),
            None,
            "ARMv7-A is not asked"
        );
        let tree = lines(&[
            "  pci      5 functions from 1 device-tree hosts (0 descriptions refused), 1 completions by MSI-X, 0 out-of-domain writes faulted",
        ]);
        assert_eq!(
            fault_problem(Arch::AArch64, &tree),
            None,
            "nor is AArch64's device-tree path"
        );
    }

    #[test]
    fn the_markers_are_the_ones_the_kernel_prints() {
        let path = crate::paths::workspace_root().join("src/kernel/src/main.rs");
        let kernel = std::fs::read_to_string(&path).expect("reading the kernel's main.rs");
        for marker in [SUCCESS_MARKER, UNCHECKED_MARKER] {
            assert!(
                kernel.contains(&format!("\"{marker}\"")),
                "{} does not print `{marker}`",
                path.display()
            );
        }
        // Every reader of the success marker matches a substring: a skipped
        // boot's marker must not contain it, or it would pass for one.
        assert!(!UNCHECKED_MARKER.contains(SUCCESS_MARKER));
    }
}
