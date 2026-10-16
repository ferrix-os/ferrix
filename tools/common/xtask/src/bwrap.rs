//! `test-bwrap`: Debian's bubblewrap on Ferrix, as root (`docs/NAMESPACES.md`
//! §8, landing N3).
//!
//! bubblewrap is what Steam's container is made with: scout's requirements
//! check runs it before every start, and pressure-vessel builds the container
//! `steamwebhelper` runs in with it. As root it makes a mount namespace alone
//! (§1.4): `clone(CLONE_NEWNS)`, a tmpfs for the new root, binds into it,
//! two `pivot_root`s and the old root detached.
//!
//! # What runs
//!
//! `tools/common/fetch/fetch-bwrap.sh` fetches `bwrap` and the glibc,
//! `libcap`, `libselinux` and `libpcre2` it was linked against; the image
//! carries them where glibc's loader looks, and a static busybox under
//! `/usr/bin` for what runs inside, since the sandboxes bind `/usr` and make
//! `/bin` a link into it. The script then runs, as root:
//!
//! * the requirements check's four argument lists (§1.1), each running
//!   `true`: plain, `--not-a-security-boundary`, `--level-prefix`, and
//!   `--perms 0700 --dir /`;
//! * a container shaped like pressure-vessel's (§1.3): `/usr` read-only,
//!   `--proc`, `/dev` bound, a tmpfs `/tmp`, a directory bound writable, a
//!   file bound read-only, `--ro-bind-data` from a descriptor and
//!   `--new-session`, running a shell that reads each back, writes through
//!   the writable bind, and prints `mountinfo` and `id`.
//!
//! Every list must exit 0, and the container must show what was bound into
//! it and nothing of the tree outside.
//!
//! # Where `/` is
//!
//! In memory, as `test-steam-bootstrap`'s is: the kernel's tmpfs, mounted on
//! the empty bottom mount every namespace has, so that `pivot_root` can move
//! it aside as it would a disk root (`docs/NAMESPACES.md` §2.1).
//!
//! As uid 1000 bubblewrap makes a user namespace too; that half is N4's and
//! N5's, and this gate gains it then.

use std::path::PathBuf;

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-bwrap",
    help_at: 710,
    help: &[
        "test-bwrap    Boot with / in memory and require Debian's bubblewrap, which tools/common/fetch/fetch-bwrap.sh fetches,",
        "              to run Steam's requirements check and a pressure-vessel-shaped container as root (docs/NAMESPACES.md §8)",
    ],
    run: |_, args| test_bwrap(args),
}];

/// Where `fetch-bwrap.sh` writes, unless `FERRIX_BWRAP` names another
/// directory, and what it writes there.
const FETCHED: &[&str] = &[
    "bwrap",
    "ld.so",
    "libc.so.6",
    "libcap.so.2",
    "libselinux.so.1",
    "libpcre2-8.so.0",
];

/// Where each fetched file goes in the image: `bwrap` beside busybox's
/// programs, the loader where its `PT_INTERP` names it, the libraries where
/// glibc's loader looks first.
fn place(name: &str) -> String {
    match name {
        "bwrap" => "usr/bin/bwrap".to_owned(),
        "ld.so" => "lib64/ld-linux-x86-64.so.2".to_owned(),
        library => format!("lib/x86_64-linux-gnu/{library}"),
    }
}

/// The busybox programs a sandbox runs, each a link to `/usr/bin/busybox`.
const SANDBOXED: &[&str] = &["true", "sh", "cat", "id", "echo", "sed"];

