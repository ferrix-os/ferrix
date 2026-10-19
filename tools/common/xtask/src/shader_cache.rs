//! `test-shader-cache`: what a game compiles through DXVK on lavapipe is
//! kept on a btrfs volume and found again by the next start, and Mesa's
//! disk-cache threads lower only themselves (docs/WINE.md §6a).
//!
//! Endfield on the N150 box spent over an hour at "Shaders are being
//! compiled" (2026-10-18). Two caches can spare a second start that work:
//! DXVK's own (its shader IR, `<exe>.dxvk.bin` in the prefix's
//! `AppData\Local\dxvk`) and Mesa's (lavapipe's LLVM code, the database in
//! `$HOME/.cache/mesa_shader_cache_db`). The gate attaches Proton's volume
//! (`fetch-proton.sh`), whose Wine, DXVK and lavapipe are the box's kind,
//! puts the home and the prefix on it -- btrfs, as the box's home is -- and
//! runs `tools/common/test/d3d11-shaders.c` twice. The first run must compile
//! (Mesa's misses) and create DXVK's file; the second must find DXVK's file,
//! miss nothing in Mesa's and draw the same pixel.
//!
//! Before the program, `tools/common/test/thread-nice.c` lowers one thread
//! with `setpriority(PRIO_PROCESS, tid, 19)`, as Mesa's disk-cache threads
//! do, and must leave its main thread and a later thread at 0. Ferrix kept
//! the value per process until 2026-10-19, so every thread of a program
//! using Mesa ran at nice 19.

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-shader-cache",
    help_at: 782,
    help: &[
        "test-shader-cache  Attach the volume tools/common/fetch/fetch-proton.sh makes, run a Direct3D 11 program with",
        "              many shaders twice through DXVK on lavapipe with its home on btrfs, and require the second run to",
        "              find DXVK's and Mesa's shader caches the first wrote; and a thread's nice value to be its own",
        "              (docs/WINE.md §6a)",
    ],
    run: |_, args| test_shader_cache(args),
}];

/// Where [`SCRIPT`] is in the image.
const RUN_PATH: &str = "bin/shader-cache";

/// Where the Direct3D program is in the image.
const PROGRAM: &str = "etc/d3d11-shaders.exe";

/// Where the nice-value probe is in the image.
const NICE: &str = "etc/thread-nice";

/// How many pixel shaders the program compiles and draws with.
const SHADERS: u32 = 100;

/// What the probe prints on Linux.
const NICE_LINUX: &str = "thread-nice: worker=19 main=0 later=0";

/// What the script prints last, whatever happened.
const END: &str = "cache: end";

/// The script, after Proton's own lines (`Wine::prelude`): yserver headless
/// on `:1` (DXVK's instance asks for Wine's surface extension, which needs a
/// display driver), a prefix and a home on `/data`, then the program twice.
/// `# between the runs` marks where the negative control loses the cache.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin XDG_RUNTIME_DIR=/tmp RUST_LOG=info
export YSERVER_ALLOW_NO_INPUT=1 YSERVER_ALLOW_SOFTWARE_VULKAN=1
export HOME=/data/shader-cache WINEPREFIX=/data/shader-cache/prefix WINEDEBUG=-all
export VK_ICD_FILENAMES=/data/usr/share/vulkan/icd.d/lvp_icd.json
echo "cache: $(/etc/thread-nice)"
/data/yserver/yserver :1 -nolisten tcp > /tmp/yserver.log 2>&1 &
server=$!
mkdir -p $HOME/app
cp /proton/files/lib/wine/dxvk/x86_64-windows/d3d11.dll /proton/files/lib/wine/dxvk/x86_64-windows/dxgi.dll /etc/d3d11-shaders.exe $HOME/app/
$WINE wineboot -i > /tmp/wineboot.log 2>&1
echo "cache: wineboot exited $?"
$WINESERVER -w
$WINE reg add 'HKCU\Software\Wine\X11 Driver' /v UseXRandR /d N /f > /tmp/reg.log 2>&1
$WINESERVER -w
waited=0
while [ ! -S /tmp/.X11-unix/X1 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
cd $HOME/app
for run in 1 2; do
    DISPLAY=:1 WINEDLLOVERRIDES="d3d11,dxgi=n" DXVK_LOG_LEVEL=info DXVK_LOG_PATH=none \
        MESA_SHADER_CACHE_SHOW_STATS=1 $WINE $HOME/app/d3d11-shaders.exe SHADERS > /tmp/run$run.txt 2>&1
    echo "cache: run $run exited $?"
    grep -E 'd3d11-shaders:|disk shader cache|cache file|DXVK: v' /tmp/run$run.txt | sed "s/^/cache: run $run: /"
    $WINESERVER -w
    # between the runs
    echo "cache: run $run left $(find $HOME/.cache/mesa_shader_cache_db -type f -exec cat {} + 2>/dev/null | wc -c) bytes in Mesa's cache"
done
$WINESERVER -k
kill $server
echo "cache: end"
"#;

/// The script the shell runs: [`SCRIPT`] behind Proton's lines.
const INIT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/proton/files/bin/wine ] || exit 3
[ -e /data/usr/share/vulkan/icd.d/lvp_icd.json ] || exit 5
busybox sh /bin/shader-cache
exit 17
"#;

