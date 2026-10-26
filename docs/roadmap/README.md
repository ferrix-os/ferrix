# Ferrix — roadmap

Ferrix is an operating system in Rust for x86-64, AArch64 and ARMv7-A. Its
first goal, compiling Rust on Ferrix, is met; the goal since is a
Hyprland-shaped desktop, then Steam, and after Steam a Windows game,
*Arknights: Endfield* (stage 23, 2026-10-10). This page is the short version; each
stage has its own page, and the details are linked at the end.

The sidebar marks each stage: ✓ done, ◐ in progress, ○ not started.

## Where it stands (2026-10-21)

**Done**

- Stages 0–12 run in every boot test on all three architectures; ARMv7-A also
  boots on an STM32MP157D-DK1 board.
- The first goal is met: `rustc` compiles and runs a program on Ferrix
  (stage 16).
- Display, input and the compositor (stages 17 and 18), networking, dynamic
  linking and sysfs.
- **Stage 13**, namespaces, cgroups and seccomp, as built: the
  mount, user, UTS, IPC, cgroup, pid and network namespaces, the cgroup
  controllers (`pids`, `memory` with reclaim and `memory.high`, `cpu`
  with `cpu.max`, `io` with `io.max`, freezing), seccomp's filters, `TRAP`
  and `TSYNC`, and Syscall User Dispatch. `test-container`, an unprivileged
  user namespace with a pid 1 under a memory limit and a seccomp filter,
  passes on all three architectures (2026-10-14). What the stage was first
  written to hold beyond that (time namespaces, unprivileged mounting,
  reclaim of the dentry and inode caches and the rest) is the
  [follow-ons](follow-ons.md), all built on 2026-10-26 on the branch
  `claude/ferrix-roadmap-completion-hkxlpp`, each landing owing the gate host's runs.

**In progress**

- **Stage 15:** the init is done (L1 to L13), its sandboxing keys all in:
  L13a (`NoNewPrivileges=`, `PrivateTmp=`, `ProtectSystem=`), L13b
  (`PrivateNetwork=`, 2026-10-05) and L13c (`SystemCallFilter=`, 2026-10-08).
  `/sbin/init` boots every image, starts `devmgr` and runs the
  desktop as a service; logins go through `authd` (`docs/AUTH.md` phases 1 and
  2 but for K-C), hyprlock's lock over it included.
