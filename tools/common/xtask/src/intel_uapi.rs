//! `test-intel-uapi`: Mesa's anv and iris on the Intel driver's render node,
//! over the fake back end, on QEMU (`docs/INTEL-GPU.md` §6, M3).
//!
//! QEMU emulates no Intel GPU, so the machine carries a stand-in: a
//! `pci-testdev` at 00:02.0 that answers as Alder Lake-N, 8086:46d4 with
//! class 030000 (`tools/common/data/qemu/0006-*.patch`). The kernel takes it
//! for the Intel integrated GPU by that identity (`device.rs`,
//! `is_intel_gpu`), devmgr matches it under `ferrix.intelgpu`, and QEMU's
//! VT-d unit translates its DMA, so K3's rule lets its driver publish a render
//! node. The driver is `intel-fake` (`src/user/system/native/drivers/gpu/intel-fake`)
//! under the name `intel`, in this image alone: the front end of
//! `ferrix-intel-xelp` over a back end that completes every batch at once and
//! runs nothing. It exercises the chardev core's K3, K5 and K6 paths and every
//! ioctl Mesa makes at start-up, never the GPU's DMA, so no K2 claim may cite
//! it (the consultant's C2, ledger 2352).
//!
//! The init waits for `/dev/dri/renderD<N>`, then runs Debian 13's
//! `vulkaninfo --summary` with anv's ICD alone, `vk-offscreen`
//! (`nvrm/test/vk-offscreen.c`: a device, an image, a buffer mapped, a clear
//! submitted and its fence waited for, the pixels read back) and
//! `eglinfo -B -p surfaceless`, from the volume
//! `tools/common/fetch/fetch-intel-uapi.sh` makes. The judge wants:
//!
//! 1. the fake's facts line and the render node published;
//! 2. anv's device, named as Mesa names ADL-N, with Mesa's Intel driver id,
//!    and `vulkaninfo` exiting 0;
//! 3. `vk-offscreen` reaching its read-back: the fake runs nothing, so the
//!    pixels are wrong, and what counts is that every call before them
//!    (objects, mmaps, a context, execbuffer and its fence) answered;
//! 4. iris as the surfaceless EGL renderer on the same device, and `eglinfo`
//!    exiting 0. GBM's platform needs a card node, which a render-only driver
//!    has not, so it is not asked for;
//! 5. anv's shader cache kept between two programs: `vk-shader-cache`
//!    (`nvrm/test/vk-shader-cache.c`, 32 compute pipelines) run twice on one
//!    `MESA_SHADER_CACHE_DIR`, the first missing in Mesa's disk cache and the
//!    second missing nothing. Everything the cache key holds that comes from
//!    the driver -- the PCI id, and the topology `QUERY` answers for anv's
//!    compiler -- must be the same in the second program as in the first,
//!    and Mesa's database (`flock`, `ftruncate`, stdio over `O_RDWR`) must
//!    read back what it wrote. Endfield on the N150 compiled its shaders
//!    for 45 minutes on every start (2026-10-22); this rules out the driver
//!    and the kernel under Mesa's cache as the cause on QEMU.
//!
//! Every `intel: unserved …` line is printed: what Mesa asked that the front
//! end does not serve, to be added as M3 to M5 find them (§8, risk 4).

use std::path::PathBuf;
use std::process::Command;

use crate::args::Args;
use crate::native::{self, Built, Program};
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, qemu, rustc, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-intel-uapi",
    help_at: 625,
    help: &[
        "test-intel-uapi",
        "              Boot the Intel driver's render node over its fake back end on a QEMU stand-in for ADL-N's GPU, and",
        "              require Mesa's anv (vulkaninfo) and iris (eglinfo) to find it (tools/common/fetch/fetch-intel-uapi.sh;",
        "              a QEMU with tools/common/data/qemu/0006; x86_64)",
    ],
    run: |_, args| test_intel_uapi(args),
}];

/// The stand-in: a `pci-testdev` answering as ADL-N's integrated GPU at
/// 00:02.0, where every Intel integrated GPU sits and the kernel looks for
/// one (`qemu::x86 devices`, before any device QEMU places itself).
pub(crate) const STANDIN: &str =
    "pci-testdev,addr=02.0,x-vendor-id=0x8086,x-device-id=0x46d4,x-class-code=0x030000";

