//! `bench-ipc`: a channel round trip between two native processes, timed.
//!
//! `docs/OPAQUE-KERNEL.md`'s trip is a disk read through the block ring and a
//! ring-3 driver, device and all. The figure an IPC design is compared by --
//! seL4's, Zircon's -- is narrower: a message to another process and back,
//! nothing else. This boots `--init`'s shell with a script that runs
//! `/sbin/ipc-bench`
//! (`src/user/system/native/ipc-bench`), which starts its own echo server in
//! a cgroup of its own and prints the floor (a native call that does not
//! sleep) and the trip, in nanoseconds.

use crate::args::Args;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell};

/// What the shell runs.
const SCRIPT: &str = r#"/sbin/ipc-bench
echo "ipc-bench: exit $?"
"#;

/// Boot, run the benchmark, and print its lines.
///
/// # Errors
///
/// No `--init`, a build or boot that fails, or a benchmark that did not
/// finish.
pub(crate) fn bench_ipc(args: &Args) -> Result<()> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new("bench-ipc needs --init, a static busybox for each architecture")
    })?;
    for arch in args.arches()? {
        let program = crate::program_for(init, arch)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        let carried = shell::carried_for(arch, &program, args)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        let lines = qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)?;
        let mut finished = false;
        for line in lines.iter().filter(|line| line.contains("ipc-bench")) {
            println!("  {arch}: {}", line.trim());
            finished |= line.contains("ipc-bench: exit 0");
        }
        if !finished {
            return Err(Error::new(format!("{arch}: ipc-bench did not finish")));
        }
    }
    Ok(())
}

/// What the shell runs for `test-ipc-equiv`.
const EQUIV_SCRIPT: &str = r#"/sbin/ipc-equiv
"#;

/// The stage-9 line that says which path `channel_write_read` takes
/// (`ferrix.fastpath`, `src/kernel/src/fastpath.rs`).
const FASTPATH_LINE: &str = "ipc fast path for channel_write_read:";

/// `test-ipc-equiv`: boot `/sbin/ipc-equiv` (`src/user/system/native/ipc-equiv`)
/// with `ferrix.fastpath=off` on every architecture asked for, and on x86-64
/// again with `ferrix.fastpath=on`, the one architecture the fast path is for
/// (`docs/OPAQUE-KERNEL.md` §9.7, part 6). Each boot must print the stage-9
/// line naming the path it was asked for and every case's line, end with
/// `ipc-equiv: exit 0`, and the two x86-64 transcripts must be the same line
/// for line.
///
/// The fast path is not built yet, so `on` takes the general path too, and
/// the comparison shows the cases are deterministic on it: the runner is
/// ready for when the fast path exists.
///
/// # Errors
///
/// No `--init`; a build or boot that fails; a boot whose stage-9 line names
/// the wrong path; a transcript that did not finish; two that differ.
/// Verifies: `L.x86_64.150`
pub(crate) fn test_ipc_equiv(args: &Args) -> Result<()> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new("test-ipc-equiv needs --init, a static busybox for each architecture")
    })?;
    for arch in args.arches()? {
        let program = crate::program_for(init, arch)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, EQUIV_SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        let carried = shell::carried_for(arch, &program, args)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let settings: &[&str] = if arch == crate::paths::Arch::X86_64 {
            &["off", "on"]
        } else {
            &["off"]
        };
        let mut transcripts = Vec::new();
        for setting in settings {
            let cmdline = format!("ferrix.fastpath={setting}\n");
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(&cmdline))?;
            let lines = qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)?;
            let said = lines
                .iter()
                .find(|line| line.contains(FASTPATH_LINE))
                .ok_or_else(|| Error::new(format!("{arch}: no stage-9 line for the fast path")))?;
            if !said.contains(&format!(
                "{FASTPATH_LINE} {setting} (as ferrix.fastpath asked)"
            )) {
                return Err(Error::new(format!(
                    "{arch}: asked for ferrix.fastpath={setting}, the boot said: {}",
                    said.trim()
                )));
            }
            let transcript: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    line.get(line.find("ipc-equiv")?..)
                        .map(|rest| rest.trim().to_owned())
                })
                .collect();
            for line in &transcript {
                println!("  {arch} fastpath={setting}: {line}");
            }
            if !transcript.iter().any(|line| line == "ipc-equiv: exit 0") {
                return Err(Error::new(format!(
                    "{arch}: ipc-equiv did not finish with ferrix.fastpath={setting}"
                )));
            }
            transcripts.push(transcript);
        }
        if let [off, on] = transcripts.as_slice()
            && off != on
        {
            let first = off
                .iter()
                .zip(on)
                .find(|(off, on)| off != on)
                .map_or_else(String::new, |(off, on)| {
                    format!("off said {off:?}, on said {on:?}")
                });
            return Err(Error::new(format!(
                "{arch}: the transcripts with the fast path off and on differ: {first}"
            )));
        }
        if transcripts.len() == 2 {
            println!("  {arch}: the transcripts with the fast path off and on are the same");
        }
    }
    Ok(())
}
