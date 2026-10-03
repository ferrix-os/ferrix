//! `test-nvrm`: devmgr's `Gpu` kind handing a device to `nvrm`'s skeleton,
//! and refusing to (`docs/NVIDIA.md` §4.1, §12.2, §12.3; NVIDIA's N1b).
//!
//! `nvrm` is `src/user/system/linux/drivers/nvrm/`: a static program built
//! against ferrousli with its Makefile, x86-64 only, with an entry that
//! takes a native start (`src/start.c`). It goes in the image's driver
//! directory twice: as `nvrm`, which devmgr matches to an NVIDIA display
//! controller (vendor `0x10de`, class 03), and as `nvrm-test`, which devmgr
//! matches to QEMU's `pci-testdev` (`1b36:0005`), the test device every
//! x86-64 machine this tool boots carries. QEMU emulates no NVIDIA function
//! and lets no device's vendor be overridden, so the second name is how a
//! boot with no GPU exercises the kind; only this gate's image carries it.
//!
//! Three boots, each judged by its lines:
//!
//! 1. **Handed over**, at 4 GiB, on the patched QEMU whose VT-d unit remaps
//!    interrupts: devmgr marks the device, finds its interrupts isolated,
//!    sets a pin budget (at 4 GiB, the floor cut to half the room, about
//!    490 MiB of the RAM the kernel counts) and starts `nvrm-test`,
//!    which reads its device from bootstrap, prints `device_isolation` with
//!    bit 1, its budget -- the one devmgr set --, its apertures, the
//!    configuration window, BAR0's first register, runs a thread, and
//!    idles.
//! 2. **Refused, budget too small**, at the default 512 MiB: an eighth of a
//!    quarter of that is under the 256 MiB `nvrm` needs, and devmgr starts
//!    nothing.
//! 3. **Refused, interrupts not isolated**: the same VT-d unit with
//!    `intremap=off`, so `device_isolation`'s bit 1 is clear, and devmgr
//!    refuses the hand-over by name after setting the mark.
//!
//! In the two refusals no `nvrm` line may appear and the kernel's devmgr
//! report counts one failed. The init of every boot is `nvrm-hold`, which
//! only keeps the machine up.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::native::{self, Built};
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, qemu, shell};

/// The seconds a boot may take when `--timeout` does not say: TCG at 4 GiB
/// is slower than the default 120 allows on a loaded host.
const TIMEOUT: u64 = 300;

/// How long after the kernel's devmgr report `nvrm` has to say it is up.
const UP_PATIENCE: Duration = Duration::from_secs(120);

/// How long a refused boot is read on after the report, for a line that
/// must not come.
const QUIET: Duration = Duration::from_secs(3);

/// The kernel's line once devmgr has reported: `devmgr   N devices, M
/// drivers, K started, F failed`.
const REPORTED: &str = " drivers, ";

/// devmgr's line when it hands the device over.
const HANDED: &str = "(interrupts isolated); handing it to nvrm";

/// The prefix of every line `nvrm` prints.
const NVRM: &str = "nvrm: ";

/// ferrix-nvos's line when it refuses nvrm's core.
const REFUSED: &str = "nvos: core refused: ";

/// The prefix of the lines ferrix-nvos prints in nvrm, its core's load
/// among them; [`STEPS`] holds both kinds in one order.
const NVOS: &str = "nvos: ";

/// `nvrm`'s lines, in order, each by what it starts with after
/// [`NVRM`].
const STEPS: &[&str] = &[
    "started on ",
    "device_isolation ",
    "pin budget ",
    "core loaded: ",
    "aperture 0: ",
    "configuration window: vendor 1b36 device 0005",
    "BAR0 mapped, ",
    "a thread ran and was joined",
    "skeleton up on ",
];

/// `nvrm`'s last line.
const UP: &str = "nvrm: skeleton up on ";