/// The volume's version, which `fetch-intel-uapi.sh` writes to
/// `intel-uapi.stamp`: raised with every change to its pins or its tree.
pub(crate) const VOLUME: &str = "1";

/// The kernel option under which devmgr drives the Intel GPU.
pub(crate) const OPTION: &str = "ferrix.intelgpu";

/// Guest RAM, in MiB: Mesa's heaps and the fake's objects.
pub(crate) const MEMORY: u32 = 4096;

/// Seconds the boot may take unless `--timeout` says otherwise.
const TIMEOUT: u64 = 300;

/// Each path glibc, the Vulkan loader, glvnd and Mesa name absolutely, and
/// where on the volume it is.
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/share/vulkan", "/data/usr/share/vulkan"),
    ("usr/share/glvnd", "/data/usr/share/glvnd"),
    ("usr/share/drirc.d", "/data/usr/share/drirc.d"),
];

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/intel-uapi-test";

/// What every line of the run's own starts with.
const SAID: &str = "intel-uapi-gate: ";

/// Wait for the render node, then the two programs, each with what it says
/// and its status.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp XDG_RUNTIME_DIR=/tmp
waited=0
node=
while [ -z "$node" ]; do
    for candidate in /dev/dri/renderD*; do
        [ -e "$candidate" ] && node=$candidate
    done
    if [ -z "$node" ]; then
        waited=$((waited + 1))
        if [ $waited -gt 120 ]; then
            echo "intel-uapi-gate: no render node after 120 s"
            exit 5
        fi
        sleep 1
    fi
done
echo "intel-uapi-gate: $node is there after ${waited}s"
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.json MESA_SHADER_CACHE_DISABLE=1 \
    /data/usr/bin/vulkaninfo --summary 2>&1
echo "intel-uapi-gate: vulkaninfo exited $?"
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.json MESA_SHADER_CACHE_DISABLE=1     /bin/vk-offscreen 2>&1
echo "intel-uapi-gate: vk-offscreen exited $?"
for run in 1 2; do
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.json MESA_SHADER_CACHE_DIR=/tmp/anv-cache \
        MESA_SHADER_CACHE_SHOW_STATS=1 /bin/vk-shader-cache > /tmp/shader-cache$run.txt 2>&1
    echo "intel-uapi-gate: vk-shader-cache run $run exited $?"
    sed "s/^/intel-uapi-gate: run $run: /" /tmp/shader-cache$run.txt
    # between the runs
done
MESA_SHADER_CACHE_DISABLE=1 /data/usr/bin/eglinfo -B -p surfaceless 2>&1
echo "intel-uapi-gate: eglinfo exited $?"
exit 0
"#;

/// The init: the volume first, then [`RUN`].
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/usr/bin/vulkaninfo ] || exit 3
busybox sh /bin/intel-uapi-test || exit 4
exit 17
"#;

/// What the script exits with when the run finished.
const STATUS: &str = "17";

/// The fake back end's line with the device's facts.
const FACTS: &str = "intel-fake: ";
/// The driver's line when the kernel published the node.
const PUBLISHED: &str = "intel: render node published";
/// The driver's line when the kernel refused it.
const REFUSED: &str = "intel: render node refused: ";
/// The front end's line for a call it does not serve.
const UNSERVED: &str = "intel: unserved ";
/// What Mesa names ADL-N (`src/intel/dev/intel_device_info.c`'s table, via
/// `include/pci_ids/iris_pci_ids.h`).
const DEVICE: &str = "Intel(R) Graphics (ADL-N)";
/// `vulkaninfo --summary`'s driver line for anv.
const ANV: &str = "DRIVER_ID_INTEL_OPEN_SOURCE_MESA";
/// `vk-offscreen`'s line once it read the pixels back, right or wrong.
const READ_BACK: &[&str] = &["pixels wrong on", "pixels cleared to"];
/// Where `vk-offscreen` is in the image.
const OFFSCREEN_PATH: &str = "bin/vk-offscreen";
/// Where `vk-shader-cache` is in the image.
const SHADER_CACHE_PATH: &str = "bin/vk-shader-cache";
/// The pipelines `vk-shader-cache` creates (its `PIPELINES`).
const PIPELINES: u32 = 32;
/// `eglinfo -B`'s renderer line, whose value names the device iris drives.
const RENDERER: &str = "OpenGL core profile renderer: ";

