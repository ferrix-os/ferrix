//! `test-rustc`: stage 16's exit, `rustc hello.rs && ./hello` on Ferrix,
//! with a smoke check that the matching Cargo starts too.
//!
//! The compiler is the rust-lang.org release and not one built here: a
//! position-independent glibc program whose LLVM is a 190 MiB shared library
//! of its own, run by Debian's `ld-linux`. It links the way it does on any
//! Linux machine, through `cc` -- gcc 15's driver -- which runs
//! `collect2`, which runs the `ld.lld` rustc points it at, which runs
//! `rust-lld`. So one compile is five programs nobody here wrote, four
//! `execve`s deep, over some 350 MiB of shared libraries mapped from btrfs;
//! the program it makes is a sixth, run from the shell.
//!
//! # Where the compiler lives
//!
//! On a btrfs volume, `tools/common/fetch/fetch-rustc-sysroot.sh` makes it from pinned
//! downloads. It carries no `ferrix-root` label, so the kernel mounts it at
//! `/data`, as it does any other btrfs disk; this test attaches it under
//! QEMU's `snapshot=on` so the image is never changed by a run. The volume
//! holds a Debian-shaped tree at its root and the toolchain under `rust/`.
//!
//! glibc's and gcc's own paths are absolute -- `PT_INTERP` names
//! `/lib64/ld-linux-x86-64.so.2`, the linker searches
//! `/lib/x86_64-linux-gnu`, `libc.so` names both, and gcc looks in
//! `/usr/lib/gcc` and `/usr/libexec` -- so the initramfs carries a symbolic
//! link for each into `/data` ([`LINKS`]). `/usr` itself stays the
//! initramfs's, where the ports are.
//!
//! # On ferrousli
//!
//! With `--interpreter ferrousli --library ferrousli`, as `test-chrome` takes
//! them, every program on the volume runs on ferrousli's loader and
//! `libc.so.6` in glibc's place: the loader goes where their `PT_INTERP`
//! names, instead of the link to Debian's, and `libc.so.6` into `/lib`, which
//! `LD_LIBRARY_PATH` puts first. That is `rustc`, `cargo`, the gcc driver,
//! `collect2`, `rust-lld` and the program they make -- LLVM and
//! `librustc_driver` among the libraries, which found in ferrousli's loader
//! `$ORIGIN`, program headers past the first page, and dependency-first
//! constructors, and in the library glibc's mutex layout.
//!
//! # Why x86-64 only
//!
//! The volume holds x86-64 binaries. An AArch64 sysroot is the same script
//! with other packages, and is not needed for the exit.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, shell, zinc};

/// The script the shell runs. Version checks first, so a failure says whether
/// the toolchain ran at all or only its link failed.
const SCRIPT: &str = r#"export PATH=/data/rust/bin:/data/usr/bin:/bin
cd /tmp
rustc -vV || exit 3
cargo -V || exit 6
echo 'fn main() { println!("rustc-gate: hello from rustc on Ferrix"); }' > hello.rs
rustc hello.rs || exit 4
./hello || exit 5
echo '#include <stdio.h>' > hello.c
echo 'int main(void) { puts("rustc-gate: hello from gcc on Ferrix"); return 0; }' >> hello.c
cc hello.c -o hello-c || exit 7
./hello-c || exit 8
while read -r l; do echo "cold $l"; done < /proc/ferrix-seam
rustc hello.rs -o hello-again || exit 4
while read -r l; do echo "warm $l"; done < /proc/ferrix-seam
exit 16
"#;

/// The search path that finds ferrousli's `libc.so.6` in `/lib` before the
/// volume's libraries.
const LIBRARY_PATH: &str = "/lib:/lib/x86_64-linux-gnu";

/// The script, with ferrousli's search path first when the compiler runs on
/// it.
fn script(ferrousli: bool) -> String {
    if ferrousli {
        format!("export LD_LIBRARY_PATH={LIBRARY_PATH}\n{SCRIPT}")
    } else {
        SCRIPT.to_owned()
    }
}

/// What the program rustc made must print.
const HELLO: &str = "rustc-gate: hello from rustc on Ferrix";

/// What the program gcc made must print: `cc` compiles C as well as links,
/// which a volume without `cc1` could not, although rustc passed.
const C_HELLO: &str = "rustc-gate: hello from gcc on Ferrix";

/// What `rustc -vV` prints first, which says the compiler ran.
const VERSION: &str = "rustc 1.97.1";

/// The matching Cargo release must start on the same glibc sysroot.
const CARGO_VERSION: &str = "cargo 1.97.1";

/// The status the script exits with when every step succeeded.
const STATUS: i32 = 16;

/// Each path glibc or gcc names absolutely, and where on the volume it is.
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/gcc", "/data/usr/lib/gcc"),
    ("usr/libexec", "/data/usr/libexec"),
];

