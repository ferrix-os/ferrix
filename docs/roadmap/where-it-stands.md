# Where it stands, in full

*Reviewed 2026-10-05, to `main` cf30aa08f.* The short version is on the [overview](README.md).

## Where it stands now (2026-09-30)

Since the 2026-09-27 wind-down, everything finished is on `main` and pushed:

* **Claude Code runs on Ferrix** (2026-09-30, `docs/CLAUDE-CODE.md`,
  `cargo xtask test-claude-code`): Anthropic's linux-x64 release takes a
  prompt, runs the model's Bash command on Ferrix and sends back its
  output, and `claude` is in the `--everything` desktop's terminals, and so
  in `remote-desktop`'s. It needed AVX: the kernel saves it with `XSAVE`
  now, and every x86-64 guest's model has x86-64-v3. On that desktop it
  runs on ferrousli, as every glibc program there does, since ferrousli got
  the POSIX timers on 2026-10-01. The Claude desktop app is next, as an
  assessment.

* **The trip to ring 3 is being cut** (2026-09-30, `docs/OPAQUE-KERNEL.md`
  § 8). The customer's direction is Linux software beside safety functions
  on a kernel that can be assured. Moving btrfs's parser out of ring 0 is a
  step of that, and it needs a cheap trip first. The `seam` check now
  traces where a 4 KiB read's 230 us go under KVM (`seam-trip`,
  `seam-count`): no missed wake-ups, but 13 switches, 3 IPIs and 60 to 85%
  of wakes landing on another processor per read. Two fixes are in, both
  reviewed by the certification consultant (2026-10-01):
  - the block ring wakes a reader once, with one copy (b7cab053). At
    depth 32 the mean fell from 10.3 ms to 2.1 ms;
  - an MSI-X vector is no longer masked per interrupt (1dcc433f). The
    depth-1 p50 fell from 291 us to 206 us.

  Lazy TLB, the ring task off the data path, and same-processor wakes are
  on `os-35/ipc-*` branches; `docs/BACKLOG.md` says what each has left.
  The claim they serve is drafted as a proposal in
  `docs/certification/CLAIM.md`.

* **Speculation domains** (2026-10-01, bf9efba95, `docs/OPAQUE-KERNEL.md`
  §9). A job marked at its creation, by the holder of MANAGE on its parent,
  is one speculation domain. A switch between two programs born in it skips
  `IBPB`, and every other switch keeps the barrier. F-60, a leave that could
  miss an installing processor, was found and closed on 2026-10-03.

* **The channel round trip, toward seL4** (2026-10-03, `docs/OPAQUE-KERNEL.md`
  §9.5 to §9.9). The customer's target is seL4's own figure, 440 ns a round
  trip on nazuna, measured with protections matched. Step 1 and five of step
  2's six pieces (2a to 2e) are on `main`, each reviewed by the certification
  consultant: a native round trip inside a domain is 2,556 ns with every
  mitigation on, down from 37 us. Redox, measured the same way, takes 1,965
  ns with no speculative defence. `bench-ipc` is exact since 2026-10-05
  (a fenced counter, sorted samples, one pinned processor, alternation;
  3349682db). Left: 2f (gated, on `po6/step2f`, waiting for the consultant's
  conditions), step 3 (3a and 3b, work in progress on `po6/step3`), step 4's
  direct switch and fast path (its groundwork on `po6/step4-prep` waits for
  review), step 5 and step 4b; the handover is
  `docs/handover/2026-10-03-ipc.md`.

