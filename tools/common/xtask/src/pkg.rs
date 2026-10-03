//! The package manager, `/bin/pkg` (`src/user/system/linux/pkg`,
//! `docs/APPS.md` §7): built for an architecture, carried by the images a
//! person runs, and `cargo xtask test-pkg`, the boot that has it install,
//! run and remove an app.
//!
//! The boot runs zinc as init with `pkg`, the stat service's package as
//! `build-apps` makes it, and three packages made here: `pkg-base`, the
//! `pkg-user` that depends on it, and `pkg-base` with one byte of a file
//! changed after its record was written. It requires the stat service
//! installed, run and removed again, and each refusal said: the changed
//! package, `pkg-user` without what it depends on, and `pkg-base`'s removal
//! while `pkg-user` needs it. Each refusal is a negative control of the check
//! behind it, and the installs that go through show the same checks pass a
//! good package.

use std::path::PathBuf;

use ferrix_pkg::manifest;
use ferrix_pkg::record::{self, Installed, Record};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell, zinc};

/// The workspace.
const DIR: &str = "src/user/system/linux/pkg";
/// Where the program goes.
const PROGRAM: &str = "bin/pkg";

/// Build `pkg` for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn build(arch: Arch) -> Result<Option<PathBuf>> {
    let Some(target) = zinc::target(arch) else {
        println!("  pkg is not built for {arch} yet");
        return Ok(None);
    };
    let target_dir = paths::target_dir().join("pkg");
    let program = target_dir.join(target).join("release").join("pkg");
    crate::builds::Build::cargo(
        format!("cargo build (pkg) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    // As an app's: RUSTFLAGS replaces the flags every config file up the
    // tree would merge, and rust-lld is the whole toolchain with the musl
    // targets' own C runtime.
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&program)
    .run()?;
    Ok(Some(program))
}

/// What an image carries for the package manager on `arch`: `/bin/pkg`.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn carried(arch: Arch) -> Result<Vec<File>> {
    let Some(program) = build(arch)? else {
        return Ok(Vec::new());
    };
    Ok(vec![File {
        path: PROGRAM.to_owned(),
        mode: 0o755,
        content: Content::Bytes(read(&program)?),
    }])
}

fn read(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// Where the boot's packages are.
const PACKAGES: &str = "var/lib/pkg-test";

/// A package of `name`, built for `arch`, depending on `depends`, holding
/// one line of text at `bin/<name>`: the boot reads it rather than runs it,
/// since its image has no `/bin/sh` for a script.
fn made(name: &str, depends: &[&str], arch: Arch) -> Result<Vec<u8>> {
    let depends: Vec<String> = depends.iter().map(|name| format!("\"{name}\"")).collect();
    let text = format!(
        "[package]\nname = \"{name}\"\nversion = \"1.0\"\ndescription = \"test-pkg's {name}\"\nlicense = \"MIT\"\n\
         abi = \"linux\"\narches = [\"{arch}\"]\ndepends = [{}]\n\n\
         [[package.files]]\nfrom = \"{name}\"\nto = \"bin/{name}\"\nmode = \"755\"\n",
        depends.join(", "),
        arch = arch.name()
    );
    let mut package = manifest::recipe(&text)
        .map_err(|error| Error::new(format!("test-pkg's {name}: {error}")))?
        .package;
    package.arches = vec![arch.name().to_owned()];
    let script = format!("{name}: here\n").into_bytes();
    let path = format!("bin/{name}");
    let record = Record {
        package,
        files: vec![Installed::of(&path, 0o755, &script)],
    };
    initramfs::package(&[
        File {
            path,
            mode: 0o755,
            content: Content::Bytes(script),
        },
        File {
            path: record::path(name),
            mode: 0o644,
            content: Content::Bytes(record::render(&record).into_bytes()),
        },
    ])
}

/// What the boot's shell runs. Each check echoes `pkg-test: ok <what>`.
const SCRIPT: &str = r#"PATH=/bin:/sbin:/usr/bin
export PATH
P=/var/lib/pkg-test
echo "pkg-test: started"
pkg install $P/statd.fxpkg && echo "pkg-test: ok installed"
pkg list | while read -r line; do case "$line" in statd*) echo "pkg-test: ok listed";; esac; done
pkg info statd | while read -r line; do case "$line" in '/sbin/ferrix-statd '*) echo "pkg-test: ok info";; esac; done
ferrix-statd --seconds 1 | while read -r line; do case "$line" in FERRIX-STAT-END*) echo "pkg-test: ok ran";; esac; done
pkg remove statd && echo "pkg-test: ok removed"
ferrix-statd --seconds 1 || echo "pkg-test: ok gone"
pkg list | while read -r line; do case "$line" in statd*) echo "pkg-test: still listed";; esac; done
pkg install $P/changed.fxpkg || echo "pkg-test: ok refused a changed file"
read -r line < /bin/pkg-base || echo "pkg-test: ok nothing of it went in"
pkg install $P/user.fxpkg || echo "pkg-test: ok refused a missing dependency"
pkg install $P/user.fxpkg $P/base.fxpkg && echo "pkg-test: ok installed together"
read -r line < /bin/pkg-user && case "$line" in 'pkg-user: here') echo "pkg-test: ok user is there";; esac
pkg remove pkg-base || echo "pkg-test: ok refused removing a dependency"
pkg remove pkg-user && pkg remove pkg-base && echo "pkg-test: ok removed both"
"#;

