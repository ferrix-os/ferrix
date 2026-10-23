//! `test-restart --boot intel` and `--boot intel-keeper`: the Intel GPU's
//! driver killed under a running Vulkan program, twice, on the QEMU
//! stand-in for ADL-N's GPU (`docs/INTEL-GPU.md` §11, §12.8).
//!
//! The image is `test-intel-uapi`'s -- the stand-in at 00:02.0 that answers
//! as 8086:46d4, and Debian's Mesa from the intel-uapi volume -- with the
//! driver split as §12.3 has it: the product keeper under the name `intel`,
//! and the fake GT (`intel-fake-gt`, a back end that completes every batch
//! at once) as its worker under `intel-gt`. Its init runs [`RUN`], which for
//! each of two rounds starts `vk-survive` (`nvrm/test/vk-survive.c`: one
//! clear and copy submitted and waited for every 2 ms, a host-visible buffer
//! checked between rounds), waits for its first round, `kill -9`s the
//! newest process of the [`Victim`]'s name, waits for another and for
//! `/dev/dri/renderD128`, and then asks the program to stop.
//!
//! * `--boot intel` kills the worker. The judge wants the keeper's word
//!   that it replaced worker N and that worker N+1 is up with the journal,
//!   the fake's facts again, no DIED from devmgr at all, and `vk-survive`
//!   ending `survived` with exit 0: every round answered and its buffer
//!   intact, the opaque crash mode's promise.
//! * `--boot intel-keeper` kills the keeper, the control (§12.8): §11's
//!   floor. The judge wants devmgr's DIED, its word that every process in
//!   the dead keeper's job ended -- the worker included, before anything
//!   else starts on the device (the consultant's condition 1, ledger
//!   3156) -- its restart, a new keeper and worker, and `vk-survive`
//!   ending on `VK_ERROR_DEVICE_LOST` with exit 2, never a hang (the script
//!   ends one after 60 s and the judge names it).
//!
//! Both want a fresh `vk-offscreen` on the node reaching its read-back after
//! each round, so the driver serves programs that open it afresh.
//!
//! The fake runs nothing on a GPU, so this shows the keeper, the journal,
//! the worker's replacement, devmgr and the chardev core, never the GT's
//! reset or a replayed batch's pixels: those are the host tests' (the
//! engine model) and the box's.

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::zinc;
use crate::{
    Error, Result, busybox, cargo, fat, initramfs, intel_uapi, native, qemu, rustc, shell,
};

/// Which process the rounds kill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Victim {
    /// `intel-gt`, the worker: the program must never know.
    Worker,
    /// `intel`, the keeper: the program's device is lost (§11).
    Keeper,
}

impl Victim {
    /// The name in `/proc/<pid>/comm`.
    const fn comm(self) -> &'static str {
        match self {
            Victim::Worker => "intel-gt",
            Victim::Keeper => "intel",
        }
    }

    /// What the program is expected to see.
    const fn expect(self) -> Expect {
        match self {
            Victim::Worker => Expect::Survived,
            Victim::Keeper => Expect::Lost,
        }
    }
}

/// What the program is expected to see of the death.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expect {
    /// Its device is lost, as Linux's i915 says of an unplugged card: the
    /// first call after the death answers `VK_ERROR_DEVICE_LOST`.
    Lost,
    /// It never knows: every round answers and its memory is kept.
    Survived,
}

/// What every line of the run's own starts with.
const SAID: &str = "gpu-restart-gate: ";

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/gpu-restart-test";
/// Where `vk-survive` and `vk-offscreen` are.
const SURVIVE_PATH: &str = "bin/vk-survive";
const OFFSCREEN_PATH: &str = "bin/vk-offscreen";

/// The init: the volume first, then [`RUN`] with the victim's name.
fn script(victim: Victim) -> String {
    format!(
        r#"export PATH=/bin HOME=/tmp
[ -x /data/usr/bin/vulkaninfo ] || exit 3
VICTIM={} busybox sh /bin/gpu-restart-test || exit 4
exit 17
"#,
        victim.comm()
    )
}

