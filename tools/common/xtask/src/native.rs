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
        package: "ferrix-restricted-check",
        binary: "restricted-check",
        directory: DIRECTORY,
    },
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
        package: "ferrix-rtl8169-driver",
        binary: "rtl8169",
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
    // The class driver of the USB serial interfaces usbhid exports, which
    // devmgr starts per interface (docs/USB-SERIAL.md §6.1).
    Program {
        package: "ferrix-usbserial",
        binary: "usbserial",
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
    // memscrub, the RAM scrubber's pace (docs/RAM-SCRUBBER.md §5): a service
    // init starts and grants the memory scrub token.
    Program {
        package: "ferrix-memscrub",
        binary: "memscrub",
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
    let served = crate::flags::this_run().is_ok_and(|flags| flags.value("fs") == Some("server"));
    let lifted =
        crate::flags::this_run().is_ok_and(|flags| flags.value("personality") == Some("user"));
    PROGRAMS
        .iter()
        .chain(lifted.then_some(&LINUXD))
        .chain(lifted.then_some(&PARTMGR))
        .chain(served.then_some(&BTRFSD))
        .chain(
            std::env::var_os("FERRIX_TEST_NETD")
                .is_some()
                .then_some(&NETD),
        )
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

/// The partition manager (`docs/microkernel/BOOT-AUTHORITY.md` §8), likewise.
pub(crate) const PARTMGR: Program = Program {
    package: "ferrix-partmgr",
    binary: "partmgr",
    directory: DIRECTORY,
};

/// A program that ends at once: under a first process's name in
/// `test-first-processes`' images.
pub(crate) const CRASH_AT_START: Program = Program {
    package: "ferrix-crash-at-start",
    binary: "crash-at-start",
    directory: DRIVERS,
};

/// The filesystem server (`docs/microkernel/BTRFSD.md`), in an `fs=server` image alone.
pub(crate) const BTRFSD: Program = Program {
    package: "ferrix-btrfsd",
    binary: "btrfsd",
    directory: DIRECTORY,
};

/// The network switch (`docs/microkernel/M8.md` §2): only `test-netd`'s image for now,
/// until the row's review.
pub(crate) const NETD: Program = Program {
    package: "ferrix-netd",
    binary: "netd",
    directory: DRIVERS,
};

/// A partition's `linuxd` that dies at once: only `test-partitions`' images.
pub(crate) const LINUXD_DIES: Program = Program {
    package: "ferrix-linuxd-dies",
    binary: "linuxd-dies",
    directory: DIRECTORY,
};

/// A block driver over memory that misbehaves on purpose: only
/// `test-btrfsd-restart`'s archive, under the name `blk`.
pub(crate) const BLK_VICTIM: Program = Program {
    package: "ferrix-blk-victim",
    binary: "blk-victim",
    directory: DRIVERS,
};

/// A test image of a safety partition: only `test-first-processes`' archive.
pub(crate) const PARTITION_HELLO: Program = Program {
    package: "ferrix-partition-hello",
    binary: "partition-hello",
    directory: DIRECTORY,
};

/// `linuxd` (`docs/microkernel/M5-LIFT.md`), in a `personality=user` image alone.
/// Not in [`PROGRAMS`]: its crates take the kernel's flags, and no other image
/// carries it.
pub(crate) const LINUXD: Program = Program {
    package: "ferrix-linuxd",
    binary: "linuxd",
    directory: DIRECTORY,
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

/// `testseconds`, `test-driver-model`'s driver of a node the kernel has
/// never named (`docs/DRIVER-MODEL.md` §8). Not in [`PROGRAMS`]: only that
/// test's images carry it, beside a `devmgr` built with its table row
/// ([`TEST_DEVMGR`]), which [`refuse_test_driver`] holds.
pub(crate) const TEST_DRIVER: Program = Program {
    package: "ferrix-test-seconds-driver",
    binary: "testseconds",
    directory: DRIVERS,
};

/// The variable that compiles `test-driver-model`'s row into `devmgr`'s
/// table of generic nodes' drivers, and its value.
pub(crate) const TEST_DEVMGR: (&str, &str) = ("FERRIX_DEVMGR_TEST_DRIVER", "on");

/// Set by `test-driver-model` alone: the one run whose images may carry
/// [`TEST_DRIVER`].
static TEST_DRIVER_ASKED: AtomicBool = AtomicBool::new(false);

/// Let this run's images carry [`TEST_DRIVER`]: `test-driver-model` only.
pub(crate) fn allow_test_driver() {
    TEST_DRIVER_ASKED.store(true, Ordering::Relaxed);
}

/// Refuse a set of native programs that carries [`TEST_DRIVER`], unless
/// `test-driver-model` is building it.
///
/// # Errors
///
/// When `natives` carries it in any other run.
pub(crate) fn refuse_test_driver(natives: &[Built]) -> Result<()> {
    if TEST_DRIVER_ASKED.load(Ordering::Relaxed) {
        return Ok(());
    }
    if natives.iter().any(|built| built.name == TEST_DRIVER.binary) {
        return Err(Error::new(
            "this image carries testseconds, which only test-driver-model's images may carry \
             (docs/DRIVER-MODEL.md section 8)",
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
/// C1, 2026-10-07); and one that carries [`TEST_DRIVER`], unless
/// `test-driver-model` is ([`refuse_test_driver`]).
///
/// # Errors
///
/// When `natives` carries the helper in any other run.
pub(crate) fn refuse_updater(natives: &[Built]) -> Result<()> {
    refuse_test_driver(natives)?;
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
    executable_pages_can_be_mapped(&elf)?;
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

/// `ferrix-process-builder` maps an executable page and never copies it
/// (`docs/microkernel/K15.md`, ledger 5726 (2)), so every executable page must
/// be one mapping can show: the segment's offset and address agree within a page,
/// its memory ends with its file contents, and no other segment shares its pages.
fn executable_pages_can_be_mapped(elf: &Elf<'_>) -> std::result::Result<(), String> {
    const PAGE: u64 = 4096;
    let pages = |segment: &Segment| {
        let first = segment.vaddr & !(PAGE - 1);
        let last = (segment.vaddr + segment.memsz + PAGE - 1) & !(PAGE - 1);
        (first, last)
    };
    for text in elf.loadable().filter(Segment::is_executable) {
        if (text.vaddr ^ text.offset) & (PAGE - 1) != 0 {
            return Err(format!(
                "its executable segment at {:#x} has an offset that disagrees within a page: \
                 it cannot be mapped",
                text.vaddr
            ));
        }
        if text.memsz > text.filesz {
            return Err(format!(
                "its executable segment at {:#x} has memory past its file contents: \
                 it cannot be mapped",
                text.vaddr
            ));
        }
        let (first, last) = pages(&text);
        let shared = elf.loadable().any(|other| {
            let (start, end) = pages(&other);
            other.vaddr != text.vaddr && start < last && first < end
        });
        if shared {
            return Err(format!(
                "its executable segment at {:#x} shares a page with another: it cannot be mapped",
                text.vaddr
            ));
        }
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

    /// A little ELF64 of these segments: flags, file offset, address, file size,
    /// memory size.
    fn little_elf(segments: &[(u32, u64, u64, u64, u64)]) -> Vec<u8> {
        let (head, phent) = (64, 56);
        let mut file = vec![0_u8; 0x4000];
        file[..7].copy_from_slice(&[0x7F, b'E', b'L', b'F', 2, 1, 1]);
        file[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        file[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        file[20..24].copy_from_slice(&1_u32.to_le_bytes());
        file[32..40].copy_from_slice(&(head as u64).to_le_bytes());
        file[52..54].copy_from_slice(&(head as u16).to_le_bytes());
        file[54..56].copy_from_slice(&(phent as u16).to_le_bytes());
        file[56..58].copy_from_slice(&(segments.len() as u16).to_le_bytes());
        for (index, &(flags, offset, at, filesz, memsz)) in segments.iter().enumerate() {
            let to = head + phent * index;
            file[to..to + 4].copy_from_slice(&1_u32.to_le_bytes());
            file[to + 4..to + 8].copy_from_slice(&flags.to_le_bytes());
            file[to + 8..to + 16].copy_from_slice(&offset.to_le_bytes());
            file[to + 16..to + 24].copy_from_slice(&at.to_le_bytes());
            file[to + 32..to + 40].copy_from_slice(&filesz.to_le_bytes());
            file[to + 40..to + 48].copy_from_slice(&memsz.to_le_bytes());
            file[to + 48..to + 56].copy_from_slice(&4096_u64.to_le_bytes());
        }
        file
    }

    /// K15: what the loader library maps, it must be able to map (ledger 5726 (2)).
    #[test]
    fn an_executable_page_that_cannot_be_mapped_is_refused() {
        const RX: u32 = 5;
        const RW: u32 = 6;
        let good = little_elf(&[
            (RX, 0x1000, 0x40_1000, 0x800, 0x800),
            (RW, 0x2000, 0x40_2000, 0x10, 0x10),
        ]);
        let mappable = |file: &[u8]| executable_pages_can_be_mapped(&Elf::parse(file).unwrap());
        assert_eq!(mappable(&good), Ok(()));
        let offset = little_elf(&[(RX, 0x1100, 0x40_1000, 0x800, 0x800)]);
        assert!(mappable(&offset).is_err());
        let bss = little_elf(&[(RX, 0x1000, 0x40_1000, 0x800, 0x2000)]);
        assert!(mappable(&bss).is_err());
        let shared = little_elf(&[
            (RX, 0x1000, 0x40_1000, 0x800, 0x800),
            (RW, 0x1800, 0x40_1800, 0x10, 0x10),
        ]);
        assert!(mappable(&shared).is_err());
    }

    /// The other two of the image check's refusals a planted image shows.
    #[test]
    fn a_writable_executable_page_is_refused() {
        let wx = little_elf(&[(7, 0x1000, 0x40_1000, 0x800, 0x800)]);
        assert!(no_page_is_writable_and_executable(&Elf::parse(&wx).unwrap()).is_err());
    }

    #[test]
    fn a_trap_is_found_only_on_an_instruction_boundary() {
        let svc = [0x01, 0x00, 0x00, 0xD4];
        assert!(holds(&[0, 0, 0, 0, 0x01, 0x00, 0x00, 0xD4], &svc, 4));
        assert!(!holds(&[0, 0x01, 0x00, 0x00, 0xD4, 0, 0, 0], &svc, 4));
        assert!(holds(&[0x90, 0x0F, 0x05], &[0x0F, 0x05], 1));
    }
}