/// Where `fetch-intel-uapi.sh` writes the volume.
pub(crate) fn volume() -> Result<PathBuf> {
    let root = match std::env::var_os("FERRIX_INTEL_UAPI") {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".local/share/ferrix/intel-uapi"),
    };
    let image = root.join("intel-uapi.img");
    let stamp = std::fs::read_to_string(root.join("intel-uapi.stamp")).unwrap_or_default();
    if !image.is_file() || stamp.trim() != VOLUME {
        return Err(Error::new(format!(
            "no intel-uapi volume of version {VOLUME} in {}: run tools/common/fetch/fetch-intel-uapi.sh",
            root.display()
        )));
    }
    Ok(image)
}

/// Refuse a QEMU without 0006's properties, by name.
pub(crate) fn refuse_unpatched_qemu() -> Result<()> {
    let qemu = paths::which("qemu-system-x86_64")
        .ok_or_else(|| Error::new("no qemu-system-x86_64 to boot the stand-in on"))?;
    let help = Command::new(&qemu)
        .args(["-device", "pci-testdev,help"])
        .output()
        .map_err(|error| Error::new(format!("running {}: {error}", qemu.display())))?;
    if String::from_utf8_lossy(&help.stdout).contains("x-vendor-id") {
        return Ok(());
    }
    Err(Error::new(format!(
        "{} has no pci-testdev x-vendor-id: rebuild it with tools/common/fetch/fetch-qemu-linux.sh, \
         whose series carries tools/common/data/qemu/0006",
        qemu.display()
    )))
}

/// `intel-fake`, the fake back end's driver.
const FAKE: Program = Program {
    package: "ferrix-intel-fake",
    binary: "intel-fake",
    directory: native::DRIVERS,
};

/// The marker every `intel-fake` build carries (its facts line) and no
/// `intel` may: [`refuse_fake`].
pub(crate) const FAKE_MARKER: &str = "every batch completes at once, nothing runs";

/// Put `intel-fake` in `natives` under the name `intel`, which devmgr starts
/// for ADL-N: this gate's image, test-compositor's direct-scanout boot and
/// `test-restart --boot intel`'s, all on the QEMU stand-in alone
/// (`Args::intel_gpu_standin`), never on a machine with a GPU.
pub(crate) fn use_fake(natives: &mut [Built], arch: Arch, release: bool) -> Result<()> {
    let fake = native::build_one(arch, release, &paths::target_dir(), &FAKE)?;
    let slot = natives
        .iter_mut()
        .find(|built| built.name == "intel")
        .ok_or_else(|| Error::new("the native programs carry no intel driver to replace"))?;
    slot.bytes = fake.bytes;
    Ok(())
}

/// Refuse an `intel` that is `intel-fake`: every image but this gate's gets
/// the GT's driver, and the fake must never reach a machine with a GPU (C2).
///
/// # Errors
///
/// The marker in `intel`'s bytes.
pub(crate) fn refuse_fake(binary: &str, bytes: &[u8]) -> Result<()> {
    let marker = FAKE_MARKER.as_bytes();
    if binary == "intel" && bytes.windows(marker.len()).any(|window| window == marker) {
        return Err(Error::new(
            "intel is intel-fake: only test-intel-uapi's image may carry the fake back end",
        ));
    }
    Ok(())
}

