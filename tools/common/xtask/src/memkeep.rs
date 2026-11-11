//! `cargo xtask test-memkeep`: memkeep keeps free memory above its low mark
//! while a file on a disk fills the page cache (`docs/MEMKEEP.md`).
//!
//! The image boots init with `memkeep.service` enabled and marks that matter
//! in a 768 MiB guest (`low` 256 MiB, `chunk` 32 MiB), and a fresh home
//! volume (`btrfs_disk::home_copy`), which the kernel mounts writable at
//! `/home`: a disk's clean cache is what `memory.reclaim` can give back,
//! where a file of the initramfs or a tmpfs has nothing to be read back from.
//! The guest's unit, in `system.slice`, writes a 1 GiB file there, syncs it,
//! reads it back -- more than the machine has, so the cache fills it to the
//! kernel's watermark -- and then waits up to [`WAIT_S`] seconds for
//! `MemFree` to reach `low`, and two more, and says what it is.
//!
//! The second boot is the negative control: the same image without the
//! unit's link in `basic.target`'s wants. Nothing lifts free memory there,
//! so `MemFree` must stay under `low`; a control that finds it over `low`
//! fails the gate, since then the first boot proves nothing about memkeep.

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-memkeep",
    help_at: 172,
    help: &[
        "test-memkeep  memkeep under init: a disk file's cache fills the machine and free memory is lifted back",
        "              to the low mark by memory.reclaim asks; the same boot without the unit stays under it",
    ],
    run: |_, args| test_memkeep(args),
}];

/// The unit, linked into `basic.target`'s wants for the first boot.
const UNIT: &str = "memkeep.service";

/// The low mark the gate sets, in KiB: a third of the guest, far above the
/// kernel's watermark, which is where the cache leaves `MemFree` without
/// the service.
const LOW_KIB: u64 = 256 * 1024;

/// The chunk the gate sets, in bytes.
const CHUNK: u64 = 32 * 1024 * 1024;

/// The guest's memory, in MiB, and the file it writes and reads, larger.
const MEMORY_MIB: u32 = 768;
const FILE_MIB: u64 = 1024;

/// The longest the guest waits for `MemFree` to reach `low` after the read:
/// six asks lift it from the watermark, and a back-off of ten seconds may
/// fall in between.
const WAIT_S: u64 = 40;

/// How far under `low` the reading may be and still count as kept, in KiB:
/// one chunk. The service stops asking once `MemFree` is at `low`, and
/// asks again only at its next second; the guest's last reading comes two
/// seconds after the first at `low`, so anything the guest took in between
/// (the shell's pages, `/proc` reads, the kernel's log) shows as a shortfall
/// that the next ask, of one chunk, makes good. The control's reading is
/// at the kernel's watermark, many chunks under.
const TOLERANCE_KIB: u64 = CHUNK / 1024;

/// The marks (`docs/MEMKEEP.md`).
fn config() -> String {
    format!(
        "# test-memkeep's marks (tools/common/xtask/src/memkeep.rs)\nlow = {LOW_KIB}\nchunk = {CHUNK}\n"
    )
}

/// The guest unit that runs [`SCRIPT`].
const GUEST_UNIT: &str = "\
# test-memkeep's guest half (tools/common/xtask/src/memkeep.rs).
[Unit]
Description=Fill the page cache from a disk and watch free memory
After=memkeep.service

[Service]
Type=oneshot
TimeoutStartSec=900
ExecStart=/bin/busybox sh /lib/memkeep-test.sh
";

/// The guest half; every line it prints starts with `memkeep-test:`.
/// `@FILE_MIB@`, `@LOW@` and `@WAIT@` are filled in.
const SCRIPT: &str = r#"
B=/bin/busybox
say() { echo "memkeep-test: $*"; }
mem() { while read k v u; do [ "$k" = "$1:" ] && { echo $v; return; }; done < /proc/meminfo; }
i=0
until $B grep -q ' /home btrfs ' /proc/mounts; do
  i=$((i + 1))
  if [ $i -gt 600 ]; then say "FAIL /home was never mounted"; say done; exit 0; fi
  $B usleep 100000
