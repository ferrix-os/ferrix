//! `test-wine-sync`: Proton's Wine waits through fsync on Ferrix, and how
//! much it saves (stage 23, E1.4 and E3.5; docs/WINE.md §11).
//!
//! GE-Proton11-7's Wine has no esync. Its fast synchronisation is fsync:
//! wineserver turns it on when `WINEFSYNC=1` and `futex_waitv` (449) answers
//! something other than `ENOSYS`, says `fsync: up and running.`, and keeps
//! every event, mutex and semaphore as a word in a POSIX shared-memory file
//! each Wine process maps; a wait is `futex_waitv` on those words and a
//! signal a `FUTEX_WAKE`, and the server is not asked.
//!
//! The gate attaches Proton's volume (`fetch-proton.sh`) with no display and
//! carries `tools/common/test/syncbench.c`, built here for x86-64 and for
//! i386 with mingw-w64. [`SCRIPT`] makes a prefix, then runs the bench twice
//! for each width: once with wineserver alone and once with fsync, each time
//! under a server it started itself, whose processor time it reads from
//! `/proc` around each run. It passes when, with fsync, the server said it
//! took fsync and a traced run of each width shows Proton's own fsync
//! wakes (`Woken up by handle`); without it, neither; and every run handed
//! its turns over to the end. It prints the figures either way.
//!
//! It needs no network, and attaches a volume, so it runs on demand as
//! `test-wine` does.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// Where [`SCRIPT`] is in the image.
const RUN_PATH: &str = "bin/wine-sync";

/// Where the 64-bit and 32-bit benches are in the image.
const BENCHES: [(&str, &str, &str); 2] = [
    ("64", "x86_64-w64-mingw32-gcc", "etc/syncbench64.exe"),
    ("32", "i686-w64-mingw32-gcc", "etc/syncbench32.exe"),
];

/// Turns each bench hands back and forth, in each of its two parts.
const ROUNDS: u32 = 20_000;

/// What wineserver says when it takes fsync.
const FSYNC_UP: &str = "fsync: up and running.";

/// What Proton's ntdll traces (`WINEDEBUG=+fsync`) each time a wait of its
/// own is woken through an fsync word.
const FSYNC_WAKE: &str = "Woken up by handle";

/// What the script prints last, whatever happened.
const END: &str = "sync: end";

/// The script, after Proton's own lines (`Wine::prelude`): a prefix, then
/// for `off` and `on` a server of its own, both benches, the server's
/// processor time around each and, for each, a short run traced for fsync.
///
/// `/proc/<pid>/stat`'s fields 14 and 15 are the server's user and system
/// time in ticks of 10 ms. `wineserver -f -p` stays in the foreground and
/// does not exit when its last client does, so the one pid holds for both
/// runs; the two seconds before the first let it make its socket, so that
/// the first client does not start a server of its own.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINEDEBUG=-all
unset DISPLAY WAYLAND_DISPLAY
mkdir -p /tmp/wine
$WINE wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "sync: wineboot exited $?"
$WINESERVER -w
ticks() { awk '{ print $14 + $15 }' /proc/$1/stat; }
for mode in off on; do
    if [ $mode = on ]; then export WINEFSYNC=1; else unset WINEFSYNC; fi
    $WINESERVER -f -p > /tmp/wine/server-$mode.log 2>&1 &
    server=$!
    sleep 2
    for bits in 64 32; do
        before=$(ticks $server)
        $WINE /etc/syncbench$bits.exe ROUNDS > /tmp/wine/bench.txt 2>&1
        status=$?
        after=$(ticks $server)
        sed "s/^/sync: $mode $bits: /" /tmp/wine/bench.txt
        echo "sync: $mode $bits: exited $status, wineserver ticks $((after - before))"
        traces=$(WINEDEBUG=+fsync $WINE /etc/syncbench$bits.exe 50 2>&1 > /dev/null | grep -c 'Woken up by handle')
        echo "sync: $mode $bits: fsync traces $traces"
    done
    ls /dev/shm | sed "s/^/sync: $mode: shm: /"
    $WINESERVER -k
    wait $server
    sed "s/^/sync: $mode: server: /" /tmp/wine/server-$mode.log
done
echo "sync: end"
"#;

/// The script the shell runs: [`SCRIPT`] behind Proton's lines.
const INIT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/proton/files/bin/wine ] || exit 3
busybox sh /bin/wine-sync
exit 17
"#;

/// What [`INIT`] exits with once the script has run.
const STATUS: i32 = 17;

/// Megabytes of guest RAM, as `test-wine`'s first boot: a prefix written
/// through the page cache.
const MEMORY: u32 = 6144;