/// The test.
///
/// # Errors
///
/// No volume, an unpatched QEMU, a build or boot failure, or a judge that
/// did not find what it wants.
pub(crate) fn test_intel_uapi(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => {
            return Err(Error::new(
                "test-intel-uapi runs on x86-64: Mesa's Intel drivers are",
            ));
        }
    };
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    refuse_unpatched_qemu()?;
    args.intel_gpu_standin = true;
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose intel driver is intel-fake");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let mut natives = native::build(arch, args.release)?;
    use_fake(&mut natives, arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let mut files = rustc::files(LINKS);
    let dir = paths::build_dir(arch).join("intel-uapi");
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    files.push(crate::ports::File {
        path: OFFSCREEN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(crate::nvidia::vk_test(&dir, "vk-offscreen")?),
    });
    files.push(crate::ports::File {
        path: SHADER_CACHE_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(crate::nvidia::vk_test(&dir, "vk-shader-cache")?),
    });
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(RUN.as_bytes().to_vec()),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let options = format!("{OPTION}\n");
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!(
        "  {arch}: anv and iris on the fake back end, {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let log = paths::build_dir(arch).join("serial.log");
    for line in lines.iter().filter(|line| line.contains(UNSERVED)) {
        println!("  {arch}: {}", line.trim());
    }
    judge(&lines).map_err(|why| {
        Error::new(format!(
            "{arch}: {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!("  {arch}: anv named {DEVICE}, and iris drove it through EGL");
    Ok(())
}

/// One thing the judge wants: what it is, and the line that shows it.
type Want<'a> = (&'a str, &'a dyn Fn(&str) -> bool);

/// What a run must show, in order; the first thing missing, said.
fn judge(lines: &[String]) -> std::result::Result<(), String> {
    let exited = lines
        .iter()
        .find_map(|line| line.trim().split(shell::EXITED).nth(1))
        .map(str::trim);
    match exited {
        Some(STATUS) => {}
        Some("3") => return Err("the volume has no vulkaninfo: is it attached?".to_owned()),
        Some("4") => {}
        other => return Err(format!("the script ended with {other:?}")),
    }
    if let Some(line) = lines.iter().find(|line| line.contains(REFUSED)) {
        return Err(format!(
            "the kernel refused the render node: `{}`",
            line.trim()
        ));
    }
    let wants: [Want<'_>; 8] = [
        ("the fake back end's facts", &|line| {
            line.contains(FACTS) && line.contains("24 EUs")
        }),
        ("the render node published", &|line| {
            line.contains(PUBLISHED)
        }),
        ("the render node in /dev/dri", &|line| {
            line.contains(SAID) && line.contains(" is there after ")
        }),
        ("anv's device", &|line| {
            line.contains("deviceName") && line.contains(DEVICE)
        }),
        ("anv's driver id", &|line| {
            line.contains("driverID") && line.contains(ANV)
        }),
        ("vulkaninfo exiting 0", &|line| {
            line.contains("intel-uapi-gate: vulkaninfo exited 0")
        }),
        ("vk-offscreen reading its pixels back", &|line| {
            line.contains("vk-offscreen: ") && READ_BACK.iter().any(|said| line.contains(said))
        }),
        ("iris's renderer", &|line| {
            line.contains(RENDERER) && line.contains(DEVICE)
        }),
    ];
    let mut rest = lines.iter();
    for (what, wanted) in wants {
        if !rest.any(|line| wanted(line)) {
            return Err(format!("no line showed {what}, in order"));
        }
    }
    if !lines
        .iter()
        .any(|line| line.contains("intel-uapi-gate: eglinfo exited 0"))
    {
        return Err("eglinfo did not exit 0".to_owned());
    }
    shader_cache(lines)
}

/// Mesa's disk cache statistics for `vk-shader-cache`'s run `number`:
/// `(hits, misses)` from its `disk shader cache:  hits = H, misses = M`.
fn cache_stats(lines: &[String], number: u32) -> Option<(u32, u32)> {
    let prefix = format!("intel-uapi-gate: run {number}: ");
    lines.iter().find_map(|line| {
        let (_, said) = line.split_once(prefix.as_str())?;
        let stats = said.split_once("disk shader cache:")?.1;
        let numbers: Vec<u32> = stats
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|n| n.parse().ok())
            .collect();
        match numbers.as_slice() {
            [hits, misses] => Some((*hits, *misses)),
            _ => None,
        }
    })
}

/// The second `vk-shader-cache` found every pipeline the first compiled.
fn shader_cache(lines: &[String]) -> std::result::Result<(), String> {
    for number in [1, 2] {
        let exited = format!("intel-uapi-gate: vk-shader-cache run {number} exited 0");
        if !lines.iter().any(|line| line.contains(&exited)) {
            return Err(format!("vk-shader-cache's run {number} did not exit 0"));
        }
    }
    let (Some((_, first_misses)), Some((hits, misses))) =
        (cache_stats(lines, 1), cache_stats(lines, 2))
    else {
        return Err("vk-shader-cache's runs printed no disk shader cache statistics".to_owned());
    };
    if first_misses == 0 {
        return Err(
            "vk-shader-cache's first run missed nothing in an empty shader cache".to_owned(),
        );
    }
    if misses != 0 || hits < PIPELINES {
        return Err(format!(
            "anv's shader cache lost what the first run compiled: the second run's \
             hits = {hits}, misses = {misses}, where Linux has {PIPELINES} hits and none missed"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    const GOOD: &str = "\
  1.0 | intel-fake: 0x00000010 8086:46d4 rev 0: facts 24 EUs, DSS 0x3, CS 19200000 Hz; every batch completes at once, nothing runs
  1.1 | intel: render node published (1 node)
  2.0 | intel-uapi-gate: /dev/dri/renderD128 is there after 0s
  3.0 |     deviceName         = Intel(R) Graphics (ADL-N)
  3.1 |     driverID           = DRIVER_ID_INTEL_OPEN_SOURCE_MESA
  3.2 | intel-uapi-gate: vulkaninfo exited 0
  3.5 | vk-offscreen: 4096 of 4096 pixels wrong on Intel(R) Graphics (ADL-N); first is (0,0,0,0)
  3.6 | intel-uapi-gate: vk-offscreen exited 1
  4.0 | OpenGL core profile renderer: Mesa Intel(R) Graphics (ADL-N)
  4.1 | intel-uapi-gate: eglinfo exited 0
  4.2 | intel-uapi-gate: vk-shader-cache run 1 exited 0
  4.2 | intel-uapi-gate: run 1: vk-shader-cache: 32 pipelines on Intel(R) Graphics (ADL-N) in 174 ms
  4.2 | intel-uapi-gate: run 1: disk shader cache:  hits = 0, misses = 64
  4.3 | intel-uapi-gate: vk-shader-cache run 2 exited 0
  4.3 | intel-uapi-gate: run 2: vk-shader-cache: 32 pipelines on Intel(R) Graphics (ADL-N) in 4 ms
  4.3 | intel-uapi-gate: run 2: disk shader cache:  hits = 32, misses = 0
  5.0 | init     the shell exited with 17";

    #[test]
    fn a_good_run_passes() {
        assert_eq!(judge(&lines(GOOD)), Ok(()));
    }

    #[test]
    fn each_missing_line_fails_by_name() {
        for (gone, why) in [
            ("intel-fake:", "facts"),
            ("render node published", "published"),
            ("deviceName", "anv's device"),
            ("driverID", "driver id"),
            ("vulkaninfo exited 0", "vulkaninfo exiting 0"),
            ("pixels wrong", "vk-offscreen"),
            ("OpenGL core profile renderer", "iris's renderer"),
        ] {
            let text: Vec<String> = lines(GOOD)
                .into_iter()
                .filter(|line| !line.contains(gone))
                .collect();
            let verdict = judge(&text);
            assert!(
                verdict.as_ref().is_err_and(|error| error.contains(why)),
                "{gone}: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_shader_cache_that_missed_again_fails() {
        let text: Vec<String> = lines(GOOD)
            .into_iter()
            .map(|line| line.replace("hits = 32, misses = 0", "hits = 0, misses = 64"))
            .collect();
        assert!(
            judge(&text).is_err_and(|error| error.contains("lost what the first run compiled"))
        );
        let text: Vec<String> = lines(GOOD)
            .into_iter()
            .filter(|line| !line.contains("disk shader cache"))
            .collect();
        assert!(judge(&text).is_err_and(|error| error.contains("no disk shader cache")));
    }

    #[test]
    fn a_refused_node_is_said() {
        let mut text = lines(GOOD);
        text.insert(
            1,
            "  1.1 | intel: render node refused: its device's DMA is not translated".to_owned(),
        );
        assert!(judge(&text).is_err_and(|error| error.contains("refused")));
    }

    #[test]
    fn the_fake_is_refused_as_intel() {
        let mut bytes = b"\x7fELF...".to_vec();
        bytes.extend_from_slice(FAKE_MARKER.as_bytes());
        assert!(refuse_fake("intel", &bytes).is_err());
        assert!(refuse_fake("intel-fake", &bytes).is_ok());
        assert!(refuse_fake("intel", b"\x7fELF the GT's driver").is_ok());
    }

    #[test]
    fn the_standin_is_adl_n_at_its_place() {
        assert!(STANDIN.contains("addr=02.0"));
        assert!(STANDIN.contains("x-vendor-id=0x8086") && STANDIN.contains("x-device-id=0x46d4"));
        assert!(STANDIN.contains("x-class-code=0x030000"));
    }
}
