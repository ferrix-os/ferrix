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

## The pool at saturation, 2026-10-14/15 (gate-throughput-1015)

About ten agents queued gates through `gate.sh` overnight. Read from
`logs/queue/pool-summary`, 2026-10-14T22:00:59Z to 2026-10-15T01:00:48Z:
177 runs, each matched to its log (all 177 matched on commit and run length),
whose header gives the tree, the slot, the load and the free space at the
start, and whose output gives the crates compiled. The host was sampled
with `vmstat 5` and `/proc/pressure` for two minutes at 01:03Z. The scripts,
the parsed runs and the sample are in
`~/.local/share/ferrix/logs/gate-throughput-1015/`.

### Where the time goes

| | Runs | Waited | Ran |
|---|---:|---:|---:|
| All | 177 | 283,156 s (78.7 h) | 45,521 s (12.6 h) |
| Median a run | | 1,576 s | 190 s |
| Mean wait, runs ending 22:xx / 23:xx / 00:xx | 57 / 55 / 64 | 929 / 1,842 / 1,983 s | |

86% of a gate was the queue, as on 2026-10-04 (85%). The five slots ran
12.6 of their 15 slot-hours (84% busy), so the queue is set by how much
work there is: an hour of run time cut is an hour or more of waiting cut.

**Per slot.** Slots 1 and 2 are never shed; slots 3 to 5 are shed under
50 GB free.

| Slot | Runs | Ran | Sheds | Cold `check` (900+ crates compiled) |
|---|---:|---:|---:|---:|
| 1 | 47 | 10,102 s | 0 | 0 |
| 2 | 38 | 10,140 s | 0 | 0 |
| 3 | 30 | 8,927 s | 3 | 3 |
| 4 | 32 | 7,993 s | 5 | 3 |
| 5 | 30 | 8,359 s | 2 | 2 |

**Warm is not one thing.** A slot is warm for the tree it last built. The
same command on the tree the slot's previous run had, against a tree new to
the slot (medians, controls left out):

| Command | Same tree | New tree | Cold slot |
|---|---:|---:|---:|
| `check` | 271 s (4) | 332 s (17, warm deps) / 492 s (26, all) | 621 s (8) |
| `test-boot --arch x86_64` | 81 s (13) | 170 s (17) | |
| `test-boot --arch armv7a` | 103 s (7) | 193 s (7) | |
| `test-boot --arch aarch64` | 121 s (3) | 151 s (5) | |
| `test-net --arch x86_64` | 66 s (2) | 156 s (16) | |
| `build --arch all --release` | 201 s (3) | 262 s (5) | |

**The processor, not memory or the disk's speed.** The load at a run's start
had a median of 32 (9 to 43) on 24 threads. A warm `test-boot` on a tree the
slot had already built took 59 to 139 s at load 24 to 36, where a quiet host
takes 12 to 20 s (*Measured, 2026-09-27* and *2026-09-28* above): the load
multiplies a boot by three to seven. In the sample, guest time was 41-77% of
the processor, user 4-54%, idle 0-11%, I/O wait 0-3%; `/proc/pressure/cpu`
some 14-45%, `memory` some 0.00-0.20 (27 GB of swap held, but swapping in
and out near zero), `io` some 4-29% and full 0.4-6% while 15-280 MB/s were
written. A third of the processor went to guests outside the pool: the
Endfield download boot's QEMU at 384% averaged over its 3.8 hours, a second
Endfield guest at 428% (8 processors), a desktop guest at 78%; two `test-net
--smp 4` gate guests ran at 340% each. The load cap counts them all: at
01:12Z the load was 46, so 39 runs waited with slot 3 free.

### Runs no verdict needed