/// Normal boots also carry ports under `/usr/libexec`, so only gcc's child
/// directory can be linked there. Put the compiler and its C driver on the
/// shell's `/bin` PATH; the gate sets its own PATH instead. glibc's and the
/// kernel's headers are the volume's `/usr/include`, where gcc looks: without
/// them `cc hello.c` stops at `stdio.h`. The editor and its language
/// server are there too (`docs/DEV-ON-FERRIX.md`, D1): helix finds its
/// runtime beside its program, and rust-analyzer on `PATH`. On a volume
/// fetched before 2026-10-11 the two links lead nowhere, and `hx` is not
/// found, as on a system without it.
pub(crate) const DEFAULT_LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/gcc", "/data/usr/lib/gcc"),
    ("usr/libexec/gcc", "/data/usr/libexec/gcc"),
    ("usr/include", "/data/usr/include"),
    ("bin/rustc", "/data/rust/bin/rustc"),
    ("bin/cargo", "/data/rust/bin/cargo"),
    // cargo runs rustdoc by name from PATH for a library's doctests:
    // without it `cargo test` on any lib crate ends in "could not execute
    // process `rustdoc`".
    ("bin/rustdoc", "/data/rust/bin/rustdoc"),
    ("bin/cc", "/data/usr/bin/cc"),
    ("bin/gcc", "/data/usr/bin/gcc"),
    ("bin/hx", "/data/helix/hx"),
    ("bin/rust-analyzer", "/data/rust/bin/rust-analyzer"),
];

/// Guest memory unless `--memory` says otherwise: the compiler's libraries
/// alone are 350 MiB, and the page cache holds them all.
const MEMORY: u32 = 4096;

/// Seconds to wait unless `--timeout` says otherwise. Generous for `tcg`,
/// where LLVM runs emulated.
const TIMEOUT: u64 = 1800;

/// Where `tools/common/fetch/fetch-rustc-sysroot.sh` writes, unless
/// `FERRIX_RUSTC_SYSROOT` names another directory.
fn directory() -> Result<std::path::PathBuf> {
    match std::env::var_os("FERRIX_RUSTC_SYSROOT") {
        Some(directory) => Ok(std::path::PathBuf::from(directory)),
        None => crate::paths::volume_directory("rustc"),
    }
}

/// The volume, in [`directory`].
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let image = directory()?.join("rustc.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-rustc-sysroot.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// The tree the volume was made from, which the script keeps beside it:
/// what `test-selfhost` puts on a volume of its own, with the source.
///
/// # Errors
///
/// When it is not there, or is from before the script fetched Cargo and the
/// standard libraries Ferrix's x86-64 image is built against.
pub(crate) fn tree() -> Result<std::path::PathBuf> {
    let tree = directory()?.join("tree");
    let needed = [
        "rust/bin/cargo",
        "rust/lib/rustlib/x86_64-unknown-none/lib",
        "rust/lib/rustlib/x86_64-unknown-uefi/lib",
    ];
    if let Some(missing) = needed.iter().find(|path| !tree.join(path).exists()) {
        return Err(Error::new(format!(
            "{} has no {missing}: run tools/common/fetch/fetch-rustc-sysroot.sh again",
            tree.display()
        )));
    }
    Ok(tree)
}

/// Attach the installed toolchain to a person-facing x86-64 boot. A machine
/// without the optional download still boots, but an explicitly named
/// `FERRIX_RUSTC_SYSROOT` must be valid rather than silently ignored.
pub(crate) fn prepare_default(arch: Arch, args: &mut Args) -> Result<()> {
    if arch != Arch::X86_64 {
        return Ok(());
    }
    let image = match volume() {
        Ok(image) => image,
        Err(error) if std::env::var_os("FERRIX_RUSTC_SYSROOT").is_some() => return Err(error),
        Err(error) => {
            println!("  rustc not in this boot: {error}");
            return Ok(());
        }
    };
    println!("  rustc from {} in the default system", image.display());
    if let Some(missing) = missing_compiler() {
        println!(
            "  warning: the volume's gcc has no {missing}, so no C program compiles on it; \
             it is from before the script fetched gcc 15 -- run \
             tools/common/fetch/fetch-rustc-sysroot.sh again"
        );
    }
    args.data_image = Some(image);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    Ok(())
}

/// What the script fetches since 2026-09-24 and a volume made before then
/// lacks: gcc 14's driver came without `cpp-14`, whose package holds `cc1`,
/// so `cc hello.c` failed on Ferrix although rustc, which only links through
/// gcc, worked.
const COMPILER: &str = "usr/libexec/gcc/x86_64-linux-gnu/15/cc1";

/// [`COMPILER`], when the tree beside the volume shows the volume lacks it.
/// A volume whose tree is gone cannot be judged, and is taken as it is.
fn missing_compiler() -> Option<&'static str> {
    let tree = directory().ok()?.join("tree");
    (tree.is_dir() && !tree.join(COMPILER).exists()).then_some(COMPILER)
}