/// What the script exits with when the run finished.
const STATUS: &str = "17";

/// Two rounds of: a program running, `$VICTIM` killed, the driver back.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp XDG_RUNTIME_DIR=/tmp
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.json MESA_SHADER_CACHE_DISABLE=1
say() { echo "gpu-restart-gate: $*"; }
newest() {
    most=0
    for p in /proc/[0-9]*; do
        read n < $p/comm 2>/dev/null
        if [ "$n" = "$1" ] && [ ${p#/proc/} -gt $most ]; then most=${p#/proc/}; fi
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
    worker=$(newest intel-gt)
    killed=$(newest $VICTIM)
    say "round $round: killing $VICTIM $killed under vk-survive $client"
    kill -9 $killed
    waited=0
    until [ $(newest $VICTIM) != $killed ] && [ $(newest $VICTIM) != 0 ] && [ -e /dev/dri/renderD128 ]; do
        waited=$((waited + 1))
        [ $waited -gt 60 ] && { say "round $round: no $VICTIM and no render node 60 s after the kill"; exit 7; }
        sleep 1
    done
    say "round $round: $VICTIM $(newest $VICTIM) is up again after ${waited}s"
    if [ $VICTIM = intel ] && [ $worker != 0 ] && [ -d /proc/$worker ]; then
        say "round $round: the dead keeper's worker $worker outlived it"
    fi
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
/// devmgr's word that a keeper's job tree ended, worker and all.
const TREE_ENDED: &str = "and every process in its job have ended";

/// The keeper's word that a worker is up, and the fake worker's facts.
const UP: &str = "intel: the GT's worker ";
const UP_TAIL: &str = " is up";
const FACTS: &str = "intel-fake: worker ";
/// The keeper's start.
const KEEPER: &str = "the keeper; the GT is driven by worker processes";

/// `vk-offscreen`'s line once it read the pixels back, right or wrong.
const READ_BACK: &[&str] = &["pixels wrong on", "pixels cleared to"];

/// The test.
///
/// # Errors
///
/// No volume, an unpatched QEMU, a build or boot failure, or a judge that
/// did not find what it wants.
pub(crate) fn test(args: &Args, victim: Victim) -> Result<()> {
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
    println!("  {arch}: building an image whose intel driver is the keeper over a fake worker");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script(victim))?;
    let mut natives = native::build(arch, args.release)?;
    intel_uapi::use_keeper_over_fake(&mut natives, arch, args.release)?;
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
        "  {arch}: the Intel driver's {} killed twice under vk-survive, {} MiB (timeout {}s)",
        victim.comm(),
        args.memory,
        args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let log = paths::build_dir(arch).join("serial.log");
    let kept = dir.join("serial.log");
    let _ = std::fs::copy(&log, &kept);
    judge(&lines, victim).map_err(|why| {
        Error::new(format!(
            "{arch}: {why}.\n  Serial output is in {}",
            kept.display()
        ))
    })?;
    println!(
        "  {arch}: the Intel driver's {} was replaced twice, and vk-survive {}",
        victim.comm(),
        match victim.expect() {
            Expect::Lost => "was told its device was lost each time, then a new program drew",
            Expect::Survived => "never knew",
        }
    );
    Ok(())
}

/// The first line from `from` on that `wanted` accepts, or what was not
/// shown.
fn find(
    lines: &[String],
    from: usize,
    what: &str,
    wanted: &dyn Fn(&str) -> bool,
) -> std::result::Result<usize, String> {
    lines
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, line)| wanted(line))
        .map(|(at, _)| at)
        .ok_or_else(|| format!("no line showed {what}, in order"))
}