/// devmgr's refusal for unisolated interrupts.
const UNISOLATED: &str = "not started: its interrupts are not isolated (device_isolation 0x";

/// devmgr's refusal for a budget under `nvrm`'s minimum, after the MiB
/// it could have had.
const TOO_SMALL: &str = " MiB is under the 256 MiB nvrm needs";

/// devmgr's budget line, before the MiB it set.
const BUDGET: &str = ": pin budget ";

/// The rest of the budget line at 4 GiB, of which the kernel counts a
/// little less: the 1 GiB floor cut to half the room.
const CUT: &str = " MiB, cut from 1024 MiB: the 1 GiB floor yields to the kernel's ceiling";

/// The guest RAM of the boot that hands the device over, in MiB.
const HANDED_MEMORY: u32 = 4096;

/// The guest RAM of the boot whose budget is too small, in MiB.
const SMALL_MEMORY: u32 = 512;

/// NVIDIA's release, as `tools/common/fetch/fetch-nvidia.sh` writes it.
const RELEASE: &str = "580.173.02";

/// Where `fetch-nvidia.sh` wrote the release: `$FERRIX_NVIDIA/580.173.02`,
/// by default under `~/.local/share/ferrix/nvidia`. Refused without RM's
/// core in it.
fn fetched() -> Result<PathBuf> {
    let root = match std::env::var_os("FERRIX_NVIDIA") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".local")
            .join("share")
            .join("ferrix")
            .join("nvidia"),
    };
    let release = root.join(RELEASE);
    let core = release.join("objects").join("nv-kernel.o");
    if !core.is_file() {
        return Err(Error::new(format!(
            "no NVIDIA {RELEASE} objects at {} (no {}): tools/common/fetch/fetch-nvidia.sh \
             fetches and builds them, or set FERRIX_NVIDIA",
            release.display(),
            core.display()
        )));
    }
    Ok(release)
}

/// What nvrm's Makefile built, in one directory: `nvrm` and `nvrm-core`,
/// `nvrm-link-test`, its core and its unpinned link, and `nvrm-hold`.
pub(crate) struct Programs {
    /// The directory.
    pub(crate) out: PathBuf,
}

impl Programs {
    /// The file `name` the Makefile made.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.out.join(name)
    }
}

/// Build `nvrm`, its core, `nvrm-link-test` and `nvrm-hold` with the
/// Makefile, against NVIDIA's fetched release. `nvrm` is held to the shape
/// the kernel's loader takes from a native image, since devmgr starts it as
/// one.
pub(crate) fn build() -> Result<Programs> {
    let source = paths::workspace_root()
        .join("src")
        .join("user")
        .join("system")
        .join("linux")
        .join("drivers")
        .join("nvrm");
    let release = fetched()?;
    let out = paths::target_dir().join("nvrm");
    let ferrousli = paths::target_dir().join("ferrousli");
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    println!(
        "  building nvrm and its core against ferrousli (NVIDIA from {})",
        release.display()
    );
    let ran = Command::new("make")
        .arg("-s")
        .arg("-C")
        .arg(&source)
        .arg(format!("-j{jobs}"))
        .arg(format!("OUT={}", out.display()))
        .arg(format!("FERROUSLI_TARGET={}", ferrousli.display()))
        .arg(format!("NVIDIA={}", release.display()))
        .status()
        .map_err(|error| Error::new(format!("running make in {}: {error}", source.display())))?;
    if !ran.success() {
        return Err(Error::new(format!(
            "building nvrm failed ({ran}); make -C {}",
            source.display()
        )));
    }
    let programs = Programs { out };
    let program = programs.path("nvrm");
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    native::verify(Arch::X86_64, &bytes).map_err(|why| {
        Error::new(format!(
            "nvrm is not a program the kernel can start as a driver: {why}"
        ))
    })?;
    Ok(programs)
}

