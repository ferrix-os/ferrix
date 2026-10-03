//! `test-nvrm-link`: NVIDIA's resource manager loaded and run with no GPU,
//! on the host and on Ferrix, and every refusal of its loader shown
//! (`docs/NVIDIA.md` §4.1 "The core", §7 N1c).
//!
//! `nvrm-link-test` is nvrm's objects with a main of its own
//! (`src/user/system/linux/drivers/nvrm/test/link.c`): it loads its core,
//! initialises RM, opens the control device, allocates a root client, asks
//! for `NV01_DEVICE_0` -- which RM must refuse, with no GPU -- and frees it
//! all, ending `nvrm-link-test: passed`.
//!
//! Beside that run come the controls, each a core or a program changed in
//! one way, which the loader must refuse with its own line and status
//! (`os/nvos/src/rmcore.rs`). A changed core is pinned into a copy of the
//! unpinned program, as the Makefile pins the real one, so that the check
//! behind the pin is the one that fires:
//!
//! | control | changed | refused by |
//! |---|---|---|
//! | flipped | one byte of the core, not pinned again | the pin |
//! | unpinned | the program with its pin still zero | the unset pin |
//! | other build | the core linked against nvrm, pinned | the build-id |
//! | past 2 GiB | the data segment moved to `0x8000_0000` | the range |
//! | rwx segment | the text segment made writable | write and execute |
//! | bad magic | the export header's first byte | the magic |
//! | rwx mapping | a page mapped rwx, `--control-rwx` | the maps check |
//!
//! Every case runs on the host first, where it is quick, and then on
//! Ferrix: the programs and cores go on a btrfs volume, which the kernel
//! mounts at `/data`, and a shell runs each case there and says its status.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::Args;
use crate::nvrm::{self, Programs};
use crate::paths::{self, Arch};
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, sha256, shell, zinc};

/// The link test's last line.
const PASSED: &str = "nvrm-link-test: passed";

/// One case: its name, the program and core it runs (file names in the
/// volume), the status it must end with and a line it must print.
struct Case {
    /// Its name, in the lines.
    name: &'static str,
    /// The program, by its name on the volume.
    program: &'static str,
    /// Its argument: a core's name on the volume, or `--control-rwx`.
    argument: &'static str,
    /// The status it must exit with.
    status: i32,
    /// A line it must print, or a piece of one.
    says: &'static str,
}

/// The cases, the good run first.
const CASES: &[Case] = &[
    Case {
        name: "good",
        program: "nvrm-link-test",
        argument: "nvrm-link-test-core",
        status: 0,
        says: PASSED,
    },
    Case {
        name: "flipped",
        program: "nvrm-link-test",
        argument: "flipped-core",
        status: 25,
        says: "nvos: core refused: the core's sha256 is not nvrm's pin",
    },
    Case {
        name: "unpinned",
        program: "nvrm-link-test.unpinned",
        argument: "nvrm-link-test-core",
        status: 20,
        says: "nvos: core refused: nvrm's core pin is unset (all zero)",
    },
    Case {
        name: "other-build",
        program: "pinned-other-build",
        argument: "other-build-core",
        status: 35,
        says: "nvos: core refused: the core was linked against another nvrm build",
    },
    Case {
        name: "past-2gib",
        program: "pinned-past-2gib",
        argument: "past-2gib-core",
        status: 30,
        says: "nvos: core refused: a segment of the core is outside [0x40000000, 0x80000000)",
    },
    Case {
        name: "rwx-segment",
        program: "pinned-rwx-segment",
        argument: "rwx-segment-core",
        status: 29,
        says: "nvos: core refused: a segment of the core is writable and executable",
    },
    Case {
        name: "bad-magic",
        program: "pinned-bad-magic",
        argument: "bad-magic-core",
        status: 32,
        says: "nvos: core refused: the core's export header has the wrong magic",
    },
    Case {
        name: "rwx-mapping",
        program: "nvrm-link-test",
        argument: "--control-rwx",
        status: 41,
        says: "nvos: core refused: /proc/self/maps shows a writable and executable mapping",
    },
];

/// A little-endian field of `N` bytes at `at`.
fn field<const N: usize>(bytes: &[u8], at: usize) -> Result<u64> {
    let slice = bytes
        .get(at..at + N)
        .ok_or_else(|| Error::new("the core is shorter than its headers say"))?;
    Ok(slice
        .iter()
        .rev()
        .fold(0, |value, byte| value << 8 | u64::from(*byte)))
}

