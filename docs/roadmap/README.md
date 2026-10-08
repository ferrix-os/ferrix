# Ferrix — roadmap

Ferrix is an operating system in Rust for x86-64, AArch64 and ARMv7-A. Its
first goal, compiling Rust on Ferrix, is met; the goal since is a
Hyprland-shaped desktop, and then Steam. This page is the short version; each
stage has its own page, and the details are linked at the end.

The sidebar marks each stage: ✓ done, ◐ in progress, ○ not started.

## Where it stands (2026-09-30)

**Done**

- Stages 0–12 run in every boot test on all three architectures; ARMv7-A also
  boots on an STM32MP157D-DK1 board.
- The first goal is met: `rustc` compiles and runs a program on Ferrix
  (stage 16).
- Display, input and the compositor (stages 17 and 18), networking, dynamic
  linking and sysfs.

**In progress**

- **Stage 13:** cgroups with pids, memory, OOM kill and CPU weight. The
  mount, user, UTS, IPC, cgroup, pid and network namespaces are in, and so
  are seccomp's filters (S3, 2026-10-04). The cgroup controllers (reclaim and
  `memory.high`, freezing, `cpu.max`, `io`) landed on 2026-10-07; time
  namespaces, seccomp's S4 to S6 and the exit program are left.
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
- **The installer:** an MVP installs Ferrix on a VM's disk (2026-09-28).
- **Chrome** runs headless and in a window, on glibc and on ferrousli, Ferrix's
  own C library. On ARMv7-A, Debian's armhf Chromium runs headless in 512 MiB
  under QEMU, and the DK1's SD card has a driver; neither has run on the board.
- **Live driver updates:** `devmgr` replaces the display driver with a new
  image while the machine runs, and goes back to the old one if the new one
  does not publish (2026-10-08, test images only: `docs/DEVMGR.md` §4.1).
- **Pixel 7:** boots natively on all eight cores, runs the desktop in a VM,
  and streams its log over USB.

**Next:** cutting test and gate run time, the customer's priority one
(`docs/TEST-TIME.md`), then Steam's workarounds and the namespaces under
them; what is red, parked and waiting is in
[Where it stands](where-it-stands.md).

**Not started**

- Stage 14 (real-time).

**Under way, but outside the list above:** stage 21, NVIDIA's own driver on the
RTX 3060 (`docs/NVIDIA.md`): N0, the kernel prerequisites, and N1, the GSP
booting with `nvidia-smi`, are on `main`.

## Forecast

- About **518 sized points** were left on 2026-10-08, the last count, at
  the forecast rate of 20 a day: they end on 2026-11-03 (2026-10-18 at 56 a
  day). The scope was 442 on 2026-09-26 and grew as rows were sized; the
  recount and what moved it are in [Status](status.md).
- Unsized work (self-hosting, bare metal, most of Steam) is not in any date.

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
|  | [Claude Code — Anthropic's coding agent on Ferrix](claude-code.md) | ◐ in progress | the command line, 2026-09-30; the desktop app being assessed |
| 13 | [Namespaces, cgroups v2, seccomp](stage-13-namespaces-cgroups-v2-seccomp.md) | ◐ in progress | month |
| 14 | [Real-time domains](stage-14-real-time-domains.md) | ○ not started | month |
| 15 | [A real userland](stage-15-real-userland.md) | ◐ in progress | week |
| 16 | [`rustc`](stage-16-rustc.md) | ✓ done | the goal; ≈ 40 guessed, 8 spent |
| 17 | [Display and input](stage-17-display-input.md) | ✓ done | 74 points, spent |
| 18 | [The compositor](stage-18-compositor.md) | ✓ done | 96 points, spent |
| 19 | [Hyprland fidelity, and the GPU](stage-19-hyprland-fidelity-gpu.md) | ◐ in progress | 178 points, about 8 left |
| 20 | [Self-hosting](stage-20-self-hosting.md) | ◐ in progress |  |
| 21 | [Bare metal, and a GPU of Ferrix's own](stage-21-bare-metal-gpu-ferrix.md) | ◐ in progress | unsized, over 100 points |
| 22 | [Steam](stage-22-steam.md) | ◐ in progress | unsized, over 300 points |
|  | [Written ahead of their stage](written-ahead.md) |  |  |
|  | [Continuously, from stage 1](continuously.md) |  |  |

<!-- End of the generated stage index. -->