/// Links in the initramfs (and persistent root) for the attached compiler.
pub(crate) fn default_links(args: &Args) -> Vec<ports::File> {
    if args.data_image.is_some() {
        files(DEFAULT_LINKS)
    } else {
        Vec::new()
    }
}

/// `links`, as the symbolic links an initramfs carries.
pub(crate) fn files(links: &[(&str, &str)]) -> Vec<ports::File> {
    links
        .iter()
        .map(|(path, target)| ports::File {
            path: (*path).to_owned(),
            mode: 0o777,
            content: ports::Content::Link((*target).to_owned()),
        })
        .collect()
}

/// Boot a shell whose script compiles a program with rustc and runs it.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// any step of the script does not do what it must.
pub(crate) fn test_rustc(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-rustc runs on x86-64 only: the volume holds x86-64 binaries",
        ));
    }
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }

    let ferrousli = args.interpreter.is_some() || !args.libraries.is_empty();

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let libc = if ferrousli { "ferrousli" } else { "glibc" };
    println!("  {arch}: building an image whose shell compiles with rustc on {libc}");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script(ferrousli))?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let mut links = if ferrousli {
        // The loader takes `/lib64`'s place, so the link to the volume's goes.
        let kept: Vec<_> = LINKS
            .iter()
            .copied()
            .filter(|(path, _)| *path != "lib64")
            .collect();
        let mut links = files(&kept);
        links.extend(shell::carried_for(
            arch,
            &tree()?.join("rust/bin/rustc"),
            &args,
        )?);
        links
    } else {
        files(LINKS)
    };
    // `selfhost` links the headers beside [`LINKS`] itself, so they are
    // this gate's own addition, for its hello.c.
    links.extend(files(&[("usr/include", "/data/usr/include")]));
    // zinc alone: the script is builtins, and every program it runs is on
    // the volume.
    let archive = initramfs::build(None, &natives, Some(&bytes), &links)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: compiling hello.rs on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    judge(arch, &lines)
}

/// Whether the transcript is a compiler and Cargo that ran, a program rustc
/// made and one gcc made that printed their lines, and a script that got to
/// its end.
fn judge(arch: Arch, lines: &[String]) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    let exited = after_boot
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    let ran = after_boot.iter().any(|line| line.starts_with(VERSION));
    let cargo_ran = after_boot
        .iter()
        .any(|line| line.starts_with(CARGO_VERSION));
    let said = after_boot.iter().any(|line| line.trim_end() == HELLO);
    let c_said = after_boot.iter().any(|line| line.trim_end() == C_HELLO);
    match exited {
        Some(status) if status == STATUS.to_string() && ran && cargo_ran && said && c_said => {
            println!(
                "  {arch}: rustc compiled hello.rs on Ferrix, Cargo started, gcc compiled \
                 hello.c, and both ran"
            );
            // What crossed the seam, as the kernel counted it: the second
            // "seam measured" row (docs/OPAQUE-KERNEL.md, S0). The counters
            // after the cold run, and after compiling hello.rs again on a
            // warm page cache; the second compile is their difference.
            for (label, line) in seam_lines(after_boot) {
                println!("  {arch}: {label} seam {line}");
            }
            if let Some(delta) = warm_delta(after_boot) {
                println!("  {arch}: the warm compile alone: {delta}");
            }
            Ok(())
        }
        Some("3") => Err(Error::new(format!("{arch}: `rustc -vV` failed"))),
        Some("6") => Err(Error::new(format!("{arch}: `cargo -V` failed"))),
        Some("4") => Err(Error::new(format!(
            "{arch}: rustc ran but did not compile and link hello.rs"
        ))),
        Some("5") => Err(Error::new(format!(
            "{arch}: rustc made a program, and it did not run"
        ))),
        Some("7") => Err(Error::new(format!(
            "{arch}: gcc did not compile hello.c (a volume without cc1: run \
             tools/common/fetch/fetch-rustc-sysroot.sh again)"
        ))),
        Some("8") => Err(Error::new(format!(
            "{arch}: gcc made a program, and it did not run"
        ))),
        Some(status) => Err(Error::new(format!(
            "{arch}: the script exited with {status}; rustc version {ran}, cargo version {cargo_ran}, hello line {said}, gcc's hello line {c_said}"
        ))),
        None => Err(Error::new(format!("{arch}: the shell never exited"))),
    }
}

