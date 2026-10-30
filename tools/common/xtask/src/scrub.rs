//! `cargo xtask test-scrub`: the RAM scrubber sweeps free memory while the
//! machine idles, and stands down where it must (`docs/RAM-SCRUBBER.md` §7).
//!
//! On x86-64 and AArch64 the image's init starts `memscrub` with a period of
//! 20 s and batches of 64, and the kernel command line carries the gate's
//! override, `ferrix.scrub=test`, which a build with the checks honours as a
//! guest: QEMU's own SMBIOS type 16 says multi-bit ECC, so without it the
//! scrubber stands down. The guest's script waits for a full sweep, keeps
//! four CPU-bound loops running and requires that nothing is tested while
//! they run, and prints `/proc/ferrix-scrub`. A second boot without the
//! override requires the stand-down and its reason. On ARMv7-A, which has
//! no write of a whole line that reads nothing, one boot requires the
//! stand-down alone.

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-scrub",
    help_at: 231,
    help: &[
        "test-scrub    The RAM scrubber under init and memscrub: a full sweep of free memory while idle, nothing",
        "              tested while busy, and the stand-down without the gate's override (ARMv7-A: the stand-down alone)",
    ],
    run: |_, args| test_scrub(args),
}];

/// The unit, linked into `basic.target`'s wants.
const UNIT: &str = "memscrub.service";

/// The gate's pace: a sweep every 20 s, 64 frames a call.
const CONFIG: &str =
    "# test-scrub's pace (tools/common/xtask/src/scrub.rs)\nperiod = 20\nidle = 50\nbatch = 64\n";

/// The guest unit that runs [`SCRIPT`].
const GUEST_UNIT: &str = "\
# test-scrub's guest half (tools/common/xtask/src/scrub.rs).
[Unit]
Description=Wait for a sweep, then load the machine and watch the scrubber stop
After=memscrub.service

[Service]
Type=oneshot
TimeoutStartSec=900
ExecStart=/bin/busybox sh /lib/scrub-test.sh
";

/// The guest half; every line it prints starts with `scrub-test:`.
const SCRIPT: &str = r#"
say() { echo "scrub-test: $*"; }
field() { busybox grep "^$1 " /proc/ferrix-scrub | busybox cut -d' ' -f2; }
say "standing $(busybox grep '^standing ' /proc/ferrix-scrub | busybox cut -d' ' -f2-)"
if [ "$(field sweeps)" = "" ]; then say "no /proc/ferrix-scrub"; say done; exit 0; fi
case "$(busybox grep '^standing ' /proc/ferrix-scrub)" in
    "standing stands down"*) say done; exit 0 ;;
esac
i=0
while [ "$(field sweeps)" = 0 ] && [ $i -lt 600 ]; do busybox sleep 1; i=$((i + 1)); done
say "sweeps $(field sweeps) tested $(field tested) managed $(field managed) bad $(field bad) suspects $(field suspects)"
busy=
for n in 1 2 3 4; do busybox sh -c 'while :; do :; done' & busy="$busy $!"; done
busybox sleep 3
before=$(field tested)
busybox sleep 5
after=$(field tested)
say "while busy: tested $before then $after"
busybox kill $busy 2>/dev/null
say done
"#;

/// The files the image carries beside the kernel: init, busybox, memscrub
/// enabled with the gate's pace, and the guest's unit and script.
fn image_files(arch: Arch) -> Result<Vec<File>> {
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox),
    });
    files.push(File {
        path: format!("etc/ferrix/units/basic.target.wants/{UNIT}"),
        mode: 0o777,
        content: Content::Link(format!("/lib/ferrix/units/{UNIT}")),
    });
    files.push(File {
        path: "etc/ferrix/memscrub.conf".to_owned(),
        mode: 0o644,
        content: Content::Bytes(CONFIG.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/getty@.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/dev/null".to_owned()),
    });
    files.push(File {
        path: "lib/scrub-test.sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/scrub-test.service".to_owned(),
        mode: 0o644,
        content: Content::Bytes(GUEST_UNIT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/multi-user.target.wants/scrub-test.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/scrub-test.service".to_owned()),
    });
    Ok(files)
}

/// `memscrub` enabled with `basic.target`, for the desktop image: it does
/// nothing where the kernel's scrubber stands down.
pub(crate) fn enabled() -> File {
    File {
        path: "etc/ferrix/units/basic.target.wants/memscrub.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/lib/ferrix/units/memscrub.service".to_owned()),
    }
}

