//! `test-wine-dispatch`: a Windows system call made by a program's own code
//! comes back to GE-Proton's Wine through Syscall User Dispatch
//! (`docs/SECCOMP.md` §13, the design's 7.4).
//!
//! The gate attaches Proton's volume (`fetch-proton.sh`) with no display and
//! carries `tools/common/test/direct-syscall.c`, built here with mingw-w64.
//! The program reads `NtQueryInformationProcess`'s number from ntdll's own
//! stub and runs a `syscall` with it from its own `.text`, outside the range
//! Wine names, then compares the answer with ntdll's. With dispatch the call
//! comes back to Wine as a `SIGSYS` and is performed there, and the program
//! prints `DIRECT-SYSCALL PASS`; without it the kernel runs it as an
//! unrelated call. It passes when the program passed and no Wine thread said
//! it could not enable dispatch.
//!
//! Run on nazuna's Linux under the same Proton first, it passes there too:
//! the probe is right before Ferrix is judged by it.

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-wine-dispatch",
    help_at: 775,
    help: &[
        "test-wine-dispatch  Attach the volume tools/common/fetch/fetch-proton.sh makes, and require a Windows program's own",
        "              direct `syscall` to come back to GE-Proton's Wine through Syscall User Dispatch and answer as ntdll's",
        "              call does (docs/SECCOMP.md §13)",
    ],
    run: |_, args| test_wine_dispatch(args),
}];

/// Where [`SCRIPT`] is in the image.
const RUN_PATH: &str = "bin/wine-dispatch";

/// Where the probe is in the image.
const PROBE: &str = "etc/direct-syscall.exe";

/// What the probe prints when its direct call answered as ntdll's.
const PASS: &str = "DIRECT-SYSCALL PASS";

/// What GE-Proton's ntdll logs on each thread when the kernel refuses
/// `prctl(PR_SET_SYSCALL_USER_DISPATCH)`.
const NO_DISPATCH: &str = "could not enable syscall user dispatch";

/// What the script prints last, whatever happened.
const END: &str = "dispatch: end";

/// The script, after Proton's own lines (`Wine::prelude`): a prefix, then
/// the probe, its output and Wine's own errors each line prefixed.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINEDEBUG=-all,warn+seh
unset DISPLAY WAYLAND_DISPLAY
mkdir -p /tmp/wine
$WINE wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "dispatch: wineboot exited $?"
grep -i 'syscall user dispatch' /tmp/wine/wineboot.log | sed 's/^/dispatch: wine: /'
$WINE /etc/direct-syscall.exe > /tmp/wine/probe.txt 2> /tmp/wine/probe.err
echo "dispatch: probe exited $?"
sed 's/^/dispatch: probe: /' /tmp/wine/probe.txt
grep -i 'syscall user dispatch' /tmp/wine/probe.err | sed 's/^/dispatch: wine: /'
$WINESERVER -k
echo "dispatch: end"
"#;

/// The script the shell runs: [`SCRIPT`] behind Proton's lines.
const INIT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/proton/files/bin/wine ] || exit 3
busybox sh /bin/wine-dispatch
exit 17
"#;

/// What [`INIT`] exits with once the script has run.
const STATUS: i32 = 17;

/// Megabytes of guest RAM: a prefix written through the page cache.
const MEMORY: u32 = 6144;

/// Seconds the boot is given unless `--timeout` says otherwise.
const TIMEOUT_SECONDS: u64 = 600;

/// The verdict on the script's lines.
fn judge(arch: Arch, lines: &[String]) -> Result<String> {
    if let Some(refused) = lines.iter().find(|line| line.contains(NO_DISPATCH)) {
        return Err(Error::new(format!(
            "{arch}: Wine could not enable Syscall User Dispatch: `{}`",
            refused.trim()
        )));
    }
    lines
        .iter()
        .find_map(|line| {
            line.split_once(PASS)
                .map(|(_, rest)| format!("{PASS}{rest}"))
        })
        .ok_or_else(|| {
            let said = lines
                .iter()
                .find_map(|line| line.split_once("dispatch: probe: "))
                .map_or("nothing", |(_, said)| said.trim());
            Error::new(format!(
                "{arch}: the direct call did not answer as ntdll's: the probe said {said}"
            ))
        })
}

/// The gate.
pub(crate) fn test_wine_dispatch(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-wine-dispatch runs on x86-64")),
    };
    let mut args = args.clone();
    args.data_image = Some(crate::wine_sync::volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT_SECONDS;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell runs a direct system call in Proton's Wine");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, INIT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let links: Vec<(&str, &str)> = crate::yserver::LINKS
        .iter()
        .chain(crate::wine::LINKS)
        .chain(crate::compositor::endfield::PROTON_LINKS)
        .copied()
        .collect();
    let mut files = rustc::files(&links);
    let script = format!(
        "{}{SCRIPT}",
        crate::compositor::endfield::Wine::Proton.prelude()
    );
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(script.into_bytes()),
    });
    files.push(crate::ports::File {
        path: PROBE.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(crate::wine::windows_program(
            "x86_64-w64-mingw32-gcc",
            "direct-syscall",
            &["-lntdll"],
        )?),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running the probe in Proton's Wine on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    match exited {
        Some(status) if status == STATUS.to_string() => {}
        Some("3") => {
            return Err(Error::new(format!(
                "{arch}: /data/proton/files/bin/wine is not there: is Proton's volume attached?"
            )));
        }
        other => {
            return Err(Error::new(format!(
                "{arch}: the dispatch script ended with {other:?}"
            )));
        }
    }
    if !lines.iter().any(|line| line.contains(END)) {
        return Err(Error::new(format!(
            "{arch}: the dispatch script never ended"
        )));
    }
    let passed = judge(arch, &lines)?;
    println!(
        "  {arch}: a direct system call from a Windows program's own code came back to Wine: {passed}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_without_a_refusal_passes() {
        let lines = vec!["dispatch: probe: DIRECT-SYSCALL PASS (number 0x19, pid 32)".to_owned()];
        assert!(judge(Arch::X86_64, &lines).is_ok());
    }

    #[test]
    fn a_refusal_or_a_failed_probe_fails() {
        let refused = vec![
            format!("dispatch: wine: 0024:err:seh:{NO_DISPATCH}"),
            "dispatch: probe: DIRECT-SYSCALL PASS".to_owned(),
        ];
        assert!(judge(Arch::X86_64, &refused).is_err());
        let failed = vec!["dispatch: probe: DIRECT-SYSCALL FAIL: differs".to_owned()];
        assert!(judge(Arch::X86_64, &failed).is_err());
    }

    #[test]
    fn the_script_says_what_the_gate_reads() {
        assert!(SCRIPT.contains(PROBE.trim_start_matches("etc/")));
        assert!(SCRIPT.contains(END));
        assert!(RUN_PATH.ends_with("wine-dispatch") && INIT.contains("/bin/wine-dispatch"));
    }
}