| What | Runs | Ran | Measured on |
|---|---:|---:|---:|
| A tree built again in another slot: rows of one commit queued as separate runs, each taking whichever slot freed | 56 of 151 runs on the 29 trees with more than one run; the 29 trees were built in 94 (tree, slot) pairs, 65 more than one each | about 5,480 s more than in a slot that had the tree (the table above, kind by kind) | the image row as seven runs: `pc2-*` on d4378e691 in all five slots, `vn3-*` and `pm-*` in four, `yt2-*`, `gw2-*`, `ca2-*` in three or four |
| The image row on a change that does not reach the image: only xtask judges or fetch scripts outside `src/`, owed today because `gate-rows` gives every `tools/common/xtask/` path the image row | 10 boots and builds on 5 commits; 2 of them on commits that touch no file the image is built or booted by | 1,176 s; 341 s of it on those 2 | `ca2-*` on 3cc6d1e2d (`claude_code.rs`, and `main.rs`, which also builds the image): five boots and a release build, 623 s; `cd5-boot-kvm` and `nb2-boot-kvm` (`main.rs`, `qemu.rs`); `es-w-boot-x86` on af91b1f87 (`wine.rs` alone) and `po-main-boot-e31` on e31ba0c8e (`frames.rs` alone) |
| Measurement repeats in a saturated pool, where the figure measures the neighbours | 9 (`gw2-meas-*`, `-r2`, `-r3` at three sizes, load 23-35) | 1,331 s | |
| The same tree and arguments passed twice | 1 (`yc-xlat3` after `yc-xlat2`) | 258 s | |
| `check-docs` beside a `check` of the same tree, which runs the same steps | 2 (`cf-docsgen-docs`, `cd5-docs`) | 127 s | |
| A `check` that failed on a Python audit after eight minutes of cargo: the audits run beside the cargo steps and finish in seconds, but `check` reports at its end | 3 | 1,142 s | `vn-check` (device-access allow-list, 506 s), `yt2-check2` (complexity budget, 371 s), `pm-check` (unsafe audit, 265 s) |

Not counted as waste: the 38 controls (8,842 s), each a different edit; and
the other 25 failed runs, which found something. All failed runs together
ran 9,894 s, 22% of the night. One failure was the pool's own: `pc2-check3`
stopped on the incremental ICE (`docs/BACKLOG.md`, 2026-10-15) and its rerun
`pc2-check4` took 1,007 s.

### The shed line thrashes

There were ten sheds between 22:21 and 00:45 (slot 3 three times, slot 4
five, slot 5 twice), 43 more from 18:00 to 22:00, and 210 in the file.
**Each of the ten was followed by a run in the same slot within 2 to 210 s,
eight of them within 50 s**, and that run built from nothing. Shedding a
slot gives back 20-27 GB, which lifts free space over 50 GB at once, so the
next pass of the wait loop takes the empty slot again. The disk gained
nothing it kept, and paid for it in builds: 8 cold `check`s at 621 s against
332 s warm, **2,677 s**, and about 250 GB written to re-warm (ten slots of
about 25 GB) while I/O pressure was 13-29%. The first boots in a re-warmed
slot were not measurably slower than new-tree boots in a warm one at
tonight's load, so the cost is in `check`.

The line sits where the disk is. Free space at a run's start had a median of
58 GB (20 to 94). The kept slots are large: `target-os7c` and
`target-gate-slot-2` hold 52 GB each, 14 GB of slot 2's in rustc's
incremental caches (`debug/incremental`), 9.6 GB in `apps/`; slot 3 held
24 GB 50 minutes after it was made again. Five slots grown to that size do
not fit beside tonight's 66 GB free. A policy that does not thrash needs a
gap between the line where a slot is given back and the line where it is
made again, at least what a cold slot grows to:

- **Make room before giving a slot back:** under the line, an idle slot's
  incremental caches go first (any slot, 1 and 2 too). They only cost the
  next build of a changed crate its incremental reuse, and the slot stays
  warm. A slot above 2 is deleted only if free space is still under the
  line afterwards.
- **Shed under 35 GB, make again at 65** (`GATE_SHED_GB`, `GATE_OPEN_GB`).
  35 GB keeps 15 GB above the refusal at 20 for the warm runs in flight. A
  slot with no target dir is made again only at 65 GB, the line plus the
  25-30 GB it grows to, so a slot given back stays given back until the
  disk can hold it. Tonight, 18 of 177 runs started under 35 GB free (22:00
  to 22:14 and 23:19); the rest started with 35 GB or more, where only the
  incremental trim would have run.

A pool that keeps fewer slots warm, rather than five slots of which three
come and go, is the other answer, and the right one if the disk stays at
97%. Free space on `/` is the customer's (morning questions).

### Cuts, ranked by run time saved on these 177 runs