/// The core's loadable segments' program header offsets, in order.
fn loads(core: &[u8]) -> Result<Vec<usize>> {
    let phoff = usize::try_from(field::<8>(core, 32)?).map_err(|_| Error::new("phoff"))?;
    let phnum = usize::try_from(field::<2>(core, 56)?).map_err(|_| Error::new("phnum"))?;
    let mut found = Vec::new();
    for index in 0..phnum {
        let at = phoff + index * 56;
        if field::<4>(core, at)? == 1 {
            found.push(at);
        }
    }
    if found.len() != 3 {
        return Err(Error::new(format!(
            "the core has {} loadable segments, not nvrm-core.ld's three",
            found.len()
        )));
    }
    Ok(found)
}

/// Write `value` as `N` little-endian bytes at `at`.
fn set<const N: usize>(bytes: &mut [u8], at: usize, value: u64) -> Result<()> {
    let slice = bytes
        .get_mut(at..at + N)
        .ok_or_else(|| Error::new("the core is shorter than its headers say"))?;
    for (index, byte) in slice.iter_mut().enumerate() {
        *byte = value.to_le_bytes().get(index).copied().unwrap_or(0);
    }
    Ok(())
}

/// Flip the low bit of the byte at `at`.
fn flip(bytes: &mut [u8], at: usize) -> Result<()> {
    let byte = bytes
        .get_mut(at)
        .ok_or_else(|| Error::new("the core is shorter than its headers say"))?;
    *byte ^= 1;
    Ok(())
}

/// The changed cores, by the name each goes on the volume as.
fn changed_cores(core: &[u8]) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let [rodata, text, data] = loads(core)?[..] else {
        return Err(Error::new("three segments"));
    };
    let mut flipped = core.to_vec();
    let middle = flipped.len() / 2;
    flip(&mut flipped, middle)?;
    // The data segment's address and its physical twin at 2 GiB.
    let mut past = core.to_vec();
    set::<8>(&mut past, data + 16, 0x8000_0000)?;
    set::<8>(&mut past, data + 24, 0x8000_0000)?;
    let mut rwx = core.to_vec();
    set::<4>(&mut rwx, text + 4, 7)?;
    let mut magic = core.to_vec();
    let header =
        usize::try_from(field::<8>(core, rodata + 8)?).map_err(|_| Error::new("offset"))?;
    flip(&mut magic, header)?;
    Ok(vec![
        ("flipped-core", flipped),
        ("past-2gib-core", past),
        ("rwx-segment-core", rwx),
        ("bad-magic-core", magic),
    ])
}

/// Copy `unpinned` to `to` with `core`'s sha256 as its pin, as the Makefile
/// pins nvrm.
fn pin(unpinned: &Path, core: &[u8], to: &Path) -> Result<()> {
    let hash = to.with_extension("sha256");
    std::fs::write(&hash, sha256::digest(core))
        .map_err(|error| Error::new(format!("writing {}: {error}", hash.display())))?;
    let ran = Command::new("objcopy")
        .arg(format!(
            "--update-section=.nvrm_core_sha256={}",
            hash.display()
        ))
        .arg(unpinned)
        .arg(to)
        .status()
        .map_err(|error| Error::new(format!("running objcopy: {error}")))?;
    if !ran.success() {
        return Err(Error::new(format!(
            "objcopy pinning {}: {ran}",
            to.display()
        )));
    }
    Ok(())
}

/// Every file the cases run, in `directory`: the built programs and cores,
/// the changed cores, and a program pinned to each changed core but the
/// flipped one, whose point is that its pin is the good core's.
fn stage(programs: &Programs, directory: &Path) -> Result<Vec<(String, PathBuf)>> {
    std::fs::create_dir_all(directory)
        .map_err(|error| Error::new(format!("making {}: {error}", directory.display())))?;
    let read = |path: &Path| {
        std::fs::read(path)
            .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
    };
    let mut files: Vec<(String, PathBuf)> = [
        "nvrm-link-test",
        "nvrm-link-test-core",
        "nvrm-link-test.unpinned",
    ]
    .iter()
    .map(|name| ((*name).to_owned(), programs.path(name)))
    .collect();
    let unpinned = programs.path("nvrm-link-test.unpinned");
    let core = read(&programs.path("nvrm-link-test-core"))?;
    for (name, bytes) in changed_cores(&core)? {
        let path = directory.join(name);
        std::fs::write(&path, &bytes)
            .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
        files.push((name.to_owned(), path));
        if name != "flipped-core" {
            let program_name = format!("pinned-{}", name.trim_end_matches("-core"));
            let program = directory.join(&program_name);
            pin(&unpinned, &bytes, &program)?;
            files.push((program_name, program));
        }
    }
    let other = read(&programs.path("nvrm-core"))?;
    let other_core = directory.join("other-build-core");
    std::fs::write(&other_core, &other)
        .map_err(|error| Error::new(format!("writing {}: {error}", other_core.display())))?;
    let program = directory.join("pinned-other-build");
    pin(&unpinned, &other, &program)?;
    files.push(("other-build-core".to_owned(), other_core));
    files.push(("pinned-other-build".to_owned(), program));
    Ok(files)
}

