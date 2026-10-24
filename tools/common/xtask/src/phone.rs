//! `cargo xtask phone-boot`: `test-boot`'s AArch64 boot on a phone's own
//! KVM, through Android's crosvm, rather than under QEMU's TCG here.
//!
//! The image is `test-boot`'s, kernel and initramfs both, wrapped by the
//! Pixel 7 loader (`src/boot/vendor/google/pixel7`) into the raw arm64
//! `Image` crosvm boots; the loader sees it is a guest and gives the kernel
//! crosvm's 16550 as its console. The three disks `test-boot` attaches go
//! with it, in the same order, so stages 10 to 12 find `vda` to `vdc` as
//! they do under QEMU. Everything lives in [`PHONE_DIR`], is pushed only
//! when it changed, and the guest's serial log comes back to
//! `build/aarch64/phone-serial.log`, judged as `test-boot` judges its own
//! (`qemu::judge`), less what crosvm's machine does not have.
//!
//! The phone is somebody's: nothing outside [`PHONE_DIR`] is written, a
//! VM already running there is left alone and the boot refused, the run
//! stops with the battery under [`MIN_BATTERY`] percent or the phone over
//! [`MAX_TENTHS_C`], and the script on the phone kills its own crosvm, by
//! its PID, whatever the boot did, even if the USB link drops.
//!
//! Real KVM on real Arm cores is the point: QEMU's TCG decodes every
//! instruction and runs one kind of core, so writeback accesses to MMIO,
//! missing cache maintenance and a big.LITTLE host stay hidden there.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::qemu::{self, Machine};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "phone-boot",
    help_at: 197,
    help: &[
        "phone-boot    test-boot's AArch64 boot on the Pixel 7's own KVM (crosvm, over adb as root), judged as",
        "              test-boot judges it; FERRIX_PIXEL7_SERIAL or ~/.local/share/ferrix/pixel7/serial names the phone",
    ],
    run: |_, args| phone_boot(args),
}];

/// Where everything this command puts on the phone goes, and nowhere else.
const PHONE_DIR: &str = "/data/local/tmp/ferrix-gate";

/// Android's crosvm, in the virtualization APEX.
const CROSVM: &str = "/apex/com.android.virt/bin/crosvm";

/// The lowest battery level, in percent, a boot starts at.
const MIN_BATTERY: u32 = 30;

/// The warmest the phone may be when a boot starts, in tenths of a degree
/// Celsius, as `dumpsys battery` reports it.
const MAX_TENTHS_C: u32 = 450;

/// The Pixel 7 loader's target and package.
const LOADER_TARGET: &str = "aarch64-unknown-none-softfloat";
const LOADER_PACKAGE: &str = "ferrix-boot-pixel7";

/// The script the phone runs as root: start crosvm on the image and disks,
/// wait for the kernel's verdict or the timeout, give a powering-off guest
/// a moment, and kill its own crosvm by PID if it still runs. It reports
/// `PID`, `MARK <seconds>` when the verdict line appeared, and `EXIT`.
/// Started under `setsid`, so a dropped adb link cannot leave crosvm behind.
const SCRIPT: &str = r#"D=$1; T=$2; CPUS=$3; MEM=$4
cd "$D" || exit 3
rm -f serial.log crosvm.log gate.sock
cp blank.img write.img || exit 3
t0=$(cut -d' ' -f1 /proc/uptime)
CROSVM run --disable-sandbox -m "$MEM" --cpus "$CPUS" -s "$D/gate.sock" \
  --serial type=file,path="$D/serial.log",num=1 \
  --block path="$D/test-disk.img",ro=true --block path="$D/btrfs-test.img",ro=true \
  --block path="$D/write.img" Image > crosvm.log 2>&1 < /dev/null &
P=$!
echo "PID $P"
end=$(( ${t0%.*} + T ))
while kill -0 $P 2>/dev/null && [ "$(cut -d. -f1 /proc/uptime)" -lt $end ]; do
  if grep -q -e FERRIX-BOOT-OK -e FERRIX-PANIC -e FERRIX-BOOT-UNCHECKED serial.log 2>/dev/null; then
    t=$(cut -d' ' -f1 /proc/uptime); echo "MARK $t0 $t"; break
  fi
  sleep 0.05
