# Status, estimates and forecast

*Rows reviewed 2026-10-04, at the day's wind-down; the velocity count and the
two charts below now run through 2026-10-04.* Over the four days of
the points era that the fleet ran, 2026-09-14 to -17, about 445 points
landed: 131, 34, 66 and 214, which is ≈ 111 a calendar day and ≈ 150 a day
the fleet was running, with 8–10 sessions, 15–20 points a session-day, and
21 points a queue-hour on both days that were measured finely. Every
estimate under 8 held, and stages 17 and 18 came in at the sizes they were
given (`docs/BACKLOG.md`, *Velocity*). The count since then, to 2026-09-24,
is ≈ 870 points in 11 calendar days (≈ 79 a day), and 2026-09-24 alone
landed ≈ 99, about 22 an hour over the landing window, on a code base of
703 k lines of Rust. Counted again on 2026-09-26, the total was ≈ 1,200
points in 13 calendar days (≈ 92 a day). Counted again now, to 2026-10-01,
five more calendar days landed ≈ 542 points (≈ 108 a day of the days
counted this time), of which ≈ 145 were estimated before the work started
(the i386 ABI's 42, the sound stack's 21, authentication's phase 1 27,
yserver's 36, and N1 to N3 of Steam's namespaces' 19) and the rest, ≈ 397,
sized afterwards from `git log`, in clusters, the same way as before
(`docs/BACKLOG.md`, *Velocity*, "Update, 2026-10-01"). Counted once more, to 2026-10-04, three more calendar days landed ≈ 358
points (≈ 119 a day), of which ≈ 60 were estimated before the work started
(NVIDIA's N0 and N1) and the rest, ≈ 298, sized afterwards from `git log`
(`docs/BACKLOG.md`, *Velocity*, "Update, 2026-10-04"). The running total is
**≈ 2,099 points in 21 calendar days, ≈ 100 a day**, or ≈ 1,178 (≈ 56 a day)
counting only what had an estimate before it started. That is historical
velocity, not a current schedule, and not the rate the roadmap's scope
falls at: of the ≈ 900 points that landed from 2026-09-27 to 10-04, only
≈ 158 came off the scope sized on 09-26, ≈ 20 a day. The *Burndown* below
forecasts the sized scope, ≈ 360 points as of 2026-10-04, at that rate.
The table records the state now.
Stages 12 to 16 were sized in words before points existed; their points are
first guesses rather than an owner's estimate and are replaced when a session
sizes them.

| what | points | current state |
|---|---|---|
| ~~Stage 19: the GPU path, Path A (`docs/GPU.md` §3)~~ *done 2026-09-19* | ~~52~~ | done |
| ~~Stage 19: the cursor plane (`docs/GPU.md` §3.10)~~ *done 2026-09-23* | ~~13~~ | done |
| ~~Stage 19: the device queue, commands in flight and a frame that waits once (`docs/GPU.md` §3.11)~~ *done 2026-09-24* | ~~13~~ | done |
| Stage 19: the desktop's speed as it is watched (`docs/GPU.md` §3.9): client pages as texture backing | 8 | in progress |
| ~~Stage 19: an X server, XWayland's 40~~ *done 2026-09-29 as yserver (`docs/YSERVER.md`)* | ~~40~~ 36 spent | done |
| Stage 19: `dwindle:precise_mouse_move`, the second-pass effects and the window rule `xray` | about 8 | in progress |
| `run-compositor --everything` carries everything: every volume, every app, nothing silently left out (the customer's rule, 2026-10-01) | unsized | under way: the volumes are fetched when missing, and a failed fetch stops the run (f7777c17, 2026-10-01); Steam in fuzzel (cb21ecd8); every app and btop built on Windows are os-3c's, next |
| The desktop's own clients: waybar, fuzzel, hyprlock and hypridle in Rust, reading the customer's own files (`docs/DESKTOP-CLIENTS.md`) | the foundation they share 21 (`docs/BACKLOG.md`); the four programs unsized | under way: the foundation's crates and hypridle on `main` (2026-09-26); waybar, drawing the customer's bar on the desktop, and fuzzel's window, run by their launcher script, on `main` and gated by boots (2026-09-27); hyprlock checks a password through `authd` on every desktop (2026-10-03); since 2026-10-04 hypridle, hyprlock, waybar, fuzzel and term are apps in `ferrix-os/apps`, built by xtask from their folders, and foot takes keys (every compositor image carries libxkbcommon's `xkb` directory; `test-foot`'s typed-line check runs since f55e8ab28, 2026-10-04) |
| After stage 19's 178: `zwp_linux_dmabuf` with a GBM-shaped allocator, and Mesa's virgl on ferrousli, for clients that draw on the GPU themselves (`docs/BACKLOG.md`) | 8, and 40 or more | not started |
| Gears (`docs/GPU.md` §6, the customer's order of 2026-09-24): vkgears through Venus on the Linux host 39, which is also stage 22's "Venus 8" and more; GLES2 gears on the DK1's GC400 32 | 71 | under way: vkgears draws through Venus (39 done, 2026-09-24); the GC400 runs a command buffer on the board, its events by interrupt (G1 and G2, 11 of its 32, 2026-09-24) |
| ~~Dynamic linking: the kernel half, ferrousli's loader, glibc's names~~ *done 2026-09-23* | ~~39~~ | done |
| ~~Dynamic linking: ferrousli's AArch64 and ARMv7-A port, which the customer put inside the stage on 2026-09-21~~ *done 2026-09-23* | ~~≈ 34~~ | done |
| ~~Stage 12, btrfs write~~ *done 2026-09-21* | ~~≈ 60~~ | done |
| ~~sysfs, fed by the services that own each fact (`docs/SYSFS.md`)~~ *done 2026-09-24* | ~~26~~ | done |
| ~~Chrome on Ferrix, headless and in a window, x86-64 (`docs/CHROME.md`)~~ *done 2026-09-24* | foot and its ports 13, the kernel's rows ≈ 30, spent | done |
| ~~Chrome: the zygote, its speed, ferrousli in glibc's place headless and in a window, the persistent btrfs root~~ *done 2026-09-26* | unsized, spent | done |
| ~~Chrome: `inotify`~~ *done 2026-09-27 (ferrix-e4)* | unsized, spent | done |
| Chrome: the GPU | unsized | under way on NVIDIA's card (stage 21): WebGL renders on the RTX 3060 through ANGLE on NVIDIA's Vulkan, 44–53 fps with software compositing; unlanded (`land-n6`, `nvidia-n2`); dmabufs for GPU compositing (N3b) next |
| Chrome on the STM32MP157D-DK1 (`docs/CHROME.md` §10) | ≈ 45–55 | not started |
| ~~Claude Code on Ferrix, the command line, x86-64, and `claude` on the `--everything` desktop (`docs/CLAUDE-CODE.md`)~~ *done 2026-09-30* | unsized, spent: the volume and gate, and `XSAVE` in the kernel | done |
| ~~Claude Code: the interactive TUI gated, the launcher, Zenbleed and GDS~~ *done 2026-09-30* | unsized, spent | done |
| ~~Claude Code on ferrousli, as the `--everything` desktop runs it: the POSIX timers, the gate on ferrousli~~ *done 2026-10-01* | unsized, spent | done |
| Claude Code: a real account, AArch64, the i386 frame's AVX state | unsized | not started |
| The Claude desktop app, which Anthropic builds for macOS and Windows only (`docs/CLAUDE-CODE.md` §7) | unsized until assessed | assessment next |
| The Pixel 7, the customer's phone (`src/boot/vendor/google/pixel7/HANDOVER.md`) | the USB device driver unsized; the desktop in the launcher app's VM done | under way: `main` boots it natively on all eight cores to `FERRIX-BOOT-OK stages 1-12`, and as a guest of the phone's own crosvm from a launcher app, with a monitor graphing the boot and `ferrix-statd`'s samples (2026-09-26); the customer chose the VM for a desktop the same day (option A), and it runs there with Chromium on it (2026-09-27); the USB device driver has its brief (`docs/vendor/google/pixel7/USB-HANDOVER.md`) and a read-only survey on a branch |
| Stage 13, namespaces, cgroups, seccomp | cgroups 85 (`docs/CGROUPS.md` §7: 27 for what init needs, 58 for the controllers); namespaces and seccomp unsized, the old guess for the whole stage was *month* ≈ 60 | under way, 2026-10-04: mount, user, UTS, IPC, cgroup, pid and network namespaces are in (network: 22384874f); seccomp filters (S3) landed 2026-10-04 (248799bdd); G1 to G5 done (27), which is all init needs from it, C8 for native services included; `pids`, `memory`'s charging and `cpu.weight` done (2026-09-26, as the certification's job quotas), and `memory`'s scoped OOM kill the same day (P1, M1, S1: 28), so 55 of 85; the rest of `memory.stat`, `memory`'s reclaim, freezing, `cpu.max` and `io` left, about 30; Steam's user and mount namespaces sized at 39 (`docs/NAMESPACES.md` §9), N1 to N3 done (19; N3 took `openat2` besides, which bubblewrap 0.12 needs); reclaim, freezing, `cpu.max`, `io`, time namespaces and S4 to S6 are on branches (`docs/roadmap/stage-13-handover.md`) |
| Stage 22, Steam: the parts with a first guess (bubblewrap's rest 13, sound 30, Venus 8; glibc's names are dynamic linking's 13 and XWayland stage 19's, both counted above) | 51; sound re-sized by `docs/AUDIO.md` as 24 for the driver, the core and a gate, spent, then alsa-lib 3 (U1) and a server unsized (U2), so 24 of the 51 left sized | sound done: playback 2026-09-26 -- a ring-3 virtio-snd driver, the audio core, `/dev/snd`, `test-audio`, Chrome playing through it, and `run-compositor --everything` bringing the card -- then alsa-lib (U1) and `pulsed`, the PulseAudio-protocol server (U2a to U2d, 18 points), 2026-09-27; bubblewrap waits on namespaces (N1 and N2 done, see stage 13); Venus not started |
| Stage 22, Steam: the 32-bit x86 ABI and what the runtime and Proton find missing | unsized, ≈ 100 as a guess; `docs/I386.md` sizes I1 to I4 at 42, I5 unsized | under way: I1 to I4 done (42 of 42): 32-bit programs, their threads, signals and fork, and Alpine's and Debian's i386 busybox; I5a, Valve's `steamcmd` logging in, 2026-09-27; yserver, the X server, 36 points, 2026-09-29; Steam's sign-in window on hyprix through yserver 2026-09-29; the client signs in and shows its store on the `--everything` desktop under ferrousli, gated by `test-steam-store`, 2026-09-30; launch-side workarounds left (`docs/STEAM.md` §3), the GPU process's route designed, 14 to 23 points (§6); a game from the library gated by `test-steam-game`, 2026-10-01, not passing yet: the install downloads and then stalls staging, cause not yet known (§7) |
| The installer (`docs/INSTALLER.md`, approved 2026-09-28) | the MVP spent; the rest in §9 | under way: the MVP, `ferrix-install` onto a VM's disk and `test-install`, 2026-09-28; the full root, the ISO, the boot menu, the graphical installer, Secure Boot, shrinking and real PCs left |
| Stage 14, real-time domains | *month* ≈ 40 | not started |
| Stage 15, a real userland | *week* ≈ 20, of which job control is spent; most of the rest landed as zinc and uutils, and what is left is an init, sized at 67 points in `docs/INIT.md` §13, of which all 67 of L1 to L10 are spent, L11's 2 too, and 16 later; authentication (`docs/AUTH.md`, approved 2026-09-26): a kernel fix first (P0) 2, spent, phase 1 27, phase 2 31 and phase 3 about 32 later; init's L13, the sandboxing keys (10), of which L13a (5: `NoNewPrivileges=`, `PrivateTmp=`, `ProtectSystem=`) landed 2026-10-04 and L13b and L13c (5) are on branches `l13b` and `l13c` | init done (L1 to L10, 2026-09-26): `run` and every desktop image boot `/sbin/init`, with `getty`, `svc`, readiness, socket activation, resource limits and the directory over `src/lib/init/svc`; L11 (`devmgr` on its restart policy) done too; L12 and L13 are later by design; authentication's P0 -- `process_create` gave a child root's credentials -- is fixed (2026-09-26), and phase 1 is done (hyprlock's lock goes over `authd`, 2026-10-03); phase 2, the desktop as a user, is done but for three items: `getty` revokes the console before every login (2026-10-04), and K-B (`np-land`, landed 2026-10-04) and K-C (not started) are left |
| ~~Stage 16, `rustc`~~ *exit met 2026-09-22* | ~~*the goal* ≈ 40~~ 8 spent | done |
| Stage 20, self-hosting | *longer*, unsized | in progress: the x86-64 image builds on Ferrix and boots (2026-09-23); the AArch64 image builds on Ferrix on the Pixel 7 and boots (2026-10-03); every build of the matrix recorded, Ferrix making them stops on FX-0001 (2026-09-24); 2026-10-04, on `main`: one kernel for every test, each test's init program and script in the initramfs under `.ferrix/init/` (the plan falls from 271 builds to 188), a 57-row matrix, and a plan mode that records builds without booting (`selfhost-matrix`); the volume carries the components and the guest clones none, which fixes CI's "rustc on Ferrix, and Ferrix built on Ferrix" job (`selfhost-components`, cb872a732; CI's next run on `main` is owed); owed: the plan made complete, a weekly CI job, and the apps and Arm C programs built by Ferrix |
| Stage 21, bare metal and a GPU of Ferrix's own | over 100, unsized | under way on NVIDIA's own driver (`docs/NVIDIA.md`): N0, the kernel prerequisites, and N1, `nvrm` booting the RTX 3060's GSP with `nvidia-smi`, are on `main` (2026-10-03); N2 to N4 (`vulkaninfo`, Chrome's WebGL) and N6 (the desktop on the customer's TV over HDMI, 1920x1080@60, with host keyboard and mouse passed in) are done but unlanded: `land-n6` is ready after a rebase, `nvidia-n2`'s tip does not build; next N3b (dmabufs), a hardware cursor, and measuring page loads |

## Burndown

Scope is the table's sized, unfinished rows on 2026-10-04, after the day's
landings: client pages 8, the second pass and `xray` 8, the GC400's remaining
21 of 32, stage 13's rest 30, stage 15's 11 (init's L13b and L13c 5, and authentication's
P2.1 and P2.2 about 8), Chrome on the DK1 50, stage 14 40, dmabuf and virgl
48, stage 22's 28 (bubblewrap's rest 5, and a guess of 23 for what the Steam
client's launch and games still find missing), and NVIDIA's N2 to N4 64 (done
but unlanded, so still in scope) and N5, CUDA, 52 -- **≈ 360 points**. The
unsized rows (stage 20, the rest of stage 21, the audio server, Chrome's GPU
compositing, the desktop clients' programs, the Pixel's USB driver) are
outside it, so the chart shows when the *sized* work ends, not when the
roadmap does.

### Why the date moved

The 2026-09-24 forecast was 475 points, done 10-01 to 10-03; the 09-26
forecast was 442 points, done 10-01 to 10-03 at 67 to 92 a day. Neither
held, and the count of what happened to the 442 says why:

| what | points |
|---|---|
| sized scope on 2026-09-26 | 442 |
| came off it by landings (yserver 40, stage 15's 27, the clients' foundation 21, the Pixel's VM desktop 17, i386's I1 to I4 42, stage 22's sized part ≈ 11, init's L13a 5, auth's P2.1 2) | − ≈ 165 |
| came off it by a lower guess (stage 22's rest, Venus) | − ≈ 43 |
| added (NVIDIA's N2 to N5 116, init's L13 10) | + 126 |
| sized scope on 2026-10-04 | 360 |

1. **The rate counted work outside the scope.** About 900 points landed in
   those eight days, but ≈ 695 of them were sized afterwards from `git log`:
   the certification, the speculation domain, the components split, Claude
   Code, the installer, the network namespaces. That work is real and takes
   nothing off the table. The 09-26 forecast burned the scope at 67 to 92 a
   day; what came off it was ≈ 20 a day.
2. **Scope arrived nearly as fast as it left.** 126 points came in against
   165 that landed, so the scope fell ≈ 39 points net by work in eight days.
   A forecast holds only for the scope it was drawn on.
3. **Five rows, 167 points, have no session on them**: client pages 8
   (in progress since 09-17), the GC400's 21 (untouched since 09-24),
   Chrome on the DK1 50, stage 14 40, dmabuf and virgl 48. A queue does not
   reach a row nobody works on.
4. **≈ 99 points are built and wait to land**: NVIDIA's N2 to N4 64
   (`land-n6` ready after a rebase, `nvidia-n2`'s tip does not build), the
   stage 13 controllers 30 (`stage13-cgctl`, gated, 11 controls owed), init's
   L13b and L13c 5 (on `l13b`, `l13c`). Landing them takes them off the scope with no new engineering.

So the forecast is drawn at ≈ 20 a day, and every re-baseline says what was
added, so a moved date reads as "scope grew" rather than "it slipped".
Getting the date earlier than that means putting sessions on the idle rows,
or taking them out of the plan, and landing the built work first.

![Burndown: 360 sized points remaining from 2026-10-04, done 10-22 at 20 a day (what came off the 09-26 scope) or 10-11 at 56 a day; below it, points landed per day from 09-14 to 10-04, about 2,100 in total](../img/burndown.svg)

In the upper chart the forecast is the shallower line, 20 a day: what came
off the fixed 09-26 scope from 09-27 to 10-04. The steeper line, 56 a day, is
every point that had an estimate before it started, 2026-09-14 to 10-04, and
is an upper bound: it holds only if every estimated point came off the
table, and NVIDIA's N0 and N1 show that some are scope that came in and
landed in the same week. The running average, 100 a day, is not drawn; it
counts the unsized work too. Neither line allows for scope added later, or
for the qualification below: a step full of unknowns may take twice its
estimate. The lower chart is what has landed, by day, and the running total;
hatched is what was sized afterwards from `git log`, all of 09-18 to 09-23
and most of every day since (`docs/BACKLOG.md`, *Velocity*).

## Gantt

Done bars are dated from the rows above and the ledger, and start where the
first landing was; they overlap because the work ran in parallel. The
forecast is one queue at 20 points a day, in the order of the table, with
the sized rows only, each marked idle, unlanded or added since 09-26; the
idle rows are hollow, because the queue reaches them only once a session is
put on them. It is a sequence for reading the size of the work, not a plan:
several of these would run side by side, and the order is the customer's to
change.

![Gantt: done work from 2026-09-13 to 10-04, ten streams in progress, and the sized remainder as one queue at 20 points a day ending 10-22, five idle rows hollow, and each in-progress row's missing part to where its work ends in the queue](../img/gantt.svg)

Both charts are drawn by `tools/common/gen/gen-roadmap-charts.py`, which holds their
numbers; change them there when the table or the velocity count changes, and
rerun it.

---

Three things qualify these estimates:

* **The kind of work.** The velocity was measured on tables, system calls
  and a renderer, where every estimate held. The GPU path is unknowns in
  every step and XWayland is a server; either may take the time the table
  gives it twice over. A new forecast needs a new count once that work has
  enough history.
* **The fleet.** The number is a fleet's. One session alone does 15–20 a
  day, and the same estimate then reads in weeks rather than days.
* **Unsized stages.** A word like *month* was written for one person before
  points existed; the guess beside it is only so that a date can be put
  down at all. Stage 21 and the rest of 22 have no date because they have
  no size, so "everything done" has no date until they are sized.
* **New scope.** The date holds for the scope it was drawn on. When an
  unsized row is designed and sized, as NVIDIA's was, its points move the
  date; the next re-baseline says how many.

---
