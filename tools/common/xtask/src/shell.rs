//! Stage 7's exit criterion, as a script with an expected transcript.
//!
//! "A static musl `busybox sh` starts, runs a script, and exits." The script
//! is given with `-c`, which puts it in `argv` and needs no filesystem: the
//! roadmap's criterion read as "runs a script", not "runs a script *file*",
//! because a file is stage 8's and would make this stage's exit wait on it.
//!
//! # What the script exercises
//!
//! The parts of a shell that are the shell rather than somebody else's
//! program: variables and arithmetic, a loop, a function with an argument,
//! `test`, `case`, `printf`, the positional parameters, and an exit status
//! the kernel reports back. Nothing that forks, because `clone`, `execve` and
//! `wait4` do not exist yet -- a command substitution or an external command
//! would fail, and the test would be measuring that rather than the ABI.
//!
//! `printf` was left out until stage 8 for a reason of its own: busybox's
//! builtin asks `fcntl(1, F_GETFL)` before it writes and prints nothing when
//! that fails. Descriptor 1 is now an open file in a descriptor table, and the
//! line below is what proves `fcntl` answers it.
//!
//! # Which shell runs it
//!
//! zinc, the shell in `src/user/system/linux/zinc/`, unless `--init` names another. Both are worth
//! running and the gate runs both: zinc is the image's shell, and a static
//! busybox is *somebody else's* binary, which is what stage 7's exit was
//! about — a shell written against this kernel works by construction and
//! proves less about the ABI.
//!
//! A static busybox is built by somebody else for each architecture, and
//! which one to trust is a decision for whoever runs this; `--init` names it.
//! Alpine's `busybox-static` package is what this was first run against:
//! static musl, one build per architecture, and no knowledge of Ferrix.
//!
//! The transcript below is the same under either. It did not move when zinc
//! took over, which is the whole of what `docs/UUTILS.md` S4 had to prove
//! about the script.

use std::path::{Path, PathBuf};

use ferrix_elf::Elf;

use crate::paths::{self, Arch};
use crate::{Error, Result, ports};

/// The script `sh -c` runs.
pub(crate) const SCRIPT: &str = r#"echo "script: started"
n=0
for i in 1 2 3 4 5; do n=$((n + i)); done
echo "script: the sum is $n"
greet() { echo "script: hello, $1"; }
greet ferrix
if [ "$n" -eq 15 ]; then echo "script: test agrees"; fi
case ferrix in fer*) echo "script: case matched";; esac
set -- a b c
echo "script: $# positional parameters"
printf 'script: %s %d\n' printf 42
exit 7
"#;

/// `cargo xtask test-linuxd --init`'s script: milestone 4's paths through linuxd
/// (`docs/microkernel/M5-LIFT.md` §7), each printing a line only it can.
pub(crate) const LIFT_SCRIPT: &str = r#"echo "lift: started"
a=$(echo forked)
echo "lift: $a through a pipe"
echo piped | { read line; echo "lift: $line both ends"; }
sh -c 'exit 5'; echo "lift: exec status $?"
sh -c 'kill -TERM $$'; echo "lift: killed status $?"
trap 'echo "lift: trapped USR1"' USR1
kill -USR1 $$
(sleep 0; echo "lift: subshell") &
wait $!
echo written > /tmp/lift; echo "lift: $(cat /tmp/lift) in tmpfs"
cat /proc/self/maps > /tmp/maps; echo "lift: maps read"
exit 9
"#;