/// The requirements check's lists and the container, each followed by a
/// line saying how it exited, then the writable bind read back outside.
const SCRIPT: &str = r#"export PATH=/bin:/usr/bin
B=/usr/bin/bwrap
echo data-from-a-descriptor > /tmp/bwrap-data
echo a-bound-file > /tmp/bwrap-file
mkdir -p /tmp/bwrap-src
echo in-a-bound-directory > /tmp/bwrap-src/inside
$B --ro-bind /etc /etc --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib32 /lib32 --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin --ro-bind /usr /usr --ro-bind-try /gnu/store /gnu/store --ro-bind-try /nix/store /nix/store --bind /proc /proc --dev-bind /dev /dev true
echo "bwrap-gate: plain exited $?"
$B --not-a-security-boundary --ro-bind /etc /etc --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib32 /lib32 --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin --ro-bind /usr /usr --ro-bind-try /gnu/store /gnu/store --ro-bind-try /nix/store /nix/store --bind /proc /proc --dev-bind /dev /dev true
echo "bwrap-gate: not-a-security-boundary exited $?"
$B --level-prefix --ro-bind /etc /etc --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib32 /lib32 --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin --ro-bind /usr /usr --ro-bind-try /gnu/store /gnu/store --ro-bind-try /nix/store /nix/store --bind /proc /proc --dev-bind /dev /dev true
echo "bwrap-gate: level-prefix exited $?"
$B --perms 0700 --dir / --ro-bind /etc /etc --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib32 /lib32 --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin --ro-bind /usr /usr --ro-bind-try /gnu/store /gnu/store --ro-bind-try /nix/store /nix/store --bind /proc /proc --dev-bind /dev /dev true
echo "bwrap-gate: perms exited $?"
$B --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 --proc /proc --dev-bind /dev /dev --tmpfs /tmp --dir /run/check --bind /tmp/bwrap-src /run/src --ro-bind /tmp/bwrap-file /run/file --ro-bind-data 3 /run/data --new-session /usr/bin/sh -c 'for f in /run/file /run/data /run/src/inside; do echo "bwrap-inside: $(cat $f)"; done; echo written > /run/src/back; cat /tmp/bwrap-file 2>/dev/null && echo bwrap-inside: the outer tmp shows; sed "s/^/bwrap-mountinfo: /" /proc/self/mountinfo; echo "bwrap-inside: $(id)"' 3< /tmp/bwrap-data
echo "bwrap-gate: container exited $?"
echo "bwrap-gate: written back: $(cat /tmp/bwrap-src/back)"
exit 21
"#;

/// What the script exits with when it ran to its end.
const STATUS: &str = "21";

/// The lists each of which must exit 0.
const LISTS: &[&str] = &[
    "plain",
    "not-a-security-boundary",
    "level-prefix",
    "perms",
    "container",
];

/// What the container must print: each thing bound into it read back, and
/// root's id.
const INSIDE: &[&str] = &[
    "bwrap-inside: a-bound-file",
    "bwrap-inside: data-from-a-descriptor",
    "bwrap-inside: in-a-bound-directory",
    "bwrap-inside: uid=0",
];

/// The mount points the container's `mountinfo` must list.
const MOUNTED: &[&str] = &[
    "/usr",
    "/proc",
    "/dev",
    "/tmp",
    "/run/src",
    "/run/file",
    "/run/data",
];

/// The directory `fetch-bwrap.sh` wrote.
fn fetched() -> Result<PathBuf> {
    let directory = match std::env::var_os("FERRIX_BWRAP") {
        Some(directory) => PathBuf::from(directory),
        None => crate::paths::volume_directory("bwrap")?,
    }
    .join("x86_64");
    for name in FETCHED {
        if !directory.join(name).is_file() {
            return Err(Error::new(format!(
                "{} is not there: tools/common/fetch/fetch-bwrap.sh fetches it",
                directory.join(name).display()
            )));
        }
    }
    Ok(directory)
}

/// What the image carries beside busybox and zinc: bubblewrap, its loader
/// and libraries, and `busybox` under `/usr/bin` with [`SANDBOXED`] linked
/// to it.
fn files(fetched: &std::path::Path, busybox: &[u8]) -> Result<Vec<File>> {
    let mut files = Vec::new();
    for name in FETCHED {
        let bytes = std::fs::read(fetched.join(name)).map_err(|error| {
            Error::new(format!("reading {}: {error}", fetched.join(name).display()))
        })?;
        files.push(File {
            path: place(name),
            mode: 0o755,
            content: Content::Bytes(bytes),
        });
    }
    files.push(File {
        path: "usr/bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox.to_vec()),
    });
    files.extend(SANDBOXED.iter().map(|name| File {
        path: format!("usr/bin/{name}"),
        mode: 0o777,
        content: Content::Link("busybox".to_owned()),
    }));
    Ok(files)
}