/// Every architecture asked for: the runs on x86-64 and AArch64, `memscrub`
/// holding the token init granted and its batches answered, a full sweep
/// with no allocation failed and nothing tested while busy, and the facade's
/// pinned disassembly; the stand-down without the override, on QEMU's type
/// 16; the stand-down on ARMv7-A whatever its table says.
///
/// Verifies: H.FAIL.9, L.mm.101, L.mm.103, L.mm.106, L.mm.107, L.armv7a.29
fn test_scrub(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.timeout_given {
        args.timeout = 600;
    }
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            let lines = boot(arch, &args, false)?;
            stands_down(&lines, "no write of a whole line here reads nothing")
                .map_err(|why| failed(arch, &why))?;
            println!("  {arch}: test-scrub: the scrubber stands down, as ARMv7-A must");
            continue;
        }
        let lines = boot(arch, &args, true)?;
        let said = swept(&lines).map_err(|why| failed(arch, &why))?;
        pinned_disassembly(arch, &args).map_err(|why| failed(arch, &why))?;
        let lines = boot(arch, &args, false)?;
        stands_down(&lines, "the firmware says the memory corrects errors")
            .map_err(|why| failed(arch, &why))?;
        println!(
            "  {arch}: test-scrub: {said}; without the gate's override it stands down on QEMU's type 16"
        );
    }
    Ok(())
}