/// What [`INIT`] exits with once the script has run.
const STATUS: i32 = 17;

/// Megabytes of guest RAM: a prefix written through the page cache, and
/// lavapipe's.
const MEMORY: u32 = 6144;

/// Seconds the boot is given unless `--timeout` says otherwise: `wineboot`
/// on btrfs and the first run's shaders compiled by LLVM.
const TIMEOUT_SECONDS: u64 = 900;

/// What one run of the program said.
#[derive(Debug, Default, PartialEq, Eq)]
struct Run {
    /// Its `draw_ms`: creating the shaders and drawing with them.
    draw_ms: Option<u32>,
    /// The pixel it read back.
    centre: Option<String>,
    /// Mesa's disk cache's hits, from its statistics line.
    hits: Option<u32>,
    /// Mesa's disk cache's misses: each a shader LLVM compiled.
    misses: Option<u32>,
    /// DXVK created its cache file.
    created: bool,
    /// DXVK found the cache file a run before this one created.
    found: bool,
    /// DXVK's own version line was there, so Wine's d3d11 did not run.
    dxvk: bool,
}

/// The value after `name=` in `line`.
fn field(line: &str, name: &str) -> Option<String> {
    line.split_whitespace().find_map(|item| {
        item.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_owned)
    })
}

/// What run `number` said, from the script's `cache: run N: ` lines.
fn run(lines: &[String], number: u32) -> Run {
    let prefix = format!("cache: run {number}: ");
    let mut run = Run::default();
    for said in lines
        .iter()
        .filter_map(|line| line.split_once(prefix.as_str()).map(|(_, said)| said))
    {
        if said.starts_with("d3d11-shaders: shaders=") {
            run.draw_ms = field(said, "draw_ms").and_then(|ms| ms.parse().ok());
            run.centre = field(said, "centre");
        } else if let Some(stats) = said.strip_prefix("disk shader cache:") {
            // "  hits = 216, misses = 0"
            let numbers: Vec<u32> = stats
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|n| n.parse().ok())
                .collect();
            if let [hits, misses] = numbers.as_slice() {
                run.hits = Some(*hits);
                run.misses = Some(*misses);
            }
        } else if said.contains("Created cache file") {
            run.created = true;
        } else if said.contains("Found cache file") {
            run.found = true;
        } else if said.contains("DXVK: v") {
            run.dxvk = true;
        }
    }
    run
}

/// The verdict on the script's lines: a summary, or what is wrong.
fn judge(lines: &[String]) -> Result<String> {
    let nice = lines
        .iter()
        .find_map(|line| {
            line.split_once("cache: thread-nice:")
                .map(|(_, said)| said.trim())
        })
        .ok_or_else(|| Error::new("thread-nice printed nothing"))?;
    if format!("thread-nice: {nice}") != NICE_LINUX {
        return Err(Error::new(format!(
            "a thread's nice value moved its process's other threads: thread-nice said `{nice}`, \
             Linux says `{NICE_LINUX}`"
        )));
    }
    let (first, second) = (run(lines, 1), run(lines, 2));
    if !first.dxvk || !second.dxvk {
        return Err(Error::new(
            "DXVK's log has no `DXVK: v` line: Wine's own d3d11 ran, or DXVK did not start",
        ));
    }
    let (Some(first_centre), Some(second_centre)) = (&first.centre, &second.centre) else {
        return Err(Error::new(format!(
            "the program did not draw in both runs: {first:?}, {second:?}"
        )));
    };
    if first_centre != second_centre {
        return Err(Error::new(format!(
            "the two runs drew different pixels: {first_centre} and {second_centre}"
        )));
    }
    match first.misses {
        Some(misses) if misses > 0 && first.created => {}
        _ => {
            return Err(Error::new(format!(
                "the first run did not start from empty caches: {first:?}"
            )));
        }
    }
    if !second.found {
        return Err(Error::new(
            "the second run did not find DXVK's shader cache the first one created",
        ));
    }
    match (second.hits, second.misses) {
        (Some(hits), Some(0)) if hits >= SHADERS => {}
        _ => {
            return Err(Error::new(format!(
                "the second run compiled its shaders again: Mesa's cache said {:?} hits and {:?} \
                 misses",
                second.hits, second.misses
            )));
        }
    }
    Ok(format!(
        "{NICE_LINUX}; draws took {} ms cold and {} ms with both caches warm (Mesa: {} misses, \
         then {} hits and none missed)",
        first.draw_ms.unwrap_or_default(),
        second.draw_ms.unwrap_or_default(),
        first.misses.unwrap_or_default(),
        second.hits.unwrap_or_default()
    ))
}