/// The volume `tools/common/fetch/fetch-proton.sh` makes.
///
/// # Errors
///
/// It has not been made, or carries another Proton than the script pins.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_PROTON_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("proton")?,
    };
    let image = directory.join("proton.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-proton.sh makes it",
            image.display()
        )));
    }
    let carried = std::fs::read_to_string(directory.join("proton.version")).unwrap_or_default();
    let pinned = crate::compositor::endfield::proton_pinned();
    if carried.trim() != pinned {
        return Err(Error::new(format!(
            "{} carries Proton {:?}, not {pinned}: tools/common/fetch/fetch-proton.sh makes it again",
            image.display(),
            carried.trim()
        )));
    }
    Ok(image)
}

/// One bench run, as the script printed it.
#[derive(Debug, Default, Clone, PartialEq)]
struct Run {
    /// Microseconds a turn took between two threads.
    threads_us: Option<f64>,
    /// Microseconds a turn took between two processes.
    processes_us: Option<f64>,
    /// Whether both parts handed every turn over, the multiple wait answered
    /// the second handle each time, and the program exited 0.
    complete: bool,
    /// The server's processor time over the run, in 10 ms ticks.
    server_ticks: Option<u64>,
    /// Lines of a traced run that say a wait was woken through fsync.
    fsync_traces: Option<u64>,
}

/// The value after `name=` in `line`.
fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(name)?.strip_prefix('='))
}

/// The run of `mode` (`off` or `on`) and `bits` (`64` or `32`) among `lines`.
fn run_of(lines: &[String], mode: &str, bits: &str) -> Run {
    let prefix = format!("sync: {mode} {bits}: ");
    let said: Vec<&str> = lines
        .iter()
        .filter_map(|line| {
            line.split_once(prefix.as_str())
                .map(|(_, rest)| rest.trim())
        })
        .collect();
    let rounds = ROUNDS.to_string();
    let mut run = Run::default();
    let (mut threads_done, mut processes_done, mut exited) = (false, false, false);
    for line in &said {
        if let Some(rest) = line.strip_prefix("syncbench: threads ") {
            run.threads_us = field(rest, "us_per_round").and_then(|v| v.parse().ok());
            threads_done =
                field(rest, "rounds") == Some(rounds.as_str()) && field(rest, "index") == Some("1");
        } else if let Some(rest) = line.strip_prefix("syncbench: processes ") {
            run.processes_us = field(rest, "us_per_round").and_then(|v| v.parse().ok());
            processes_done =
                field(rest, "rounds") == Some(rounds.as_str()) && field(rest, "child") == Some("0");
        } else if let Some(rest) = line.strip_prefix("exited ") {
            exited = rest.starts_with("0,");
            run.server_ticks = rest
                .rsplit_once("wineserver ticks ")
                .and_then(|(_, ticks)| ticks.trim().parse().ok());
        } else if let Some(rest) = line.strip_prefix("fsync traces ") {
            run.fsync_traces = rest.trim().parse().ok();
        }
    }
    run.complete = threads_done && processes_done && exited;
    run
}

/// Whether the server of `mode` said it took fsync.
fn server_took_fsync(lines: &[String], mode: &str) -> bool {
    let prefix = format!("sync: {mode}: server: ");
    lines.iter().any(|line| {
        line.split_once(prefix.as_str())
            .is_some_and(|(_, said)| said.contains(FSYNC_UP))
    })
}

/// The four runs, judged: fsync taken with `on` and only then, every run
/// complete. Answers the figures, one line each.
fn judge(arch: Arch, lines: &[String]) -> Result<Vec<String>> {
    if !server_took_fsync(lines, "on") {
        return Err(Error::new(format!(
            "{arch}: wineserver did not take fsync with WINEFSYNC=1: it never said `{FSYNC_UP}` \
             (the `sync: on: server:` lines say what it said)"
        )));
    }
    if server_took_fsync(lines, "off") {
        return Err(Error::new(format!(
            "{arch}: wineserver said `{FSYNC_UP}` without WINEFSYNC"
        )));
    }
    let mut figures = Vec::new();
    for mode in ["off", "on"] {
        for (bits, _, _) in BENCHES {
            let run = run_of(lines, mode, bits);
            if !run.complete {
                return Err(Error::new(format!(
                    "{arch}: the {bits}-bit bench with fsync {mode} did not hand all {ROUNDS} \
                     turns over in both parts and exit 0: {run:?}"
                )));
            }
            let traces = run.fsync_traces.unwrap_or(0);
            if mode == "on" && traces == 0 {
                return Err(Error::new(format!(
                    "{arch}: wineserver took fsync, but the {bits}-bit bench's traced run shows \
                     no `{FSYNC_WAKE}`: its waits did not go through fsync"
                )));
            }
            if mode == "off" && traces != 0 {
                return Err(Error::new(format!(
                    "{arch}: the {bits}-bit bench traced {traces} fsync wakes with fsync off"
                )));
            }
            let us =
                |value: Option<f64>| value.map_or_else(|| "?".to_owned(), |v| format!("{v:.1}"));
            figures.push(format!(
                "fsync {mode:3} {bits}-bit: {} us a turn between threads, {} between processes, \
                 wineserver {} ms of processor time, {traces} fsync wakes traced",
                us(run.threads_us),
                us(run.processes_us),
                run.server_ticks
                    .map_or_else(|| "?".to_owned(), |t| (t * 10).to_string()),
            ));
        }
    }
    Ok(figures)
}