done
j=0; while kill -0 $P 2>/dev/null && [ $j -lt 40 ]; do sleep 0.05; j=$((j+1)); done
if kill -0 $P 2>/dev/null; then echo "STOPPED"; kill $P; sleep 1; kill -9 $P 2>/dev/null; fi
wait $P; echo "EXIT $?"
"#;

/// The files the phone needs, by the name they have there, and where each
/// comes from here.
struct Upload {
    name: &'static str,
    from: PathBuf,
}

fn phone_boot(args: &Args) -> Result<()> {
    let arch = args.single_arch()?;
    if arch != Arch::AArch64 {
        return Err(Error::new(
            "phone-boot boots AArch64 only: give --arch aarch64",
        ));
    }
    let phone = Phone::named()?;
    phone.ready()?;

    let began = Instant::now();
    crate::fat::set_strip_kernel(true);
    crate::fat::keep_parts(true);
    let (_, kernel) = crate::build_image(arch, args)?;
    let image = wrap(arch)?;
    let uploads = [
        Upload {
            name: "Image",
            from: image,
        },
        Upload {
            name: "test-disk.img",
            from: crate::test_disk::ensure()?,
        },
        Upload {
            name: "btrfs-test.img",
            from: crate::btrfs_disk::ensure()?,
        },
        Upload {
            name: "blank.img",
            from: crate::btrfs_disk::ensure_blank(arch)?,
        },
    ];
    let built = began.elapsed();

    let pushing = Instant::now();
    phone.push(&uploads)?;
    let pushed = pushing.elapsed();

    println!(
        "  {arch}: booting on the phone's KVM ({} processors, {} MiB, timeout {}s)",
        args.smp, args.memory, args.timeout
    );
    let booting = Instant::now();
    let said = phone.boot(args)?;
    let log = paths::build_dir(arch).join("phone-serial.log");
    std::fs::write(
        &log,
        phone.root_output(&format!("cat {PHONE_DIR}/serial.log"))?,
    )?;
    let ran = booting.elapsed();
    let text = std::fs::read_to_string(&log)?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();

    let to_mark = said
        .mark
        .map_or_else(|| "none".to_owned(), |s| format!("{s:.1}s"));
    println!(
        "  {arch}: build {:.1}s, push {:.1}s, boot {:.1}s (to the verdict line {to_mark}), total {:.1}s",
        built.as_secs_f64(),
        pushed.as_secs_f64(),
        ran.as_secs_f64(),
        began.elapsed().as_secs_f64()
    );
    if said.stopped {
        println!("  {arch}: crosvm was still running after the verdict, and was stopped");
    }
    if let Some(line) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        let at = lines.iter().position(|l| l == line).unwrap_or(0);
        for line in lines.iter().skip(at).take(4) {
            println!("    {line}");
        }
        return Err(Error::new(format!(
            "the {arch} kernel panicked on the phone's KVM; see {}",
            log.display()
        )));
    }
    if lines
        .iter()
        .any(|line| line.contains(qemu::UNCHECKED_MARKER))
    {
        return Err(Error::new(format!(
            "the {arch} kernel skipped its checks; see {}",
            log.display()
        )));
    }
    if !lines.iter().any(|line| line.contains(qemu::SUCCESS_MARKER)) {
        return Err(Error::new(format!(
            "the {arch} kernel never printed `{}` within {}s on the phone; see {} and {PHONE_DIR}/crosvm.log there",
            qemu::SUCCESS_MARKER,
            args.timeout,
            log.display()
        )));
    }
    qemu::judge(arch, &lines, &log, &kernel, args, Machine::Crosvm)?;
    println!("  {arch}: boot ok on the phone's KVM");
    Ok(())
}

