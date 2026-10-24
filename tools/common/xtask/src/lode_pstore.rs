//! `cargo xtask test-log-pstore`: `lode -b -1` ends with what the machine
//! said before it hung (`docs/LOGGING.md` §5, the pstore import).
//!
//! One QEMU, kept across a reset (RAM kept, as a warm reset does), its
//! image's lode keeping its store on a btrfs volume at `/data`, as
//! `test-log-budget`'s does. Boot 1 stores a line, seals the store, stops
//! lode and writes one more line to the kernel log, which lode never sees:
//! the machine has hung. The gate resets it. Boot 2's loader keeps boot 1's
//! log as the ESP's `FERRIX/LASTKERNEL.TXT` (stage 12's write disk is
//! rewritten from its fixture first, as for every boot); boot 2's lode reads it off the
//! ESP and imports the lines it had not stored as the end of boot 1. The
//! gate requires
//!
//! 1. boot 2's loader to have kept the file;
//! 2. lode's record to say it imported at least one line of boot 1;
//! 3. `lode -b -1` to end with boot 1's last line, to hold its last words
//!    once, and the line lode stored before the hang once, not doubled.
//!
//! x86-64 (KVM) only: QEMU's Arm machines attach the boot image as a
//! virtio-mmio disk, which no ring-3 driver serves.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::Qmp;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-log-pstore",
    help_at: 404,
    help: &[
        "test-log-pstore Boot init with lode, stop it and hang, reset, and require that `lode -b -1` ends",
        "              with what boot 1 said last, from the loader's LASTKERNEL.TXT (docs/LOGGING.md §5)",
    ],
    run: |_, args| test_pstore(args),
}];

/// The volume's name under `build/<arch>/`.
const VOLUME: &str = "log-pstore.img";

/// What boot 1 says when it has hung and may be reset.
const READY: &str = "pstore-test: ready for the reset";

/// What boot 2 says last.
const DONE: &str = "pstore-test: done";

/// The line boot 1 writes after lode is gone.
const LAST_WORDS: &str = "the last words, which lode never stored";

/// The line before it, which lode stored.
const STORED: &str = "a line lode stored before the hang";

/// How long each part may take.
const PATIENCE: Duration = Duration::from_secs(240);

/// Both boots' script: which one it is, by a mark on the volume.
const SCRIPT: &str = r#"bb=/bin/busybox
say() { echo "pstore-test: $*"; }
i=0
until [ -n "$($bb pidof lode)" ]; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode never ran"; say done; exit 1; }
  $bb usleep 100000
done
$bb mkdir -p /data/journal
/bin/svc restart lode.service > /dev/null 2>&1
i=0; until /bin/lode rotate > /dev/null 2>&1; do i=$((i+1)); [ $i -gt 50 ] && break; $bb usleep 100000; done
i=0
until $bb ls /data/journal/ 2>/dev/null | $bb grep -q 'fxj$'; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode's store never came to /data"; say done; exit 1; }
  $bb usleep 100000
done
# What lode stored in /run before the store came to /data, the kernel's
# line naming its pstore boot among it, moved over (journalctl --flush):
# a real system has /var/log/journal from the start.
/bin/lode --flush > /dev/null 2>&1
if [ ! -e /data/boot-1 ]; then
  echo "<14>pstore-test: a line lode stored before the hang" > /dev/kmsg
  i=0
  until /bin/lode -b -o cat 2>/dev/null | $bb grep -q 'a line lode stored before the hang'; do
    i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode never stored the line"; say done; exit 1; }
    $bb usleep 100000
  done
  /bin/lode rotate > /dev/null 2>&1
  $bb touch /data/boot-1
  $bb sync
  /bin/svc stop lode.service > /dev/null 2>&1
  i=0; while [ -n "$($bb pidof lode)" ] && [ $i -lt 100 ]; do i=$((i+1)); $bb usleep 100000; done
  echo "<14>pstore-test: the last words, which lode never stored" > /dev/kmsg
  say "ready for the reset"
  while true; do $bb sleep 60; done
fi
# Boot 2: the import, then what boot 1 ends with.
i=0
until /bin/lode -b -o cat 2>/dev/null | $bb grep -q 'LASTKERNEL.TXT'; do
  i=$((i+1)); [ $i -gt 600 ] && break
  $bb usleep 100000
done
say "said: $(/bin/lode -b -o cat 2>/dev/null | $bb grep 'LASTKERNEL.TXT' | $bb tail -n 1)"
say "imported: $(/bin/lode -b -o json 2>/dev/null | $bb grep -o '"PSTORE_IMPORTED":"[0-9]*"' | $bb tail -n 1 | $bb tr -dc 0-9)"
say "last: $(/bin/lode -b -1 -o cat 2>/dev/null | $bb tail -n 1)"
say "words once: $(/bin/lode -b -1 -o cat 2>/dev/null | $bb grep -c 'the last words, which lode never stored')"
say "stored once: $(/bin/lode -b -1 -o cat 2>/dev/null | $bb grep -c 'a line lode stored before the hang')"
say done
"#;

/// The script's unit.
const UNIT: &str = "\
# test-log-pstore's script (tools/common/xtask/src/lode_pstore.rs).
[Unit]
Description=Hang after lode, then read what the reset kept
After=lode.service

[Service]
Type=oneshot
TimeoutStartSec=600
ExecStart=/bin/busybox sh /lib/lode-pstore.sh
";