/// Where nvrm reads its core on the NVIDIA volume, which Ferrix mounts at
/// `/data` (`src/main.c`, `CORE_PATH`).
pub(crate) const CORE_IN_VOLUME: &str = "usr/lib/ferrix/nvrm-core";

/// Make `image`, a btrfs volume holding `files` -- each a path in the
/// volume and the file to copy there -- with `mkfs.btrfs --rootdir` from a
/// tree beside it. The kernel mounts a volume with no `ferrix-root` label
/// at `/data`.
pub(crate) fn volume(image: &Path, files: &[(&str, PathBuf)]) -> Result<()> {
    let tree = image.with_extension("tree");
    if tree.exists() {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| Error::new(format!("clearing {}: {error}", tree.display())))?;
    }
    let mut bytes = 0_u64;
    for (inside, from) in files {
        let to = tree.join(inside);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Error::new(format!("making {}: {error}", parent.display())))?;
        }
        bytes += std::fs::copy(from, &to).map_err(|error| {
            Error::new(format!(
                "copying {} to {}: {error}",
                from.display(),
                to.display()
            ))
        })?;
    }
    // Room for btrfs's own trees beside the files; mkfs.btrfs wants 114 MiB.
    let size = (bytes.div_ceil(1 << 20) + 64).max(128);
    let _ = std::fs::remove_file(image);
    let file = std::fs::File::create(image)
        .map_err(|error| Error::new(format!("creating {}: {error}", image.display())))?;
    file.set_len(size << 20)
        .map_err(|error| Error::new(format!("sizing {}: {error}", image.display())))?;
    drop(file);
    let made = Command::new("mkfs.btrfs")
        .arg("-q")
        .arg("--rootdir")
        .arg(&tree)
        .arg(image)
        .status()
        .map_err(|error| Error::new(format!("running mkfs.btrfs: {error}")))?;
    if !made.success() {
        let _ = std::fs::remove_file(image);
        return Err(Error::new(format!(
            "mkfs.btrfs {}: {made}",
            image.display()
        )));
    }
    Ok(())
}

/// The driver images `nvrm` goes in as: `nvrm` for NVIDIA's devices and
/// `nvrm-test` for the test device.
pub(crate) fn drivers(bytes: &[u8]) -> [Built; 2] {
    ["nvrm", "nvrm-test"].map(|name| Built {
        name,
        directory: native::DRIVERS,
        bytes: bytes.to_vec(),
    })
}

/// One of the gate's boots.
struct Boot {
    /// What it shows, for the lines printed.
    name: &'static str,
    /// Guest RAM, in MiB.
    memory: u32,
    /// Whether the VT-d unit leaves interrupt remapping off.
    unisolated: bool,
    /// Whether `nvrm` must come up, or devmgr refuse it.
    handed: bool,
}

const BOOTS: [Boot; 3] = [
    Boot {
        name: "handed over",
        memory: HANDED_MEMORY,
        unisolated: false,
        handed: true,
    },
    Boot {
        name: "refused, budget too small",
        memory: SMALL_MEMORY,
        unisolated: false,
        handed: false,
    },
    Boot {
        name: "refused, interrupts not isolated",
        memory: HANDED_MEMORY,
        unisolated: true,
        handed: false,
    },
];