| # | Cut | Removes | Saved, measured on | Where |
|---|---|---|---:|---|
| T1 | **One tree, one slot.** Each slot remembers the tree it last checked out. A waiting run whose tree a free slot last built takes that slot out of turn, and the head of the queue prefers it. The rows of one commit queued together then run one after another in the slot that built it. Later, in xtask: `gate-rows --since main --run`, the owed rows as one invocation in one slot, one checkout and one build per architecture. | the same tree built again in another slot | up to **5,480 s (1.5 h, 12%)**, the 56 runs above; the slot memory alone about half to three quarters of that, since a commit's queued rows still start in the first two slots that free. At 00:57Z `h6-*` queued 24 runs on one commit at once | `gate.sh` (`gate.sh.proposed`); xtask for `--run` |
| T2 | **No shed thrash**: incremental caches first, shed under 35 GB, made again only at 65 | cold `check`s in re-made slots; ~250 GB of writes a night | **2,677 s (0.7 h)**, the 8 cold checks; and slots that stay warm | `gate.sh` (`gate.sh.proposed`) |
| T3 | **Runs no verdict needs**: measure in a quiet window or with `GATE_SLOT` when the pool is idle, not at load 23-35; a passed tree is not run again; `check` already runs what `check-docs` does | measurements of the neighbours, repeats | **1,716 s (0.5 h)**: `gw2-meas-*` 1,331 s, `yc-xlat3` 258 s, `cf-docsgen-docs` and `cd5-docs` 127 s | the agents' briefs; the product owner declines them |
| T4 | **Audits first**: `check` runs the audits (seconds, no build) before the cargo steps, and stops when one fails; or the agent runs `cargo xtask check-docs`, which runs them, before it queues `check` | eight minutes of cargo before an audit's verdict | **about 1,050 s (0.3 h)**, `vn-check`, `yt2-check2`, `pm-check` | `check.rs` |
| T5 | **`gate-rows` narrower for xtask judges**: a change only to a test's judge (`claude_code.rs`, `wine.rs`, `frames.rs`, `netboot.rs`) or a fetch script owes `check` and its own test, not the five boots and the release build; the files that build, start or read every image (`main.rs`, `qemu.rs`, `cargo.rs`, `paths.rs`, `parallel.rs`, `initramfs.rs`, `builds.rs`) keep the image row | image rows the change does not reach | **341 s** on the 2 commits that touched only judges; up to 1,176 s if `main.rs`'s command table were told apart from its image build, which a path rule cannot do | `gate_rows.rs` and *What a landing runs*; it narrows a gate, so the certification consultant first |
| T6 | **Priority for runs that block a landing** (`GATE_PRIORITY=1`): served before the others, first come first served among themselves | no run time; a landing's wait. `po-main-boot-e31`, the boot of `main` after a landing, waited 1,182 s for 151 s of running | latency | `gate.sh` (`gate.sh.proposed`) |
| T7 | **A load-aware pool**, to measure: guests outside the pool used a third of the processor, the load cap then held 39 runs with a slot free, and a boot ran three to seven times slower than on a quiet host. Measure each slot count's runs an hour at a known load before choosing one; an Endfield guest is the customer's, not the pool's to stop | | not measured | `gate.sh` |
| T8 | **`CARGO_INCREMENTAL=0` in the slots**, to measure: 14 GB a big slot and the ICE of `pc2-check3` against a slower rebuild of a changed crate | | not measured | `gate.sh` |

T1 to T5 together take about 11,260 s, 3.1 of the night's 12.6 run-hours
(25%). With the slots 84% busy, the queue shortens by about as much.

**`gate.sh.proposed`** (`~/.local/share/ferrix/fleet/`, not installed; the
coordinator decides, and installs it with `mv` once no run is waiting,
*Changing the pool* in the product-owner skill) carries T1, T2 and T6. It
keeps every verdict, log and line format the same; a run taken out of turn,
and one with priority, says so in its log's `queued` line.

## Caching and assembly: the baseline, 2026-10-15 (bs-b)

The customer's target (2026-10-15): with warm caches, a rebuild after a
small edit, through to a bootable test image, and `cargo xtask check`, in a
minute or less, and the gold case in seconds. This is the half of that work
that is caching and assembly (branches `bs-b-*`); the compile itself (the
kernel crate, profiles, the linker) is `bs-a-*`'s. Measured in a worktree of
`main` 4605abd72 with a target directory of its own, at a load of 27 to 41 on
24 threads (the pool was full and an Endfield guest was writing to the
disk), so the figures are a loaded host's; the logs and the scripts are in
`~/ferrix-logs/bs-b/`.

