//! Native programs: built for the kernel's targets, checked, and put in the
//! initramfs.
//!
//! A native program is a crate under `src/user/system/native/` that links `src/user/system/native/rt`. It is
//! built for the same target as the kernel — freestanding, soft float,
//! statically relocated — because that is what the kernel's ELF loader takes,
//! and it is checked here before it goes anywhere: an image the loader would
//! refuse, or one whose entry is not the runtime's `_start`, fails the build
//! with the reason rather than a boot with none.
//!
//! # What the check can and cannot say
//!
//! It holds the *shape*: an `ET_EXEC` for the right machine, no interpreter
//! and no dynamic section, no page both writable and executable, an entry
//! point in an executable segment above `MMAP_MIN_ADDR` that begins with
//! `_start`'s own instructions, and the architecture's trap instruction in the
//! text. It cannot say the program *runs*. That waits on `process_create`
//! (0x1030) and `process_start` (0x1031), which let the kernel's boot check
//! start an initramfs program as a native process; until they land, no boot
//! runs these programs.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ferrix_elf::{EM_AARCH64, EM_ARM, EM_X86_64, ET_EXEC, Elf, PT_DYNAMIC, PT_INTERP, Segment};

use crate::cargo;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// A native program in the tree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Program {
    /// Its cargo package.
    pub(crate) package: &'static str,
    /// Its binary, which is also its name in the initramfs.
    pub(crate) binary: &'static str,
    /// The initramfs directory it goes in: [`DIRECTORY`] for system programs,
    /// [`DRIVERS`] for the drivers `devmgr` starts.
    pub(crate) directory: &'static str,
}

/// Every native program the initramfs carries.
pub(crate) const PROGRAMS: &[Program] = &[
    Program {
        package: "ferrix-channel-echo",
        binary: "channel-echo",
        directory: DIRECTORY,
    },
    Program {
        package: "ferrix-pong",
        binary: "pong",
        directory: DIRECTORY,
    },
    Program {
        package: "ferrix-ipc-bench",
        binary: "ipc-bench",
        directory: DIRECTORY,
    },
    Program {
        package: "ferrix-ipc-equiv",
        binary: "ipc-equiv",
        directory: DIRECTORY,
    },
    Program {
        package: "ferrix-devmgr",
        binary: "devmgr",
        directory: DIRECTORY,
    },
    Program {
        package: "ferrix-blk",
        binary: "blk",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-nvme-driver",
        binary: "nvme",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-ahci-driver",
        binary: "ahci",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-sdmmc",
        binary: "sdmmc",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-net-driver",
        binary: "net",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-yt6801-driver",
        binary: "yt6801",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-gpu",
        binary: "gpu",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-efifb",
        binary: "efifb",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-ltdc",
        binary: "ltdc",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-input",
        binary: "input",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-usbhid",
        binary: "usbhid",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-usbdev",
        binary: "usbdev",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-power-driver",
        binary: "power",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-gc400-driver",
        binary: "gc400",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-intel-driver",
        binary: "intel",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-vport",
        binary: "vport",
        directory: DRIVERS,
    },
    Program {
        package: "ferrix-snd",
        binary: "snd",
        directory: DRIVERS,
    },
    // lode, the log manager (docs/LOGGING.md): the service init starts in a
    // sealed job, and the reader `/bin/lode` and `/bin/journalctl` link to.
    Program {
        package: "ferrix-lode",
        binary: "lode",
        directory: DIRECTORY,
    },
    // Not a driver: the helper `devmgr` publishes drivers' deaths and
    // restarts through (docs/DESKTOP.md §4.2), handed to it as an image
    // the way drvupdated is, and named by no table entry.
    Program {
        package: "ferrix-devmgr-events",
        binary: "devmgr-events",
        directory: DRIVERS,
    },
];

/// Where drivers are unpacked, relative to the root, with a `MANIFEST`
/// listing them one per line: what the kernel hands `devmgr`
/// (`docs/DEVMGR.md` §2).
pub(crate) const DRIVERS: &str = "lib/drivers";
/// The manifest's name in [`DRIVERS`].
pub(crate) const MANIFEST: &str = "MANIFEST";

/// Where native programs are unpacked, relative to the root: system programs,
/// off the shell's `PATH=/bin`.
pub(crate) const DIRECTORY: &str = "sbin";

/// A native program, built and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Built {
    /// Its name in its directory.
    pub(crate) name: &'static str,
    /// Its directory, relative to the root.
    pub(crate) directory: &'static str,
    /// The ELF image.
    pub(crate) bytes: Vec<u8>,
}