/// The facade as built (ledger 5057 (8)). x86-64: `fill_without_reading`'s
/// disassembly holds `movnti` and `sfence` and no ordinary store to memory
/// through a register (`mov %r, (%r)`) but the stack's spills -- the
/// frame's words are written by `movnti` alone. AArch64: it holds `dc zva`
/// then `dsb ish`, and the eviction it calls holds `dc civac` then `dsb sy`.
fn pinned_disassembly(arch: Arch, args: &Args) -> std::result::Result<(), String> {
    let kernel = cargo::build_kernel(arch, args.release).map_err(|error| error.to_string())?;
    let tool = if arch == Arch::AArch64 {
        "aarch64-linux-gnu-objdump"
    } else {
        "objdump"
    };
    let output = std::process::Command::new(tool)
        .args(["-d", "--no-show-raw-insn", "-C"])
        .arg(&kernel.elf)
        .output()
        .map_err(|error| format!("{tool} could not run: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let function = |name: &str| {
        text.find(&format!("{name}>:"))
            .and_then(|start| text.get(start..))
            .map(|rest| rest.split("\n\n").next().unwrap_or_default())
            .ok_or(format!("the kernel has no {name}"))
    };
    let fill = function("scrub::fill_without_reading")?;
    if arch == Arch::AArch64 {
        let evict = function("scrub::evict_to_coherency")?;
        let evict = if evict.contains("clean_invalidate_to_poc") {
            function("cpu::clean_invalidate_to_poc")?
        } else {
            evict
        };
        return match (
            in_order(fill, "dc\tzva", "dsb\tish"),
            in_order(evict, "dc\tcivac", "dsb\tsy"),
        ) {
            (true, true) => Ok(()),
            (false, _) => {
                Err("fill_without_reading's disassembly lacks dc zva then dsb ish".to_owned())
            }
            (_, false) => {
                Err("evict_to_coherency's disassembly lacks dc civac then dsb sy".to_owned())
            }
        };
    }
    if !fill.contains("movnti") || !fill.contains("sfence") {
        return Err("fill_without_reading's disassembly lacks movnti or sfence".to_owned());
    }
    let ordinary = fill.lines().find(|line| {
        let line = line.trim();
        line.contains("\tmov ")
            && line.ends_with(')')
            && !line.contains("movnti")
            && !line.contains("(%rsp)")
            && !line.contains("(%rbp)")
    });
    if let Some(line) = ordinary {
        return Err(format!(
            "fill_without_reading stores to memory with a plain mov: {line}"
        ));
    }
    Ok(())
}

/// Whether `first` appears in `text` and `then` after it.
fn in_order(text: &str, first: &str, then: &str) -> bool {
    text.find(first)
        .and_then(|at| text.get(at..))
        .is_some_and(|rest| rest.contains(then))
}

/// A failure, with where the serial log is.
fn failed(arch: Arch, why: &str) -> Error {
    Error::new(format!(
        "{arch}: test-scrub: {why}.\n  Serial output is in {}",
        crate::paths::build_dir(arch).join("serial.log").display()
    ))
}

/// One boot, with or without the gate's override, 512 MiB of memory.
fn boot(arch: Arch, args: &Args, overridden: bool) -> Result<Vec<String>> {
    let files = image_files(arch)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let option = if overridden { " ferrix.scrub=test" } else { "" };
    let options = format!("{}{option}\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let mut qemu_args = args.clone();
    qemu_args.memory = 512;
    qemu_args.memory_given = true;
    println!(
        "  {arch}: booting init with memscrub{}",
        if overridden {
            ", the gate's override on"
        } else {
            ", no override"
        }
    );
    qemu::watch_lines(arch, &image, &kernel, &qemu_args, "scrub-test: done")
}

/// The run: the override taken, a full sweep with every frame of it counted
/// and none bad, and nothing tested while the machine was busy.
fn swept(lines: &[String]) -> std::result::Result<String, String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    if !has("scrub: runs: stand-down overridden for the gate") {
        return Err("the gate's override was not taken".to_owned());
    }
    if !has("scrub    a clean frame tested and given back as it was") {
        return Err("stage 2's scrubber check did not run under the override".to_owned());
    }
    let said = |what: &str| {
        lines.iter().find_map(|line| {
            line.split_once(what)
                .map(|(_, rest)| rest.trim().to_owned())
        })
    };
    let swept = said("scrub-test: sweeps ").ok_or("the guest said nothing of a sweep")?;
    let word = |key: &str| {
        swept
            .split(' ')
            .skip_while(|word| *word != key)
            .nth(1)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let sweeps = swept
        .split(' ')
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if sweeps == 0 {
        return Err(format!("no full sweep within ten minutes: {swept}"));
    }
    if word("tested") == 0 || word("bad") != 0 || word("suspects") != 0 {
        return Err(format!(
            "a sweep of good memory found it bad, or tested nothing: {swept}"
        ));
    }
    if !has("memscrub: sweep 1 done") {
        return Err("memscrub did not say the sweep's end".to_owned());
    }
    let busy = said("scrub-test: while busy: tested ").ok_or("no busy reading")?;
    let (before, after) = busy
        .split_once(" then ")
        .ok_or("a busy reading not understood")?;
    if before != after {
        return Err(format!(
            "the scrubber tested frames while the machine was busy: {busy}"
        ));
    }
    Ok(format!(
        "{sweeps} sweep(s), {} frames tested of {} managed, none bad; nothing tested while busy",
        word("tested"),
        word("managed")
    ))
}

/// The stand-down, said with `reason`, and the guest finding it so.
fn stands_down(lines: &[String], reason: &str) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    let said = lines
        .iter()
        .find(|line| line.contains("scrub: stands down") || line.contains("scrub: runs"))
        .ok_or("the kernel never said whether the scrubber runs")?;
    if !said.contains(reason) {
        return Err(format!(
            "the scrubber did not stand down for the reason expected: {said}"
        ));
    }
    if !has("scrub-test: standing stands down") {
        return Err("/proc/ferrix-scrub did not say the stand-down".to_owned());
    }
    Ok(())
}

/// The body of `fn name` in `source`, by brace count from its signature.
#[cfg(test)]
fn body<'a>(source: &'a str, name: &str) -> &'a str {
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("no fn {name}"));
    let open = start
        + source
            .get(start..)
            .and_then(|rest| rest.find('{'))
            .expect("a body");
    let mut depth = 0usize;
    for (at, byte) in source.get(open..).unwrap_or_default().bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return source
                        .get(open..=open + at)
                        .expect("a body on char boundaries");
                }
            }
            _ => {}
        }
    }
    panic!("fn {name} does not close");
}

