//! `test-restart --boot intel`: the Intel GPU's driver killed under a
//! running Vulkan program, twice, on the QEMU stand-in for ADL-N's GPU
//! (`docs/INTEL-GPU.md` §11).
//!
//! The image is `test-intel-uapi`'s: the stand-in at 00:02.0 that answers as
//! 8086:46d4, the driver `intel-fake` under the name `intel` (the GT's
//! front end over a back end that completes every batch at once), and
//! Debian's Mesa from the intel-uapi volume. Its init runs [`RUN`], which for
//! each of two rounds starts `vk-survive` (`nvrm/test/vk-survive.c`: one
//! clear and copy submitted and waited for every 2 ms, a host-visible buffer
//! checked between rounds), waits for its first round, `kill -9`s the newest
//! `intel`, waits for devmgr to have started another and for
//! `/dev/dri/renderD128` to be back, and then asks the program to stop. The
//! judge wants, for each round:
//!
//! 1. devmgr's DIED for the driver, and then its word that it started it
//!    again and that it published (`restart 1`, then `restart 2`);
//! 2. the driver's facts line again after each death: the back end came up
//!    a second and a third time;
//! 3. the program's end as [`Expect`] says: `VK_ERROR_DEVICE_LOST` and exit
//!    status 2 for a driver whose clients' files die with it -- never a
//!    hang, which the script ends after 60 s and the judge names -- or, for a
//!    driver whose clients' objects outlive it, every round answered and the
//!    buffer intact, exit 0;
//! 4. a fresh `vk-offscreen` on the new node reaching its read-back, so the
//!    driver started again serves programs that open it afresh.
//!
//! The fake runs nothing on a GPU, so this shows the restart and the error
//! and recovery paths of devmgr, the kernel's chardev core and the driver's
//! front end, never the GT's reset: that is the box's (`docs/INTEL-GPU.md`
//! §11, the box run).

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::zinc;
use crate::{
    Error, Result, busybox, cargo, fat, initramfs, intel_uapi, native, qemu, rustc, shell,
};

/// What the program is expected to see of the driver's death.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expect {
    /// Its device is lost, as Linux's i915 says of an unplugged card: the
    /// first call after the death answers `VK_ERROR_DEVICE_LOST`.
    Lost,
    /// It never knows: every round answers and its memory is kept. The
    /// opaque crash mode's (`docs/INTEL-GPU.md` §12), a plan so far: only
    /// the judge's tests ask for it until it is built.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the opaque crash mode is planned, not built")
    )]
    Survived,
}

/// What the product does today (`docs/INTEL-GPU.md` §11).
pub(crate) const EXPECT: Expect = Expect::Lost;

/// What every line of the run's own starts with.
const SAID: &str = "gpu-restart-gate: ";

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/gpu-restart-test";
/// Where `vk-survive` and `vk-offscreen` are.
const SURVIVE_PATH: &str = "bin/vk-survive";
const OFFSCREEN_PATH: &str = "bin/vk-offscreen";

/// The init: the volume first, then [`RUN`].
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/usr/bin/vulkaninfo ] || exit 3
busybox sh /bin/gpu-restart-test || exit 4
exit 17
"#;

/// What the script exits with when the run finished.
const STATUS: &str = "17";