/// `tools/common/test/thread-nice.c`, built static with the host's compiler.
fn thread_nice() -> Result<Vec<u8>> {
    let source = crate::paths::workspace_root().join("tools/common/test/thread-nice.c");
    let dir = crate::paths::build_dir(Arch::X86_64);
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    let out = dir.join("thread-nice");
    let built = std::process::Command::new("cc")
        .args([
            "-O2", "-Wall", "-Wextra", "-Werror", "-static", "-pthread", "-o",
        ])
        .arg(&out)
        .arg(&source)
        .status()
        .map_err(|error| Error::new(format!("running cc: {error}")))?;
    if !built.success() {
        return Err(Error::new(format!(
            "building {} failed ({built}); it needs the host's static libc",
            source.display()
        )));
    }
    std::fs::read(&out).map_err(|error| Error::new(format!("reading {}: {error}", out.display())))
}

/// The gate.
pub(crate) fn test_shader_cache(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-shader-cache runs on x86-64")),
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
    println!("  {arch}: building an image that runs {SHADERS} shaders through DXVK twice");
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
        SCRIPT.replace("SHADERS", &SHADERS.to_string())
    );
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(script.into_bytes()),
    });
    files.push(crate::ports::File {
        path: PROGRAM.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(crate::wine::windows_program(
            "x86_64-w64-mingw32-gcc",
            "d3d11-shaders",
            &["-ld3d11", "-ldxgi", "-ld3dcompiler_47", "-luuid"],
        )?),
    });
    files.push(crate::ports::File {
        path: NICE.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(thread_nice()?),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running the program twice in Proton's Wine on Ferrix with {} MiB (timeout {}s)",
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
        Some("5") => {
            return Err(Error::new(format!(
                "{arch}: the volume has no lavapipe (/data/usr/share/vulkan/icd.d/lvp_icd.json)"
            )));
        }
        other => {
            return Err(Error::new(format!(
                "{arch}: the shader-cache script ended with {other:?}"
            )));
        }
    }
    if !lines.iter().any(|line| line.contains(END)) {
        return Err(Error::new(format!(
            "{arch}: the shader-cache script never ended"
        )));
    }
    let summary = judge(&lines).map_err(|error| Error::new(format!("{arch}: {error}")))?;
    println!("  {arch}: the second run found the shader caches the first wrote: {summary}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing() -> Vec<String> {
        [
            "cache: thread-nice: worker=19 main=0 later=0",
            "cache: run 1: info:  DXVK: v3.1-19-g601930949",
            "cache: run 1: info:  Created cache file: C:\\users\\steamuser\\AppData\\Local\\dxvk\\x.dxvk.bin",
            "cache: run 1: d3d11-shaders: shaders=100 hlsl_ms=150 draw_ms=5175 centre=fcfa49ff",
            "cache: run 1: disk shader cache:  hits = 102, misses = 114",
            "cache: run 2: info:  DXVK: v3.1-19-g601930949",
            "cache: run 2: info:  Found cache file: C:\\users\\steamuser\\AppData\\Local\\dxvk\\x.dxvk.bin",
            "cache: run 2: d3d11-shaders: shaders=100 hlsl_ms=153 draw_ms=578 centre=fcfa49ff",
            "cache: run 2: disk shader cache:  hits = 216, misses = 0",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
    }

    #[test]
    fn two_runs_the_second_warm_pass() {
        let summary = judge(&passing()).expect("passes");
        assert!(summary.contains("5175 ms cold and 578 ms"), "{summary}");
    }

    #[test]
    fn a_nice_value_of_the_process_fails() {
        let mut lines = passing();
        lines[0] = "cache: thread-nice: worker=19 main=19 later=19".to_owned();
        let error = judge(&lines).expect_err("fails").to_string();
        assert!(
            error.contains("moved its process's other threads"),
            "{error}"
        );
    }

    #[test]
    fn a_second_run_that_misses_fails() {
        let mut lines = passing();
        lines[8] = "cache: run 2: disk shader cache:  hits = 102, misses = 114".to_owned();
        let error = judge(&lines).expect_err("fails").to_string();
        assert!(error.contains("compiled its shaders again"), "{error}");
    }

    #[test]
    fn a_lost_dxvk_cache_or_a_changed_pixel_fails() {
        let mut lost = passing();
        lost[6] = "cache: run 2: info:  Created cache file: x".to_owned();
        assert!(judge(&lost).is_err());
        let mut changed = passing();
        changed[7] =
            "cache: run 2: d3d11-shaders: shaders=100 draw_ms=578 centre=00000000".to_owned();
        assert!(judge(&changed).is_err());
    }

    #[test]
    fn the_script_says_what_the_gate_reads() {
        assert!(SCRIPT.contains(PROGRAM.trim_start_matches("etc/")));
        assert!(SCRIPT.contains(NICE));
        assert!(SCRIPT.contains(END));
        assert!(SCRIPT.contains("# between the runs"));
        assert!(RUN_PATH.ends_with("shader-cache") && INIT.contains("/bin/shader-cache"));
    }
}