/// Why `output`, a case's lines, and `status` are not what `case` must
/// show.
fn judge(case: &Case, status: Option<i32>, output: &str) -> Option<String> {
    if status != Some(case.status) {
        return Some(format!(
            "{} exited {status:?}, not {}",
            case.name, case.status
        ));
    }
    if !output.lines().any(|line| line.contains(case.says)) {
        return Some(format!("{} never said `{}`", case.name, case.says));
    }
    if case.status == 0 && output.contains("nvos: core refused") {
        return Some(format!("{} passed but its core was refused", case.name));
    }
    None
}

/// Run every case on the host from `directory`.
fn on_host(directory: &Path, files: &[(String, PathBuf)]) -> Result<()> {
    let find = |name: &str| {
        files
            .iter()
            .find(|(file, _)| file == name)
            .map(|(_, path)| path.clone())
            .unwrap_or_else(|| directory.join(name))
    };
    for case in CASES {
        let argument = if case.argument.starts_with("--") {
            case.argument.to_owned()
        } else {
            find(case.argument).display().to_string()
        };
        let ran = Command::new(find(case.program))
            .arg(argument)
            .output()
            .map_err(|error| Error::new(format!("running {}: {error}", case.program)))?;
        let mut output = String::from_utf8_lossy(&ran.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&ran.stderr));
        if let Some(why) = judge(case, ran.status.code(), &output) {
            return Err(Error::new(format!("on the host: {why}:\n{output}")));
        }
        println!("  host: {} exited {} as it must", case.name, case.status);
    }
    Ok(())
}

/// The shell script that runs every case on Ferrix: each program with its
/// argument, its lines prefixed with the case, then its status.
fn script() -> String {
    let mut script =
        String::from("export PATH=/bin HOME=/tmp\n[ -x /data/nvrm-link-test ] || exit 3\n");
    for case in CASES {
        let argument = if case.argument.starts_with("--") {
            case.argument.to_owned()
        } else {
            format!("/data/{}", case.argument)
        };
        script.push_str(&format!(
            "/data/{} {argument} > /tmp/case.log 2>&1\nstatus=$?\n\
             sed 's/^/nvrm-link {}: /' /tmp/case.log\n\
             echo \"nvrm-link {}: exit $status\"\n",
            case.program, case.name, case.name
        ));
    }
    script.push_str("exit 17\n");
    script
}

/// What the script exits with once every case has run.
const STATUS: i32 = 17;

/// Run every case on Ferrix, from a volume holding `files`.
fn on_ferrix(arch: Arch, files: &[(String, PathBuf)], args: &Args) -> Result<()> {
    let image_path = paths::build_dir(arch).join("nvrm-link-volume.img");
    let inside: Vec<(&str, PathBuf)> = files
        .iter()
        .map(|(name, path)| (name.as_str(), path.clone()))
        .collect();
    nvrm::volume(&image_path, &inside)?;
    let mut args = args.clone();
    args.data_image = Some(image_path);
    if !args.timeout_given {
        args.timeout = 300;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let script = script();
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &[])?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running the cases on Ferrix from a volume (timeout {}s)",
        args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let log = paths::build_dir(arch).join("serial.log");
    let exited = lines
        .iter()
        .find_map(|line| line.trim().split(shell::EXITED).nth(1))
        .map(str::trim);
    if exited != Some(STATUS.to_string().as_str()) {
        return Err(Error::new(format!(
            "{arch}: the cases' script ended with {exited:?}, not {STATUS} (3: no volume).\n  \
             Serial output is in {}",
            log.display()
        )));
    }
    for case in CASES {
        let prefix = format!("nvrm-link {}: ", case.name);
        let said: Vec<&str> = lines
            .iter()
            .filter_map(|line| {
                line.find(&prefix)
                    .and_then(|at| line.get(at + prefix.len()..))
            })
            .collect();
        let status = said
            .iter()
            .find_map(|line| line.strip_prefix("exit "))
            .and_then(|status| status.trim().parse().ok());
        if let Some(why) = judge(case, status, &said.join("\n")) {
            return Err(Error::new(format!(
                "{arch}: {why}.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: {} exited {} as it must", case.name, case.status);
    }
    Ok(())
}

/// The gate, on x86-64, the one architecture nvrm is built for.
pub(crate) fn test_nvrm_link(args: &Args) -> Result<()> {
    if args.arches()? != [Arch::X86_64] {
        return Err(Error::new(
            "test-nvrm-link runs on x86_64 only: nvrm is built for the x86-64 Linux ABI",
        ));
    }
    let programs = nvrm::build()?;
    let directory = paths::target_dir().join("nvrm-link-controls");
    let files = stage(&programs, &directory)?;
    on_host(&directory, &files)?;
    on_ferrix(Arch::X86_64, &files, args)
}

#[cfg(test)]
mod tests;