- **Stage 19:** the desktop composites on the GPU, and yserver, an X server
  in Rust, shows X windows on it; client pages as texture backing are left (the
  second-pass effects, `xray` and `no_screen_share`, landed on 2026-10-07, and
  `zwp_linux_dmabuf_v1` with a GBM-shaped allocator on 2026-10-08). waybar, fuzzel, hypridle and hyprlock,
  rewritten in Rust, run the customer's own config on `run-compositor
  --everything` (its X11 link landed on 2026-10-05).
- **Stage 20:** Ferrix builds its own x86-64 image, and since 2026-10-03 its
  own AArch64 image on Arm hardware: inside Ferrix on a Pixel 7, in crosvm.
  Its matrix's plan mode is complete since 2026-10-05 (S-2).
- **Stage 22 (Steam):** sound plays through `/dev/snd` and a PulseAudio-protocol
  server, Chrome plays video with sound, 32-bit x86 programs run, and Valve's
  `steamcmd` logs in to Steam. The Steam client draws its sign-in window
  through yserver (2026-09-29), with launch-side workarounds
  (`docs/STEAM.md`).
- **The channel round trip, toward seL4 (440 ns):** 2,556 ns at the
  2026-10-05 wind-down, from 37 us; step 1, step 2, step 3, step 4's fast path
  (behind `ferrix.fastpath=on`, x86-64) and step 5's ERAPS and vector reset
  are in, and so are the cuts of 2026-10-07 and 2026-10-08 (the FS and GS
  skip, user-side inlining, a live count of filtered threads): about 1,048 ns
  on 4066f41dd in the faster of a boot's two modes, 873 ns after the FS and GS
  skip in one quiet window, and the later figures were taken on other bases and
  are not summed ([the page](ipc-round-trip.md)); the rest of step 5 is left
  (`docs/OPAQUE-KERNEL.md` §9.11). Two landings of 2026-10-08 evening are not
  cuts: `cpu.stat`'s charge is kept back per run queue (the controllers had
  made it a walk up the job tree at every direction; about 237 instructions
  fewer a round trip, preliminary), and a wait holds preemption off through its
  last look (F-69, a lost wake that stalled `bench-ipc` on ARMv7-A), which costs
  about 60 ns on the general path until a cheaper hold is written. On
  ARMv7-A a switch between two programs that never used VFP moves none
  (lazy VFP, §9.15), not yet timed.
- **The installer:** an MVP installs Ferrix on a VM's disk (2026-09-28); the
  root volume is made to its partition's size (I3, 2026-10-19); A/B system
  slots give an update a trial boot and a way back (2026-10-11); NVMe, AHCI and
  a PC's xHCI have drivers (the NVMe driver has driven a real WD SN720 through
  VFIO and installed Ferrix onto it); a netboot stick boots a bare-metal PC
  (2026-10-15). The first release, v0.1.0, was tagged on 2026-10-20 and built
  none of its assets; v0.1.1 (2026-10-21) carries the x86-64 desktop as a zip
  that plain QEMU boots (`docs/RELEASES.md`). `test-install` fails on
  `main` in the live medium's boot (a BACKLOG row).
- **The customer's N150 box (stage 21):** Ferrix booted on it on 2026-10-15
  to a desktop on the firmware framebuffer, `/` on its SATA drive and the
  network up, on a branch; its drivers (AHCI, the YT6801's network, the
  framebuffer, VT-d, SMBIOS, an event log for panics) and an Intel GPU render
  driver, whose first GPU commands ran on the box on 2026-10-20, are on
  `main`. The kernel bring-up is evidenced on QEMU only so far.
- **The switch to a microkernel (M0, `docs/MICROKERNEL.md`):** ordered by the
  customer on 2026-10-13. The build-flag system with its four kernel
  profiles, the load ratchet and a Ferrocene-version type-check are on
  `main`; so are the two fixes `smt=off` waited on (F-73, F-64) and the
  tooling for the performance baselines, which are not measured yet; M1's
  designs (K5, K6, K7, K10, K12) are reviewed, with their code on branches
  ([its page](microkernel-m0.md)).
- **Chrome** runs headless and in a window, on glibc and on ferrousli, Ferrix's
  own C library. On ARMv7-A, Debian's armhf Chromium runs headless in 512 MiB
  under QEMU, and the DK1's SD card has a driver; neither has run on the board.
- **Live driver updates:** `devmgr` replaces the display driver with a new
  image while the machine runs, and goes back to the old one if the new one
  does not publish (2026-10-08, test images only: `docs/DEVMGR.md` §4.1).
- **Pixel 7:** boots natively on all eight cores, runs the desktop in a VM,
  and streams its log over USB.
- **Stage 23 (Arknights: Endfield):** the road from Steam to a Windows game
  through Wine with DXVK on NVIDIA's Vulkan, put on the roadmap by the
  customer on 2026-10-10; about 205 points as a first guess. Step 0 answered
  on 2026-10-13: the game's anti-cheat refuses to run in a virtual machine,
  so steps 4 to 6 wait for Ferrix on the machine. Wine starts on Ferrix,
  Direct3D 11 draws through DXVK and a Unity game starts in it, with no card,
  and Endfield's launcher draws its install dialog in Debian's Wine and in
  GE-Proton11-7's (2026-10-14); the game's download stopped on ENOSPC from
  Ferrix's btrfs, whose cause (a writable mount's page cache was never
  reclaimed) is fixed on `main` since 2026-10-15 and not yet run again.
  Syscall User Dispatch and seccomp's `TRAP`, which Proton's Wine and the
  anti-cheat's direct calls need, landed on 2026-10-18. The GPU steps stand
  on stage 21's unlanded branches and on the N150 box's Intel GPU driver
  ([its page](stage-23-arknights-endfield.md)).
- **Stage 24 (the game path):** where a fullscreen game can be faster on
  Ferrix than on a stock Linux desktop: input to scan-out, and even frames
  under a background load. A design (`docs/GAME-PATH.md`) the customer made a
  stage on 2026-10-10; its rig's R1 to R3 (the frame recorder, the latency
  probe, the load and the pinned guests) landed on 2026-10-14, I1 (input's
  time and thread) on 2026-10-15, and nothing is measured on the card yet. About
  68 points before the game domain; the game domain (52) is stage 14's
  ground and an open decision, as eight more are. It stands on stage 21's
  unlanded zero-copy work ([its page](stage-24-game-path.md)).
- **Stage 25 (Yoru, the main UI):** the customer's design "Yoru WM" as the
  desktop's look and shell on hyprix (`docs/DESKTOP.md` §6, 2026-10-24).
  Built: the tokens, the dock and its stash, the launcher's look, the
  sidebars, the wallpaper picker, Nocturne for GTK and `portald`, and
  Chrome's GTK volumes; to do: the palette, snapping, the helpers, the
  lock, notifications and switcher in the tokens, and a Files app
  ([its page](stage-25-yoru.md)).

**Next:** cutting test and gate run time, the customer's priority one
(`docs/TEST-TIME.md`), then Steam's workarounds and the namespaces under
them; what is red, parked and waiting is in
[Where it stands](where-it-stands.md).

**Not started**

- Stage 14 (real-time).

**Under way, but outside the list above:** stage 21, NVIDIA's own driver on the
RTX 3060 (`docs/NVIDIA.md`): N0, the kernel prerequisites, and N1, the GSP
booting with `nvidia-smi`, are on `main`, and so are N2 to N4, the desktop on
the card's monitor and the kernel half of N3b's dmabufs.

## Forecast

- About **518 sized points** were left on 2026-10-08, the last count, at
  the forecast rate of 20 a day: they end on 2026-11-03 (2026-10-18 at 56 a
  day). The scope was 442 on 2026-09-26 and grew as rows were sized; the
  recount and what moved it are in [Status](status.md).
- Unsized work (self-hosting, bare metal, most of Steam) is not in any date,
  and neither are stage 23's and stage 24's first guesses.

## Details

- [Where it stands, in full](where-it-stands.md): every stage's state, one
  paragraph each.
- [Status, estimates and forecast](status.md): the status table, velocity,
  burndown and Gantt charts.
- [The native channel round trip](ipc-round-trip.md): domain-call p50 against
  seL4's 440 ns, a chart and a table of every point.
- [How this roadmap works](about.md): the two rules that order the stages, and
  how sizes are given.
- [How to edit the roadmap](HOW-TO-EDIT.md).

<!-- The stage index below is generated from the headings by `python3 tools/common/gen/split-roadmap.py index`; do not edit it by hand. -->

## The stages

One file a section, in the roadmap's order. The status is the heading's
✅ or `done`, and otherwise the status table's rows for that stage;
the size is what the heading says. [HOW-TO-EDIT.md](HOW-TO-EDIT.md) says
how to change a stage, add one, or carry a branch's edits of the old
single file across.

| Stage | Section | Status | Size, as the heading gives it |
|---|---|---|---|
| 0 | [Foundation](stage-00-foundation.md) | ✓ done |  |
| 1 | [Boot, both architectures](stage-01-boot-both-architectures.md) | ✓ done |  |
| 2 | [Physical and virtual memory](stage-02-physical-virtual-memory.md) | ✓ done |  |
| 3 | [Traps, interrupts, time](stage-03-traps-interrupts-time.md) | ✓ done |  |
| 4 | [SMP](stage-04-smp.md) | ✓ done |  |
|  | [ARMv7-A — a third architecture](armv7a.md) | ✓ done |  |
| 5 | [Tasks and the scheduler](stage-05-tasks-scheduler.md) | ✓ done |  |
| 6 | [User mode](stage-06-user-mode.md) | ✓ done |  |
| 7 | [The Linux syscall ABI](stage-07-linux-syscall-abi.md) | ✓ done |  |
| 8 | [VFS, initramfs, the pseudo-filesystems](stage-08-vfs-initramfs-pseudo-filesystems.md) | ✓ done |  |
| 9 | [The native ABI: handles, channels, ports, VMOs](stage-09-native-abi.md) | ✓ done |  |
| 10 | [Userspace drivers](stage-10-userspace-drivers.md) | ✓ done |  |
| 11 | [Block core and btrfs, read](stage-11-block-core-btrfs-read.md) | ✓ done |  |
|  | [Networking — sockets, a net core, virtio-net](networking.md) | ✓ done |  |
|  | [Dynamic linking — PIE, `PT_INTERP`, a loader](dynamic-linking.md) | ✓ done | done 2026-09-23: 39 points, and ferrousli's port at ≈ 34 |
| 12 | [btrfs, write](stage-12-btrfs-write.md) | ✓ done | ≈ 60 points, spent |
|  | [sysfs — the device tree, fed by the services that own it](sysfs.md) | ✓ done | 26 points, spent |
|  | [Chrome — a browser on Ferrix](chrome.md) | ◐ in progress | headless and in a window, 2026-09-24; the DK1 ≈ 45–55 points |
|  | [Claude Code — Anthropic's coding agent on Ferrix](claude-code.md) | ◐ in progress | the command line, 2026-09-30; the desktop app assessed, 2026-10-14 |
| 13 | [Namespaces, cgroups v2, seccomp](stage-13-namespaces-cgroups-v2-seccomp.md) | ✓ done | month |
|  | [Namespace and cgroup follow-ons](follow-ons.md) | ◐ in progress | unsized; built 2026-10-26, landing owed |
| 14 | [Real-time domains](stage-14-real-time-domains.md) | ○ not started | month |
| 15 | [A real userland](stage-15-real-userland.md) | ◐ in progress | week |
| 16 | [`rustc`](stage-16-rustc.md) | ✓ done | the goal; ≈ 40 guessed, 8 spent |
| 17 | [Display and input](stage-17-display-input.md) | ✓ done | 74 points, spent |
| 18 | [The compositor](stage-18-compositor.md) | ✓ done | 96 points, spent |
| 19 | [Hyprland fidelity, and the GPU](stage-19-hyprland-fidelity-gpu.md) | ◐ in progress | 178 points, about 8 left |
| 20 | [Self-hosting](stage-20-self-hosting.md) | ◐ in progress |  |
| 21 | [Bare metal, and a GPU of Ferrix's own](stage-21-bare-metal-gpu-ferrix.md) | ◐ in progress | unsized, over 100 points |
| 22 | [Steam](stage-22-steam.md) | ◐ in progress | unsized, over 300 points |
| 23 | [Arknights: Endfield](stage-23-arknights-endfield.md) | ◐ in progress | ≈ 205 points as a first guess, step 5 unknown |
| 24 | [The game path](stage-24-game-path.md) | ◐ in progress | ≈ 68 points before the game domain, about 190 with it and the later rows |
| 25 | [Yoru, the main UI](stage-25-yoru.md) | ◐ in progress | 63 points estimated; about 30 built |
|  | [Microkernel M0 — the build flags, the baselines, the checks](microkernel-m0.md) | ◐ in progress | ≈ 45–60 points |
|  | [Written ahead of their stage](written-ahead.md) |  |  |
|  | [Continuously, from stage 1](continuously.md) |  |  |

<!-- End of the generated stage index. -->