/// Build and check every native program for `arch`.
pub(crate) fn build(arch: Arch, release: bool) -> Result<Vec<Built>> {
    build_in(arch, release, &paths::target_dir())
}

/// [`build`], into `target_dir`.
pub(crate) fn build_in(arch: Arch, release: bool, target_dir: &Path) -> Result<Vec<Built>> {
    PROGRAMS
        .iter()
        .map(|program| build_one(arch, release, target_dir, program))
        .collect()
}

/// `drvupdated`, the helper that takes driver updates for `devmgr`
/// (`docs/DEVMGR.md` §4.1). Not in [`PROGRAMS`]: until the image's
/// verification has its review, only the images
/// `cargo xtask test-restart --update` boots carry it (the certification
/// consultant's C1, 2026-10-07), which [`refuse_updater`] holds.
pub(crate) const UPDATER: Program = Program {
    package: "ferrix-drvupdated",
    binary: "drvupdated",
    directory: DRIVERS,
};

/// One program of the tree, built and checked.
pub(crate) fn build_one(
    arch: Arch,
    release: bool,
    target_dir: &Path,
    program: &Program,
) -> Result<Built> {
    let path = cargo::build_native(arch, release, program.package, program.binary, target_dir)?;
    let bytes = std::fs::read(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    verify(arch, &bytes).map_err(|why| {
        Error::new(format!(
            "{} for {arch} is not a program the kernel can start: {why}",
            program.binary
        ))
    })?;
    refuse_armed(program.binary, &bytes)?;
    crate::intel_uapi::refuse_fake(program.binary, &bytes)?;
    Ok(Built {
        name: program.binary,
        directory: program.directory,
        bytes,
    })
}

/// Replace the virtio-net driver in `natives` with one built fault-armed
/// (`FERRIX_NET_FAULT`): a frame it sends holding its marker makes it corrupt
/// its own ring, so that the kernel ends the ring under a live driver, as a
/// torn index once did. `test-net`'s last program asks for that, mid-transfer,
/// and requires the driver to be started again and the transfer to finish
/// (`net::restart_command`). Never in any other image: a frame anyone can
/// send must not be able to take a real machine's network down.
///
/// # Errors
///
/// The build failing, or no `net` in `natives` to replace.
pub(crate) fn arm_net_fault(natives: &mut [Built], arch: Arch, release: bool) -> Result<()> {
    let program = PROGRAMS
        .iter()
        .find(|program| program.binary == "net")
        .ok_or_else(|| Error::new("no virtio-net driver among the native programs"))?;
    let target = paths::target_dir().join("net-fault");
    let path = cargo::build_native_with(
        arch,
        release,
        program.package,
        program.binary,
        &target,
        &[("FERRIX_NET_FAULT", "armed")],
    )?;
    let bytes = std::fs::read(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    verify(arch, &bytes).map_err(|why| {
        Error::new(format!(
            "the fault-armed net for {arch} is not a program the kernel can start: {why}"
        ))
    })?;
    let slot = natives
        .iter_mut()
        .find(|built| built.name == program.binary)
        .ok_or_else(|| Error::new("the native programs carry no net to replace"))?;
    slot.bytes = bytes;
    Ok(())
}

/// Refuse a `net` that carries [`arm_net_fault`]'s fault: every image but
/// `test-net`'s gets this build, and a driver whose network any sender can
/// take down must not reach one -- through a `FERRIX_NET_FAULT` left in the
/// environment, say. An unarmed build compiles the marker out entirely.
///
/// # Errors
///
/// The marker in `net`'s bytes.
fn refuse_armed(binary: &str, bytes: &[u8]) -> Result<()> {
    let marker = crate::net::FAULT_MARKER.as_bytes();
    if binary == "net" && bytes.windows(marker.len()).any(|window| window == marker) {
        return Err(Error::new(
            "net was built fault-armed (FERRIX_NET_FAULT is set?): only test-net's images \
             may carry that driver",
        ));
    }
    Ok(())
}

/// Set by `test-restart --update` alone: the one run whose images may
/// carry [`UPDATER`].
static UPDATES_ASKED: AtomicBool = AtomicBool::new(false);

/// Let this run's images carry [`UPDATER`]: `test-restart --update` only.
pub(crate) fn allow_updater() {
    UPDATES_ASKED.store(true, Ordering::Relaxed);
}

/// Refuse a set of native programs that carries [`UPDATER`], unless
/// `test-restart --update` is building it (the certification consultant's
/// C1, 2026-10-07).
///
/// # Errors
///
/// When `natives` carries the helper in any other run.
pub(crate) fn refuse_updater(natives: &[Built]) -> Result<()> {
    if UPDATES_ASKED.load(Ordering::Relaxed) {
        return Ok(());
    }
    if natives.iter().any(|built| built.name == UPDATER.binary) {
        return Err(Error::new(
            "this image carries drvupdated, which only test-restart --update may carry until \
             driver updates are verified (docs/DEVMGR.md 4.1, the consultant's C1)",
        ));
    }
    Ok(())
}

/// A page, as the kernel's loader applies permissions to them.
const PAGE: u64 = 4096;

/// The lowest address the kernel maps anything at: `MMAP_MIN_ADDR` in
/// `src/kernel/src/user/space.rs`.
const MMAP_MIN_ADDR: u64 = 0x1_0000;

/// What an architecture's native program must look like.
#[derive(Debug, Clone, Copy)]
struct Expected {
    /// `e_machine`.
    machine: u16,
    /// The first instructions of `src/user/system/native/rt`'s `_start`, as bytes.
    start: &'static [u8],
    /// The trap instruction a native call is made with.
    trap: &'static [u8],
    /// The instruction width the trap is aligned to: 1 where instructions have
    /// no fixed width.
    step: usize,
}

/// The shape `arch`'s programs are held to, from `src/user/system/native/rt/src/arch/`.
const fn expected(arch: Arch) -> Expected {
    match arch {
        // xor ebp, ebp ; and rsp, -16 ; call
        Arch::X86_64 => Expected {
            machine: EM_X86_64,
            start: &[0x31, 0xED, 0x48, 0x83, 0xE4, 0xF0, 0xE8],
            trap: &[0x0F, 0x05],
            step: 1,
        },
        // mov x29, #0 ; mov x30, #0 ; svc #0
        Arch::AArch64 => Expected {
            machine: EM_AARCH64,
            start: &[0x1D, 0x00, 0x80, 0xD2, 0x1E, 0x00, 0x80, 0xD2],
            trap: &[0x01, 0x00, 0x00, 0xD4],
            step: 4,
        },
        // mov r11, #0 ; mov lr, #0 ; svc #0
        Arch::Armv7a => Expected {
            machine: EM_ARM,
            start: &[0x00, 0xB0, 0xA0, 0xE3, 0x00, 0xE0, 0xA0, 0xE3],
            trap: &[0x00, 0x00, 0x00, 0xEF],
            step: 4,
        },
    }
}

/// Whether `image` is a native program the kernel can start on `arch`.
pub(crate) fn verify(arch: Arch, image: &[u8]) -> std::result::Result<(), String> {
    let expected = expected(arch);
    let elf = Elf::parse(image).map_err(|error| error.to_string())?;
    elf.check_machine(expected.machine)
        .map_err(|error| error.to_string())?;
    if elf.header().elf_type != ET_EXEC {
        return Err(format!(
            "its type is {}, not ET_EXEC, and the kernel relocates nothing",
            elf.header().elf_type
        ));
    }
    elf.validate_segments().map_err(|error| error.to_string())?;
    if let Some(segment) = elf
        .segments()
        .find(|segment| segment.kind == PT_INTERP || segment.kind == PT_DYNAMIC)
    {
        return Err(format!(
            "it has a segment of type {}: a native program is static",
            segment.kind
        ));
    }
    no_page_is_writable_and_executable(&elf)?;
    entry_is_the_runtimes(&elf, expected)?;
    let traps = elf
        .loadable()
        .filter(Segment::is_executable)
        .any(|segment| {
            segment
                .data(image)
                .is_ok_and(|text| holds(text, expected.trap, expected.step))
        });
    if !traps {
        return Err("its text holds no trap instruction, so it makes no call".to_owned());
    }
    Ok(())
}

/// The kernel's loader refuses a page both writable and executable.
fn no_page_is_writable_and_executable(elf: &Elf<'_>) -> std::result::Result<(), String> {
    let mut writable = BTreeSet::new();
    let mut executable = BTreeSet::new();
    for segment in elf.loadable() {
        let first = segment.vaddr / PAGE;
        let end = segment.vaddr_end().unwrap_or(segment.vaddr).div_ceil(PAGE);
        if segment.is_writable() {
            writable.extend(first..end);
        }
        if segment.is_executable() {
            executable.extend(first..end);
        }
    }
    match writable.intersection(&executable).next() {
        Some(page) => Err(format!(
            "the page at {:#x} would be writable and executable",
            page * PAGE
        )),
        None => Ok(()),
    }
}

/// The entry point is in user space, in an executable segment, and is
/// `src/user/system/native/rt`'s `_start`.
fn entry_is_the_runtimes(elf: &Elf<'_>, expected: Expected) -> std::result::Result<(), String> {
    let entry = elf.entry();
    if entry < MMAP_MIN_ADDR {
        return Err(format!("its entry point {entry:#x} is below MMAP_MIN_ADDR"));
    }
    let in_text = elf.loadable().any(|segment| {
        segment.is_executable()
            && segment.vaddr <= entry
            && segment.vaddr_end().is_some_and(|end| entry < end)
    });
    if !in_text {
        return Err(format!(
            "its entry point {entry:#x} is not in an executable segment"
        ));
    }
    let length = u64::try_from(expected.start.len()).unwrap_or(u64::MAX);
    if elf.vaddr_to_bytes(entry, length) != Some(expected.start) {
        return Err(format!(
            "its entry point {entry:#x} does not begin with src/user/system/native/rt's _start"
        ));
    }
    Ok(())
}

/// Whether `text` holds `needle` at an instruction boundary `step` apart.
fn holds(text: &[u8], needle: &[u8], step: usize) -> bool {
    if step <= 1 {
        text.windows(needle.len()).any(|window| window == needle)
    } else {
        text.chunks_exact(step).any(|word| word == needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `net` holding the fault marker is refused, and nothing else is.
    #[test]
    fn an_armed_net_is_refused_outside_test_net() {
        let armed = [
            b"\x7fELF..".as_slice(),
            crate::net::FAULT_MARKER.as_bytes(),
            b"..",
        ]
        .concat();
        assert!(refuse_armed("net", &armed).is_err());
        assert!(refuse_armed("net", b"\x7fELF plain").is_ok());
        assert!(refuse_armed("gpu", &armed).is_ok());
    }

    /// The target directory of the tests' own, so the build running them is
    /// never waited on.
    fn test_target_dir() -> std::path::PathBuf {
        paths::target_dir().join("xtask-native-test")
    }

    /// The tree's programs for `arch`, built into [`test_target_dir`].
    fn built(arch: Arch) -> Vec<Built> {
        build_in(arch, false, &test_target_dir()).unwrap()
    }

    /// Each program built for the three architectures in one cargo call,
    /// which compiles them side by side: 18 calls rather than 54 one after
    /// another, the longest test of `check` (148 s of `tests`' 208 s in a
    /// gate slot on 2026-10-15, `docs/TEST-TIME.md`), and then each ELF
    /// checked.
    #[test]
    fn every_program_builds_and_passes_the_check_on_every_architecture() {
        for program in PROGRAMS {
            let built = cargo::build_native_every_arch(
                false,
                program.package,
                program.binary,
                &test_target_dir(),
            )
            .unwrap();
            assert_eq!(built.len(), Arch::ALL.len(), "{}", program.binary);
            for (arch, path) in built {
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(verify(arch, &bytes), Ok(()), "{arch} {}", program.binary);
            }
        }
    }

    #[test]
    fn the_check_refuses_what_the_kernel_would() {
        let image = built(Arch::X86_64).remove(0).bytes;

        let wrong_machine = verify(Arch::AArch64, &image).unwrap_err();
        assert!(wrong_machine.contains("machine"), "{wrong_machine}");

        let mut relocatable = image.clone();
        relocatable[16..18].copy_from_slice(&3_u16.to_le_bytes());
        assert!(
            verify(Arch::X86_64, &relocatable)
                .unwrap_err()
                .contains("ET_EXEC")
        );

        let elf = Elf::parse(&image).unwrap();
        let text = elf.loadable().find(Segment::is_executable).unwrap();
        let entry_offset = usize::try_from(text.offset + (elf.entry() - text.vaddr)).unwrap();
        let mut other_entry = image.clone();
        other_entry[entry_offset] ^= 0xFF;
        assert!(
            verify(Arch::X86_64, &other_entry)
                .unwrap_err()
                .contains("_start")
        );

        assert!(verify(Arch::X86_64, b"\x7fELF").is_err());
    }

    #[test]
    fn a_trap_is_found_only_on_an_instruction_boundary() {
        let svc = [0x01, 0x00, 0x00, 0xD4];
        assert!(holds(&[0, 0, 0, 0, 0x01, 0x00, 0x00, 0xD4], &svc, 4));
        assert!(!holds(&[0, 0x01, 0x00, 0x00, 0xD4, 0, 0, 0], &svc, 4));
        assert!(holds(&[0x90, 0x0F, 0x05], &[0x0F, 0x05], 1));
    }
}