/// The test, on x86-64, the one architecture `nvrm` is built for.
pub(crate) fn test_nvrm(args: &Args) -> Result<()> {
    if args.arches()? != [Arch::X86_64] {
        return Err(Error::new(
            "test-nvrm runs on x86_64 only: nvrm is built for the x86-64 Linux ABI",
        ));
    }
    let arch = Arch::X86_64;
    let log = paths::build_dir(arch).join("serial.log");
    let programs = build()?;
    let nvrm = std::fs::read(programs.path("nvrm"))
        .map_err(|error| Error::new(format!("reading nvrm: {error}")))?;
    let hold = programs.path("nvrm-hold");
    let volume_image = paths::build_dir(arch).join("nvrm-volume.img");
    volume(
        &volume_image,
        &[(CORE_IN_VOLUME, programs.path("nvrm-core"))],
    )?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &hold, shell::SCRIPT)?;
    let mut natives = native::build(arch, args.release)?;
    natives.extend(drivers(&nvrm));
    let image = fat::write_image(arch, &loader, &kernel, &natives, None)?;
    for boot in &BOOTS {
        let mut args = args.clone();
        if !args.timeout_given {
            args.timeout = TIMEOUT;
        }
        args.memory = boot.memory;
        args.unisolated_interrupts = boot.unisolated;
        args.data_image = Some(volume_image.clone());
        let lines = run(arch, &image, &kernel, &args, boot)?;
        let why = if boot.handed {
            judge_handed(&lines)
        } else if boot.unisolated {
            judge_refused(&lines, UNISOLATED)
        } else {
            judge_refused(&lines, TOO_SMALL)
        };
        if let Some(why) = why {
            return Err(Error::new(format!(
                "{arch}: the {} boot did not show what it must: {why}.\n  Serial output is in {}",
                boot.name,
                log.display()
            )));
        }
        let shown = lines
            .iter()
            .filter(|line| line.contains("devmgr   gpu ") || line.contains(NVRM))
            .count();
        println!(
            "  {arch}: {} at {} MiB: {shown} lines from devmgr and nvrm as required",
            boot.name, boot.memory
        );
    }
    Ok(())
}

/// Boot `image` for `boot`: up to the kernel's devmgr report, then on until
/// `nvrm` is up, or for a quiet while when it must not come. Every line,
/// those after the report included.
fn run(arch: Arch, image: &Path, kernel: &Path, args: &Args, boot: &Boot) -> Result<Vec<String>> {
    let mut after = Vec::new();
    let mut lines = qemu::watch_then(arch, image, kernel, args, REPORTED, |at| {
        if boot.handed {
            let deadline = Instant::now() + UP_PATIENCE;
            let _ = at.read_more(deadline, |lines| {
                lines.iter().any(|line| {
                    line.contains(UP) || line.contains("nvrm: stopped") || line.contains(REFUSED)
                })
            })?;
        } else {
            at.read_what_was_said(QUIET)?;
        }
        after = at.after().to_vec();
        at.stop_when_done();
        Ok(())
    })?;
    lines.extend(after);
    Ok(lines)
}

/// The kernel's devmgr report: how many started and how many failed.
fn report(lines: &[String]) -> Option<(u32, u32)> {
    let line = lines.iter().find(|line| line.contains(REPORTED))?;
    let number = |after: &str| -> Option<u32> {
        let words: Vec<&str> = line.split([' ', ',']).filter(|w| !w.is_empty()).collect();
        let at = words.iter().position(|word| *word == after)?;
        words.get(at.checked_sub(1)?)?.parse().ok()
    };
    Some((number("started")?, number("failed")?))
}

/// The place a line names after `marker`: `bb:dd.f`, up to the next space,
/// comma or semicolon, without a colon that ends it.
fn place_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let at = line.find(marker)? + marker.len();
    let rest = line.get(at..)?;
    let end = rest.find([' ', ',', ';']).unwrap_or(rest.len());
    Some(rest.get(..end)?.trim_end_matches(':'))
}

/// The MiB a line says after `marker`, as `N MiB`.
fn mib_after(line: &str, marker: &str) -> Option<u64> {
    let at = line.find(marker)? + marker.len();
    let rest = line.get(at..)?;
    rest.split(' ').next()?.parse().ok()
}