/// The lines the boot must print, in any order.
const WANTED: [&str; 14] = [
    "pkg-test: started",
    "pkg-test: ok installed",
    "pkg-test: ok listed",
    "pkg-test: ok info",
    "pkg-test: ok ran",
    "pkg-test: ok removed",
    "pkg-test: ok gone",
    "pkg-test: ok refused a changed file",
    "pkg-test: ok nothing of it went in",
    "pkg-test: ok refused a missing dependency",
    "pkg-test: ok installed together",
    "pkg-test: ok user is there",
    "pkg-test: ok refused removing a dependency",
    "pkg-test: ok removed both",
];

/// The lines the boot must not print.
const UNWANTED: [&str; 1] = ["pkg-test: still listed"];

/// `cargo xtask test-pkg`: on each architecture, `pkg` installs, runs and
/// removes the stat service, and refuses what it must.
///
/// # Errors
///
/// A build that fails, or a line the boot did not print.
pub(crate) fn test_pkg(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let Some(program) = build(arch)? else {
            continue;
        };
        let statd = crate::apps::built_package("statd", arch, args.release)?
            .ok_or_else(|| Error::new(format!("the statd app is not built for {arch}")))?;
        let base = made("pkg-base", &[], arch)?;
        let changed = initramfs::with_files_changed(&base, |name, data| {
            Ok((name == "bin/pkg-base").then(|| [data, b"changed\n".as_slice()].concat()))
        })?;
        let package = |name: &str, bytes: Vec<u8>| File {
            path: format!("{PACKAGES}/{name}.fxpkg"),
            mode: 0o644,
            content: Content::Bytes(bytes),
        };
        let files = vec![
            File {
                path: PROGRAM.to_owned(),
                mode: 0o755,
                content: Content::Bytes(read(&program)?),
            },
            package("statd", read(&statd)?),
            package("base", base),
            package("changed", changed),
            package("user", made("pkg-user", &["pkg-base"], arch)?),
        ];
        let shell_program = zinc::built(arch)?.ok_or_else(|| {
            Error::new(format!(
                "zinc is not built for {arch}, and test-pkg runs its checks in it"
            ))
        })?;
        let script = format!("{SCRIPT}exit {}\n", shell::STATUS);
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &shell_program, &script)?;
        let natives = native::build(arch, args.release)?;
        let initramfs = initramfs::build(None, &natives, None, &files)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
        let printed = |want: &str| lines.iter().any(|line| line.trim_end().ends_with(want));
        let missing: Vec<&str> = WANTED.into_iter().filter(|want| !printed(want)).collect();
        let unwanted: Vec<&str> = UNWANTED.into_iter().filter(|line| printed(line)).collect();
        if !missing.is_empty() || !unwanted.is_empty() {
            let tail: Vec<&str> = lines
                .iter()
                .rev()
                .take(40)
                .rev()
                .map(String::as_str)
                .collect();
            return Err(Error::new(format!(
                "{arch}: test-pkg is missing [{}] and printed [{}]\n  The boot's last lines:\n    {}",
                missing.join(", "),
                unwanted.join(", "),
                tail.join("\n    ")
            )));
        }
        println!("{arch}: pkg installed, ran and removed the stat service, and refused all three");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SCRIPT, WANTED};

    #[test]
    fn every_wanted_line_is_one_the_script_echoes() {
        for want in WANTED {
            assert!(SCRIPT.contains(&format!("\"{want}\"")), "{want}");
        }
    }
}