/// Wrap the kernel and initramfs the image was just written with in the
/// Pixel 7 loader, as the raw `Image` crosvm boots, in `build/<arch>/phone`.
fn wrap(arch: Arch) -> Result<PathBuf> {
    let parts = crate::fat::parts_dir(arch);
    let kernel = parts.join("KERNEL.ELF");
    let initrd = parts.join("INITRD.IMG");
    let digest = |path: &Path| -> Result<String> { Ok(crate::sha256::hex(&std::fs::read(path)?)) };
    // The digests are what make a changed payload rebuild the loader: its
    // build script embeds the files and tracks only these variables.
    let status = Command::new(crate::cargo::cargo())
        .current_dir(paths::workspace_root())
        .args([
            "build",
            "-q",
            "-p",
            LOADER_PACKAGE,
            "--target",
            LOADER_TARGET,
            "--release",
        ])
        .env("FERRIX_PIXEL7_KERNEL", &kernel)
        .env("FERRIX_PIXEL7_KERNEL_DIGEST", digest(&kernel)?)
        .env("FERRIX_PIXEL7_INITRD", &initrd)
        .env("FERRIX_PIXEL7_INITRD_DIGEST", digest(&initrd)?)
        .status()?;
    if !status.success() {
        return Err(Error::new("the Pixel 7 loader did not build"));
    }
    let elf = paths::target_dir()
        .join(LOADER_TARGET)
        .join("release")
        .join(LOADER_PACKAGE);
    let directory = paths::build_dir(arch).join("phone");
    std::fs::create_dir_all(&directory)?;
    let image = directory.join("Image");
    let objcopy = crate::flash::llvm_objcopy()
        .ok_or_else(|| Error::new("llvm-objcopy not found: rustup component add llvm-tools"))?;
    let status = Command::new(objcopy)
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&image)
        .status()?;
    if !status.success() {
        return Err(Error::new("llvm-objcopy could not make the loader's Image"));
    }
    Ok(image)
}

/// What the phone's script said.
struct Said {
    /// Seconds from crosvm's start to the verdict line, if one came.
    mark: Option<f64>,
    /// Whether crosvm had to be stopped after it.
    stopped: bool,
}

/// The phone, by its adb serial.
struct Phone {
    serial: String,
}

impl Phone {
    /// `FERRIX_PIXEL7_SERIAL`, else the one line of
    /// `~/.local/share/ferrix/pixel7/serial`, as the Pixel 7's own tools
    /// name it: the repository names no phone.
    fn named() -> Result<Phone> {
        let serial = std::env::var("FERRIX_PIXEL7_SERIAL").ok().or_else(|| {
            let home = std::env::var_os("HOME")?;
            let file = Path::new(&home).join(".local/share/ferrix/pixel7/serial");
            std::fs::read_to_string(file).ok()
        });
        match serial.map(|serial| serial.trim().to_owned()) {
            Some(serial) if !serial.is_empty() => Ok(Phone { serial }),
            _ => Err(Error::new(
                "no phone named: set FERRIX_PIXEL7_SERIAL or write ~/.local/share/ferrix/pixel7/serial",
            )),
        }
    }