### A warm `build --arch x86_64`, and what happens after cargo

| Run | Total | Natives (18 cargo calls) | Kernel compile | Image written |
|---|---:|---:|---:|---:|
| Cold, new target dir (components cloned) | 107 s | | | |
| No-op | 33.0 s | 3.4 s | 0.3 s | **28.8 s** |
| One string in `src/kernel/src/main.rs` changed | 125.1 s | 6.3 s | 53.4 s | **65.1 s** |
| `test-boot --arch x86_64`, no-op, to `FERRIX-BOOT-OK` | 75.1 s | 3.9 s | | **22.6 s**, then 1.5 s copying the writable btrfs disk, then a 46 s boot |

**The image write is the largest cost a warm build has.** `fat.rs` builds
the 512 MiB FAT image in memory and writes it with one `write` of 536,870,912
bytes into a truncated file (`strace`: the one call took 32 s of a 33 s
no-op). 173 MiB of it are data (the 145 MB debug kernel ELF, the 29 MB
initramfs, the loader); the rest are zeros, written out as zeros. On a host
whose disk is busy the kernel throttles the writer to the disk's pace, so a
build that changed nothing waits half a minute for 512 MiB of writeback, and
every image a gate writes pays the same.

**What an edit changes in the image.** Compared in 4 KiB blocks, the image
after the one-string kernel edit above differs from the one before in **4
blocks (16 KiB)** of 131,072; in 64 KiB blocks, 3. Of the 512 MiB, 173 MiB
are not zero. So writing only the blocks that differ from the file already
there turns 512 MiB of writes into kilobytes, and a no-op into none, with
the file afterwards byte for byte the image a fresh build writes.

The natives: each of the 18 programs is one `cargo build` call, about 0.2 s
each when nothing changed (3.4-6.3 s at this load). The test disks
(`test-disk.img`, `btrfs-test.img`, the blank btrfs images) are already
rewritten only when missing or of another version.

### A new worktree or gate slot, cold

A cold `build --arch x86_64` took 107 s here and left 2.6 GB of target
directory. A cold `check` takes 621 s in the pool against 271 s warm on the
same tree (*The pool at saturation* above). A gate slot's target directory
(slot 2, 52 GB) is mostly `debug/` (20 GB: the host workspace, 14 GB of it
rustc's incremental caches), `apps/` (9.6 GB), the three kernel targets (14
GB), `mitigations-off/` (3.6 GB), `check-compositor/` (3.8 GB) and
`xtask-native-test/` (1.6 GB). Nothing is shared between slots or worktrees:
the same commit built in two slots compiles twice, and a slot shed and made
again (eight times on 2026-10-14/15) compiles everything again.

### `check`: what runs in turn, and what an unchanged input runs anyway

From the 119 passed `check` runs in the pool's logs of the last three days,
grouped by how many crates each compiled (a warm slot on the same tree
compiles fewer than 60):

| Step (median s) | < 60 crates (31 runs) | 60-400 (56) | 400 or more (32) |
|---|---:|---:|---:|
| `check` in all | 152 | 304 | 529 |
| the workspaces of their own, beside (`check-side`) | **110** | **273** | **537** |
| of them: compositor tests | 39 | 46 | 94 |
| main workspace: tests | 46 | 71 | 150 |
| main workspace: documentation | 4 | 43 | 134 |
| kernel clippy, both settings | 10 | 29 | 58 |
| the audits, at once (critical: the architecture document) | 31 | 34 | 53 |