/// The script's end, as the judge reads it.
fn ended(lines: &[String]) -> std::result::Result<(), String> {
    let exited = lines
        .iter()
        .find_map(|line| line.trim().split(shell::EXITED).nth(1))
        .map(str::trim);
    match exited {
        Some(STATUS) => Ok(()),
        Some("3") => Err("the volume has no vulkaninfo: is it attached?".to_owned()),
        other => {
            let said = lines
                .iter()
                .rev()
                .find(|line| line.contains(SAID))
                .map_or("", |line| line.trim());
            Err(format!(
                "the script ended with {other:?}; its last word: `{said}`"
            ))
        }
    }
}

/// What a run must show; the first thing missing, said.
pub(crate) fn judge(lines: &[String], victim: Victim) -> std::result::Result<(), String> {
    ended(lines)?;
    for (what, bad) in [
        ("vk-survive hung", "vk-survive hung after the death"),
        (
            "outlived it",
            "a dead keeper's worker outlived its job's kill",
        ),
    ] {
        if let Some(line) = lines
            .iter()
            .find(|line| line.contains(SAID) && line.contains(what))
        {
            return Err(format!("{bad}: `{}`", line.trim()));
        }
    }
    if victim == Victim::Worker
        && let Some(died) = lines
            .iter()
            .find(|line| line.contains(DIED) && line.contains(DIED_TAIL))
    {
        return Err(format!(
            "devmgr saw the driver die, which a worker's death must not be: `{}`",
            died.trim()
        ));
    }
    let mut at = find(lines, 0, "the first worker up at boot", &|line| {
        line.contains(&format!("{UP}1{UP_TAIL}"))
    })?;
    for round in 1..=2_u32 {
        at = judge_round(lines, at, victim, round)?;
    }
    Ok(())
}