* **Steam signs in and shows its store** on the `--everything` desktop,
  its 64-bit side on ferrousli (2026-09-30, `docs/STEAM.md` §5), and
  `cargo xtask test-steam-store` gates it with a test account, signing in
  with no one there. `run-compositor --everything` fetches Steam's volume
  when it is missing, and fuzzel lists Steam to start it again
  (2026-10-01). It still runs with launch-side workarounds, each owned by a
  fix (`docs/STEAM.md` §3): namespaces and seccomp for pressure-vessel and
  Chromium's sandbox, the `/proc` gaps behind the runtime's logger, the
  `SIGBUS` that needs a 16 GiB guest, and the web helper's GPU process
  off. F-55, which the GPU route through Venus met first, is closed; the
  rest of that route is designed (`docs/STEAM.md` §6) and not started.
  The second exit step, a game from the library, has its gate,
  `test-steam-game` (2026-10-01, not passing yet): Teeworlds is claimed for
  the test account, installed on request and downloaded in about 90 s
  since the guest's network took Steam's dozen connections (3b1b1de7,
  020dc9b2), and then Steam's staging of the files stands still, for a
  reason not yet known (`docs/STEAM.md` §7, BACKLOG).
* **`--everything` is everything** (the customer's rule, 2026-10-01): the
  desktop carries every feature, volume and app, and nothing is left out
  with a line saying how it could have been added. The volume half is in:
  a volume not fetched yet (rustc, Chrome, steamcmd, Claude Code, the
  Steam window) is fetched before the merge, and a failed fetch stops the
  run (`everything.rs`, f7777c17). Left: every app on that desktop, not
  just the `default` ones, and btop built through WSL on a Windows host
  (os-3c, `docs/APPS.md`). vkgears is listed wherever the host gives Venus,
  which a Windows host cannot (`docs/GPU.md` §3).
* **yserver is done**, all 36 of its points (`docs/YSERVER.md`, Y1 to Y7):
  X windows show on hyprix with their input, sizes, close, dialogs, menus
  and clipboard, and a window of a fixed size floats at that size. A
  floating window that draws its own title bar, such as Steam's sign-in
  window, can be dragged by it (2026-10-03: `_NET_WM_MOVERESIZE` becomes
  `xdg_toplevel.move`, which hyprix now carries out). It is the X server
  stage 19 counted 40 for.
* **Namespaces**: N1, per-mount flags (2026-09-28), N2, binds and
  detach, and N3, mount namespaces with `pivot_root` and `openat2`
  (2026-09-30), are in, each reviewed by the certification consultant:
  Debian's bubblewrap runs as root (`cargo xtask test-bwrap`). Every
  namespace stands on an empty bottom mount, as a booted Linux machine's
  `/` does, so `pivot_root` works with `/` in memory too, and Steam's own
  requirements check exits 0 as root in `test-steam-bootstrap`. N4, user
  namespaces, and the rest of stage 13 are os-7c's, which the customer
  asked to take the stage to done (`docs/NAMESPACES.md` §12).
* **An installer MVP** (2026-09-28, `docs/INSTALLER.md` §11):
  `ferrix-install` partitions a VM's disk and installs the system, and
  `cargo xtask test-install` boots the result through OVMF. The customer
  approved the full design the same day.
* **Test run time**, cut 2 (2026-09-28, `docs/TEST-TIME.md`): a finished
  guest is stopped rather than given its grace, and `test-compositor --arch
  x86_64` went from 900 s to 512-616 s. Cut 3 (2026-09-30): `--arch all` of
  `test-boot`, `test-init` and `test-audio` runs the architectures at once,
  `test-boot` from 136-139 s to 46 s and `test-init` from 171-182 s to
  72-86 s on a loaded host; `test-compositor` stays in turn, since its
  x86-64 frame budget failed when three suites emulated side by side.
* **The one red gate is fixed** (2026-09-30): `test-compositor`'s submap
  boot left a window's slide undrawn about one run in two, because hyprix's
  loop owed no frame after one that started an animation. A compositor boot
  whose kernel panics now also asks QEMU where every processor is
  (`panic-registers.txt`), for FX-0001, seen twice there.
* **NVIDIA's own driver drives the RTX 3060's monitor** (2026-10-03 and
  -04, stage 21, `docs/NVIDIA.md`; on `main` since 2026-10-05, `land-n6`,
  a84992dc5):
  * Chrome's WebGL renders on the card, through ANGLE on NVIDIA's Vulkan.
  * The desktop runs on the customer's TV on the 3060's HDMI port, with
    NVKMS inside `nvrm` and `nvrm` as the display core's copying driver.
  * The customer drives it with a dedicated keyboard and mouse.

  It runs at 44–53 fps, with software compositing. The conditions the
  landing left are BACKLOG rows O1 to O6 (consultant ledger 318). Next, in
  the customer's order:
  1. dmabufs for Chrome's GPU compositing (N3b, half built on
     `nvidia-n2`);
  2. a hardware cursor;
  3. measuring page loads.

  Also on `main` (2026-10-05): the chardev core's queue to `nvrm` is
  held to its room (F-63, closed), and stage 10's chardev self-check, with a
  fake driver, requires it and the HELLO rules, copies and drains (N10's
  code half, N12, N13; FX-1013). N5's switch, D1 to D10 and
  `test-nvidia-smi` stay owed.