/// The counters the script printed, labelled `cold` and `warm`, from the lines
/// the shell ran rather than the ones it echoed.
fn seam_lines(lines: &[String]) -> Vec<(&'static str, String)> {
    ["cold", "warm"]
        .into_iter()
        .filter_map(|label| {
            let prefix = format!("{label} seam syscalls ");
            lines.iter().find_map(|line| {
                let at = line.find(&prefix)?;
                let rest = line.get(at + label.len() + " seam ".len()..)?;
                Some((label, rest.trim().to_owned()))
            })
        })
        .collect()
}

/// The warm compile's own counters: `warm` less `cold`, field by field.
fn warm_delta(lines: &[String]) -> Option<String> {
    let seen = seam_lines(lines);
    let [(_, cold), (_, warm)] = seen.as_slice() else {
        return None;
    };
    let numbers = |text: &str| -> Vec<(String, u64)> {
        let words: Vec<&str> = text.split_whitespace().collect();
        words
            .chunks(2)
            .filter_map(|pair| match pair {
                [name, value] => Some(((*name).to_owned(), value.parse().ok()?)),
                _ => None,
            })
            .collect()
    };
    let (cold, warm) = (numbers(cold), numbers(warm));
    if cold.len() != warm.len() || cold.is_empty() {
        return None;
    }
    let parts: Vec<String> = cold
        .iter()
        .zip(&warm)
        .map(|((name, before), (_, after))| format!("{name} {}", after.saturating_sub(*before)))
        .collect();
    Some(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_links_keep_ports_libexec_and_expose_rustc_on_path() {
        let args = Args {
            data_image: Some("rustc.img".into()),
            ..Args::default()
        };
        let links = default_links(&args);
        assert!(links.iter().any(|file| file.path == "bin/rustc"));
        assert!(links.iter().any(|file| file.path == "bin/cargo"));
        assert!(links.iter().any(|file| file.path == "bin/rustdoc"));
        assert!(links.iter().any(|file| file.path == "bin/cc"));
        assert!(links.iter().any(|file| file.path == "usr/libexec/gcc"));
        assert!(links.iter().any(|file| file.path == "usr/include"));
        assert!(!links.iter().any(|file| file.path == "usr/libexec"));
        assert!(default_links(&Args::default()).is_empty());
    }

    #[test]
    fn default_toolchain_links_coexist_with_libexec_ports() {
        let args = Args {
            data_image: Some("rustc.img".into()),
            ..Args::default()
        };
        let mut carried = vec![ports::File {
            path: "usr/libexec/git-core/git".to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(b"git".to_vec()),
        }];
        carried.extend(default_links(&args));
        let bytes = initramfs::build(None, &[], None, &carried).unwrap();
        let archive = ferrix_cpio::Archive::new(&bytes);
        assert_eq!(
            archive
                .find("usr/libexec/git-core/git")
                .unwrap()
                .unwrap()
                .data,
            b"git"
        );
        assert_eq!(
            archive.find("usr/libexec/gcc").unwrap().unwrap().data,
            b"/data/usr/libexec/gcc"
        );
        assert_eq!(
            archive.find("bin/rustc").unwrap().unwrap().data,
            b"/data/rust/bin/rustc"
        );
        assert_eq!(
            archive.find("bin/cargo").unwrap().unwrap().data,
            b"/data/rust/bin/cargo"
        );
    }

    fn transcript(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_compile_that_ran_passes() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            "rustc 1.97.1 (8bab26f4f 2026-07-14)",
            "cargo 1.97.1 (test build)",
            HELLO,
            C_HELLO,
            "  init     the shell exited with 16",
        ]);
        assert!(judge(Arch::X86_64, &lines).is_ok());
    }

    /// The Rust half alone is not a pass: a volume without `cc1` gave that.
    #[test]
    fn a_c_compile_is_needed_too() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            "rustc 1.97.1 (8bab26f4f 2026-07-14)",
            "cargo 1.97.1 (test build)",
            HELLO,
            "  init     the shell exited with 16",
        ]);
        assert!(judge(Arch::X86_64, &lines).is_err());
    }

    #[test]
    fn a_hello_before_the_marker_does_not_count() {
        let lines = transcript(&[
            HELLO,
            qemu::SUCCESS_MARKER,
            "rustc 1.97.1 (8bab26f4f 2026-07-14)",
            "cargo 1.97.1 (test build)",
            "  init     the shell exited with 16",
        ]);
        assert!(judge(Arch::X86_64, &lines).is_err());
    }

    #[test]
    fn each_failing_step_is_named() {
        for (status, words) in [
            ("3", "-vV"),
            ("4", "link"),
            ("5", "did not run"),
            ("6", "cargo -V"),
            ("7", "cc1"),
            ("8", "gcc made a program"),
        ] {
            let exit = format!("  init     the shell exited with {status}");
            let lines = transcript(&[qemu::SUCCESS_MARKER, &exit]);
            let error = judge(Arch::X86_64, &lines).unwrap_err().to_string();
            assert!(error.contains(words), "{status}: {error}");
        }
    }
}
