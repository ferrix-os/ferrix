# Where it stands, in full

*Reviewed 2026-09-27.* The short version is on the [overview](README.md).

## Where we left off (2026-09-27)

The fleet wound down on the afternoon of 2026-09-27 with everything finished
on `main` and pushed. Work that was not finished is on branches, pushed to
GitHub, each with a row in `docs/BACKLOG.md` and a handover file:

| Branch | What | Left |
|---|---|---|
| `boot-21b` | W-8 boot requirements, part b (root, init, power, random, F-51) | the consultant's diff review, a rebase with carry, then 21c (`devmgr`) |
| `w8-armv7a` | W-8 file 24, ARMv7-A and `arch/arm_common` (drafts) | write the SysML file against the code as F-48 and F-50 left it |
| `f46-power-off` | F-46, power-off gates that took a triple fault for a power-off | correct the message, rebase, gate, the consultant's OK |
| `hyprlock` | hyprlock over `authd` (AUTH P1.5) | rebase and its boots |

Not on a branch yet: F-49 (ARMv7-A `psci_system` not declaring `r12`
clobbered; designed in its BACKLOG row) and the `ferrix.devmgr=init` coverage
boot (one SUITE line).

**Next, in the customer's order:**

1. **Test and gate run time** (`docs/TEST-TIME.md`). Phase 1 measured it: an
   item gate takes 828 s at a load of 12 to 30, a compositor boot 31.9 s of
   which 12.6 s is the guest, and `test-compositor` over 1,107 s. The Arm
   firmware waits are cut (7 s a boot pair); the other cuts are ordered in
   that file. Targets: an item gate in 5 minutes, a desktop boot in 60 s, CI
   green within an hour.
2. **Steam's X server**: the yserver feasibility pass was met and the design
   approved on 2026-09-28 (`docs/YSERVER.md`), and Y1 to Y5 were done the
   same day: yserver runs as a client of hyprix, and X windows show on it,
   take its input, its sizes and its close, float as dialogs where they are
   transients, and open their menus as its popups. Y7 put yserver on the
   `--everything` desktop, and Y6 (2026-09-29) made X's clipboard and
   primary selection and hyprix's one, both ways, which finishes the
   design's 36 points.
   **Steam's sign-in window is on hyprix** (2026-09-29, `docs/STEAM.md`,
   `cargo xtask test-steam-window`): the client installs itself from the
   bootstrap and its browser helper draws through yserver, with launch-side
   workarounds each owned by a kernel or yserver fix now under way.
3. **W-8**: 21b, 21c and file 24, then the modules the traceability gate does
   not yet hold complete.
4. **The desktop**: fuzzel's second-press toggle, hyprlock P1.5, hypridle.

**Waiting on the customer** (`docs/BACKLOG.md`, *Waiting on the customer*):
F-43 (W^X for programs, or a narrower claim), the Common Criteria version
(F-52), FMT_SMF.1 and FMT_MTD.1, and whether
the customer's Python desktop scripts are rewritten for Ferrix. Hardware
confirmations (F-44 on the Pixel 7, F-48 and F-50 on the DK1) wait for the
product owner's word in daytime.

CI: the Miri job's six-hour overrun is fixed (cf357e94), and the two fuzz
crashes the night found are fixed (1bc64cc1, 3af00937); the first run to
show all three green had not finished at the wind-down.

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

Dynamic linking is done: Debian's glibc busybox runs on its own `ld-linux`,
and on ferrousli's loader and `libc.so.6` in glibc's place, on all three
architectures, since ferrousli itself was ported to AArch64 and ARMv7-A on
2026-09-23.

Stage 15 has job control and, since 2026-09-26, a real init: `/sbin/init` runs
services in cgroups of their own over `src/lib/init/svc`'s manager, with `svc` to
drive it, readiness, socket activation and resource limits, gives the console
a getty, and powers the machine off, on all three architectures (`cargo xtask
test-init`). `run` and the desktop boot it, and the compositor is its service.

Stage 15's next is authentication (`docs/AUTH.md`, approved by the customer on
2026-09-26): its kernel fix, P0, is in (a native process runs as the one that
made it), and phase 1 is `authd`, passwords and a real hyprlock, 27 points.

Stage 13 is under way, cgroups first because init needs them: cgroup2 with
`pids`, `memory` and its scoped OOM kill, and `cpu.weight` (2026-09-26);
reclaim, freezing, `cpu.max`, `io`, namespaces and seccomp are left. Of
the namespaces, Steam's user and mount ones are being built
(`docs/NAMESPACES.md`): per-mount flags (N1, 2026-09-28) and binds (N2,
2026-09-30) are in, mount namespaces themselves (N3) are next.

Chrome runs on Ferrix (2026-09-24): Google's prebuilt Chrome for Testing,
headless and in a window on the compositor, on x86-64, and both on ferrousli's
loader and C library in glibc's place (2026-09-26). Since 2026-09-26 it runs
with its zygote, idles at 13% of a processor where it took 443%, turns a box
at 60 frames a second where it managed 1.5, and plays sound (*Chrome*, after
sysfs, and `docs/CHROME.md`).

Sound is a ring-3 virtio-snd driver, an audio core in the kernel and
`/dev/snd`, gated by `cargo xtask test-audio` on x86-64 and AArch64
(`docs/AUDIO.md`, 2026-09-26).

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

What stage 19 still owes is XWayland, `dwindle:precise_mouse_move` and the
second-pass effects; Mesa and `zwp_linux_dmabuf`, for clients that draw on the
GPU themselves, are priced beside it.

The desktop's own clients -- waybar, fuzzel, hyprlock and hypridle, written in
Rust -- are half done. waybar and fuzzel are on `main` (2026-09-27):
`cargo xtask run-compositor --everything` boots the customer's own
`hyprland.conf`, dotfiles, fonts and monitor EDID, waybar draws their bar,
SUPER+R runs their launcher script into fuzzel, and the clipboard is shared
with the host's through xtask. hyprlock's lock over `authd` (P1.5) is parked
on branch `hyprlock`, and hypridle is not started.

Stage 21 is bare metal with a card of Ferrix's own, and stage 22 is Steam,
whose 32-bit x86 ABI is under way (`docs/I386.md`): I1 to I4 are on `main`
(32-bit programs, their threads, signals and fork, Alpine's and Debian's i386
busybox), and I5a: Valve's `steamcmd` logs in to Steam, from `test-steamcmd`
and from the `--everything` desktop's terminal (2026-09-27). The Steam client
itself draws through yserver, a Rust X11 server, with a rootless Wayland
backend of Ferrix's own: its sign-in window is on hyprix since 2026-09-29,
with launch-side workarounds listed in `docs/STEAM.md`. Ferrix also boots on the customer's Pixel
7: natively on all eight cores to `FERRIX-BOOT-OK stages 1-12`, and as a guest
of the phone's own crosvm from a launcher app, which shows a desktop in that
VM with Chromium on it (2026-09-27); during a native boot a USB serial port
streams the kernel log (2026-09-26). The kernel's certification
set (`docs/certification/`) closed F-23, F-31 and F-35 on 2026-09-26: fallible
allocation, side-channel defences with KASLR, and job quotas. On 2026-09-27
it closed F-21b with an audit record of the TSF's own decisions, claimed in the
Security Target; measured the certified item's statement coverage at 90.1%,
89.9% and 84.5% on x86-64, AArch64 and ARMv7-A; and traced 602 low-level
requirements, 396 of them verified by a named check. Its register stands at 15
findings open and 39 closed, of 54.

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