* **Sound** is done: alsa-lib (U1) and `pulsed`, a PulseAudio-protocol
  server Chrome plays through on the desktop (U2a to U2d, 2026-09-27).
* **Windows**: the desktop runs under WHPX with the TSC as its clock
  (2026-09-29), where it had fallen back to TCG.
* **The tree moved** into `src/`, `tools/` and `docs/` (2026-09-29,
  `docs/LAYOUT.md`), tests into `src/tests/`, and the kernel's interface
  cores and discovery code into `interfaces/` and `discovery/`.
* **Discovery and drivers** (2026-09-30, stage 10's last section): the
  kernel finds devices through one `Finder` trait, run once in order, and
  ACPI-or-device-tree is decided in one host-tested place. Ring-3 drivers
  share `ferrix-driver`, where a DMA buffer cannot be freed before its
  device has reset; virtio-input and virtio-blk are on it, seven drivers
  are not yet, and no driver's graceful STOP is reachable yet.

**Red on `main`** (`docs/BACKLOG.md`, *Red on `main`*): on 2026-10-05
`main` had two stage 10 panics, both fixed. FX-1012, one boot in 27 to 160
(`iommu/check.rs` G3 racing the console's pump thread, which had ended 340 of
a night's 9,234 boots as "QEMU killed"): G3 and R5 take their check byte in
the receive path, out of the pump's reach (a6116e822). The seam panic: the block ring's
index was read byte by byte on the driver side, so a reader could see half a
`u32`; the accessors are single accesses now and the virtio notify has a fence
(1e113bb31, 3f0e56b2b). The net ring has the same flaw and is unowned, 3 points.
The Windows gateway test lost a guest's frame when a timed receive raced a
datagram; the gateway waits by peek now (e321befc4). Fixed and on `main`
(0ece8826e's batch):
`test-init --arch all` failing on the revoke reader, on prompts typed too
early and on `su`'s half-read password, and stage 5's moving lock check
panicking because an idle processor could take another idle one's last
waiting task (`sched::steal_from` now leaves it that task); `xtask`'s serial
reader, which sent the "QEMU signal 15" after a stage 10 panic, decodes
lossily. H.SCHED.1's idle-kick rule has no requirement yet. As of 2026-09-30
the submap boot's row is done. Four flake rows gained sightings or were filed
on 2026-09-30, each with its log: FX-0001 (twice, at the submap boot's
`L`), the compositor's frame budget under load (x86-64 and ARMv7-A), the
audit self-check's FX-0309, and the semaphore check's "a waiter returned
without waiting".

**Parked on branches**, pushed, each with a `docs/BACKLOG.md` row:

| Branch | What | Left |
|---|---|---|
| `boot-21b` | W-8 boot requirements, part b (root, init, power, random, F-51) | the consultant's diff review, a rebase with carry, then 21c (`devmgr`) |
| `w8-armv7a` | W-8 file 24, ARMv7-A and `arch/arm_common` (drafts) | write the SysML file against the code as F-48 and F-50 left it |
| `f46-power-off` | F-46, power-off gates that took a triple fault for a power-off | correct the message, rebase, gate, the consultant's OK |

**Next**, in the order the customer last gave: test and gate run time
(the rest of cut 2, then KVM by default on x86-64 where the host has it,
`docs/TEST-TIME.md` *Next*); Steam, retiring its workarounds, with stage
13's namespaces and seccomp under them (os-7c); W-8's 21b, 21c and file 24;
fuzzel's second-press toggle; the rest of authentication's phase 2 (below).

**Waiting on the customer** (`docs/BACKLOG.md`, *Waiting on the customer*):
F-43 (W^X for programs, or a narrower claim), the Common Criteria version
(F-52), FMT_SMF.1 and FMT_MTD.1, whether the customer's Python desktop
scripts are rewritten for Ferrix, and the installer's reference PC
(`docs/INSTALLER.md` §10, decision 3). Hardware confirmations (F-44 on the
Pixel 7, F-48 and F-50 on the DK1) wait for the product owner's word in
daytime.

## Stage by stage

Stages 0–12 and 16 are done, and so are networking, 17 and 18.

Stages 0–12 are in the boot test on all three architectures, and the boot
marker reads `FERRIX-BOOT-OK stages 1-12`.

ARMv7-A joined after stage 3 — see *ARMv7-A* after stage 4 — and has run on
hardware: an STM32MP157D-DK1 at two cores reached `FERRIX-BOOT-OK stages 1-9`
and ran stage 7's script at `fd4442e`.

Stage 7's exit is somebody else's static musl busybox running a script on
every architecture, checked by `cargo xtask test-shell` rather than the boot
test because it needs a binary the repository does not carry. Since the exit,
a program can `fork`, `execve` and `wait4`; its section lists what the Linux
surface still owes.

Stage 8's self-checks are in the boot test — the root filesystem unpacked from
an initramfs, every process's descriptor table, the calls that take a path,
pipes, `/dev` and `/proc` — and its exit, `cargo xtask test-vfs` running `ls
-R /proc`, `cat /proc/self/maps` and one shell script whose applets are forked
programs, passes on all three architectures; it is a test of its own for the
reason stage 7's is.

Stage 9's exit runs in the boot test itself: two programs in user mode
exchange messages and a handle over a channel, and a job kill takes down a
process tree, with ports, interrupts delivered to them and device memory a
driver can map built on the same objects.

Stage 10's exit runs in the boot test itself: a virtio-blk driver in ring 3,
started by `devmgr`, reads sectors through the block ring with VT-d on x86-64
and the `SMMUv3` on AArch64 translating, and a deliberate out-of-domain write
faulted on both; ARMv7-A runs it in degraded trusted mode, as decided. What
the stage still owes — trusting decoding-off BARs, AMD-Vi, and one unexplained
flake — is after the exit in its section.

Stage 12's exit is met: Ferrix writes btrfs. Every boot mounts a blank volume
writable on a third disk, builds a tree on it, unmounts, mounts it again and
reads it all back, and `cargo xtask test-btrfs` then has host `btrfs check
--check-data-csum` judge that same image, clean on all three architectures.
`fsync` writes a log tree the next mount replays, and `cargo xtask
test-powerfail` kills QEMU in the middle of writing, replays at the next boot
and has `btrfs check` judge the volume before and after: 249 seeds across the
three architectures, 226 of them leaving a log to replay, every one clean.
`cargo xtask run` now boots with `/` on a persistent btrfs volume. Pages
written through `MAP_SHARED` are written back too (2026-09-23): a linker's
output, which `lld` writes through a mapping, lost its pages when the file
left the cache.

Stage 16's exit, the goal, is met: `cargo xtask test-rustc` compiles
`hello.rs` on Ferrix with the rust-lang.org `rustc`, linked through `cc` and
`rust-lld`, from a btrfs volume, and runs what it made, on x86-64 and in CI
since 2026-09-22.

Stage 20, self-hosting, has its first step: Ferrix builds its own x86-64
image. `cargo xtask test-selfhost` runs the same `cargo xtask build` a person
runs on a Linux host inside Ferrix, with Cargo, from the tree and its vendored
crates on a btrfs volume, and the image it made passes the boot test.
Since 2026-10-03 it does so on Arm hardware too: on a Pixel 7, the phone's own
desktop image boots in crosvm with pid 1 a script on the toolchain's volume,
Ferrix builds its AArch64 image there in about nine minutes, and
`cargo xtask test-selfhost --arch aarch64 --volume` finds the volume a clean
btrfs and boots the image to the end of its self-checks
(`tools/vendor/google/pixel7/selfhost.sh`).
The whole matrix built by Ferrix is not done: on 2026-10-04 its 57 rows were
recorded (271 builds, 188 once a test's init went into the initramfs and one
kernel served every test), and that change, with the record-without-booting
mode a weekly CI job needs, landed on `main` the same day (branch
`selfhost-matrix`, ending at 77783565a). CI's self-hosting job was red
from the components split (2263225e1, 2026-10-03), because the guest
tried to clone the component repositories with no network; the volume now
carries the components and the guest clones none (`selfhost-components`,
cb872a732); CI's job has passed on `main` since, from 77783565a (run
37229234877) on. The plan is complete since 2026-10-05 (S-2, ce133294c and
a3b240c6b): the tests that made builds after their first boot, which plan mode
refuses, now build every variant before it, and a plan run records 162
distinct builds against 139.

Dynamic linking is done: Debian's glibc busybox runs on its own `ld-linux`,
and on ferrousli's loader and `libc.so.6` in glibc's place, on all three
architectures, since ferrousli itself was ported to AArch64 and ARMv7-A on
2026-09-23.

Stage 15 has job control and, since 2026-09-26, a real init: `/sbin/init` runs
services in cgroups of their own over `src/lib/init/svc`'s manager, with `svc` to
drive it, readiness, socket activation and resource limits, gives the console
a getty, and powers the machine off, on all three architectures (`cargo xtask
test-init`). `run` and the desktop boot it, and the compositor is its service.
Since 2026-10-04 a unit can be sandboxed with L13a's `NoNewPrivileges=`,
`PrivateTmp=` and `ProtectSystem=` (1e1f2543a, `docs/INIT.md` §4.5);
L13b's `PrivateNetwork=` landed on 2026-10-05 (3349682db); L13c's
`SystemCallFilter=` is built on branch `po6/l13c` and waits to land (its batch
failed one `test-vfs` cgroup `rmdir` that `main` passes, cause not known). `test-init` types each password once, when its prompt is
on the console (`ferrix-auth-client` flushes before it shows the prompt), and
judges the revoke reader on one look (2026-10-05).

Stage 15's authentication (`docs/AUTH.md`, approved by the customer on
2026-09-26) has phase 1 done and most of phase 2. Phase 1: its kernel fix,
P0 (a native process runs as the one that made it), Argon2id, `authd`,
`passwd` and `authctl`, gated by `cargo xtask test-auth` on all three
architectures (2026-09-27), and hyprlock over `authd` (P1.5, 2026-10-03).
Phase 2, all landed on 2026-10-03: `--everything`'s desktop runs as `ferrix`
under `sessiond` (P2.4), hyprix unlocks only on `authd`'s grant (P2.5),
`login` on the console (P2.3), a session that ends with its compositor
(P2.7), `su` for `wheel` (P2.6), and `/dev/tty` as the caller's own
terminal. On 2026-10-04 getty began revoking the console before every login
(0f94a6d1a, landed in batch 22384874f), so a program left by one login
reads nothing of the next one's password. K-B (P2.1), NP, landed the same night
(7e9a2806f): `/proc`'s private links are decided by `ptrace_may_access`,
dumpability included, and `/proc/<pid>/fdinfo` exists (`docs/handover/2026-10-04-np.md`).
Left: K-C (P2.2) is not started;
the other desktop images still run as root; and ending a user's processes
at logout is the customer's call.

Stage 13 is under way, cgroups first because init needs them: cgroup2 with
`pids`, `memory` and its scoped OOM kill, and `cpu.weight` (2026-09-26);
mount, user, UTS, IPC, cgroup and pid namespaces, seccomp's checker and
hook (S1, S2), and network namespaces (2026-10-04, 22384874f) are in.
Seccomp filters (S3) landed on 2026-10-04 (248799bdd: `seccomp(2)`, `prctl(PR_SET_SECCOMP)`, chains per thread);
reclaim, freezing, `cpu.max`, `io`, time namespaces and S4 to S6 are on
branches (`stage-13-handover.md`).

Chrome runs on Ferrix (2026-09-24): Google's prebuilt Chrome for Testing,
headless and in a window on the compositor, on x86-64, and both on ferrousli's
loader and C library in glibc's place (2026-09-26). Since 2026-09-26 it runs
with its zygote, idles at 13% of a processor where it took 443%, turns a box
at 60 frames a second where it managed 1.5, and plays sound (*Chrome*, after
sysfs, and `docs/CHROME.md`).

Sound is a ring-3 virtio-snd driver, an audio core in the kernel and
`/dev/snd`, gated by `cargo xtask test-audio` on x86-64 and AArch64
(`docs/AUDIO.md`, 2026-09-26), with alsa-lib on ferrousli and `pulsed`, a
PulseAudio-protocol server that mixes any number of streams, which Chrome
plays through on the desktop (2026-09-27).

An installer MVP puts Ferrix on a VM's own disk (`docs/INSTALLER.md`,
2026-09-28): `cargo xtask build --installer` carries `ferrix-install`, which
writes a GPT with an ESP and the btrfs root, and `cargo xtask test-install`
boots the installed disk alone through OVMF.