    /// `adb -s <serial>` with `arguments`, its output, failing on its status.
    fn adb(&self, arguments: &[&str]) -> Result<String> {
        let output = Command::new("adb")
            .arg("-s")
            .arg(&self.serial)
            .args(arguments)
            .output()
            .map_err(|error| Error::new(format!("running adb: {error}")))?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "adb {} failed: {}",
                arguments.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// `command` run as root on the phone, its standard output as bytes.
    fn root_output(&self, command: &str) -> Result<Vec<u8>> {
        let output = Command::new("adb")
            .arg("-s")
            .arg(&self.serial)
            .arg("exec-out")
            // adb hands the device's shell one line: the quotes keep the
            // whole command su's, its `&` and redirections included.
            .arg(format!("su -c '{command}'"))
            .output()
            .map_err(|error| Error::new(format!("running adb: {error}")))?;
        if !output.status.success() {
            return Err(Error::new(format!("on the phone, `{command}` failed")));
        }
        Ok(output.stdout)
    }

    /// Whether the phone may be used now: reachable, rooted, its battery
    /// and temperature within bounds, and no crosvm already running there.
    fn ready(&self) -> Result<()> {
        let battery = self.adb(&["shell", "dumpsys battery"])?;
        let field = |name: &str| {
            battery.lines().find_map(|line| {
                line.trim()
                    .strip_prefix(name)
                    .and_then(|rest| rest.trim().parse::<u32>().ok())
            })
        };
        let (Some(level), Some(tenths)) = (field("level:"), field("temperature:")) else {
            return Err(Error::new(
                "the phone did not report its battery level and temperature",
            ));
        };
        println!(
            "  phone {}: battery {level}%, {}.{} C",
            self.serial,
            tenths / 10,
            tenths % 10
        );
        if level < MIN_BATTERY {
            return Err(Error::new(format!(
                "the phone's battery is at {level}%, under {MIN_BATTERY}%: not booting"
            )));
        }
        if tenths > MAX_TENTHS_C {
            return Err(Error::new(format!(
                "the phone is at {}.{} C, over {} C: not booting",
                tenths / 10,
                tenths % 10,
                MAX_TENTHS_C / 10
            )));
        }
        let running = self.root_output("pidof crosvm || true")?;
        if !String::from_utf8_lossy(&running).trim().is_empty() {
            return Err(Error::new(
                "a crosvm already runs on the phone, somebody's VM: not touching it",
            ));
        }
        let _ = self.root_output(&format!("mkdir -p {PHONE_DIR} && chmod 777 {PHONE_DIR}"))?;
        Ok(())
    }

    /// Put each upload on the phone unless the copy there is the same,
    /// told by a `.sha256` file beside it.
    fn push(&self, uploads: &[Upload]) -> Result<()> {
        let there = String::from_utf8_lossy(&self.root_output(&format!(
            "cd {PHONE_DIR} && cat *.sha256 2>/dev/null || true"
        ))?)
        .into_owned();
        for upload in uploads {
            let sum = crate::sha256::hex(&std::fs::read(&upload.from)?);
            let line = format!("{sum}  {}", upload.name);
            if there.lines().any(|known| known.trim() == line) {
                continue;
            }
            let target = format!("{PHONE_DIR}/{}", upload.name);
            let _ = self.adb(&["push", &upload.from.display().to_string(), &target])?;
            let written = String::from_utf8_lossy(&self.root_output(&format!(
                "cd {PHONE_DIR} && sha256sum {0} > {0}.sha256 && cat {0}.sha256",
                upload.name
            ))?)
            .into_owned();
            if written.trim() != line {
                return Err(Error::new(format!(
                    "{} did not arrive whole on the phone",
                    upload.name
                )));
            }
            println!("  phone: {} pushed", upload.name);
        }
        Ok(())
    }

    /// Run the boot, and read what the phone's script said.
    fn boot(&self, args: &Args) -> Result<Said> {
        let script = SCRIPT.replace("CROSVM", CROSVM);
        let local = paths::build_dir(Arch::AArch64).join("phone").join("run.sh");
        std::fs::write(&local, script)?;
        let _ = self.adb(&[
            "push",
            &local.display().to_string(),
            &format!("{PHONE_DIR}/run.sh"),
        ])?;
        // A margin over the guest's own timeout for crosvm's start and stop.
        let command = format!(
            "setsid sh {PHONE_DIR}/run.sh {PHONE_DIR} {} {} {} > {PHONE_DIR}/run.out 2>&1 < /dev/null & wait",
            args.timeout, args.smp, args.memory
        );
        let deadline = Duration::from_secs(args.timeout + 30);
        let started = Instant::now();
        let result = self.root_output(&command);
        if result.is_err() || started.elapsed() > deadline {
            println!(
                "  phone: the adb link ended early; the phone's script still stops crosvm by itself"
            );
        }
        let out = String::from_utf8_lossy(&self.root_output(&format!("cat {PHONE_DIR}/run.out"))?)
            .into_owned();
        let mark = out.lines().find_map(|line| {
            let mut words = line.strip_prefix("MARK ")?.split(' ');
            let from: f64 = words.next()?.parse().ok()?;
            let to: f64 = words.next()?.parse().ok()?;
            Some(to - from)
        });
        if !out.lines().any(|line| line.starts_with("EXIT ")) {
            return Err(Error::new(format!(
                "the phone's script did not finish: {}",
                out.trim()
            )));
        }
        Ok(Said {
            mark,
            stopped: out.lines().any(|line| line == "STOPPED"),
        })
    }
}