/// One round, from line `at`: where it ended.
fn judge_round(
    lines: &[String],
    at: usize,
    victim: Victim,
    round: u32,
) -> std::result::Result<usize, String> {
    let mine = format!("{SAID}round {round}: ");
    let killed = find(lines, at, &format!("round {round}'s kill"), &|line| {
        line.contains(&format!("{mine}killing {}", victim.comm()))
    })?;
    match victim {
        Victim::Worker => {
            // Worker `round` dies; worker `round + 1` takes the journal.
            let dead = round;
            let next = round + 1;
            let replaced = find(
                lines,
                killed,
                &format!("the keeper replacing worker {dead}"),
                &|line| line.contains(&format!("{UP}{dead} is replaced (Died)")),
            )?;
            let _ = find(
                lines,
                replaced,
                &format!("worker {next}'s facts"),
                &|line| line.contains(&format!("{FACTS}{next}:")),
            )?;
            let _ = find(lines, replaced, &format!("worker {next} up"), &|line| {
                line.contains(&format!("{UP}{next}{UP_TAIL}"))
            })?;
        }
        Victim::Keeper => {
            // devmgr says the tree ended before the kernel prints its DIED:
            // both after the kill, and the restart after both.
            let died = find(
                lines,
                killed,
                &format!("devmgr's DIED in round {round}"),
                &|line| line.contains(DIED) && line.contains(DIED_TAIL),
            )?;
            let tree = find(
                lines,
                killed,
                &format!("the dead keeper's job tree ended in round {round}"),
                &|line| line.contains(TREE_ENDED),
            )?;
            let _ = find(
                lines,
                died.max(tree),
                &format!("devmgr's restart {round}"),
                &|line| line.contains(&format!("{RESTARTED} {round})")),
            )?;
            let keeper = find(
                lines,
                tree,
                &format!("a new keeper in round {round}"),
                &|line| line.contains(KEEPER),
            )?;
            let _ = find(
                lines,
                keeper,
                &format!("the new keeper's first worker up in round {round}"),
                &|line| line.contains(&format!("{UP}1{UP_TAIL}")),
            )?;
        }
    }
    let (ending, status) = match victim.expect() {
        Expect::Lost => ("device lost at round", 2),
        Expect::Survived => ("vk-survive: survived:", 0),
    };
    let end = find(
        lines,
        killed,
        &format!(
            "vk-survive's end as expected ({:?}) in round {round}",
            victim.expect()
        ),
        &|line| line.contains(&mine) && line.contains(ending),
    )?;
    let exit = find(
        lines,
        end,
        &format!("vk-survive's exit status in round {round}"),
        &|line| line.contains(&format!("{mine}vk-survive exited {status}")),
    )?;
    let after = format!("{mine}after: vk-offscreen: ");
    find(
        lines,
        exit,
        &format!("a fresh vk-offscreen reading its pixels back on the node in round {round}"),
        &|line| line.contains(&after) && READ_BACK.iter().any(|said| line.contains(said)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    const BOOT: &str = "\
  0.8 | intel: 0x00000010 8086:46d4 rev 0: the keeper; the GT is driven by worker processes it starts and replaces (docs/INTEL-GPU.md §12)
  0.9 | intel-fake: worker 1: 0x00000010 8086:46d4 rev 0: facts 24 EUs, DSS 0x3, CS 19200000 Hz; every batch completes at once, nothing runs
  0.9 | intel: the GT's worker 1 is up: 2 engines, 24 EUs, CS 19200000 Hz";

    const WORKER_ROUND: &str = "\
  @.0 | gpu-restart-gate: round @: killing intel-gt 9 under vk-survive 40
  @.1 | intel: the GT's worker @ is replaced (Died); worker restart @ of 8
  @.2 | intel-fake: worker #: 0x00000010 8086:46d4 rev 0: facts 24 EUs, DSS 0x3, CS 19200000 Hz; every batch completes at once, nothing runs
  @.3 | intel: the GT's worker # is up, 3 ms after its start; replayed 2 VMs, 40 bindings, 2 contexts, 1 requests
  @.4 | gpu-restart-gate: round @: intel-gt 41 is up again after 1s
  @.5 | gpu-restart-gate: round @: vk-survive: ready on Intel(R) Graphics (ADL-N)
  @.6 | gpu-restart-gate: round @: vk-survive: survived: 900 rounds, every one answered, the kept buffer kept its bytes
  @.7 | gpu-restart-gate: round @: vk-survive exited 0
  @.8 | gpu-restart-gate: round @: after: vk-offscreen: 65536 of 65536 pixels wrong on Intel(R) Graphics (ADL-N); first is (0,0,0,0)";

    const KEEPER_ROUND: &str = "\
  @.0 | gpu-restart-gate: round @: killing intel 9 under vk-survive 40
  @.1 | devmgr   the driver of 0x00000010 and every process in its job have ended
  @.1 |   devmgr   the driver of 0x00000010 ended with status 137; the device is quiesced
  @.2 | intel: 0x00000010 8086:46d4 rev 0: the keeper; the GT is driven by worker processes it starts and replaces (docs/INTEL-GPU.md §12)
  @.2 | intel-fake: worker 1: 0x00000010 8086:46d4 rev 0: facts 24 EUs; every batch completes at once, nothing runs
  @.2 | intel: the GT's worker 1 is up: 2 engines, 24 EUs, CS 19200000 Hz
  @.3 |   devmgr   the driver of 0x00000010 was started again and published (restart @)
  @.4 | gpu-restart-gate: round @: intel 41 is up again after 1s
  @.6 | gpu-restart-gate: round @: vk-survive: device lost at round 77: vkQueueSubmit answered VK_ERROR_DEVICE_LOST
  @.7 | gpu-restart-gate: round @: vk-survive exited 2
  @.8 | gpu-restart-gate: round @: after: vk-offscreen: 65536 of 65536 pixels wrong on Intel(R) Graphics (ADL-N); first is (0,0,0,0)";

    fn run(round: &str) -> String {
        let one = round.replace('@', "1").replace('#', "2");
        let two = round.replace('@', "2").replace('#', "3");
        format!("{BOOT}\n{one}\n{two}\n  9.0 |   init     the shell exited with 17")
    }

    fn without(text: &str, gone: &str) -> Vec<String> {
        lines(text)
            .into_iter()
            .filter(|line| !line.contains(gone))
            .collect()
    }

    #[test]
    fn good_runs_pass() {
        assert_eq!(judge(&lines(&run(WORKER_ROUND)), Victim::Worker), Ok(()));
        assert_eq!(judge(&lines(&run(KEEPER_ROUND)), Victim::Keeper), Ok(()));
    }

    #[test]
    fn each_missing_worker_line_fails_by_name() {
        for (gone, why) in [
            ("killing intel-gt", "kill"),
            ("is replaced (Died)", "replacing worker"),
            ("intel-fake: worker 3:", "worker 3's facts"),
            ("worker 3 is up", "worker 3 up"),
            ("vk-survive: survived", "as expected"),
            ("vk-survive exited 0", "exit status"),
            ("after: vk-offscreen", "fresh vk-offscreen"),
        ] {
            let verdict = judge(&without(&run(WORKER_ROUND), gone), Victim::Worker);
            assert!(
                verdict.as_ref().is_err_and(|error| error.contains(why)),
                "{gone}: {verdict:?}"
            );
        }
    }

    #[test]
    fn each_missing_keeper_line_fails_by_name() {
        for (gone, why) in [
            ("killing intel ", "kill"),
            ("ended with status 137", "DIED"),
            ("every process in its job", "job tree"),
            ("published (restart 2)", "restart 2"),
            ("device lost at round", "as expected"),
            ("vk-survive exited 2", "exit status"),
            ("after: vk-offscreen", "fresh vk-offscreen"),
        ] {
            let verdict = judge(&without(&run(KEEPER_ROUND), gone), Victim::Keeper);
            assert!(
                verdict.as_ref().is_err_and(|error| error.contains(why)),
                "{gone}: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_lost_device_is_not_survival_and_a_death_devmgr_saw_fails_the_worker_run() {
        assert!(judge(&lines(&run(KEEPER_ROUND)), Victim::Worker).is_err());
        assert!(judge(&lines(&run(WORKER_ROUND)), Victim::Keeper).is_err());
        let lost = run(WORKER_ROUND)
            .replace(
                "vk-survive: survived: 900 rounds, every one answered, the kept buffer kept its bytes",
                "vk-survive: device lost at round 77: vkQueueSubmit answered VK_ERROR_DEVICE_LOST",
            )
            .replace("vk-survive exited 0", "vk-survive exited 2");
        assert!(judge(&lines(&lost), Victim::Worker).is_err());
        let seen = run(WORKER_ROUND).replace(
            "  1.1 | intel: the GT's worker 1 is replaced",
            "  1.1 |   devmgr   the driver of 0x00000010 ended with status 137; the device is quiesced\n  1.1 | intel: the GT's worker 1 is replaced",
        );
        assert!(
            judge(&lines(&seen), Victim::Worker).is_err_and(|error| error.contains("devmgr saw"))
        );
    }

    #[test]
    fn a_hung_program_and_an_outliving_worker_are_named() {
        let hung = run(KEEPER_ROUND).replace(
            "round 1: vk-survive: device lost at round 77: vkQueueSubmit answered VK_ERROR_DEVICE_LOST",
            "round 1: vk-survive hung",
        );
        assert!(judge(&lines(&hung), Victim::Keeper).is_err_and(|error| error.contains("hung")));
        let outlived = run(KEEPER_ROUND).replace(
            "  1.4 | gpu-restart-gate: round 1: intel 41 is up again after 1s",
            "  1.4 | gpu-restart-gate: round 1: intel 41 is up again after 1s\n  1.4 | gpu-restart-gate: round 1: the dead keeper's worker 8 outlived it",
        );
        assert!(
            judge(&lines(&outlived), Victim::Keeper).is_err_and(|error| error.contains("outlived"))
        );
    }
}