/// `cargo xtask test-btrfsd`'s script: a volume mounted and read through `btrfsd`.
pub(crate) const BTRFSD_SCRIPT: &str = r#"echo "btrfsd-row: started"
mkdir -p /mnt
mount -t btrfs -o ro /dev/vdb /mnt; echo "btrfsd-row: mount status $?"
ls /mnt > /tmp/listed; echo "btrfsd-row: listed $(wc -l < /tmp/listed) names"
find /mnt -type f | head -n 200 > /tmp/files
while read file; do cat "$file" | wc -c >/dev/null || echo "btrfsd-row: could not read $file"; done < /tmp/files
echo "btrfsd-row: read $(wc -l < /tmp/files) files"
sha256sum /mnt/big.txt
mkdir -p /rw
mount -t btrfs /dev/vdc /rw; echo "btrfsd-row: writable mount status $?"
mkdir -p /rw/row/here && echo through-the-server > /rw/row/here/note
ln -s here/note /rw/row/link; ln /rw/row/here/note /rw/row/hard
sync; echo "btrfsd-row: wrote $(cat /rw/row/link)"
mv /rw/row/hard /rw/row/moved; rm /rw/row/here/note; echo "btrfsd-row: survives its first name $(cat /rw/row/moved)"
echo more >> /rw/row/moved; echo "btrfsd-row: appended $(wc -c < /rw/row/moved)"
sync; umount /rw; echo "btrfsd-row: unmount status $?"
exit 9
"#;

/// `cargo xtask test-btrfsd-restart`'s script. The dying disk's read goes on over
/// each next driver, the hanging disk's fails within its bound, and one that does
/// neither reads as it did, beside them.
pub(crate) const BTRFSD_RESTART_SCRIPT: &str = r#"echo "btrfsd-row: started"
dd if=/dev/vda bs=4096 count=900 2>/dev/null | sha256sum
sha256sum /dev/vdc
echo "btrfsd-row: vdc read status $?"
sha256sum /dev/vdb
echo "btrfsd-row: vdb read status $?"
exit 9
"#;

/// What [`BTRFSD_SCRIPT`] runs.
pub(crate) fn btrfsd_carried(busybox: &Path) -> Result<Vec<ports::File>> {
    let bytes = std::fs::read(busybox)
        .map_err(|error| Error::new(format!("reading {}: {error}", busybox.display())))?;
    Ok([
        "bin/sh",
        "bin/cat",
        "bin/mount",
        "bin/mkdir",
        "bin/ls",
        "bin/wc",
        "bin/find",
        "bin/head",
        "bin/sha256sum",
        "bin/dd",
        "bin/ln",
        "bin/sync",
        "bin/mv",
        "bin/rm",
        "bin/umount",
    ]
    .into_iter()
    .map(|path| ports::File {
        path: path.to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(bytes.clone()),
    })
    .collect())
}

/// What [`LIFT_SCRIPT`] runs: busybox under each applet's name.
pub(crate) fn lift_carried(busybox: &Path) -> Result<Vec<ports::File>> {
    let bytes = std::fs::read(busybox)
        .map_err(|error| Error::new(format!("reading {}: {error}", busybox.display())))?;
    Ok(["bin/sh", "bin/cat", "bin/sleep", "bin/kill"]
        .into_iter()
        .map(|path| ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(bytes.clone()),
        })
        .collect())
}

/// [`LIFT_SCRIPT`]'s lines, in order.
pub(crate) const LIFT_EXPECTED: &[&str] = &[
    "lift: started",
    "lift: forked through a pipe",
    "lift: piped both ends",
    "lift: exec status 5",
    "lift: killed status 143",
    "lift: trapped USR1",
    "lift: subshell",
    "lift: written in tmpfs",
    "lift: maps read",
];

/// The lines the script must print, in this order.
pub(crate) const EXPECTED: &[&str] = &[
    "script: started",
    "script: the sum is 15",
    "script: hello, ferrix",
    "script: test agrees",
    "script: case matched",
    "script: 3 positional parameters",
    "script: printf 42",
];

/// The status the script exits with. Not zero, so a shell that died and
/// reported success cannot pass.
pub(crate) const STATUS: i32 = 7;

/// The kernel's line when the first program exits, before the status.
pub(crate) const EXITED: &str = "init     the shell exited with";

/// The kernel's line when the first program could not be started at all.
pub(crate) const NOT_STARTED: &str = "init     the shell could not be started";