/// Two rounds of: a program running, its driver killed, the driver back.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp XDG_RUNTIME_DIR=/tmp
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.json MESA_SHADER_CACHE_DISABLE=1
say() { echo "gpu-restart-gate: $*"; }
newest() {
    most=0
    for p in /proc/[0-9]*; do
        read n < $p/comm 2>/dev/null
        if [ "$n" = intel ] && [ ${p#/proc/} -gt $most ]; then most=${p#/proc/}; fi
    done
    echo $most
}
waited=0
until [ -e /dev/dri/renderD128 ]; do
    waited=$((waited + 1))
    [ $waited -gt 120 ] && { say "no render node after 120 s"; exit 5; }
    sleep 1
done
for round in 1 2; do
    rm -f /tmp/vk-survive.stop /tmp/survive.out
    /bin/vk-survive > /tmp/survive.out 2>&1 &
    client=$!
    waited=0
    until grep -q "vk-survive: ready" /tmp/survive.out 2>/dev/null; do
        if ! kill -0 $client 2>/dev/null || [ $waited -gt 60 ]; then
            sed "s/^/gpu-restart-gate: round $round: /" /tmp/survive.out
            say "round $round: vk-survive never got going"
            exit 6
        fi
        waited=$((waited + 1))
        sleep 1
    done
    killed=$(newest)
    say "round $round: killing intel $killed under vk-survive $client"
    kill -9 $killed
    waited=0
    until [ $(newest) != $killed ] && [ $(newest) != 0 ] && [ -e /dev/dri/renderD128 ]; do
        waited=$((waited + 1))
        [ $waited -gt 60 ] && { say "round $round: no intel and no render node 60 s after the kill"; exit 7; }
        sleep 1
    done
    say "round $round: intel $(newest) is up again after ${waited}s"
    sleep 2
    : > /tmp/vk-survive.stop
    waited=0
    while kill -0 $client 2>/dev/null; do
        waited=$((waited + 1))
        if [ $waited -gt 60 ]; then
            say "round $round: vk-survive hung"
            kill -9 $client
            break
        fi
        sleep 1
    done
    wait $client
    status=$?
    sed "s/^/gpu-restart-gate: round $round: /" /tmp/survive.out
    say "round $round: vk-survive exited $status"
    /bin/vk-offscreen 2>&1 | sed "s/^/gpu-restart-gate: round $round: after: /"
done
exit 0
"#;

/// The kernel's line for devmgr's DIED, and for its RESTARTED of a driver
/// that published (`src/kernel/src/discovery/devmgr.rs`).
const DIED: &str = "devmgr   the driver of";
const DIED_TAIL: &str = "ended with status 137";
const RESTARTED: &str = "was started again and published (restart";

/// The fake back end's facts line, once per start.
const FACTS: &str = "intel-fake: ";

/// `vk-offscreen`'s line once it read the pixels back, right or wrong.
const READ_BACK: &[&str] = &["pixels wrong on", "pixels cleared to"];

/// The test.
///
/// # Errors
///
/// No volume, an unpatched QEMU, a build or boot failure, or a judge that
/// did not find what it wants.
pub(crate) fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let mut args = args.clone();
    args.data_image = Some(intel_uapi::volume()?);
    intel_uapi::refuse_unpatched_qemu()?;
    args.intel_gpu_standin = true;
    if !args.memory_given {
        args.memory = intel_uapi::MEMORY;
    }
    if !args.timeout_given {
        args.timeout = 420;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose intel driver is intel-fake");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let mut natives = native::build(arch, args.release)?;
    intel_uapi::use_fake(&mut natives, arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let mut files = rustc::files(intel_uapi::LINKS);
    let dir = paths::build_dir(arch).join("gpu-restart");
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    for (path, name) in [
        (SURVIVE_PATH, "vk-survive"),
        (OFFSCREEN_PATH, "vk-offscreen"),
    ] {
        files.push(crate::ports::File {
            path: path.to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(crate::nvidia::vk_test(&dir, name)?),
        });
    }
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(RUN.as_bytes().to_vec()),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let options = format!("{}\n", intel_uapi::OPTION);
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!(
        "  {arch}: the Intel driver killed twice under vk-survive, {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let log = paths::build_dir(arch).join("serial.log");
    let kept = dir.join("serial.log");
    let _ = std::fs::copy(&log, &kept);
    judge(&lines, EXPECT).map_err(|why| {
        Error::new(format!(
            "{arch}: {why}.\n  Serial output is in {}",
            kept.display()
        ))
    })?;
    println!(
        "  {arch}: the Intel driver was started again twice, and vk-survive {}",
        match EXPECT {
            Expect::Lost => "was told its device was lost each time, then a new program drew",
            Expect::Survived => "never knew",
        }
    );
    Ok(())
}

/// What a run must show; the first thing missing, said.
pub(crate) fn judge(lines: &[String], expect: Expect) -> std::result::Result<(), String> {
    let exited = lines
        .iter()
        .find_map(|line| line.trim().split(shell::EXITED).nth(1))
        .map(str::trim);
    match exited {
        Some(STATUS) => {}
        Some("3") => return Err("the volume has no vulkaninfo: is it attached?".to_owned()),
        other => {
            let said = lines
                .iter()
                .rev()
                .find(|line| line.contains(SAID))
                .map_or("", |line| line.trim());
            return Err(format!(
                "the script ended with {other:?}; its last word: `{said}`"
            ));
        }
    }
    // The driver's start at boot, then each round in order. Within a round
    // the new driver's facts and devmgr's RESTARTED race, since devmgr takes
    // an engine's driver at its word: each is looked for after the death.
    let find = |from: usize, what: &str, wanted: &dyn Fn(&str) -> bool| {
        lines
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, line)| wanted(line))
            .map(|(at, _)| at)
            .ok_or_else(|| format!("no line showed {what}, in order"))
    };
    let mut at = find(0, "the driver's facts at boot", &|line| {
        line.contains(FACTS)
    })?;
    for round in 1..=2_u32 {
        let mine = format!("{SAID}round {round}: ");
        if let Some(hung) = lines
            .iter()
            .find(|line| line.contains(&format!("{mine}vk-survive hung")))
        {
            return Err(format!(
                "vk-survive hung after the driver's death: `{}`",
                hung.trim()
            ));
        }
        let killed = find(at, &format!("round {round}'s kill"), &|line| {
            line.contains(&format!("{mine}killing intel"))
        })?;
        let died = find(
            killed,
            &format!("devmgr's DIED in round {round}"),
            &|line| line.contains(DIED) && line.contains(DIED_TAIL),
        )?;
        let restarted = find(died, &format!("devmgr's restart {round}"), &|line| {
            line.contains(&format!("{RESTARTED} {round})"))
        })?;
        let up = find(
            died,
            &format!("the back end up again in round {round}"),
            &|line| line.contains(FACTS),
        )?;
        let (ended, status) = match expect {
            Expect::Lost => ("device lost at round", 2),
            Expect::Survived => ("vk-survive: survived:", 0),
        };
        let end = find(
            killed,
            &format!("vk-survive's end as expected ({expect:?}) in round {round}"),
            &|line| line.contains(&mine) && line.contains(ended),
        )?;
        let exit = find(
            end,
            &format!("vk-survive's exit status in round {round}"),
            &|line| line.contains(&format!("{mine}vk-survive exited {status}")),
        )?;
        let after = format!("{mine}after: vk-offscreen: ");
        at = find(
            exit,
            &format!(
                "a fresh vk-offscreen reading its pixels back on the new node in round {round}"
            ),
            &|line| line.contains(&after) && READ_BACK.iter().any(|said| line.contains(said)),
        )?;
        let _ = (restarted, up);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    const ROUND: &str = "\
  @.0 | gpu-restart-gate: round @: killing intel 9 under vk-survive 40
  @.1 |   devmgr   the driver of 0x00000010 ended with status 137; the device is quiesced
  @.2 | intel-fake: 0x00000010 8086:46d4 rev 0: facts 24 EUs, DSS 0x3, CS 19200000 Hz; every batch completes at once, nothing runs
  @.3 |   devmgr   the driver of 0x00000010 was started again and published (restart @)
  @.4 | gpu-restart-gate: round @: intel 41 is up again after 1s
  @.5 | gpu-restart-gate: round @: vk-survive: ready on Intel(R) Graphics (ADL-N)
  @.6 | gpu-restart-gate: round @: vk-survive: device lost at round 77: vkQueueSubmit answered VK_ERROR_DEVICE_LOST
  @.7 | gpu-restart-gate: round @: vk-survive exited 2
  @.8 | gpu-restart-gate: round @: after: vk-offscreen: 65536 of 65536 pixels wrong on Intel(R) Graphics (ADL-N); first is (0,0,0,0)";

    fn good() -> String {
        format!(
            "  0.9 | intel-fake: 0x00000010 8086:46d4 rev 0: facts 24 EUs\n{}\n{}\n  9.0 |   init     the shell exited with 17",
            ROUND.replace('@', "1"),
            ROUND.replace('@', "2")
        )
    }

    #[test]
    fn a_good_run_passes() {
        assert_eq!(judge(&lines(&good()), Expect::Lost), Ok(()));
    }

    #[test]
    fn each_missing_line_fails_by_name() {
        for (gone, why) in [
            ("killing intel", "kill"),
            ("ended with status 137", "DIED"),
            ("published (restart 2)", "restart 2"),
            ("device lost at round", "as expected"),
            ("vk-survive exited 2", "exit status"),
            ("after: vk-offscreen", "fresh vk-offscreen"),
        ] {
            let text: Vec<String> = lines(&good())
                .into_iter()
                .filter(|line| !line.contains(gone))
                .collect();
            let verdict = judge(&text, Expect::Lost);
            assert!(
                verdict.as_ref().is_err_and(|error| error.contains(why)),
                "{gone}: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_restart_that_never_came_fails() {
        // devmgr before this change: DIED, quiesced, and nothing more.
        let text: Vec<String> = lines(&good())
            .into_iter()
            .filter(|line| !line.contains("was started again"))
            .collect();
        assert!(judge(&text, Expect::Lost).is_err_and(|error| error.contains("restart 1")));
    }

    #[test]
    fn a_hung_program_is_named() {
        let text: Vec<String> = lines(&good())
            .into_iter()
            .map(|line| {
                if line.contains("round 1: vk-survive: device lost") {
                    "  1.6 | gpu-restart-gate: round 1: vk-survive hung".to_owned()
                } else {
                    line
                }
            })
            .collect();
        assert!(judge(&text, Expect::Lost).is_err_and(|error| error.contains("hung")));
    }

    #[test]
    fn a_lost_device_is_not_survival() {
        assert!(judge(&lines(&good()), Expect::Survived).is_err());
        let survived = good()
            .replace(
                "vk-survive: device lost at round 77: vkQueueSubmit answered VK_ERROR_DEVICE_LOST",
                "vk-survive: survived: 900 rounds, every one answered, the kept buffer kept its bytes",
            )
            .replace("vk-survive exited 2", "vk-survive exited 0");
        assert_eq!(judge(&lines(&survived), Expect::Survived), Ok(()));
        assert!(judge(&lines(&survived), Expect::Lost).is_err());
    }
}