Networking is done: sockets, a net core and a ring-3 virtio-net driver, with
`curl` fetching over HTTPS and `git` cloning inside the guest.

sysfs is done (2026-09-24): `/sys` is a view of the devices as enumeration,
the ring-3 drivers' cores and `devmgr` describe them, and a driver unbound and
bound through it goes and comes back (`docs/SYSFS.md`).

Stages 17 and 18 are met, and the compositor runs: `cargo xtask
test-compositor` boots it as init on Ferrix, and two Wayland clients tile on
the card, pixel for pixel as the renderer draws them, on x86-64, AArch64 and,
since 2026-09-23, ARMv7-A.

Stage 19 is under way, and the GPU path chosen on 2026-09-18 is built: the
desktop composites on the GPU through `/dev/dri/renderD128`, and the screen is
shown the very texture the compositor drew into, which takes a 1920x1080 frame
of a video wallpaper behind a blurred translucent terminal from 39 ms in
software to 12 (`docs/GPU.md` §3.7 and §3.8; 60 fps is 16.7).

Stage 19's X server is yserver (`docs/YSERVER.md`, 36 points against the
stage's 40 for XWayland, done 2026-09-29). What the stage still owes is
client pages as texture backing, `dwindle:precise_mouse_move` and the
second-pass effects; Mesa and `zwp_linux_dmabuf`, for clients that draw on
the GPU themselves, are priced beside it.