/// Boot a shell whose script runs bubblewrap as root.
///
/// # Errors
///
/// When the files are missing, the image cannot be built, the boot fails, or
/// a list did not do what it must.
pub(crate) fn test_bwrap(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => {
            return Err(Error::new(
                "test-bwrap runs on x86-64: the bubblewrap it runs is Debian's amd64 one",
            ));
        }
    };
    let fetched = fetched()?;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell runs Debian's bubblewrap as root");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let shell_bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let busybox_bytes = std::fs::read(&busybox)
        .map_err(|error| Error::new(format!("reading {}: {error}", busybox.display())))?;
    let carried = files(&fetched, &busybox_bytes)?;
    let archive = initramfs::build(Some(&busybox), &natives, Some(&shell_bytes), &carried)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running bubblewrap with / in memory (timeout {}s)",
        args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
    judge(arch, &lines)
}

/// Whether the script's lines say every list ran and the container showed
/// what was bound into it, and only that.
fn judge(arch: Arch, lines: &[String]) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    let said = |needle: &str| after_boot.iter().any(|line| line.contains(needle));
    let exited = after_boot
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    if exited != Some(STATUS) {
        return Err(Error::new(format!(
            "{arch}: the bubblewrap script ended with {exited:?}, not at its end"
        )));
    }
    for list in LISTS {
        if !said(&format!("bwrap-gate: {list} exited 0")) {
            return Err(Error::new(format!(
                "{arch}: bubblewrap's {list} list did not exit 0; its lines above say why"
            )));
        }
    }
    for line in INSIDE {
        if !said(line) {
            return Err(Error::new(format!(
                "{arch}: the container did not print {line:?}"
            )));
        }
    }
    if said("bwrap-inside: the outer tmp shows") {
        return Err(Error::new(format!(
            "{arch}: the container saw the /tmp outside it through its own tmpfs"
        )));
    }
    let mountinfo: Vec<&str> = after_boot
        .iter()
        .filter_map(|line| line.split_once("bwrap-mountinfo: ").map(|(_, rest)| rest))
        .collect();
    for point in MOUNTED {
        let listed = mountinfo
            .iter()
            .any(|line| line.split(' ').nth(4) == Some(point));
        if !listed {
            return Err(Error::new(format!(
                "{arch}: the container's mountinfo did not list {point}"
            )));
        }
    }
    if !said("bwrap-gate: written back: written") {
        return Err(Error::new(format!(
            "{arch}: a write through the container's writable bind did not show outside it"
        )));
    }
    println!(
        "  {arch}: bubblewrap ran the requirements check's four lists and a pressure-vessel-shaped \
         container as root, {} mounts in it",
        mountinfo.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader is where bwrap's `PT_INTERP` names it, every library where
    /// glibc looks, and every program a sandbox runs is a link to busybox.
    #[test]
    fn the_image_carries_bwrap_where_glibc_looks() {
        assert_eq!(place("ld.so"), "lib64/ld-linux-x86-64.so.2");
        assert_eq!(place("libcap.so.2"), "lib/x86_64-linux-gnu/libcap.so.2");
        assert_eq!(place("bwrap"), "usr/bin/bwrap");
        let dir = std::env::temp_dir().join(format!("bwrap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in FETCHED {
            std::fs::write(dir.join(name), name.as_bytes()).unwrap();
        }
        let files = files(&dir, b"busybox").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        for name in SANDBOXED {
            assert!(
                files
                    .iter()
                    .any(|file| file.path == format!("usr/bin/{name}")
                        && matches!(&file.content, Content::Link(target) if target == "busybox"))
            );
        }
        assert_eq!(files.len(), FETCHED.len() + 1 + SANDBOXED.len());
    }

    /// A line naming every list, and the script's own ending, judged.
    #[test]
    fn the_judge_wants_every_list_and_the_container() {
        let mut lines: Vec<String> = vec![qemu::SUCCESS_MARKER.to_owned()];
        lines.extend(
            LISTS
                .iter()
                .map(|list| format!("bwrap-gate: {list} exited 0")),
        );
        lines.extend(INSIDE.iter().map(|line| (*line).to_owned()));
        lines.extend(
            MOUNTED
                .iter()
                .map(|point| format!("bwrap-mountinfo: 1 1 0:1 / {point} rw - tmpfs tmpfs rw")),
        );
        lines.push("bwrap-gate: written back: written".to_owned());
        lines.push(format!("{}{STATUS}", shell::EXITED));
        assert!(judge(Arch::X86_64, &lines).is_ok());
        lines.retain(|line| !line.contains("perms exited"));
        assert!(judge(Arch::X86_64, &lines).is_err());
    }
}
