# Test and gate run time

The customer's priority one from 2026-09-27: *"we seriously need to bring down
test run time."* This is phase 1's record -- where a landing gate's time goes --
and the cuts it points to. Cut since: the Arm firmware waits (item 4),
the waits on finished guests (item 3, "Cut 2") and the architectures one
after another (item 2, "Cut 3"). Owner: os-98 (was ferrix-90); phase 3,
from 2026-10-01, os-5d.

Targets to argue with (the coordinator's): a standard item gate within 5 min
on a quiet host, each desktop boot within 60 s, CI green within an hour. The
rule: no gate checks less. Where a switch from TCG to KVM loses what TCG
catches, a TCG variant stays in CI.

## Measured, 2026-09-27

On main 11532464, one step at a time in a worktree of its own
(`~/ferrix-logs/gate-time/`: `measure.sh`, `summary.txt`, and a log per step
whose lines `stamp.py` stamped with seconds since the step began). The host
was **not** quiet: the load average at each step's start is given, and the
fleet's other gates ran beside it. So the figures are upper bounds. Their
proportions are what they are for.

**A: a standard item gate after a one-line kernel change** (`touch
src/kernel/src/main.rs`):

| Step | Wall | Load | Where it goes |
|---|---:|---:|---|
| `cargo xtask check` | 193 s | 12 | Clippy, of which 12 cross-target passes (6 whole-kernel), all sequential |
| `build --arch all --release` | 293 s | 14 | Three release kernels, one after another |
| `test-boot --arch x86_64` (TCG) | 37 s | 22 | ~20 s building the debug image, then the guest |
| `test-boot --arch aarch64` | 94 s | 21 | 74 s building, 20 s the guest, 5.3 s of it EDK2's wait |
| `test-boot --arch armv7a` | 58 s | 31 | 40 s building, 17 s the guest, 2.0 s of it U-Boot's countdown |
| `test-boot --arch armv7a --smp 2` | 24 s | 29 | Built already |
| `test-init --arch all` | 129 s | 26 | Three arches, one after another |
| **Total** | **828 s** | | about 14 min |

**B: the same, nothing changed** -- the floor each step pays:

| Step | Wall | Load |
|---|---:|---:|
| `test-boot --arch x86_64`, TCG | 20.5 s | 16 |
| `test-boot --arch x86_64 --accel kvm` | 16.4 s | 16 |
| `test-boot --arch aarch64` | 24.7 s | 17 |
| `test-boot --arch armv7a` | 19.3 s | 16 |
| `build --arch all --release` | 12.5 s | 14 |
| `cargo xtask check` | 154 s | 16 |

A warm x86-64 boot is 4 s faster on KVM than on TCG: the guest is not most of
a boot. `check` with nothing to rebuild still takes two and a half minutes.

**C: gates past the item gate:**

| Step | Wall | Load |
|---|---:|---:|
| `test-shell --arch x86_64` | 31 s | 12 |
| `test-boot --arch x86_64` after it | 12 s | 10 |
| `test-restart --arch x86_64` | 22 s | 9 |
| `test-compositor --arch x86_64` | **> 1,107 s**, stopped for the wind-down after 33 of its boots | 9-15 |
| `cargo xtask coverage --arch x86_64` (TCG, drcov) | about 22 min, 13:59 to 14:21 | 3-24 |

(`test-net` was not measured: it needs `--init`, which the script left out.)

A compositor boot averaged **31.9 s**: 12.6 s from the guest's first serial
line to its last, and **17.9 s** from that last line to the next boot's image.
That gap is the 5 s power-off grace, a 2 s trailing read, the stop and the
next build's cargo invocations. So 56% of test-compositor is not the guest.

## Measured, 2026-09-28: switching the built-in init

On main a0772269, in a worktree and target dir of their own, at load 1.5 to
9 (`~/ferrix-logs/gate-time/tt-cut1/`: `before-summary.txt`,
`switch-builds.txt` and a log per step). The kernel step is cargo's own
"Finished in" for `ferrix-kernel`:

| Build | Kernel step | Load |
|---|---:|---:|
| debug, cold (the crate and every dependency), each arch | 16-18 s | 2-4 |
| debug, init switched, x86_64 (inside test-boot, test-shell, test-vfs) | 1.5-3.3 s | 1.5-5.5 |
| debug, init switched, aarch64 / armv7a (`build`, with and without `--init`) | 1.5-1.9 s / 1.6-2.3 s | 4-9 |
| release, cold, x86_64 | 72 s | 7.5 |
| release, init switched, x86_64 | 69-71 s | 4.5-6 |

A flavour's debug kernel target is 1.2-1.4 GB per arch, 730 MB of it the
kernel's incremental cache.

x86_64, one after another, on a target dir every flavour had been built in:

| Step | Wall | Load |
|---|---:|---:|
| `test-boot` | 12.3 s | 2.0 |
| `test-shell --init` (Alpine's musl busybox) | 45.1 s | 2.0 |
| `test-boot` | 12.4 s | 1.5 |
| `test-vfs --init` (the same) | 15.6 s | 3.3 |

So the gates do not pay what cost 1 below supposed. The image row's gate
(`check`, `build --release`, the four test-boots, test-init) builds only the
kernel with no init and never switches; in the stage 7 and 8 row each
test-shell program and test-vfs is a flavour's first use, which a target
dir per flavour would make a cold build (16-18 s, not 2 s). Keying the dir
by the init's digest had problems of its own: test-net's command list holds
the ports of the host's stub servers, so its digest changes every run and
each run would make a new dir that is never reused; a rebuilt busybox would
start cold; and a digest in `--target-dir` enters `builds.rs`'s build keys,
so test-selfhost's replay would miss whenever the init is an earlier build's
output, whose bytes Ferrix makes differently. The two variants left, dirs
per flavour for `--release` only and the init carried in the image rather
than built into the kernel (a `src/kernel/` change, for the certification
consultant), are sound but not worth it today: neither takes time off a
gate as the gates are.

test-shell's 45 s is not the kernel build (1.5 s). With `--init` it boots
four times: the built-in script, the same shell started by `ferrix.init=`,
`poweroff -f -n` after writing `/data/k7`, and reading it back under
`ferrix.onexit=panic`. Each guest runs about 9.2 s, and the last one's panic
is followed by the 5 s power-off grace before QEMU is stopped (cost 3). The
31 s in C above was test-shell with zinc, which boots twice.

## Where the time goes: the five likely costs

From a read of `tools/common/xtask/src` and `src/kernel/src` (file and line in the survey,
kept in the owner's handover), checked against the timings above:

1. **The kernel crate rebuilds whenever a gate embeds a different init.**
   `FERRIX_INIT`, `FERRIX_INIT_SCRIPT`, `FERRIX_INIT_COMMANDS` and their
   digests are `rerun-if-env-changed` (`src/kernel/build.rs`), and every flavour
   builds into one target dir. **Measured and dropped** (above, *Measured,
   2026-09-28*): in the debug profile, which every test gate boots, the
   rebuild is incremental and costs about 2 s; only a release build pays the whole
   kernel crate again, about 70 s, and no gate switches flavours in release.
   A target dir per flavour would turn each flavour's first build into a
   cold one and win back 2 s only when the same worktree used it again.
   Cut 1 was dropped by the product owner on 2026-09-28. **Closed by
   another route, 2026-10-04 (stage 20):** a test's init is no longer
   compiled in at all. Its program, script and commands go in the image's
   initramfs under `.ferrix/init/` (`init::set_inputs`), so one kernel serves
   every test and `src/kernel/build.rs` reads no `FERRIX_INIT*`. A gate saves
   only the 2 s relink measured above; what it was for is stage 20's
   self-hosted builds, where the plan of the 57-row matrix fell from 66
   kernel builds to 6 (271 builds to 188) and Ferrix no longer writes some
   sixty 125 MB kernel ELFs onto the btrfs volume whose pages it keeps in
   memory.
2. **Every `--arch all` loop is sequential**: `build`, `test-boot`,
   `test-init`, `test-compositor`, `test-audio` and `coverage`. The host has
   24 threads and a boot uses 4. **Cut:** run the three arches' boots at
   once, with output buffered per arch and failures reported per arch.
3. **The power-off grace, 5 s, on every boot whose guest is still running
   when its check returns** (`qemu.rs` `POWER_OFF_GRACE`): every compositor,
   restart, sysfs, audio, display and chrome boot, about 30 per arch in
   test-compositor alone. Add 2 s trailing reads in about 22 compositor
   boots, and settle sleeps of 1.5 to 9 s in jobs, init, auth, sysfs and
   restart. **Cut:** a guest the check is done with is stopped, not waited
   for, and fixed settles become waits on the line they wait for. **Cut**
   by the PO session on 2026-09-28, for the gates below ("Cut 2").
4. **Firmware waits on every Arm boot**: EDK2 5.3 s on aarch64, U-Boot's 2 s
   autoboot on armv7a. **Cut** by ferrix-79 (was ferrix-e4) on 2026-09-27:
   to 0.37 s and 0.18 s (the BACKLOG's done row).
5. **TCG by default everywhere** (`qemu::accelerator`), for reproducibility
   and because CI has no hypervisor. It costs less than expected on a warm
   test-boot (4 s of 20), but more in long guests: chrome's timeouts are
   written for emulation. **Cut:** KVM by default on x86-64 where the host
   has it and nothing checked needs TCG, with CI's TCG boots kept.

Also found, smaller or later:
- **`check` is sequential**: 18 Python gates, five workspaces with target
  dirs of their own, and 12 cross clippies that could be one cargo call with
  several `--target`s.
- **CI**: no job has `timeout-minutes`. Miri is one job of 16 crates one
  after another. The fuzz job spends 30 s on each of 37 targets.
- **The guest's own fixed waits are small**: the boot self-checks sleep
  about 1.5 s in all. The ~6 s of checks on an Arm boot is emulated work.
  test-compositor's boots wait for `hyprix:`, not `FERRIX-BOOT-OK`, so they
  could boot with `ferrix.checks=skip`: the checks are proved by test-boot.

## Cut 2: a finished guest stopped, not waited for (2026-09-28)

xtask only; owner the PO session. Each wait was read for what it protects
(`git log -S` on it) before it was cut; one that protects a check stayed.

- **The power-off grace.** `qemu::finish` gave every guest 5 s to power
  itself off once its hook returned, then asked QEMU to stop. A guest that
  powers off -- `test-boot`, `test-shell`, a hook that types `poweroff` and
  waits for `reboot: Power down` -- is gone in well under a second, so the
  grace cost those nothing; a guest that never powers off paid all of it,
  and every one of `test-compositor`'s 33 boots was ended by SIGTERM after
  the full 5 s. Nothing looked at those 5 s: the transcript ends when the
  hook returns, and `Ended::powered_off` is read only by
  `watch_to_power_off`, whose hook waits for the port to close. A hook now
  says `Watching::stop_when_done()` and QEMU is asked to stop at once --
  still SIGTERM, so a coverage run's drcov table is written as before. Said
  by test-compositor's boots (idle's too), test-restart, test-sysfs and
  test-jobs. Hooks whose guest powers off keep the grace: test-init,
  test-auth, the K7 boots, and test-audio's, whose WAV QEMU's backend is
  still writing.
- **The compositor's trailing 2 s read**, `read_more(|_| false)`, there for
  "whatever else the guest said by now" (1856fd08): now
  `Watching::read_what_was_said`, which reads until the guest has been
  quiet for 0.5 s, 2 s at most. The full 2 s stays where the frame reports
  it lets in are judged, since they come a second or more apart: the
  pointer boot's sweep count, the slide's frame bound, and the cursor
  boot's two reads, which bracket a frame count. The driver-restart boot's
  3 s is now a wait for devmgr's second `was started again and published`,
  which races the compositor's `the card is back` and is what
  `judge_restart` counts, then the same quiet read.
- **30 s waits for answers nothing printed.** Seven boots -- bar, submap,
  taskbar, screenshot, lock, typing and fuzzel -- press SUPER C and SUPER W
  after their pictures
  and wait out `SETTLE`, 30 s, for `hyprctl clients` to answer. Their
  configurations do not bind those keys, and none of them judges the
  answer. `ask_the_sockets` now runs only where the configuration binds
  both to `hyprctl` (a unit test holds the four that do).
- **Settles before the first keystroke**: test-restart and test-sysfs 3 s,
  test-jobs, test-auth and test-init's two boots 1.5 s. Now
  `Watching::wait_for_shell`, which types `echo xtask-shell-$((6 * 7))`
  until a line says `xtask-shell-42`, again every 2 s. A prompt ends no
  line, so the answer is the first thing that shows the shell reads; typed
  early, the line waits in the terminal. Under TCG it came 1.6 s after the
  marker. The 3 s were also for "the drivers to have published": restart
  and sysfs now wait for the kernel's `published` lines they need, which
  come before the marker.
- **test-jobs**: the settle after each `kill %1` is gone; the `jobs` after
  it is typed again every 0.5 s until it says `terminated`. The three before
  a pipeline's kill, Ctrl-Z and Ctrl-C stay: they wait for a job to be
  exec'd and hold the terminal, which nothing on the console says, and the
  shell that could be asked is waiting on that job.
- **test-init**: lazy.service's 1.5 s is now `svc status` asked until its
  `STATUS=` shows, and judged there as before; the grandchild's poll is
  0.5 s apart rather than 1.5 s, for the same 30 s in all.

Measured with `~/ferrix-logs/gate-time/tt2/measure.sh` (the same
`stamp.py`), each step alone, x86_64 under TCG, before on main a0772269 in
a worktree of its own and after on the branch, both warm. The host carried
other sessions' work (a Miri run, Steam's end-to-end): loads as given.

| Step | Before (2 runs) | Load | After (3 or 4 runs) | Load |
|---|---:|---:|---:|---:|
| `test-restart` | 23.2, 24.8 s | 4-12 | 17.0, 15.2, 19.8 s | 2.5-15 |
| `test-sysfs` | 19.4, 27.0 s | 5-15 | 15.2, 13.8, 18.8 s | 3.6-13 |
| `test-jobs` | 26.2, 35.6 s | 4.5-15 | 23.5, 19.6, 25.8 s | 4.3-12 |
| `test-init --arch x86_64` | 19.6, 24.9 s | 3.6-12 | 15.8, 16.7, 15.9 s | 6.2-11 |
| `test-init --arch all`, warm | 55.9 s | 7.3 | 38.6 s | 7.8 |
| `test-auth --arch x86_64` | 47.6, 49.4 s | 3.6-10 | 46.6, 45.6 s (and a boot hang, below) | 6.8-9.3 |
| `test-compositor --arch x86_64` | **900 s**, 33 boots | 5.3 | **512, 549, 616 s** (and two failures, below) | 9-18 |

`test-compositor --arch x86_64` ran to its end for the first time, before
and after. Per boot, the time from the guest's last line to QEMU being
stopped went from 13.2 s to 0.8 s on average (434 s to 27 s over the 33
boots); the guest's own time is 11.2 s either way. What is left of a boot's
15.5 s is the guest and about 4 s of building and starting.

One `test-auth` run hung in the kernel before stage 3, the shape of the
BACKLOG's open row for that hang; nothing this cut changes runs before the
marker. Two full `test-compositor` runs failed in the animation boot at
loads of 20 to 28, with the host saturated by other sessions: once on the
slowest frame (12.3 s, over the 5 s bound) and once on the slide's last
picture, both the shapes the BACKLOG's frame-budget row already records
under load. Run alone, alternating with main, six `--boot animation` runs
at loads of 20 to 36 all passed, three each.

Not cut, since each waits for something checked: fuzzel-user's 5 s and 3 s
(boots of the user's own configuration), idle-user's 3 s linger, the cursor
boot's reads, test-audio's settle and grace. The other gates' hooks
(test-input, test-pty, test-seat, test-display, test-clipboard, test-adb,
test-badapple, test-foot, test-vkgears, test-video, the chrome and bench
boots, `test-compositor --gl`) still take the grace; each can say
`stop_when_done` once it is read for whether its guest powers off, and its
gate is run. The grace's 5 s of an idle guest also ran under drcov; they
checked nothing, but the next coverage re-measure is the one to show
whether they reached a statement nothing else does.

## Cut 3: the architectures at once (2026-09-30)

xtask only; owner os-98. `test-boot`, `test-init` and `test-audio` with
more than one architecture now run one child an architecture, all at once (`tools/common/xtask/src/parallel.rs`): xtask starts
itself again with the same arguments and that one `--arch`, each child's
output goes to `build/<arch>/xtask-<command>.log`, each is said as it ends,
and when the last has ended every log is printed whole, in the
architectures' order, and then each one's verdict; a failure on one hides
none of the others. `FERRIX_ARCHES_IN_TURN=1` keeps the old order.

What the boots wrote in one place is one an architecture now: stage 12's
writable disk, `test-init`'s volumes and the fresh root a test boots are
under `build/<arch>/` (`btrfs_disk`); the read-only images were already
written whole and renamed into place. `run`'s own root, `build/root.img`,
stays where a person's system is. cargo takes its own lock on the target
directory, so the builds queue while the boots overlap.

Measured with `~/ferrix-logs/os98-par-measure.sh` on nazuna, main 70ad8969
against the branch, alternating, each warm, the host carrying other
sessions' work:

| Step | In turn (2 runs) | Load | At once (2 runs) | Load |
|---|---:|---:|---:|---:|
| `test-boot --arch all` | 139.1, 135.9 s | 54-60 | 46.4 s (and one stall, below) | 55 |
| `test-init --arch all` | 181.9, 170.7 s | 52-54 | 86.3, 72.0 s | 34-47 |

`test-compositor` stays in turn. Run at once in the gate (2026-09-30, load
37 to 51) its three suites took 1,200 s where they take about 1,800 s in
turn, but x86-64's failed on the frame budget -- 6.7 s against the 5 s a
frame under emulation is allowed -- while AArch64's and ARMv7-A's passed:
three compositors emulating side by side push a budget the host's load
already decides (its flake row in `docs/BACKLOG.md`) over more often, and a
gate that fails for its neighbours checks less. It can join the others once
its frames are judged in the guest's own time rather than the host's.

The other at-once `test-boot` stopped with both Arm guests silent after
stage 11 for 105 s: nazuna's disk had filled (another session freed 57 GB
at that moment), and QEMU pauses a guest whose disk image cannot grow,
which the stage 12 disk, written by every boot, then had to. The in-turn
`test-init` that failed did so on the semaphore self-check, a row of its
own in `docs/BACKLOG.md`.

## Measured, 2026-10-01: where a verification waits

The customer, 2026-10-01: confirming that a feature works takes too long.
os-5d read the gate logs on nazuna and asked eight sessions what their last
verifications cost (os-79, os-ad, os-c7, os-c8, os-db, os-e8, os-fb and
os-3c). Most of the time is not spent in the guest, whose boot reaches
`FERRIX-BOOT-OK` in 8 to 10 s. It goes to four things around it:

| Cost | Seen | Where |
|---|---|---|
| Waiting for the one gate worktree | 34 of a 40-minute batch of Arm rows waited behind other sessions' gates; 6 ran | `os7c-queue.sh`, `logs/queue/osad-*` |
| Cold builds in a new worktree's target dir | `check` cold about 15 min, warm 3.5 to 4.5 min at load 18-28; each new branch pays it | os-3c, os-db, os-c8 |
| Steps the change does not reach | A docs-only landing runs the whole `check`, 3.5 to 9 min (338 docs-only commits since 2026-09-11); a moved `main` re-runs the whole 5-row cycle, about 7 min, 6 times in one landing | os-db, os-79 |
| Work every boot repeats | `test-compositor` boots about 33 times an architecture and each boot runs the stage 1-12 self-checks, 8 to 10 s, before its scenario: 512-616 s on x86_64, 1,200 s on each Arm (`logs/os98-par-gate/compositor-all.log`) | the gate logs |

Also: a release kernel relinks with fat LTO in 2 to 3 min after a one-line
edit (os-c7); Steam's gates reach sign-in 135 to 155 s into a boot, so each
round of learning one screen costs 5 to 8 min (os-e8); a negative control
re-run because its diff was not logged (os-ad); port scripts that delete
their build dir rebuild git, foot or Mesa from nothing (os-3c, os-c8).
nazuna ran at load 18 to 20 of 24 threads, its disk at 96%.

`check` with a warm target dir, from `os-db-audit-rows/check.log`
(4 min 21 s at load 18-28): cargo's own "Finished" times add to about
150 s and the tests to about 60 s, so about a minute is starting cargo
and the Python audits. The six kernel clippies are 11 to 16 s each, one
after another.

## Phase 3: the plan (os-5d, 2026-10-01)

In the order of the time they take off a verification. A is the gate
host's scripts and touches no code; B and C are xtask, and C2 a kernel
option the checks already have. Each lands as a slice of its own with
before and after from this file's method. The rules above still hold, and
these with them (os-3c, os-ad): a negative control runs in the same build
as its check and shows it fired; a row a change touches runs on every
architecture it builds for; generated docs and coverage are regenerated,
never skipped. C2 and B1's table of paths to rows go to the certification
consultant before they land.

| # | Cut | Takes off | Points |
|---|---|---|---:|
| A1 | **A pool of warm gate slots** (`~/.local/share/ferrix/fleet/gate.sh`): each slot a worktree at a fixed path with a target dir that is kept, a run takes the first free one. Slot 1 is the stage-13 queue's, so `os7c-queue.sh` keeps working | the queue's wait: os-ad's 40 min to about 15 | 3 |
| A2 | **A disk guard**: a run is refused, not started, under 20 GB free; a report of target dirs and worktrees nothing has used for a day, for their owners | gates that fail on a full disk and read as code failures | 1 |
| A3 | **Controls the runner logs**: commit, the diff that ran, the expected text and a FIRED / DID NOT FIRE line | re-runs for evidence that was not kept | 1 |
| A4 | **Builds that are kept**: ports rebuilt only when stale (BACKLOG row, `os-12/ports-autobuild`), and the cost of a cold target dir measured against sccache for a new worktree | 5 to 10 min where a change touches ports or apps; most of a first `check` | 3 |
| B1 | **`xtask gate --since main`**: the changed paths pick the rows of *What a landing runs*, and the rows it chose are printed for the report; `check` skips a workspace the branch does not touch; a docs-only diff runs only the steps that read docs, generated docs still regenerated; after a rebase, only rows whose paths the new commits touched | a docs landing 3.5-9 min to under 1; most of a re-gate | 3 |
| C1 | **`check` at once**: one clippy call with every `--target`, the Python audits beside cargo, a time printed for each step | 154 s to about 70-90 s (to measure) | 2 |
| C2 | **`test-compositor` with `ferrix.checks=skip`**: its boots wait for `hyprix:`; the checks are proved by the gate's `test-boot` | 4 to 5 min an architecture | 1 |
| C3 | **KVM by default on x86-64** (item 5, *Next* below); CI keeps TCG | the long x86 guests: compositor, chrome, steam | 2 |
| C4 | **`--profile iterate`**: release with thin LTO and 16 codegen units, for working on a kernel; gates keep fat LTO | 2-3 min to about 30-45 s a kernel edit | 1 |
| C5 | **A guest saved and resumed at a named point** (QEMU savevm or migrate to file) for gates that boot long, Steam and Chrome first, and `--hold` to keep a guest up after its verdict; a spike first, since the GL devices may refuse migration | a Steam round 8 min to 1-2 | 5 |

A kernel-touching feature, estimated: today 0-34 min waiting, 4-15 min of
`check`, 3-7 min of rows, 10-20 min more with the compositor; after A to C3,
little waiting, about 1.5 min of `check`, 2-4 min of rows, about 5 min of
compositor on x86_64.

### A1-A3 in place (2026-10-01)

`gate.sh run <ref> <tag> <xtask args...>` and `gate.sh control <ref> <tag>
<file> <old> <new> [--all] (--expect TEXT | --expect-line TEXT) -- <xtask
args...>` take the arguments `os7c-queue.sh` takes, and `gate.sh status`
shows the slots, the load and the disk; `gate.sh` alone prints its own
description. A run waits while the load is above 36 and is refused under
20 GB free. Waiting runs are served in the order they queued (a ticket
each in `logs/queue/waiting`, which `status` lists); runs through
`os7c-queue.sh` take slot 1 by its own lock.

The log is `logs/queue/<tag>.log`. Its header has what a reviewer needs to
cite the run: the full commit and tree hashes, the slot and that its
worktree was clean after the checkout (a dirty slot is refused), the UTC
times queued and started, the seconds waited, `uptime`, `rustc -V`, each
QEMU's path and version, and the exact xtask arguments; then `== xtask
output`, the end time and the run's length, a verdict, and `queue: exit N`.
`logs/queue/INDEX` has a line a run (UTC time, tag, full hash, mode,
verdict, log), for the auditor and the consultant, and
`logs/queue/pool-summary` a line a run with the seconds waited and run.

**Verdicts.** `run: PASSED` only when xtask exited 0 and, for `test-boot`,
the output holds `FERRIX-BOOT-OK`. A control's edit is refused when its text
matches nowhere, more than once (unless `--all`), or changes nothing; the
log keeps the matched line numbers and the diff that ran. `--expect TEXT`
ends `control: FIRED (panic): <the line>` only when xtask failed and a line
of its output, never the header, holds both `FERRIX-PANIC` and TEXT.
`--expect-line` drops the panic, for a control whose failure is not a kernel
panic, and then also needs a guest's serial line (`  <seconds> | ...`) in
the output, proof that the build finished and a guest started; it ends
`control: FIRED (line, no panic): <the line>` with a `guest started:` line
above. Anything else is `control: DID NOT FIRE (why)`. The certification
consultant accepts either FIRED line as a control's evidence (os-ad,
2026-10-01). The first version searched the whole log, whose header quotes
TEXT, so any failure read FIRED (finding D1, the same day); fixed before any
control evidence was cited, with these runs on main 204f7214 and ff5e45ef
(`logs/queue/os5d-d1-*.log`), from before the verdict named its mode:

| Run | Edit | Verdict |
|---|---|---|
| fires | the last-message check's comparison turned around | `control: FIRED: 5.55 \| FERRIX-PANIC stage 9 self-check failed: a closed peer's messages were not read back in order` |
| passes | a comment changed | `control: DID NOT FIRE (xtask passed)` |
| nobuild | `compile_error!` carrying the expected text, which the compiler's output quotes four times | `control: DID NOT FIRE (xtask failed with exit 1, but no output line with FERRIX-PANIC holds the expected text)` |
| nochange | old and new the same | `control: REFUSED (the edit changed nothing)` |
| twice | a text that matches more than once | `control: REFUSED (matches more than once; --all not given)` |
| run | `test-boot --arch x86_64 --accel kvm` | `run: PASSED` |

**Slots.** Slot 1 is `os7c-n4` with `target-os7c`, the stage-13 queue's,
whose lock it shares; slot 2 `gate-slot-2` with `target-gate-slot-2`; slot 3
`gate-slot-3` with `target-gate-slot-3`, opened the same afternoon when seven
runs waited for two slots at a load of 7 (113 GB free). `target-gate-slot-2`
held 15 GB after one `check` and one boot, `target-os7c` 31 GB after a day of
stage-13 rows. While free space is under 50 GB a run does not take slot 3,
and once slot 3 is idle `gate.sh` deletes its worktree and target dir by
exact name and says so in `pool-summary`.

First runs, main ac7b1ca0, at load 10 to 17: slot 2's first `check`, from
an empty target dir, 338 s; a warm `check` on the next branch 152 s; a
control through `test-boot --arch x86_64 --accel kvm` 44 s. None waited.

**Deadlock, 2026-10-01 21:15.** A run held to one slot (`GATE_SLOT=3`,
`cs-sshdt`) took a ticket but never waits its turn, and slot 3 is not used
under 50 GB free; holding the oldest ticket, it stopped every run behind it
with all three slots free (20 waiting at 21:28, 43 GB free). The fix, not
installed: such a run's ticket says `(slot N only)` and `head_of_queue`
passes over a live ticket that ends so (eight lines). Until
then, either free space above 50 GB, or end that one run.

The phase's owner keeps the slots. When the pool winds down, the owner
deletes `gate-slot-2` and `gate-slot-3` with their target dirs, by exact
name; slot 1 stays the stage-13 queue's, and its owner keeps or deletes it.

### The Windows host as a gate (2026-10-01, not usable yet)

To take load off nazuna, `check` ran cold on the Windows PC, in fresh
worktrees `win-slot-1` (367047688) and `win-slot-2` (main 020dc9b2). The
compositor's, zinc's and ferrousli's steps run there in WSL's Ubuntu,
whose cargo target dirs are `~/.cache/ferrix/target/<worktree path>`.
Neither run finished:

| Run | Ended | After | Why |
|---|---|---|---|
| win-slot-1 | `compositor: tests`, `rustdoc` for `compositor-anim` | 425 s | `Input/output error (os error 5)` starting the program; the step alone passed afterwards |
| win-slot-2 | `compositor: tests`, two waybar tests | 380 s | `Read-only file system (os error 30)` writing under `/tmp` |

Both were the host, not the code: C: had 0 bytes free. WSL's disk is a
file on C: (`ext4.vhdx`, 366 GB that day, beside Docker's 226 GB) that
grows with every target dir and never shrinks on its own; when it could
not grow, ext4 saw write errors and remounted itself `emergency_ro`.
Recovery: 33 GB of Visual Studio installer leftovers in `%TEMP%` and the
pip and npm caches deleted; Ubuntu stopped; the disk attached bare to
another distribution and `e2fsck -f -n` run on it, which found it clean.
Prepared for the user, not yet confirmed run: deleting the build caches
of 34 worktrees that no longer exist (19 GB), then `diskpart compact
vdisk`.

The steps that did finish, slowest first (win-slot-2, cold): tests
126.9 s, compositor tests 61.7 s, the audits at once 59.2 s (the
architecture document the longest), documentation 33.7 s, doc tests
20.3 s, compositor clippy 15.5 s, host clippy 15.3 s; every other step
under 12 s. Slot 2 on nazuna takes 338 s for the whole of a cold `check`.

Before the Windows host takes gates: a cap on its target dirs, or the
WSL disk moved off C:, and a disk guard as `gate.sh`'s (A2) that refuses
a run with C: under 20 GB; then a cold and a warm `check` measured to
the end. Not done: a retry when WSL fails to start a program, which
would also hide a real failure, so it waits until the disk is ruled out.

## Next

Phase 2 in the order the numbers give: (3) stop rather than wait for a
finished guest, done for the gates in "Cut 2" above; (2) arches in
parallel, done in "Cut 3" for all but test-compositor; then (5), KVM by
default on x86-64. For (5), read on 2026-09-30 and not started: the choice
is `qemu::accelerator`, whose `None` means `tcg` today. The plan is `kvm`
for an x86-64 guest on an x86-64 Linux host whose QEMU lists it, and `tcg`
otherwise, so CI, which has no `/dev/kvm`, keeps its TCG boots unchanged;
`tcg` whenever `FERRIX_QEMU_PLUGIN` is set, since a coverage plugin sees
only translated blocks; and `seam.rs` passing `tcg` itself, since its
measurement was made under it. The frame budgets of `test-compositor`
are written for emulation and stay as they are, looser than KVM needs. Cut 1 was measured and
dropped (2026-09-28, above). Each lands as its own slice under `land.sh`,
with before and after from this table's method. Still to measure on a
quiet window the coordinator can call: `test-net`.