done
say "booted: MemTotal $(mem MemTotal) kB, MemFree $(mem MemFree) kB"
$B dd if=/dev/zero of=/home/big bs=1048576 count=@FILE_MIB@ 2>/dev/null || say "FAIL writing /home/big"
$B sync
$B cat /home/big > /dev/null || say "FAIL reading /home/big"
say "read: MemFree $(mem MemFree) kB, Cached $(mem Cached) kB right after"
i=0
while [ $i -lt @WAIT@ ] && [ "$(mem MemFree)" -lt @LOW@ ]; do $B sleep 1; i=$((i + 1)); done
$B sleep 2
say "verdict: MemFree $(mem MemFree) kB after $((i + 2)) s, low @LOW@ kB, Cached $(mem Cached) kB"
say done
"#;

/// The files the image carries beside the kernel: init, busybox, the marks,
/// the guest's unit and script, and with `kept` the service's link.
fn image_files(arch: Arch, kept: bool) -> Result<Vec<File>> {
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    let script = SCRIPT
        .replace("@FILE_MIB@", &FILE_MIB.to_string())
        .replace("@LOW@", &LOW_KIB.to_string())
        .replace("@WAIT@", &WAIT_S.to_string());
    let bytes = |path: &str, mode: u32, content: &[u8]| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(content.to_vec()),
    };
    let link = |path: &str, to: &str| File {
        path: path.to_owned(),
        mode: 0o777,
        content: Content::Link(to.to_owned()),
    };
    files.extend([
        bytes("bin/busybox", 0o755, &busybox),
        bytes("etc/ferrix/memkeep.conf", 0o644, config().as_bytes()),
        link("etc/ferrix/units/getty@.service", "/dev/null"),
        bytes("lib/memkeep-test.sh", 0o755, script.as_bytes()),
        bytes(
            "etc/ferrix/units/memkeep-test.service",
            0o644,
            GUEST_UNIT.as_bytes(),
        ),
        link(
            "etc/ferrix/units/multi-user.target.wants/memkeep-test.service",
            "/etc/ferrix/units/memkeep-test.service",
        ),
    ]);
    if kept {
        files.push(enabled());
    }
    Ok(files)
}

/// `memkeep` enabled with `basic.target`, as `memscrub` is, for the
/// desktop image.
pub(crate) fn enabled() -> File {
    File {
        path: format!("etc/ferrix/units/basic.target.wants/{UNIT}"),
        mode: 0o777,
        content: Content::Link(format!("/lib/ferrix/units/{UNIT}")),
    }
}

/// Both boots on every architecture asked for: free memory lifted to `low`
/// with the service, and left under it without.
fn test_memkeep(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.timeout_given {
        args.timeout = 600;
    }
    for arch in args.arches()? {
        let lines = boot(arch, &args, true)?;
        let kept = judge_kept(&lines).map_err(|why| failed(arch, &why))?;
        let lines = boot(arch, &args, false)?;
        let control = judge_control(&lines).map_err(|why| failed(arch, &why))?;
        println!("  {arch}: test-memkeep: {kept}; without the service {control}");
    }
    Ok(())
}

/// A failure, with where the serial log is.
fn failed(arch: Arch, why: &str) -> Error {
    Error::new(format!(
        "{arch}: test-memkeep: {why}.\n  Serial output is in {}",
        crate::paths::build_dir(arch).join("serial.log").display()
    ))
}

/// One boot, with the service enabled or without it, on a fresh home volume.
fn boot(arch: Arch, args: &Args, kept: bool) -> Result<Vec<String>> {
    let files = image_files(arch, kept)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = init::command_line_of(arch);
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let mut qemu_args = args.clone();
    if !qemu_args.memory_given {
        qemu_args.memory = MEMORY_MIB;
        qemu_args.memory_given = true;
    }
    qemu_args.home_image = Some(btrfs_disk::home_copy(arch, "memkeep-home.img")?);
    println!(
        "  {arch}: booting init {}, {} MiB of memory, a {FILE_MIB} MiB file written and read on /home",
        if kept {
            "with memkeep"
        } else {
            "without memkeep (the control)"
        },
        qemu_args.memory
    );
    let lines = qemu::watch_lines(arch, &image, &kernel, &qemu_args, "memkeep-test: done")?;
    for line in lines.iter().map(|line| line.trim()) {
        if line.contains("memkeep-test: ") || line.contains("memkeep: ") {
            println!("  {arch}: {line}");
        }
    }
    Ok(lines)
}