* **The side workspaces are the critical path**, and they run one after
  another: loom, compositor, init, auth, pkg, installer, media, adbd, nvos
  and every app, each in its own target directory. Run side by side, their
  time is their longest (the compositor's tests), not their sum.
* **Every step runs whatever changed.** A change to the kernel alone still
  runs the compositor's tests, every app's clippy and tests, the
  architecture document and the Ferrocene type check of crates it did not
  touch. A step whose inputs are unchanged since a passed run gives the
  same verdict; skipping it needs those inputs written down exactly (the
  sources, path dependencies, `Cargo.lock`, toolchain, flags, environment,
  and the files its tests read while they run), which is what a cache keyed
  on them has to prove.
* **xtask's own tests: 148 of the 208 s of `tests`** in `es-check-land2`
  (05:02Z): `native::tests::every_program_builds_and_passes_the_check_on_every_architecture`
  builds all 18 native programs for three architectures, 54 cargo calls in
  turn, into `xtask-native-test/` (the only "has been running for over 60
  seconds" in the pool's recent logs, five times).

### What a loop in seconds needs, and what stands in its way

| Cost today (warm, one-file edit) | What removes it | Who |
|---|---|---|
| Image: 22-65 s writing 512 MiB | write only the blocks that differ (kilobytes for a kernel edit), proven equal to a fresh write | bs-b, first |
| Natives: 18 cargo no-op calls, 3-6 s | one call, or skip them when their inputs' digest is unchanged | bs-b |
| Kernel: one rustc, 53 s at load 34 (debug) | the crate split, the linker | bs-a |
| The kernel ELF is 145 MB (debug info) and is written, read and copied whole | a stripped copy in the image with the full ELF beside it (`--strip-kernel` exists, opt-in) | bs-a / bs-b |
| `check`: every step on every run | steps keyed by the digest of their inputs; side workspaces at once | bs-b |
| New worktree or slot: everything compiled again | a compiler cache shared across target directories, same tree one slot (T1), no shed thrash (T2) | bs-b, gate.sh by the coordinator |
| The boot: 23-46 s for `test-boot`'s stages 1-12 at this load | not caching: a boot is the test | |

### Cut B1: the image's changed blocks only (bs-b-image)

`fat.rs` hands the image to `image_file::write`, which reads the file already
there in 64 KiB blocks, compares, and writes only the blocks that differ; a
new file is made sparse, its zero blocks never written. The image line says
how much was written (`; 192 KiB written`). Same worktree, target directory
and load (31-36) as the baseline:

| Run | Before | After | Image written |
|---|---:|---:|---:|
| No-op `build --arch x86_64` | 33.0 s | **5.2 s** | 0 KiB |
| One string in `src/kernel/src/main.rs` | 125.1 s | **30.3 s** (25.5 s of it the kernel) | 192 KiB |
| No image there yet | | 7.8 s | 177,280 KiB, the 173 MiB of data |

**The same file as a fresh write.** After the kernel edit, the patched image
was moved aside and the same tree built again with no image there, so it
was written fresh: both hash to `d016d739f437...`, byte for byte one file.
The unit tests in `image_file.rs` hold the cases a patch can get wrong (a
block that changed, data where the new image has zeros, a file longer or
shorter than the image, and a block a guest wrote into the image since the
last build); with the comparison made to say every block is the same, four
of the six fail.

### Cut B2: `check`'s units by their inputs (bs-b-stepcache)

`FERRIX_STEP_CACHE=on` makes each of `check`'s workspaces of their own --
the compositor's, init's, auth's, pkg's, the installer's, media's, adbd's,
nvos's and every app's -- a unit that is skipped when a run that passed read
exactly the inputs it would read now (`tools/common/xtask/src/step_cache.rs`
says how, `docs/certification/TOOLS.md` says what it shall and shall not
do). On a miss the unit runs under `strace -ff -ttt` as `cargo xtask
check-unit`; when it passes, the record keeps the contents of every file it
read or looked up, every directory it listed, every other path it looked up
and every program it ran, with xtask's digest, the environment, the
toolchains, the kernel release, the processor and the render devices. A unit
that talks to a process the trace does not see (a network socket, a Unix
socket it did not bind), reads a temporary file it had not made fresh, or
reads another step's outputs is run and not recorded. The log says `ran,
recorded`, `ran, not recorded: <why>` or `cached pass from <time> on
<commit>, <target directory>` for each unit, and counts both. loom (the
kernel's orderings) and every step of the main workspace stay uncached: they
read the certified item (the certification consultant, ledger lines 1447,
1461, 1494 and 1508).

**It is off unless asked for**, and release runs and the baseline keep it
off: a gate turns it on only once its end-to-end control has fired through
`gate.sh`'s host-side control mode (proposed with the pool's T1/T2,
`~/.local/share/ferrix/fleet/gate.sh.proposed-bsb`).

`cargo xtask check-side` in one worktree and target directory, at a load of
about 20 to 30:

| Run | Wall |
|---|---:|
| Warm, unchanged tree, no cache | 65.8 s |
| First run with the cache, recording all 25 units | 223.2 s (load 36-48) |
| Warm, unchanged tree, every unit a cached pass | **7.9 s** |

**The compositor runs with no system bus.** Its GPU tests start
`virgl_test_server --use-egl-surfaceless`, which asked the system D-Bus
something; a host daemon answers from state no record holds, so a unit that
talks to it is not recorded (the consultant, ledger 1508). `check` now sets
`DBUS_SYSTEM_BUS_ADDRESS` to an address with nothing at it for the
compositor's steps, cached or not; its 1,040 tests pass so.

**Controls.** A broken assertion in hyprix's `plane.rs` after a cached pass:
the compositor's unit missed, ran, and failed the run. A same-size edit of
`src/lib/init/restart/src/lib.rs` (a type renamed to a name of its length),
a file init's unit only looks up on a warm build: init missed, ran, and
failed the run with the compile error; with the file restored, every
recorded unit was a cached pass again. Each of the cache's 15 checks
sabotaged in turn fails the unit tests that hold it.

### Cut B3: xtask's native-program test, the architectures at once (bs-b-natives)

`native::tests::every_program_builds_and_passes_the_check_on_every_architecture`
built the 18 native programs for three architectures as 54 cargo calls one
after another (148 s of `tests`' 208 s in a gate slot on a tree new to it,
`es-check-land2`). It now makes one call per program naming the three
kernel targets, which cargo compiles side by side, as `build --arch all`
does (`cargo::build_ahead`), and checks each ELF as before. With the test's
target directory emptied, alternating in one worktree:

| Run | Before | After |
|---|---:|---:|
| 1 (load 36 / 33) | 34.8 s | 13.2 s |
| 2 (load 31 / 31) | 43.2 s | 17.0 s |
| Warm, nothing changed | 6.7 s | 2.8 s |

## The compile: the baseline, 2026-10-15 (bs-a)

The other half of the customer's target above: what a warm rebuild spends
in rustc after a one-line edit in the kernel (branches `bs-a-*`). Measured
on `main` 4605abd72 in a worktree and target directory of its own, at
`nice 10`, loads 13 to 46 on 24 threads. Wall times on nazuna swing by a
factor of three with the other sessions' load and disk writeback, so each
comparison below alternates its two sides edit by edit and also gives the
processor time (`user`) and the bytes written (`/usr/bin/time %O`), which
the load does not move. The edit is `~/ferrix-logs/bs-a/scripts/edit.sh`:
a new `let _ = core::hint::black_box(N_u32);` in `sys_eventfd`
(`src/kernel/src/syscall/eventfd.rs`, load ring), clean for rustfmt and
clippy; logs and scripts in `~/ferrix-logs/bs-a/`.

### Where an edit's compile goes (x86-64, debug)

| Kernel rustc | Wall | What |
|---|---:|---|
| cold (`-Z time-passes`, load 40) | 48.4 s | type check 4.5, borrow check 7.8, monomorphization 4.0, codegen 17.1, crate-local ThinLTO 16.2 |
| after the edit (load 40) | 23.9 s | **persisting the incremental dep graph 19.8 s** (178 MB `dep-graph.bin` and 100 MB query cache: the crate is 237k lines), ThinLTO 4.1, codegen 0.8, link 0.35 |
| after the edit, processor time | | 26 CPU-s, 476 MB written; ThinLTO is about 17 CPU-s of it |

* **The kernel is one crate**: whatever file changes, rustc re-hashes,
  re-checks what depends on it and writes the whole crate's incremental
  state again (280 MB). That part grows with the crate, not with the edit.
* **rustc runs ThinLTO inside the crate** in the dev profile (opt-level 1,
  incremental, 256 codegen units): about 17 of an edit's 26 CPU-s.
* **The debug kernel's ELF is 148 MB, 137 MB of it DWARF** (`.debug_info`
  57, `.debug_str` 34, `.debug_loc` 29); objects, ELF and image carry it.
  aarch64 146 MB, armv7a 182 MB.
* The link is 0.35 s (rust-lld); the host crates link with rust-lld too
  (the default for x86_64-unknown-linux-gnu since Rust 1.90). No linker
  change is owed.

`check` after the edit, 118 s at load 35-39 (warm): its compile steps are
the **kernel clippy**, 38 s, six passes (three targets, mitigations on and
off) of 6.3 CPU-s each, 4.5 s of which is clippy's lint pass over the whole
crate and 2.4 s the dep graph again; and **the Ferrocene type check**, 18 s
for three targets (105 s the first time after a cold build). The rest is not
compiling: the host tests' run time, 45 s (`ferrix-btrfs-write` 20 s, xtask
14 s, `ferrix-btrfs` 6 s), and, beside them, the audits (29 s) and the side
workspaces (114 s, the compositor's tests 39 s; bs-b's section above). A
warm `check` after a kernel edit spends about 300 CPU-s in all.

### What a compile in seconds needs

| Cost (one kernel edit) | What removes it | State |
|---|---|---|
| DWARF: 148 MB ELF, 476 MB written an edit | line tables only in the dev profile | **Cut C1**, below |
| crate-local ThinLTO, 17 CPU-s an edit | `lto = "off"` in the dev profile, a `coverage` profile keeping the measured build | **Cut C2**, below |
| one 237k-line crate: dep graph and lint pass grow with it | the crate split along the rings: core (104k lines) <- item (14k) <- load (122k) <- the binary; an edit in load then recompiles about half of today's crate, an edit in core all of it | design OK IF (1)-(12), ledger 1477; four conditions to answer in the design before code |
| six kernel clippy passes, each linting the whole crate | the split (lints only the crates the edit reaches) | with the split |

### Cut C1: line tables only in the dev profile (bs-a-dev-profile)

`[profile.dev] debug = "line-tables-only"` (was `true`), for the whole
workspace; `--gdb` builds the kernel with the full DWARF again
(`--config profile.dev.debug=true`, kept beside the other build). The
release profile, the reference configuration, is unchanged. Nothing reads
more than the line tables: xtask's symbolizer reads the symbol table, and
`coverage-report.py` and `decision-coverage.py` read `objdump
--dwarf=decodedline`.

| | Before | After |
|---|---:|---:|
| Debug kernel ELF, x86-64 / aarch64 / armv7a | 148 / 146 / 182 MB | **49 / 49 / 59 MB** |
| Kernel rustc after the edit: processor time, written | 26 CPU-s, 476 MB | **20 CPU-s, 332 MB** |
| `build --arch x86_64` after the edit (4 rounds alternated, load 18-22, before bs-b's image cut) | 20.9, 23.3, 24.8, 27.4 s; 27 CPU-s; 988 MB written | **5.8, 5.8, 9.1, 14.5 s**; 20.5 CPU-s; 845 MB written |
| `check` after the edit (2 rounds alternated, load 22-28) | 133, 100 s; 299, 295 CPU-s | 150, 160 s; 298, 295 CPU-s |

`check` does not move: clippy and the type checks make no code, so the
debug level does not reach them, and its wall time is the load's.
An image now carries a 47 MB kernel and a 17 MB initramfs (the native
programs' line tables) where it carried 145 and 29 MB.

**The debug coverage build changes slightly.** The code is not byte for
byte the same (x86-64 `.text` 6,608,708 bytes against 6,603,684), so the
statement rows `coverage-report.py` counts in `src/kernel/src` differ in a
few places, at the same source:

| Architecture | Rows, full DWARF | Rows, line tables | Only before | Only after | In the residual |
|---|---:|---:|---:|---:|---|
| x86_64 | 51,137 | 51,145 | 20 | 28 | `mm.rs` 872 (no row of its own with line tables) |
| aarch64 | 48,942 | 48,964 | 8 | 30 | none |
| armv7a | 49,567 | 49,578 | 20 | 31 | none |

`carry-coverage.py` carries by source line and cannot see a change made
only in the generated code, so the coverage figures name the build they
measured: full DWARF until this landing, line tables from the next
re-measure (`docs/certification/VERIFICATION.md` §3.2). The certification
consultant's review: ledger line 1463.

### Cut C2: no LTO in the dev profile, the measured build kept (bs-a-coverage-profile)

`[profile.dev] lto = "off"`: rustc's default for an optimised incremental
build is ThinLTO across the crate's own 256 codegen units, which after a
one-line edit re-optimises the whole kernel. The debug kernel the coverage
figures measured is kept as `[profile.coverage]`, the dev profile with
`lto = false` (rustc's default, crate-local ThinLTO), and xtask builds it
for every boot a TCG plugin traces (`FERRIX_QEMU_PLUGIN`, which `cargo xtask
coverage` sets for each gate it runs), so no traced boot can measure the
other build. This is route B of the certification consultant's ruling
(ledger line 1472); the coverage scripts now name the profile from the
ELF's path, `target/<triple>/<profile>/` or the suite's kept copy
`ferrix-kernel-<profile>-<digest>.elf`, and refuse a path that names none,
where any path without `/release/` used to be reported as debug.

| Kernel rustc after the edit, x86-64, 4 rounds alternated | dev profile, no LTO | `coverage` profile (crate-local ThinLTO) |
|---|---:|---:|
| Processor time | **2.6, 3.1, 3.5, 3.7 CPU-s** | 19.8, 20.0, 21.8, 21.9 CPU-s |
| Written | **301 MB** | 332 MB |
| Wall (load 28-66, the host saturated) | 14.7-91.8 s | 20.0-92.0 s |

The edit's processor time falls by six sevenths; what is left of its wall
time is writing the crate's incremental state, which the crate split
addresses. Code size: the debug kernel's `.text` is 5.75 MB against 6.60 MB
on x86-64 (4.99 against 5.60 on aarch64, 5.84 against 6.66 on armv7a), the
copies of small functions inlined into every caller no longer made.

**The measured build is unchanged.** At one source, the `coverage`
profile's kernel and the dev profile as it was (`--config
profile.dev.lto=false`) are the same ELF file, byte for byte, on all
three architectures (so every loadable section, `.text`, `.rodata`, `.data`
and the relocations, is the same; `cmp` on the whole files). The dev kernel the gates boot now has about ten thousand
more statement rows (functions that were inlined everywhere and dropped
now exist on their own): x86-64 61,046 against 51,145, aarch64 58,737
against 48,964, armv7a 59,147 against 49,578. No coverage figure measures
that build; `docs/certification/VERIFICATION.md` §3.2 says so.

**What boots the `coverage` build.** No gate does: the suite's own run
does, under the plugin. On this branch a traced `test-boot --arch x86_64
--accel tcg` built the `coverage` profile (xtask says `traced: the coverage
profile`), booted to `FERRIX-BOOT-OK` and was reported by both scripts as
`coverage profile` (one boot: certified item 8,398 of 9,971 statements,
84.2%). A whole `cargo xtask coverage --arch all` on nazuna did not get
that far, for the host, not the build: at loads of 30 to 60 its x86-64 boots
ran past the default 120 s under drcov, and the Arm boots could not load
the plugin at all (`libdrcov.so` is built for QEMU 10.2.1's plugin API 5,
the `qemu-system-aarch64` and `-arm` on PATH are 9.2.4 with API 4). The
next full measurement needs a 10.2 Arm QEMU and `--timeout`. Until then the
build it will measure is the one main's gates booted: its `.text` is main's
dev kernel's ELF, byte for byte.

**The rows, and the load.** On dbe733179 `check`, `build --arch all
--release`, `test-boot` on x86-64 (KVM and TCG), AArch64, ARMv7-A and
ARMv7-A with two processors, and `test-threads --arch all` passed (gate
tags `bsa-lo-*`). `test-container --arch all` failed once, on a new
sentence of stage 5's moving lock check on ARMv7-A (a `docs/BACKLOG.md`
row), and passed when run again. A dev kernel without LTO is slower in
the guest (one boot each: ARMv7-A to `FERRIX-BOOT-OK` 25.7 against 42.2
guest seconds, x86-64 under KVM 19.2 against 26.4, at different loads), and
the stage 13 checks that measure a share of the wall clock fail under a
loaded host (their rows in `docs/BACKLOG.md`). So 49 ARMv7-A boots were
alternated between main and the branch, four or five at once at loads 20
to 45: main failed 2 of 27 and the branch 1 of 22, all three on those
stage 13 checks; the boot times overlapped (main 31-60 s, the branch
31-71 s). No rate of failure the change adds could be told from the load's.

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
