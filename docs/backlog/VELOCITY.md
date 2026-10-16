# Backlog: velocity, measured

Moved out of [`docs/BACKLOG.md`](../BACKLOG.md) on 2026-10-16. `tools/common/gen/gen-roadmap-charts.py` carries the per-day figures.

## Velocity, measured on 2026-09-18

Points are what a session says before it starts (the rule of 2026-09-13,
`docs/ROADMAP.md`'s opening) and velocity is what landed, counted
afterwards. This is the first count over the whole points era, from the
evening of 2026-09-13, when the first estimates were written, to the morning
of 2026-09-18. Sources: the product-owner ledger kept at the time (which
measured 2026-09-14 at the time), the landings recorded in this file, and
the roadmap's stage totals -- stage totals for stages 17, 18 and 19 rather
than their rows, so that nothing is counted twice.

| day | points landed | what |
|---|---|---|
| 2026-09-13 | 0 | the first estimates written that evening; the day's landings were sized before the rule and are not counted |
| 2026-09-14 | 131 | measured by the product owner at 23:20: 19 landings across ten sessions -- stages 7, 8, 9, 10, 11, the mm stack, the board -- ≈ 21 points a queue-hour |
| 2026-09-15 | 34 | 00:20–03:00: memfd 5, threads 5 (7), the POSIX gap document 3, the native Windows busybox 5, the branch cleanup 3, netwire 8, inet ABI 3; then the fleet stopped until the evening of the 16th |
| 2026-09-16 | 66 | from 19:40: threads 6 (3), the zinc gate 2, the compositor's parser 5, display L1 3, ferrousli's threads 8, a busybox fix 1, and the rest of networking (44 of its 50 estimated), whose exit was met at 22:50 |
| 2026-09-17 | 214 | stage 17 (74) and stage 18 (96) less the 8 above, both exits met; stage 19's first ≈ 44 (rules, dispatchers, layouts, groups, monitors, plugins, blur and shadows); static PIE 3, the threads exit 2, ferrousli-misc 3 |
| 2026-09-18 | unpointed | the compositor's frame time, pacing, the kernel's fault fix, the cores, 1080p and wallpapers, the GPU and Steam decisions: none was sized before it started, so none counts |

**About 445 points in four calendar days, 2026-09-14 to -17: ≈ 111 a
calendar day, and ≈ 150 a day the fleet was actually running** (the 15th was
three hours). The finer number the ledger measured on the 14th, 21 points a
queue-hour, held on the 17th too by this count. Ten sessions ran on the 14th
and about eight on the 17th, so a session-day is 15–20 points, and every
estimate under 8 held both days.

What the number is good for and what it is not:

* It sizes the pointed remainder: stage 19's ≈ 100 and dynamic linking's 39
  are a fleet-day each at the 17th's pace *if the work is of the kind that
  was measured* -- protocol tables, syscalls, a renderer -- which the GPU
  path (unknowns in every step) and XWayland (a server) are not. Stages 21
  and 22 are unsized and the number says nothing about them.
* Three calibrations are mixed in it: session estimates against each other's
  yardsticks, the product owner's "unmeasured" sizing of stages 17–19 on
  the 13th, and the rows that carry their own points. The stage totals for
  17 and 18 came in where they were sized (74 and 96), which is the one
  check the mix has passed.
* The 18th's landings are counted at zero, not because they were small
  (sized afterwards they would be about 35: the backdrop 8, pacing and the
  card's damage 5, the fault fix 3, the cores and the shadow 8, 1080p and
  wallpapers 8, the two decisions 3) but because a size given after the fact
  is not an estimate. The next count should not have such a row.

### Update, 2026-09-24

**The code base** (`git ls-files` on `main` at 046ea79a): 702,675 lines of
Rust in 2,080 tracked files of all kinds, plus 47,769 lines of C, headers and
assembly (ferrousli's and the ports' glue). By tree: libs 188,154, compositor
169,567, kernel 122,989, ferrousli 114,688, zinc 52,011, xtask 34,449, user
9,152, fuzz 7,970, boot 3,474. The docs are 26,714 lines of Markdown, 25,061
of them under `docs/`. Nothing is excluded or generated-filtered, so read the
Rust total as an upper bound.

**Today.** The 66 commits that landed on 2026-09-24 add 54,477 lines and
remove 4,309, of which 45,560 added and 2,054 removed are Rust: a net
+50,168 lines in the day, ≈ 7 % of everything above.

| landing | points | of |
|---|---|---|
| sysfs (`b65b5e41`) | 26 | 26 |
| the init push: G3, G4, L1, L2, L3, G5 | ≈ 23 | 22 |
| vkgears through Venus (V1–V5) | 39 | 39 |
| the GC400's first two steps (G1, G2) | 11 | 11 |

That is **≈ 99 points landed on 2026-09-24**, against the ≈ 54 a day the
backfill below gives for 09-18 to 09-23. The landings ran from about 18:15
to 22:45, so about 4.5 hours: **≈ 22 points an hour**, level with the 21 an
hour measured on the 14th and the 17th. It is a few sessions, not a
fleet: four agents ran in parallel on the init push and two other sessions
landed the rest.

Not counted, because nothing was sized before it started (the rule above):
Chrome's headless run, foot on the compositor, timerfd and signalfd, the
control queue and the debug FPS overlay, ferrousli's glibc names for Chrome,
stage 20's build recording, splice and copy_file_range. Sized afterwards they
would be perhaps 60–80 more, which is why 99 is a floor. The day's cost was
not in the code: the sysfs landing was gated three times and the gears
landing four times because `main` moved under every gate, and FX-1004 flaked
in two of the rows.

**The days between, 09-18 to 09-23**, are not in the table above because no
session reported them. They were sized afterwards from `git log`, in clusters
against the same scale, and are lower in confidence: ≈ 50, 50, 55, 96 and 20
for 09-18 to 09-22, about 324 in six days (btrfs write's 60 included), ≈ 54 a
calendar day. With the first count's 445 and today's 99 the running
total is ≈ 870 points in 11 calendar days, ≈ 79 a day. The drop from the
first week's 111 a day reads as a change in the kind of work (the zsh
compatibility tail, and GPU, dynamic linking and btrfs write, whose steps
each have unknowns) and not as a stall.

### Update, 2026-09-26

Counted from the 2026-09-24 count (88cd2740) to 02afa6c8, 356 commits, by
the same rule: a landing counts at the estimate written before its work
started, and what had none is sized afterwards from `git log`, in
clusters, on the same scale, and said to be so.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-09-24, after the count | 0 | ≈ 27 | the DK1's desktop at the speed of a hand, its cursor plane, its HDMI modes, the console sending by interrupt, zinc's start |
| 2026-09-25 | 0 | ≈ 26 | the certification audit: the item's boundary, coverage, the Security Target and the rest of its set, SMEP, SMAP and PAN, the trap return and `StatLayout` inverted; one or two sessions |
| 2026-09-26 | 103 | ≈ 175 | estimated: init L4 to L9 45, audio L1 to L7 24, cgroups P1, M1 and S1 28, `SCM_CREDENTIALS` 2, the vDSO ≈ 4; afterwards: the certification's F-23, F-31 with KASLR, F-34, F-36, F-37, the item's split and its coverage suite ≈ 70, the Pixel 7 ≈ 39, Chrome on ferrousli, its speed and its desktop ≈ 33, and the terminal, fuzzel and Linux-compat fixes ≈ 33; about twelve sessions |

So **≈ 26 on 2026-09-25 and ≈ 278 on 2026-09-26**, 103 of the latter
estimated before it started. S1 counts its 13 although it was built
differently from its plan, and the vDSO's 4 is its share of a joint
estimate; both are soft. The running total is **≈ 1,200 points in 13
calendar days, ≈ 92 a day**, or ≈ 973 (≈ 75 a day) counting after
2026-09-24 only what had an estimate. What was estimated landed at ≈ 67 a
day over 2026-09-24 to -26, and that is the rate the roadmap's sized scope,
≈ 442 points, burns at: the unsized work beside it takes nothing off it.
The roadmap's *Burndown* lists that scope.

### Update, 2026-10-01

Counted from 02afa6c8 (the 2026-09-26 count, 17:37) to ae4c0d89a, by the same
rule, over the calendar days 2026-09-27 through 10-01: a landing counts at
the estimate written before its work started, and what had none is sized
afterwards from `git log`, in clusters, on the same scale, and said to be
so. The evening of 2026-09-26 itself, 17:37 to midnight -- which the desktop
clients' foundation, the EDID monitor rules and the first `waybar` and
`fuzzel` work sit in -- falls between this count and the one before it and
is in neither.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-09-27 | 90 | ≈ 150 | estimated: the 32-bit x86 ABI's I1 to I4, 42 (`docs/I386.md`), the sound stack's alsa-lib and `pulsed`, 21 (`docs/AUDIO.md`), authentication's phase 1, 27 (`docs/AUTH.md`); afterwards: the certification item's unsafe-site trace closed (F-26), its high-level requirements (51) and low-level requirements across object/, memory, the architectures, SMP, console and the IOMMU, F-41 through F-52 and W-8's design ≈ 70, the host-guest clipboard both ways ≈ 8, waybar's bar and PulseAudio module with fuzzel's window ≈ 13, the adb-over-USB bridge finishing ≈ 5, driver restart-on-death ≈ 5, the Linux personality's `inotify` and pidfds ≈ 5, hyprlock's library ≈ 5, the stripped 128 MiB boot row ≈ 5, Windows build and QEMU fixes ≈ 8, the opaque-kernel seam measurement proposed and shelved ≈ 8, and a dozen small kernel fixes ≈ 13; well over a hundred commits from more sessions than any day before it |
| 2026-09-28 | 0 | ≈ 44 | the installer's MVP (`ferrix-install`, `test-install`) ≈ 13, System V semaphores for Steam ≈ 8, test-time's cut 2 ≈ 5, Steam's bootstrapper and its semaphore permission fix ≈ 5, CI and Miri infrastructure fixes ≈ 8, small VFS and `/proc` fixes ≈ 5 |
| 2026-09-29 | 36 | ≈ 26 | estimated: yserver, the X server, 36 (`docs/YSERVER.md`); afterwards: Steam's sign-in window through yserver ≈ 8, the repo layout move to `src/`, `tools/` and `docs/` ≈ 5, packet pipes with `pipe2(O_DIRECT)` dropping Steam's last preloaded shim ≈ 5, ferrousli's glibc names for Steam's 64-bit side and multiarch loader search ≈ 5, `/proc`'s 32-bit inode and offset fixes ≈ 3 |
| 2026-09-30 | 19 | ≈ 83 | estimated: N1 to N3 of Steam's mount namespaces, 19 (`docs/NAMESPACES.md` §9); afterwards: Claude Code on Ferrix, its gate, `XSAVE` and the AVX withholding for Zenbleed and GDS ≈ 21, the bottom mount under every namespace ≈ 3, the red submap fix ≈ 3, test-time's cut 3 ≈ 3, the discovery Finder ≈ 8, `ferrix-driver`'s start (input and `virtio-blk` moved onto it) ≈ 8, seccomp's S1 (the verifier and interpreter) ≈ 8, hyprix's `client.rs` split into modules ≈ 8, the stat/`btop`/`ferrofetch` apps ≈ 5, the repo layout's `interfaces/` and `discovery/` module declarations ≈ 3, and a dozen more kernel fixes (the robust futex list, `get_robust_list`'s missing check filed as P2, the MSI-X storm bound, `no_new_privs` and dumpability, `libudev`'s uevent socket) ≈ 13 |
| 2026-10-01 | 0 | ≈ 94 | the speculation domain landed ahead of its full gate (`bf9efba95`) ≈ 21, the seL4 channel-round-trip plan recorded (`75c03a259`, docs only) ≈ 5, pid namespaces and the small namespaces (UTS, IPC, cgroup, `setns`) ≈ 13, seccomp's S2 (a registered filter at all four entries) ≈ 8, the gate pool and test-time phase 3 design ≈ 5, `pkg`, the package manager, and `test-pkg` ≈ 8, the apps launcher's icons, Bad Apple!!'s fetch, and building the ports and `btop` on Windows through WSL ≈ 8, more WSL build fixes ≈ 5, the certification's after-the-fact reviews and the verification audit's first findings ≈ 8, the stage 13 handover and branch-audit docs at the product owner's wind-down ≈ 5, and smaller items (the gateway's segments in flight, the net-throughput hang filed, booting under KVM by default, `--iterate`) ≈ 8 |

So **≈ 240 on 2026-09-27, ≈ 44 on 2026-09-28, ≈ 62 on 2026-09-29, ≈ 102 on
2026-09-30 and ≈ 94 on 2026-10-01**, 145 of the 542 estimated before the
work started. The running total is **≈ 1,742 points in 18 calendar days,
≈ 97 a day**, or ≈ 1,118 (≈ 62 a day) counting after 2026-09-24 only what
had an estimate. Two things keep this count softer than the one before it:
the certification item's requirements-and-trace work on 09-27 is the
largest single cluster in it (≈ 70) and is sized the way a whole subsystem
would be, against commit messages rather than a design doc's own number,
because the item has no points column of its own; and the evening of
2026-09-26 noted above fell between the two counts and is counted in
neither.

### Update, 2026-10-04

Counted from ae4c0d89a (the 2026-10-01 count) to 67efb9fb1, by the same rule,
over the calendar days 2026-10-02 through 10-04. The twelve commits dated
2026-10-01 that reached `main` after the last count (the channel round trip's
first steps, network namespaces' first commits) are counted on the day the
work landed, not the day it was written. Work that is done but not on `main`
-- NVIDIA's N2 to N4 and N6, S3, L13, `np-land`, `selfhost-matrix` -- is not
counted until it lands.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-10-02 | 22 | ≈ 128 | estimated: NVIDIA's N0, the kernel prerequisites other than N0g (`docs/NVIDIA.md` §7, 28 with N0g); afterwards: the channel round trip's first steps (`channel_write_read`, the sync wake, `bench-ipc`, the timer's skip) ≈ 21, hyprix and yserver's frame pacing, refresh rates and the flush thread ≈ 21, btrfs in the certified item and its fallible allocation ≈ 20, btrfs's ENOSPC and mirror fixes ≈ 8, System V shared memory ≈ 8, UVM's self-tests on Ferrix (C0a) ≈ 8, F-58 and F-59 and their fixes ≈ 8, NVIDIA's feasibility, CUDA and N0d/f/g designs ≈ 13, the opaque kernel's §9.7 and §9.8 designs and the seL4 measurement ≈ 13, and smaller items ≈ 8 |
| 2026-10-03 | 38 | ≈ 115 | estimated: N0g, 6, and N1, 32 (`nvrm` boots the RTX 3060's GSP, `nvidia-smi`); afterwards: the channel round trip's steps 2a to 2e ≈ 21, login on the console, `sessiond`, `/home` on its own disk, `pulsed`'s socket and the desktop as a user ≈ 16, ferrousli, zinc, the Pixel 7 tools, the website and the apps moving into repositories of their own ≈ 13, Ferrix building its AArch64 image on the Pixel 7 ≈ 13, the patched QEMU's interrupt-remapping block and its CI cache ≈ 8, F-60's fix ≈ 5, hyprix's window drag, damage and terminal fixes ≈ 13, Chrome's CJK fonts, `xdg-open` and a persistent profile ≈ 8, the brand's logo ≈ 3, and smaller items ≈ 15 |
| 2026-10-04 | 0 | ≈ 55 | network namespaces (`CLONE_NEWNET`, veth pairs, a stack per namespace) ≈ 21, hypridle, hyprlock, waybar, fuzzel and term as apps ≈ 13, foot taking keys ≈ 5, `getty` revoking the console before every login ≈ 5, the product owner's and the gate pool's rules and the wind-down roadmap ≈ 8, and a gateway test fix and coverage carried ≈ 3 |

So **≈ 150 on 2026-10-02, ≈ 153 on 2026-10-03 and ≈ 55 on 2026-10-04**, 60 of
the 358 estimated before the work started. The running total is
**≈ 2,099 points in 21 calendar days, ≈ 100 a day**, or ≈ 1,178 (≈ 56 a day)
counting only what had an estimate. This count is softer than the one before
it: 2026-10-04 is a half day, and nearly all of it is sized from commit
messages. The roadmap's charts (`tools/common/gen/gen-roadmap-charts.py`) are
redrawn from it.

### Update, 2026-10-05

Counted from 67efb9fb1 (the 2026-10-04 count) to `main`'s cf30aa08f, by the
same rule, over the one calendar day 2026-10-05: a landing counts at the
estimate written before its work started, and what had none is sized
afterwards from `git log`, in clusters. Work that is built but not on `main`
-- the cgroup controllers, L13c, 2f, steps 3a and 3b, N5 and bubblewrap's
user-namespace gate -- is not counted until it lands.

| day | estimated before | sized afterwards | what |
|---|---|---|---|
| 2026-10-05 | 86 | ≈ 29 | estimated: NVIDIA's N2 to N4, landed as `land-n6` (a84992dc5, batch 20261005T190105Z), 64; stage 20's S-2, plan mode complete, 5; FX-1012, the stage 10 G3 and R5 race with the console's pump, 6; the stage 10 seam panic, the block ring's byte-wise `u32` on the driver side, 4; the Windows gateway's peek, 4; `bench-ipc` made exact 1; init's L13b, 2 of L13's 10; afterwards: the certification's C1 work (the re-carried anchors, the item boundary, N1e's text, F-62) ≈ 8, the gateway's retransmission-timer fix ≈ 4, F-63 and the chardev self-check N12 ≈ 4, the red batch (`test-init`'s answers, `xtask`'s lossy serial reader, `sched::steal_from` ending stage 5's moving-lock panic, `su`) ≈ 5, the `--everything` fixes (the X11 link against the xkb placeholder, component checkouts on Windows) ≈ 3, the docs-steward agent, the forecast and the docs rounds ≈ 3, and the product owner's docs and skill ≈ 2 |

So **≈ 115 on 2026-10-05**, 86 of them estimated before the work started. The
running total is **≈ 2,214 points in 22 calendar days, ≈ 101 a day** (2,099
to 10-04, and 115), or ≈ 1,264 (≈ 57 a day) counting only what had an estimate
(1,178 and 86). The sizes afterwards (the 29) are the same kind of guess as
before. 2026-10-05 is a full day, and 64 of its 86 estimated points are one
landing that was built on the two days before. The roadmap's charts are redrawn
from it, and the forecast's scope, 647 points, is in `docs/roadmap/status.md`.

---