/// The guest's verdict reading: `MemFree` and the low mark, in KiB.
fn verdict(lines: &[String]) -> std::result::Result<(u64, u64), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    if let Some(failure) = lines
        .iter()
        .find(|line| line.contains("memkeep-test: FAIL"))
    {
        return Err(format!("the guest failed a step: {}", failure.trim()));
    }
    let line = lines
        .iter()
        .find_map(|line| {
            line.split_once("memkeep-test: verdict: ")
                .map(|(_, rest)| rest)
        })
        .ok_or("the guest gave no verdict")?;
    let figure = |after: &str| {
        line.split(after)
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| format!("no `{after}` figure in `{line}`"))
    };
    Ok((figure("MemFree ")?, figure("low ")?))
}

/// The boot with the service: its marks taken from the file, at least one
/// ask of a slice said, and `MemFree` at `low` within the tolerance.
fn judge_kept(lines: &[String]) -> std::result::Result<String, String> {
    let (free, low) = verdict(lines)?;
    let marks = format!("memkeep: keeping MemFree at or above {LOW_KIB} kB, {CHUNK} bytes an ask");
    if !lines.iter().any(|line| line.contains(&marks)) {
        return Err(format!(
            "memkeep never said it took the gate's marks (`{marks}`)"
        ));
    }
    let asked = lines
        .iter()
        .find(|line| line.contains("memkeep: asks ") && line.contains(".slice "))
        .ok_or("memkeep said no ask of a slice")?;
    if free.saturating_add(TOLERANCE_KIB) < low {
        return Err(format!(
            "MemFree was {free} kB after the read, under the low mark {low} kB by more than one chunk \
             ({TOLERANCE_KIB} kB); memkeep asked: {}",
            asked.trim()
        ));
    }
    Ok(format!("MemFree {free} kB against a low mark of {low} kB"))
}

/// The control: no line from the service, and `MemFree` under `low`.
fn judge_control(lines: &[String]) -> std::result::Result<String, String> {
    let (free, low) = verdict(lines)?;
    if let Some(line) = lines.iter().find(|line| line.contains("memkeep: ")) {
        return Err(format!(
            "the control ran memkeep, whose unit it does not enable: {}",
            line.trim()
        ));
    }
    if free >= low {
        return Err(format!(
            "the control's MemFree was {free} kB, at or above the low mark {low} kB with no service \
             to lift it: the read did not fill the cache, so the boot with memkeep proves nothing"
        ));
    }
    Ok(format!("MemFree stays at {free} kB, under it"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_kept_boot_passes_and_one_left_under_low_does_not() {
        let marks =
            format!("memkeep: keeping MemFree at or above {LOW_KIB} kB, {CHUNK} bytes an ask of x");
        let ask = "memkeep: asks 6 of 33554432 bytes (system.slice 6), 0 without progress, 0 refused; MemFree 30000 kB before, 270000 kB after";
        let good = lines(&[
            &marks,
            ask,
            "memkeep-test: verdict: MemFree 262000 kB after 9 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_kept(&good).is_ok());
        let under = lines(&[
            &marks,
            ask,
            "memkeep-test: verdict: MemFree 40000 kB after 42 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_kept(&under).is_err());
        let silent = lines(&[
            &marks,
            "memkeep-test: verdict: MemFree 300000 kB after 2 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_kept(&silent).is_err());
    }

    #[test]
    fn the_control_must_stay_under_low() {
        let under = lines(&[
            "memkeep-test: verdict: MemFree 30000 kB after 42 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_control(&under).is_ok());
        let over = lines(&[
            "memkeep-test: verdict: MemFree 300000 kB after 2 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_control(&over).is_err());
        let ran = lines(&[
            "memkeep: asks 1 of 33554432 bytes (system.slice 1), 0 without progress, 0 refused; MemFree 1 kB before, 2 kB after",
            "memkeep-test: verdict: MemFree 30000 kB after 42 s, low 262144 kB, Cached 1 kB",
        ]);
        assert!(judge_control(&ran).is_err());
    }
}