/// The files a dynamically linked `--init` needs beside it: `--interpreter`
/// at the path the program's `PT_INTERP` names, and each `--library` in
/// `/lib` under its own file name.
///
/// The interpreter's path is read from the program rather than given, so the
/// test cannot pass by putting a linker somewhere the program would not have
/// looked. `/lib` for the libraries because both linkers this has to serve
/// search it with no configuration: glibc's has it in its built-in list after
/// the multiarch directories, and ferrousli's has it first.
///
/// # Errors
///
/// A program that names no interpreter when one was given, and any file that
/// cannot be read.
pub(crate) fn carried(
    program: &Path,
    interpreter: Option<&Path>,
    libraries: &[PathBuf],
) -> Result<Vec<ports::File>> {
    let mut files = Vec::new();
    if let Some(interpreter) = interpreter {
        let image = read(program)?;
        let elf = Elf::parse(&image)
            .map_err(|error| Error::new(format!("{}: {error:?}", program.display())))?;
        let path = match elf.interpreter() {
            Some(Ok(path)) => String::from_utf8_lossy(path).into_owned(),
            Some(Err(error)) => {
                return Err(Error::new(format!("{}: {error:?}", program.display())));
            }
            None => {
                return Err(Error::new(format!(
                    "{} names no interpreter, so --interpreter has nowhere to go",
                    program.display()
                )));
            }
        };
        let Some(path) = path.strip_prefix('/') else {
            return Err(Error::new(format!(
                "{} names the interpreter {path}, which is not absolute",
                program.display()
            )));
        };
        files.push(ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(read(interpreter)?),
        });
    }
    for library in libraries {
        let name = library
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::new(format!("{} has no file name", library.display())))?;
        files.push(ports::File {
            path: format!("lib/{name}"),
            mode: 0o755,
            content: ports::Content::Bytes(read(library)?),
        });
    }
    Ok(files)
}

/// [`carried`] for `test-shell`'s `--interpreter` and `--library` as given:
/// `{arch}` replaced, and [`FERROUSLI`] naming ferrousli's own loader and
/// `libc.so.6`, built from this tree once for both flags.
///
/// # Errors
///
/// As [`carried`] and [`ferrousli_shared`].
pub(crate) fn carried_for(
    arch: Arch,
    program: &Path,
    args: &crate::args::Args,
) -> Result<Vec<ports::File>> {
    let mut shared = None;
    let mut expand = |path: &String, file: &str| -> Result<PathBuf> {
        if path != FERROUSLI {
            return Ok(PathBuf::from(path.replace("{arch}", arch.name())));
        }
        let dir = match &shared {
            Some(dir) => PathBuf::clone(dir),
            None => shared.insert(ferrousli_shared(arch)?).clone(),
        };
        Ok(dir.join(file))
    };
    let interpreter = match &args.interpreter {
        Some(path) => Some(expand(path, "ld.so")?),
        None => None,
    };
    let libraries = args
        .libraries
        .iter()
        .map(|path| expand(path, "libc.so.6"))
        .collect::<Result<Vec<_>>>()?;
    carried(program, interpreter.as_deref(), &libraries)
}

/// The name `--interpreter` and `--library` take for ferrousli's own: its
/// loader, and itself linked as `libc.so.6`.
pub(crate) const FERROUSLI: &str = "ferrousli";