The desktop's own clients -- waybar, fuzzel, hyprlock and hypridle, written in
Rust -- are all on `main`. waybar and fuzzel are on `main` (2026-09-27):
`cargo xtask run-compositor --everything` boots the customer's own
`hyprland.conf`, dotfiles, fonts and monitor EDID, waybar draws their bar,
SUPER+R runs their launcher script into fuzzel, and the clipboard is shared
with the host's through xtask. hypridle is on `main` too (2026-09-26), its
`idle` boots passing on x86-64 and AArch64. hyprlock is on `main` too
(2026-10-03), its lock over `authd`. Since 2026-10-04 they, and term, are
apps in `ferrix-os/apps`, built by xtask from their folders (def906ba2), and
foot takes keys: every compositor image carries libxkbcommon's
`usr/share/X11/xkb`. `test-foot`'s check that a typed line reaches foot's
program runs and passes since f55e8ab28 (2026-10-04), which gives `test-foot` zinc as `/bin/sh`
for its keyboard check; before that the check never ran.

Stage 21 is bare metal with a card of Ferrix's own. NVIDIA's driver
already draws the desktop and Chrome's WebGL on the RTX 3060's own
monitor, through libvirt (on `main` since 2026-10-05; see its page). Stage 22 is
Steam,
whose 32-bit x86 ABI is under way (`docs/I386.md`): I1 to I4 are on `main`
(32-bit programs, their threads, signals and fork, Alpine's and Debian's i386
busybox), and I5a: Valve's `steamcmd` logs in to Steam, from `test-steamcmd`
and from the `--everything` desktop's terminal (2026-09-27). The Steam client
itself draws through yserver, a Rust X11 server, with a rootless Wayland
backend of Ferrix's own: its sign-in window is on hyprix since 2026-09-29,
and on the `--everything` desktop, its 64-bit side on ferrousli, it signs
in and shows its store (2026-09-30, gated by `test-steam-store`), with
launch-side workarounds listed in `docs/STEAM.md`. Ferrix also boots on the customer's Pixel
7: natively on all eight cores to `FERRIX-BOOT-OK stages 1-12`, and as a guest
of the phone's own crosvm from a launcher app, which shows a desktop in that
VM with Chromium on it (2026-09-27), and each release carries that desktop,
full and minimal (2026-09-28); during a native boot a USB serial port
streams the kernel log (2026-09-26). The kernel's certification
set (`docs/certification/`) closed F-23, F-31 and F-35 on 2026-09-26: fallible
allocation, side-channel defences with KASLR, and job quotas. On 2026-09-27
it closed F-21b with an audit record of the TSF's own decisions, claimed in the
Security Target; measured the certified item's statement coverage at 90.1%,
89.9% and 84.5% on x86-64, AArch64 and ARMv7-A; and traced 602 low-level
requirements, 396 of them verified by a named check. On 2026-10-05 the
register filed and closed F-62, a carry of the coverage evidence that nobody made: the gate now
fails on it, and the anchors two landings had skipped were carried again; it filed and
closed F-63 too, the queue from the chardev core to `nvrm` able to grow
without bound, which abandoned requests now leave and admission counts. The
register stands at 16 open and 48 closed, of 64. CI on `main` is green again (run
37331100176, after a rerun of a flaky job), after the Windows gateway test
`a_lost_segment_is_sent_again_alone` stopped racing the gateway's timer
(39e520e31); another gateway test, `resets_a_connection_to_a_port_nothing_listens_on`,
flaked once on Windows and has a row in `docs/BACKLOG.md`.

Stage 17's display iteration is done: `/dev/dri/card0` served by a ring-3
virtio-gpu driver, with `cargo xtask test-display` requiring a compositor's
colour pixel for pixel on all three architectures. Its input iteration is done
too, to the same standard: `/dev/input/eventN` served by a ring-3 virtio-input
driver and a kernel input core, with `cargo xtask test-input` sending a key
and a touch in at QEMU's far end over QMP and requiring them back out of the
nodes on all three architectures, and a negative control that must fail.
`epoll`, `eventfd` and `ioctl(FIONBIO)` are in the boot test, so the kernel
side of iteration 2's prerequisites is done, and `card0` has the primary plane
and `type` property Smithay's legacy path needs (E4). The compositor reads
those nodes now, so stage 17 is met: `cargo xtask test-seat` types into a
window on Ferrix from QEMU's far end. Each stage's section below says what
exists. The marker will not move until a stage meets its exit criterion.