/// Why the lines of the handed-over boot are not what they must be.
fn judge_handed(lines: &[String]) -> Option<String> {
    let Some(handed) = lines.iter().find(|line| line.contains(HANDED)) else {
        return Some("devmgr did not hand the GPU over with its isolation line".to_owned());
    };
    let Some(place) = place_after(handed, "devmgr   gpu ").map(str::to_owned) else {
        return Some(format!("devmgr's line names no device: `{handed}`"));
    };
    if !handed.contains("device_isolation 0x2") && !handed.contains("device_isolation 0x3") {
        return Some(format!("devmgr's line does not show bit 1: `{handed}`"));
    }
    let Some(budget) = lines
        .iter()
        .find(|line| line.contains("devmgr   gpu ") && line.contains(BUDGET) && line.contains(CUT))
    else {
        return Some("devmgr did not set the 4 GiB machine's cut budget with its line".to_owned());
    };
    if place_after(budget, "devmgr   gpu ") != Some(place.as_str()) {
        return Some("devmgr's budget line names another device".to_owned());
    }
    let Some(set) = mib_after(budget, BUDGET) else {
        return Some(format!("devmgr's budget line has no MiB: `{budget}`"));
    };
    if !(256..=512).contains(&set) {
        return Some(format!(
            "devmgr set {set} MiB, not the half of a 4 GiB machine's ceiling it must"
        ));
    }
    if let Some(stopped) = lines
        .iter()
        .find(|line| line.contains("nvrm: stopped") || line.contains(REFUSED))
    {
        return Some(format!("nvrm stopped: `{stopped}`"));
    }
    let said: Vec<&str> = lines
        .iter()
        .filter_map(|line| {
            [NVRM, NVOS].iter().find_map(|prefix| {
                line.find(prefix)
                    .and_then(|at| line.get(at + prefix.len()..))
            })
        })
        .collect();
    let mut next = said.iter();
    for step in STEPS {
        if !next.any(|line| line.starts_with(step)) {
            return Some(format!("nvrm never said `{NVRM}{step}…` in its place"));
        }
    }
    let line = |step: &str| {
        said.iter()
            .find(|line| line.starts_with(step))
            .copied()
            .unwrap_or_default()
    };
    let started = line("started on ");
    if place_after(started, "started on ") != Some(place.as_str()) {
        return Some(format!(
            "nvrm started on another device than {place}: `{started}`"
        ));
    }
    if !line("core loaded: ").contains(", base 0x40000000, ") {
        return Some(format!(
            "nvrm's core is not at its base: `{}`",
            line("core loaded: ")
        ));
    }
    if !line("device_isolation ").contains("interrupts isolated (bit 1)") {
        return Some("nvrm did not read bit 1 of device_isolation".to_owned());
    }
    if mib_after(line("pin budget "), "pages (") != Some(set) {
        return Some(format!("nvrm's budget is not the {set} MiB devmgr set"));
    }
    match report(lines) {
        Some((started, 0)) if started > 0 => None,
        Some((_, failed)) => Some(format!("devmgr reported {failed} failed")),
        None => Some("the kernel never printed devmgr's report".to_owned()),
    }
}

/// Why the lines of a refused boot are not what they must be: devmgr's
/// line `refusal`, no `nvrm` at all, and one failed in the report.
fn judge_refused(lines: &[String], refusal: &str) -> Option<String> {
    let Some(refused) = lines
        .iter()
        .find(|line| line.contains("devmgr   gpu ") && line.contains(refusal))
    else {
        return Some(format!("devmgr never said `… {refusal}`"));
    };
    if lines.iter().any(|line| line.contains(HANDED)) {
        return Some("devmgr handed the GPU over as well".to_owned());
    }
    if let Some(line) = lines.iter().find(|line| line.contains(NVRM)) {
        return Some(format!(
            "nvrm ran although devmgr refused the device (`{refused}`): `{line}`"
        ));
    }
    match report(lines) {
        Some((_, 1)) => None,
        Some((_, failed)) => Some(format!(
            "devmgr reported {failed} failed, not the one GPU it refused"
        )),
        None => Some("the kernel never printed devmgr's report".to_owned()),
    }
}

#[cfg(test)]
mod tests;