/// `cargo xtask test-log-pstore`.
fn test_pstore(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch != Arch::X86_64 {
            return Err(Error::new(
                "test-log-pstore runs on x86_64: QEMU's Arm machines attach the boot image as a \
                 virtio-mmio disk, which no ring-3 driver serves, so the ESP is not readable \
                 there (the loader's half is test-postmortem's, on aarch64 too)",
            ));
        }
        test_arch(arch, args)?;
    }
    Ok(())
}

/// The image's files: init, busybox, lode with its fetch, the store's link
/// to `/data`, and the script's unit.
fn files(arch: Arch) -> Result<Vec<File>> {
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    let bytes = |path: &str, mode: u32, content: &str| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(content.as_bytes().to_vec()),
    };
    let link = |path: &str, to: &str| File {
        path: path.to_owned(),
        mode: 0o777,
        content: Content::Link(to.to_owned()),
    };
    files.extend([
        File {
            path: "bin/busybox".to_owned(),
            mode: 0o755,
            content: Content::Bytes(busybox),
        },
        crate::lode::enabled(),
        crate::lode::pstore_enabled(),
        link("etc/ferrix/units/getty@.service", "/dev/null"),
        link("var/log/journal", "/data/journal"),
        bytes("lib/lode-pstore.sh", 0o755, SCRIPT),
        bytes("etc/ferrix/units/pstore-test.service", 0o644, UNIT),
        link(
            "etc/ferrix/units/multi-user.target.wants/pstore-test.service",
            "/etc/ferrix/units/pstore-test.service",
        ),
    ]);
    Ok(files)
}

/// One architecture's two boots.
fn test_arch(arch: Arch, args: &Args) -> Result<()> {
    let files = files(arch)?;
    println!("  {arch}: building an image whose lode keeps its store on a btrfs volume");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    let mut booted = args.clone();
    let port = crate::display::free_port()?;
    booted.qmp_port = Some(port);
    booted.keep_ram = true;
    booted.data_image = Some(btrfs_disk::blank_copy(arch, VOLUME)?);
    booted.data_image_kept = true;
    booted.timeout = booted.timeout.max(600);
    let mut boot_2: Vec<String> = Vec::new();
    let mut failure = None;
    let lines = qemu::watch_then(arch, &image, &kernel, &booted, "pstore-test: ", |at| {
        let ready = at
            .read_more(Instant::now() + PATIENCE, |lines| {
                lines
                    .iter()
                    .any(|line| line.contains(READY) || line.contains("pstore-test: FAIL"))
            })
            .map_err(|error| Error::new(error.to_string()))?;
        if let Some(line) = at
            .after()
            .iter()
            .find(|line| line.contains("pstore-test: FAIL"))
        {
            failure = Some(line.trim().to_owned());
            at.stop_when_done();
            return Ok(());
        }
        if !ready {
            failure = Some(format!("boot 1 never said `{READY}`"));
            at.stop_when_done();
            return Ok(());
        }
        println!("  {arch}: boot 1 has hung with its last words unstored; resetting it");
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        // Stage 12's write disk as every boot starts with it: the reset keeps
        // the disks as they are, and boot 1's check left its tree there.
        let _ = qmp.execute("stop", None)?;
        btrfs_disk::rewrite_blank_in_place(arch)?;
        let _ = qmp.execute("system_reset", None)?;
        let _ = qmp.execute("cont", None)?;
        let seen = at.after().len();
        let _ = at
            .read_more(Instant::now() + PATIENCE, |lines| {
                lines
                    .get(seen..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(DONE))
            })
            .map_err(|error| Error::new(error.to_string()))?;
        boot_2 = at.after().get(seen..).unwrap_or_default().to_vec();
        at.stop_when_done();
        Ok(())
    })?;
    let _ = lines;
    if let Some(why) = failure {
        return Err(Error::new(format!("{arch}: {why}")));
    }
    judge(&boot_2).map_err(|why| {
        Error::new(format!(
            "{arch}: {why}\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!("  {arch}: lode -b -1 ends with what boot 1 said before it hung, from LASTKERNEL.TXT");
    Ok(())
}

/// What boot 2 said, judged.
fn judge(boot_2: &[String]) -> std::result::Result<(), String> {
    let said = |what: &str| {
        boot_2
            .iter()
            .find_map(|line| {
                line.split_once(what)
                    .map(|(_, rest)| rest.trim().to_owned())
            })
            .unwrap_or_default()
    };
    if !boot_2
        .iter()
        .any(|line| line.contains("kept in FERRIX/LASTKERNEL.TXT"))
    {
        return Err("boot 2's loader did not keep boot 1's log in FERRIX/LASTKERNEL.TXT".into());
    }
    let imported: u32 = said("pstore-test: imported:").parse().unwrap_or(0);
    if imported == 0 {
        return Err(format!(
            "lode imported nothing of boot 1 from LASTKERNEL.TXT: `{}`",
            said("pstore-test: said:")
        ));
    }
    // Boot 1's last line is the script's own, said after the last words.
    let last = said("pstore-test: last:");
    if !last.ends_with(READY) {
        return Err(format!(
            "lode -b -1 does not end with boot 1's last line, `{READY}`: its last line is `{last}`"
        ));
    }
    let words = said("pstore-test: words once:");
    if words != "1" {
        return Err(format!(
            "boot 1's last words, `{LAST_WORDS}`, which lode never stored, are in lode -b -1 \
             {words} times, not once"
        ));
    }
    let once = said("pstore-test: stored once:");
    if once != "1" {
        return Err(format!(
            "`{STORED}`, which lode stored before the hang, is in boot 1 {once} times, not once"
        ));
    }
    Ok(())
}