#[cfg(test)]
mod tests {
    use super::body;

    /// The facade's build evidence (ledger 5045 (B4); QEMU models no caches,
    /// so no boot can show it): on x86-64 the writes are `MOVNTI` alone,
    /// stepped by the line `CPUID` reports, each line masked, `SFENCE`
    /// after; the eviction `CLFLUSH` per reported line then `MFENCE`. On
    /// AArch64 the writes are `DC ZVA` per block of `DCZID_EL0`'s size, a
    /// `DSB` after, the stores only then; the eviction `DC CIVAC` per line
    /// of `CTR_EL0` then `DSB SY`.
    ///
    /// Verifies: `L.x86_64.243`, `L.aarch64.68`
    #[test]
    fn the_scrub_facade_writes_without_reading_and_evicts_every_line() {
        let x86 = include_str!("../../../../src/kernel/src/arch/x86_64/scrub.rs");
        let x86_cpu = include_str!("../../../../src/kernel/src/arch/x86_64/cpu.rs");
        let fill = body(x86, "fill_without_reading");
        for needed in [
            "cpu::clflush_line()",
            "movnti",
            "sfence",
            "disable()",
            "restore(saved)",
        ] {
            assert!(fill.contains(needed), "x86-64's fill lacks {needed}");
        }
        assert!(
            !fill.contains("write_volatile") && !fill.contains("write("),
            "x86-64's fill makes an ordinary store"
        );
        assert!(body(x86, "evict_to_coherency").contains("cpu::clean_for_walker"));
        let flush = body(x86_cpu, "clean_for_walker");
        for needed in ["clflush_line()", "clflush [", "mfence", "at += line"] {
            assert!(flush.contains(needed), "x86-64's eviction lacks {needed}");
        }

        let arm = include_str!("../../../../src/kernel/src/arch/aarch64/scrub.rs");
        let arm_cpu = include_str!("../../../../src/kernel/src/arch/aarch64/cpu.rs");
        let fill = body(arm, "fill_without_reading");
        let zva = fill.find("dc zva").expect("AArch64's fill lacks DC ZVA");
        let barrier = fill.find("dsb ish").expect("AArch64's fill lacks its DSB");
        let store = fill
            .find("write_volatile")
            .expect("AArch64's fill writes nothing");
        assert!(
            zva < barrier && barrier < store,
            "AArch64 stores before its zeroing completes"
        );
        assert!(
            fill.contains("4u64 << (dczid() & 0xF)"),
            "AArch64's block is not DCZID_EL0's"
        );
        assert!(body(arm, "scrub_ready").contains("DZP"));
        assert!(body(arm, "evict_to_coherency").contains("cpu::clean_invalidate_to_poc"));
        let flush = body(arm_cpu, "clean_invalidate_to_poc");
        for needed in ["data_line()", "dc civac", "dsb sy", "at += line"] {
            assert!(flush.contains(needed), "AArch64's eviction lacks {needed}");
        }
    }
}