/// Build ferrousli's loader and `libc.so.6` for `arch` with
/// `src/user/system/linux/ferrousli/tools/build-shared.sh`, and answer the directory holding them
/// as `ld.so` and `libc.so.6`.
///
/// Built every time rather than when stale: cargo is incremental, and the
/// link that follows it is seconds, where a stale pair in a gate would
/// measure the wrong tree.
///
/// # Errors
///
/// A Windows host without WSL (the script is bash with binutils' `nm`, and
/// links x86-64's with the host's Linux `cc`), and a failed build.
pub(crate) fn ferrousli_shared(arch: Arch) -> Result<PathBuf> {
    let out = paths::build_dir(arch).join("ferrousli-shared");
    let ferrousli = paths::workspace_root().join("src/user/system/linux/ferrousli");
    if cfg!(windows) {
        // The script in WSL, as `check --ferrousli` runs its cargo steps,
        // writing into this checkout's build directory through `/mnt`.
        crate::wsl::require_toolchain(
            "ferrousli's libc.so.6 is built by a bash script with a Linux host's tools",
        )?;
        let status = crate::wsl::bash(
            &ferrousli,
            "exec tools/build-shared.sh --arch \"$1\" \"$(wslpath -u \"$2\")\"",
            &[arch.name(), &out.to_string_lossy()],
        )
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|error| Error::new(format!("could not run wsl.exe: {error}")))?;
        if !status.success() {
            return Err(Error::new(format!(
                "tools/build-shared.sh --arch {arch} in WSL: {status}"
            )));
        }
        return Ok(out);
    }
    // A build `FERRIX_BUILDS` may record or replay, as busybox's is.
    let mut build = crate::builds::Build::bash(
        "src/user/system/linux/ferrousli/tools/build-shared.sh",
        &ferrousli,
    )
    .args(["tools/build-shared.sh", "--arch", arch.name()])
    .args([out.as_os_str()])
    .output(&out);
    if let Some(dir) = crate::ferrousli::target_dir(std::env::var_os("CARGO_TARGET_DIR")) {
        build = build.env("CARGO_TARGET_DIR", dir);
    }
    build.run()?;
    Ok(out)
}

/// A file's bytes, or an error that names it.
fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ELF64 header with one `PT_INTERP` segment holding `interp`.
    fn with_interpreter(interp: &[u8]) -> Vec<u8> {
        let mut image = vec![0_u8; 64 + 56];
        image[0..4].copy_from_slice(b"\x7fELF");
        image[4] = 2; // ELFCLASS64
        image[5] = 1; // ELFDATA2LSB
        image[6] = 1; // EV_CURRENT
        image[16..18].copy_from_slice(&3_u16.to_le_bytes()); // ET_DYN
        image[18..20].copy_from_slice(&62_u16.to_le_bytes()); // EM_X86_64
        image[20..24].copy_from_slice(&1_u32.to_le_bytes());
        image[32..40].copy_from_slice(&64_u64.to_le_bytes()); // e_phoff
        image[52..54].copy_from_slice(&64_u16.to_le_bytes()); // e_ehsize
        image[54..56].copy_from_slice(&56_u16.to_le_bytes()); // e_phentsize
        image[56..58].copy_from_slice(&1_u16.to_le_bytes()); // e_phnum
        let at = image.len() as u64;
        let header = &mut image[64..120];
        header[0..4].copy_from_slice(&3_u32.to_le_bytes()); // PT_INTERP
        header[4..8].copy_from_slice(&4_u32.to_le_bytes()); // PF_R
        header[8..16].copy_from_slice(&at.to_le_bytes());
        header[32..40].copy_from_slice(&(interp.len() as u64).to_le_bytes());
        header[40..48].copy_from_slice(&(interp.len() as u64).to_le_bytes());
        header[48..56].copy_from_slice(&1_u64.to_le_bytes());
        image.extend_from_slice(interp);
        image
    }

    fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("xtask-shell-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_interpreter_goes_where_the_program_asks_and_libraries_in_lib() {
        let program = scratch("prog", &with_interpreter(b"/lib64/ld-linux-x86-64.so.2\0"));
        let linker = scratch("ld.so", b"the linker");
        let libc = scratch("libc.so.6", b"the library");
        let files = carried(&program, Some(&linker), &[libc]).unwrap();
        let placed: Vec<_> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(placed, ["lib64/ld-linux-x86-64.so.2", "lib/libc.so.6"]);
        assert!(matches!(&files[0].content, ports::Content::Bytes(b) if b == b"the linker"));
    }

    #[test]
    fn a_static_program_has_nowhere_to_put_an_interpreter() {
        let mut image = with_interpreter(b"x\0");
        image[56..58].copy_from_slice(&0_u16.to_le_bytes()); // no segments
        let program = scratch("static", &image);
        let linker = scratch("ld2.so", b"");
        assert!(carried(&program, Some(&linker), &[]).is_err());
    }
}