/// Boot a shell on Proton's volume whose script runs the bench with
/// wineserver alone and with fsync, and require fsync taken and used.
///
/// # Errors
///
/// When the volume is missing, the benches or the image cannot be built,
/// the boot fails, or [`judge`] finds fsync not taken or a run incomplete.
pub(crate) fn test_wine_sync(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-wine-sync runs on x86-64")),
    };
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell runs Proton's Wine with and without fsync");
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
        "{}{}",
        crate::compositor::endfield::Wine::Proton.prelude(),
        SCRIPT.replace("ROUNDS", &ROUNDS.to_string())
    );
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(script.into_bytes()),
    });
    for (_, compiler, path) in BENCHES {
        files.push(crate::ports::File {
            path: path.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(crate::wine::windows_program(
                compiler,
                "syncbench",
                &[],
            )?),
        });
    }
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running the bench in Proton's Wine on Ferrix with {} MiB (timeout {}s)",
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
                "{arch}: the fsync script ended with {other:?}"
            )));
        }
    }
    if !lines.iter().any(|line| line.contains(END)) {
        return Err(Error::new(format!("{arch}: the fsync script never ended")));
    }
    for figure in judge(arch, &lines)? {
        println!("  {arch}: {figure}");
    }
    println!("  {arch}: Proton's Wine took fsync on Ferrix, from its 64-bit and 32-bit sides");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of a passing run, as the script prints them.
    fn passing() -> Vec<String> {
        let mut lines = Vec::new();
        for mode in ["off", "on"] {
            for bits in ["64", "32"] {
                let traces = if mode == "on" { 40 } else { 0 };
                lines.extend([
                    format!("  9.0 | sync: {mode} {bits}: syncbench: pointer=8"),
                    format!(
                        "  9.1 | sync: {mode} {bits}: syncbench: threads rounds={ROUNDS} ms=900 \
                         us_per_round=45.0 index=1"
                    ),
                    format!(
                        "  9.2 | sync: {mode} {bits}: syncbench: processes rounds={ROUNDS} ms=1200 \
                         us_per_round=60.0 child=0"
                    ),
                    format!("  9.3 | sync: {mode} {bits}: exited 0, wineserver ticks 120"),
                    format!("  9.4 | sync: {mode} {bits}: fsync traces {traces}"),
                ]);
            }
        }
        lines.push(format!("  9.5 | sync: on: server: {FSYNC_UP}"));
        lines.push(format!("  9.6 | {END}"));
        lines
    }

    /// A run with fsync taken and used from both widths passes, and its
    /// figures are read.
    #[test]
    fn a_run_with_fsync_taken_passes() {
        let figures = judge(Arch::X86_64, &passing()).unwrap_or_default();
        assert_eq!(figures.len(), 4);
        assert!(figures[0].contains("45.0 us a turn between threads, 60.0 between processes"));
        assert!(figures[3].contains("wineserver 1200 ms"));
        assert_eq!(
            run_of(&passing(), "on", "32"),
            Run {
                threads_us: Some(45.0),
                processes_us: Some(60.0),
                complete: true,
                server_ticks: Some(120),
                fsync_traces: Some(40),
            }
        );
    }

    /// A server that never took fsync fails by the line the negative control
    /// requires; so do a width with no fsync wakes and a run cut short.
    #[test]
    fn fsync_not_taken_or_not_used_fails() {
        let lines: Vec<String> = passing()
            .into_iter()
            .filter(|line| !line.contains(FSYNC_UP))
            .collect();
        let error = judge(Arch::X86_64, &lines).err().map(|e| e.to_string());
        assert!(error.is_some_and(|e| e.contains("wineserver did not take fsync")));

        let lines: Vec<String> = passing()
            .into_iter()
            .map(|line| {
                line.replace(
                    "sync: on 32: fsync traces 40",
                    "sync: on 32: fsync traces 0",
                )
            })
            .collect();
        let error = judge(Arch::X86_64, &lines).err().map(|e| e.to_string());
        assert!(error.is_some_and(|e| e.contains("32-bit bench's traced run shows no")));

        let lines: Vec<String> = passing()
            .into_iter()
            .map(|line| line.replace("index=1", "index=2"))
            .collect();
        assert!(judge(Arch::X86_64, &lines).is_err());
    }

    /// The script runs each bench at [`ROUNDS`], counts the trace the gate
    /// requires, and ends with [`END`].
    #[test]
    fn the_script_says_what_the_gate_reads() {
        assert!(SCRIPT.contains("syncbench$bits.exe ROUNDS"));
        assert!(SCRIPT.contains(&format!("grep -c '{FSYNC_WAKE}'")));
        assert!(SCRIPT.contains(&format!("echo \"{END}\"")));
        for (bits, _, path) in BENCHES {
            assert_eq!(path, format!("etc/syncbench{bits}.exe"));
        }
    }
}
