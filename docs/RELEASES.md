# Ferrix — releases

These are snapshots of older Ferrix milestones, newest first. Each one records
what worked and what was still missing at the time. For the current state,
read [the roadmap](roadmap/where-it-stands.md).

Milestone tags are annotated and checked by the project owner before they are
published. The release workflow uses the matching section from this file as
its notes, and attaches the Pixel 7 app's desktop in its two editions, which
the app updates the phone from ([tools/vendor/google/pixel7/README.md](../tools/vendor/google/pixel7/README.md#updates)),
and the x86-64 desktop for plain QEMU, `ferrix-x86_64-desktop.zip`, which
[`tools/common/release/package-desktop.py`](../tools/common/release/package-desktop.py)
builds, with each image's licence inventory and the corresponding sources of
every GPL and LGPL program in them ([LICENSES.md](LICENSES.md)). Every
release also carries the desktop's wallpaper, `ferrix-wallpaper-1920x1080.png`,
`-2560x1440.png` and `-3840x2160.png`, which the desktop images show
(`/bin/wallpaper` on every screen) and
[`gen-brand-images.py wallpaper`](../tools/common/gen/gen-brand-images.py)
draws into [`docs/brand/wallpaper/`](brand/wallpaper/).

Before a release tag is pushed, the workflow runs once as a dry run for the
commit to be tagged: `gh workflow run release.yml -f dry_run=true -f
ref=<commit>` builds the three assets exactly as the tag would and keeps
them as the run's artifacts, with no release made. The assets are built
from pinned downloads, and their hosts can treat GitHub's runners as no
other machine: v0.1.0's first run built none of its assets, because
gitlab.alpinelinux.org, where busybox's config comes from, answers the
runners 418. Each pinned download that such a host serves names a mirror
too, which is checked against the same pin. See the [discovery plan](marketing/PLAYBOOK.md#what-to-do-next-in-order)
for how future releases should be presented.

## v0.2.0 (2026-10-27), the first live ISO

A live ISO you can boot: Ferrix's desktop, Yoru, starts from memory, with
an installer that puts Ferrix on a disk of its own. Download
`ferrix-v0.2.0-x86_64.iso` below, check it against `SHA256SUMS`, and follow
[Try Ferrix](https://github.com/ferrix-os/ferrix/blob/develop/docs/TRY-FERRIX.md):
it has the QEMU command line, how to write a USB stick, and what the
installer asks.

**This is a development preview, not a finished system.** The image is
built from the `present` profile with slot signing off, so it carries no
signature a loader checks, and it is not signed for Secure Boot: turn
Secure Boot off to start it. Parts of authentication and process isolation
are still being finished. Do not put anything on it that you cannot lose.

What is in it:

- **A live session.** The ISO boots under UEFI as a CD and, written with
  `dd` or Rufus's DD mode, as a USB stick or a virtual disk: El Torito and
  a GUID partition table over the same EFI system partition. `/` is in
  memory and the session logs in as `ferrix` with no password; nothing on
  the machine's disks is touched.
- **Yoru on hyprix**: Ferrix's own Wayland compositor, with the dock,
  sidebars, launcher and wallpaper picker in the Nocturne design; a
  terminal running zinc, the zsh-compatible shell; Settings, the Files app
  (shelf), `btop`, `curl`, `git`, `foot`, `ferrofetch` and the rest of the
  apps a desktop image carries.
- **Install Ferrix**, a graphical installer in the style of Ubuntu's:
  language, keyboard, the disk -- erase it whole, with a warning in red, or
  install into free space or one partition you choose, leaving the others
  alone --, your account, the time zone, a summary, progress, and restart.
  The installed system starts with your session locked behind your
  password. It is gated end to end in QEMU: installed onto a blank virtual
  disk, booted alone, the account and password the installer was given.
- **Drivers outside the kernel**: disk, network, graphics, input and
  console drivers run as processes behind the IOMMU and are restarted when
  they crash.
- **Licences and sources**: `THIRD_PARTY_LICENSES.MD` in the ISO (and
  `LICENSES.MD` on its EFI system partition, and the asset
  `ferrix-v0.2.0-x86_64-iso-THIRD-PARTY-LICENSES.md`) lists every part of
  the image, its licence and the licence's text;
  `ferrix-v0.2.0-sources.tar.gz` holds the exact source of every GPL and
  LGPL program in it (busybox, git, alsa-utils, alsa-lib). The ISO carries
  no firmware, no Google Chrome and no NVIDIA software, and no artwork but
  Ferrix's own.

Known to work:

- **QEMU/KVM with virtio devices and an Intel IOMMU** (`intel-iommu`, and
  `iommu_platform=on` on the virtio devices), as Try Ferrix shows: the live
  desktop, and installing onto a virtio disk.
- **The Intel N150 mini PC** Ferrix is developed on: its Intel graphics,
  its AHCI SATA disk and its Motorcomm YT6801 Ethernet.
- **DMA and interrupt isolation** through Intel VT-d and AMD-Vi.
- **NVIDIA graphics only through the virtual-machine path**, a GPU passed
  to a VM, not on a PC's own NVIDIA card.

What does not, yet:

- **Little bare-metal coverage.** Ferrix has run on very few PCs. Expect
  firmware, graphics or input it does not know.
- **An IOMMU is required.** Ferrix starts a driver only for a device the
  IOMMU isolates, and this image does not turn that off (the `present`
  profile does not permit `ferrix.unisolated` on x86-64): on a machine or
  a virtual machine without VT-d or AMD-Vi, no driver starts and the screen
  stays empty. VirtualBox and VMware are untested.
- **No Wi-Fi** and no Bluetooth; AMD graphics are not driven.
- **Installing needs the medium as a disk Ferrix drives.** In a virtual
  machine attach the ISO as a virtio disk; booted as a CD, or from a USB
  stick on a PC, the live session works but the installer cannot read the
  system to copy (Ferrix has no CD driver and does not serve USB disks yet).
- The installer does not resize another system's partition, write a
  firmware boot entry, or keep an MBR disk's partitions.
- English only.

What to check:

```
sha256sum -c SHA256SUMS
cargo xtask iso --arch x86_64                     # the same ISO from source
cargo xtask test-iso --arch x86_64                # boots it as a CD and a USB stick
cargo xtask test-iso-install --arch x86_64        # installs it, boots the disk
```

## v0.1.1 (2026-10-21), features at 336e03bca

The first release with its downloads: the x86-64 desktop as one zip,
booted by a script with plain QEMU. v0.1.0 was tagged but built none of its
assets (see below). Download `ferrix-x86_64-desktop.zip` below, unpack it,
and run

```
sh ferrix-x86_64-desktop/run.sh                                                  # Linux, macOS
powershell -ExecutionPolicy Bypass -File ferrix-x86_64-desktop\run.ps1           # Windows
```

The [install guide](https://github.com/ferrix-os/ferrix/blob/main/docs/INSTALL.md)
walks through it step by step, with pictures, on Windows and Linux. The
script uses KVM, WHPX or HVF where it can and QEMU's emulator otherwise,
and finds the UEFI firmware QEMU or the distribution installed.

Since v0.1.0:

- **The release builds on GitHub's runners.** Every pinned download names a
  mirror, checked against the same pin, and the workflow has a dry run that
  builds all three assets for a commit before it is tagged; this tag's
  commit passed one.
- **Ferrix's own DHCP client.** `dhcpc`, in Rust, takes the address,
  route and DNS servers and keeps the lease renewed, where busybox's
  `udhcpc` and a shell script did; busybox itself is still in the image.
- **Fixed:** a boot self-check of the character-device core (stage 10) that
  failed about one boot in ten under load: it measured the queue's room
  before earlier requests had left it.

What it has:

- **In the zip:** the hyprix Wayland compositor with a terminal running
  zinc, the zsh-compatible shell; ferrousli's busybox; and the apps an
  image carries by default, among them `btop`, `curl`, `git`, `foot`,
  `fuzzel`, `waybar` and `ferrofetch`. The network is QEMU's own, so
  `curl https://example.com` works from the first prompt.
- **Linux programs:** Chrome, `rustc`, `git`, `curl` and Valve's
  `steamcmd` run unpatched on Ferrix's own Rust kernel, 32-bit x86 programs
  included; Steam signs in and starts a native game; Claude Code runs a
  session on the desktop. Those that need a volume of their own are in a
  build from source (`cargo xtask run-compositor --everything`), not in the
  zip.
- **Drivers outside the kernel:** disk, network, graphics, input, sound and
  console drivers run as processes, behind the IOMMU on x86-64 and AArch64,
  and are restarted when they crash.
- **Users and storage:** passwords checked by `authd` as Argon2id hashes; a
  btrfs root that keeps what is written to it, a command-line installer
  (`ferrix-install`, to virtual disks so far: no graphical installer and no
  live medium yet), and A/B system slots with a way back.
- **Three architectures** in the boot tests, and Ferrix has booted on an
  STM32MP157D-DK1 and a Pixel 7.
- **Licences and sources:** `ferrix-v0.1.1-THIRD-PARTY-LICENSES.md` (also
  in the zip) lists every part of the x86-64 image, the licence it is
  shipped under and every licence text, and each Pixel 7 edition has its own
  (`ferrix-pixel7-<edition>-THIRD-PARTY-LICENSES.md`). `ferrix-v0.1.1-sources.tar.gz`
  holds the exact upstream source, build script, patches and config of
  every GPL and LGPL program in these images (busybox, git, alsa-utils,
  alsa-lib) and of what is linked into one, and the Debian package list
  of the Pixel 7 Chromium volume with where each package's source is
  ([docs/LICENSES.md](https://github.com/ferrix-os/ferrix/blob/main/docs/LICENSES.md)).

Known limits:

- The zip leaves out Chrome, Steam and the Rust toolchain, which run from
  volumes of several gigabytes that a build fetches; `/` is in memory, so
  nothing survives QEMU exiting; the screen is drawn in software, with no
  host GPU; the keyboard layout is US; there is no sound card.
- With released QEMU under WHPX the guest gets one processor: QEMU 11.1's
  WHPX faults Ferrix's ring-3 drivers with more.
  `tools/common/fetch/fetch-qemu-windows.sh` builds one that does not.
- QEMU's window can open at 640x480 on Windows; make it bigger and the
  desktop follows.
- On an Apple Silicon Mac the x86-64 guest is emulated and slow.
- The zip's desktop runs as root with no login. Ferrix is not ready to be
  anybody's everyday OS: parts of authentication and process isolation are
  unfinished.

What to test:

```
sh ferrix-x86_64-desktop/run.sh --vnc 5        # no window: VNC on 127.0.0.1:5905
cargo xtask run-compositor --arch x86_64        # the same desktop from source
cargo xtask test-boot --arch all
```

## v0.1.0 (2026-10-20), features at fb56e0cd5

Tagged, but published with no downloads: its release run built none of the
three assets, because a host of one pinned download refuses GitHub's
runners. v0.1.1 is the same system with the downloads.

The first numbered release, and the first one anybody can boot without
building it: the x86-64 desktop as one zip, booted by a script with plain
QEMU. Download `ferrix-x86_64-desktop.zip` below, unpack it, and run

```
sh ferrix-x86_64-desktop/run.sh                                                  # Linux, macOS
powershell -ExecutionPolicy Bypass -File ferrix-x86_64-desktop\run.ps1           # Windows
```

The script uses KVM, WHPX or HVF where it can and QEMU's emulator
otherwise, and finds the UEFI firmware QEMU or the distribution installed.
Checked before the tag on Ubuntu with its own QEMU 10.2.1 and OVMF, under
KVM and emulated, and on Windows 11 with QEMU 11.1.0 under WHPX: the
compositor's `hyprix: card0` line came within seconds of the loader in
each.

- **In the zip:** the hyprix Wayland compositor with a terminal running
  zinc, the zsh-compatible shell; ferrousli's busybox; and the apps an
  image carries by default, among them `btop`, `curl`, `git`, `foot`,
  `fuzzel`, `waybar` and `ferrofetch`. The network is QEMU's own, so
  `curl https://example.com` works from the first prompt.
- **Linux programs:** Chrome, `rustc`, `git`, `curl` and Valve's
  `steamcmd` run unpatched on Ferrix's own Rust kernel, 32-bit x86 programs
  included; Steam signs in and starts a native game; Claude Code runs a
  session on the desktop. Those that need a volume of their own are in a
  build from source (`cargo xtask run-compositor --everything`), not in the
  zip.
- **Drivers outside the kernel:** disk, network, graphics, input, sound and
  console drivers run as processes, behind the IOMMU on x86-64 and AArch64,
  and are restarted when they crash.
- **Users and storage:** passwords checked by `authd` as Argon2id hashes; a
  btrfs root that keeps what is written to it, a command-line installer
  (`ferrix-install`, to virtual disks so far: no graphical installer and no
  live medium yet), and A/B system
  slots with a way back.
- **Three architectures** in the boot tests, and Ferrix has booted on an
  STM32MP157D-DK1 and a Pixel 7.
- **Licences and sources:** `ferrix-v0.1.0-THIRD-PARTY-LICENSES.md` (also
  in the zip) lists every part of the x86-64 image, the licence it is
  shipped under and every licence text, and each Pixel 7 edition has its own
  (`ferrix-pixel7-<edition>-THIRD-PARTY-LICENSES.md`). `ferrix-v0.1.0-sources.tar.gz`
  holds the exact upstream source, build script, patches and config of
  every GPL and LGPL program in these images (busybox, git, alsa-utils,
  alsa-lib) and of what is linked into one, and the Debian package list
  of the Pixel 7 Chromium volume with where each package's source is
  ([docs/LICENSES.md](https://github.com/ferrix-os/ferrix/blob/main/docs/LICENSES.md)).

Known limits:

- The zip leaves out Chrome, Steam and the Rust toolchain, which run from
  volumes of several gigabytes that a build fetches; `/` is in memory, so
  nothing survives QEMU exiting; the screen is drawn in software, with no
  host GPU; the keyboard layout is US; there is no sound card.
- With released QEMU under WHPX the guest gets one processor: QEMU 11.1's
  WHPX faults Ferrix's ring-3 drivers with more.
  `tools/common/fetch/fetch-qemu-windows.sh` builds one that does not.
- On an Apple Silicon Mac the x86-64 guest is emulated and slow.
- The zip's desktop runs as root with no login. Ferrix is not ready to be
  anybody's everyday OS: parts of authentication and process isolation are
  unfinished.

What to test:

```
sh ferrix-x86_64-desktop/run.sh --vnc 5        # no window: VNC on 127.0.0.1:5905
cargo xtask run-compositor --arch x86_64        # the same desktop from source
cargo xtask test-boot --arch all
```

## stage-11.1-network-display-and-threads — 2026-09-16, features at a89aeb25

Two days of the fleet landing on `main` directly: a network that works, a
colour on the screen from a Linux program, threads finished, ferrousli
most of the way through POSIX.1-2024, and the first ports.

- **Networking, exit met.** A net core beside the block core: `AF_INET` and
  `AF_INET6` TCP and UDP, raw IPv4 and IPv6 sockets (`ping`, `ping6`),
  `AF_PACKET` (`udhcpc` gets a lease), `AF_NETLINK` route sockets (`ip`
  configures `eth0`), `/proc/net` rendered as Linux prints it, and a
  virtio-net driver in ring 3 started by `devmgr`. `xtask` carries its own
  gateway over loopback UDP, so `run --net` reaches the network on every
  host, Windows included, and `cargo xtask test-net` runs thirteen programs
  on all three architectures, hermetically.
- **`AF_UNIX`** sockets with path and abstract names and descriptor passing
  over `SCM_RIGHTS`; Wayland's transport.
- **Threads, done.** Signals, stops and `execve` across a process's threads,
  the futex lock kind with the `munmap`/`brk` races closed,
  `/proc/<pid>/task`, and a Rust `std::thread`/`Mutex`/`mpsc` program
  printing `threads: all ok` on all three architectures under
  `cargo xtask test-threads`. A static PIE loads at a base of its own, so
  `rustc`'s default x86-64 binary runs unchanged.
- **Files and users.** `memfd_create` with seals; file permissions checked
  against the caller's credentials, ownership of what a process makes, the
  root-only calls refused to others, and set-user-id programs running as
  their owner.
- **Display, iteration 1.** A display core takes a ring-3 virtio-gpu
  driver's card and publishes `/dev/dri/card0` with the legacy DRM subset a
  software compositor uses; `cargo xtask run --arch x86_64 --display --init
  blank` puts a colour on the screen from a Linux program, checked pixel by
  pixel on x86-64 and AArch64. The firmware framebuffer stays a panic's.
- **The event loop's calls.** `epoll` (level, edge, one-shot, a set inside a
  set), `eventfd2`, and `ioctl(FIONBIO)` on every file: iteration 2's kernel side, done. Input is designed in `docs/INPUT.md` as evdev over a
  ring-3 virtio-input driver, with the ABI probe and the virtio-input
  protocol landed.
- **The compositor's first crates:** the `hyprland.conf` parser and
  Hyprland's layout and dispatcher core, host-tested and fuzzed.
- **ferrousli** answers 996 of POSIX.1-2024's 1243 interfaces (247 missing, none
  stubbed): the whole double and float `<math.h>`, `<fenv.h>`, regex and
  libgen, barriers, semaphores, scheduling calls and C11 `<threads.h>`,
  `<search.h>`, `glob`, the wide-character I/O families, name resolution
  and the netdb databases, atomics, `assert.h`. Its busybox is what every
  gate runs, rebuilt by `cargo xtask busybox` when stale, natively on
  Windows too.
- **zinc,** the zsh-compatible shell in Rust, is in every initramfs with a
  line editor and tab completion, runs zsh scripts and sources oh-my-zsh's setup, and completes commands, files, parameters and users the zsh way (`/bin/zinc`, also `/bin/zsh`).
- **Ports:** curl (HTTP, with mbedTLS built in) and btop, both built against
  ferrousli, are in every x86-64 image: fetch files by name or byte-exact,
  and watch CPU, memory, network and processes live on the console.
  Programs over 4 MiB now start, `/proc/<pid>/mounts` exists, and
  ferrousli gained thread cancellation and what libc++ needs. HTTPS is
  not usable yet: it waits on the firmware clock and a seeded `getrandom`.
- **Windows parity:** `xtask` on Windows runs `run --net`, `run` under WHPX,
  the ferrousli gate and ARMv7-A U-Boot through WSL, `watch-serial` and
  `flash`; gates for landings run on the Linux host over ssh.
- **Decisions of record:** the customer holds the product-owner seat and
  landings go to `main` directly at stable points, each gated on the Linux
  host; the architecture stays what it is, with dynamic linking staged
  behind it; input is evdev over virtio-input; stage 17 is 74 points, 18 is
  96.

Known limits, each with a backlog row:

- `poll`, `ppoll` and `epoll` waits recheck every 5 ms rather than waking on
  the event; `EPOLLRDHUP` and `EPOLLPRI` are never reported; a zero-length
  `eventfd` write returns 0, not `EINVAL`.
- `AF_UNIX`: a socket passed over its own connection is never freed (no
  cycle pass); `SCM_CREDENTIALS` and `SO_PASSCRED` are `EOPNOTSUPP`;
  `MSG_PEEK` on a message carrying descriptors installs none; `sendto`
  with an address on a datagram socket is `EOPNOTSUPP`.
- Threads: `set*id` changes credentials for every thread, a written
  deviation; a thread's `comm` is its process's; the two-last-threads
  exit check does not provably race.
- `card0` is legacy DRM only, one exclusive master, no atomic commit, no
  planes or properties yet (E4), no input devices yet (input L3–L7).
- `timerfd` and `signalfd` do not exist; dynamic linking (`PT_INTERP`) does
  not; the display does not run on ARMv7-A's QEMU machine.
- Stage 10's VT-d stale-record intermittent (P0, reproduced once under
  load) and BAR trust are open; FX-0701, a stop across threads, has a lead.
- The musl busybox's `su` has never worked, and is not a regression; its
  `nc` has no `-U`. The ferrousli busybox is the one to use.

What to test:

```
cargo xtask test-net --arch x86_64 --init ferrousli
cargo xtask run --arch x86_64 --net --init ferrousli
cargo xtask run --arch x86_64 --display --init blank
cargo xtask test-threads --arch all
cargo xtask test-vfs --arch aarch64 --init ferrousli
```

## stage-11-ring-3-disk-and-btrfs — 2026-09-14

Stages 10 and 11 done: a user-mode disk driver behind the IOMMU, and btrfs
read through it; threads begun; ferrousli's busybox as the userland.

- Stage 10's exit: `/sbin/blk`, a virtio-blk driver running in ring 3 on the
  native runtime, serves the disk through the block ring with VT-d (x86-64)
  and the SMMUv3 (AArch64) translating its DMA, a deliberate out-of-domain
  write faults, and a driver's death resets its device before any pinned
  frame is freed. ARMv7-A runs its driver in degraded trusted mode, as
  decided. `FERRIX-BOOT-OK stages 1-10`.
- Stage 11's exit: an image made by real `mkfs.btrfs` is mounted read-only
  at `/mnt` through that driver and 101 files, 17 directories and a link
  read back byte for byte on all three architectures, with every data sector
  checked against the checksum tree and the default subvolume honoured.
  `FERRIX-BOOT-OK stages 1-11`.
- The page cache is the inode's VMO on tmpfs and btrfs alike: files fill
  from a page source, and `mmap` of a file with `MAP_SHARED` maps those pages
  — writes go both ways, `msync` answers, a touch past the end is `SIGBUS`.
  The VMO reverse map takes a VMO's pages out of every space that maps them
  before their frames go back, shooting down only where they may be cached.
- Threads have begun: every task has a thread with signal state of its own,
  `exit` ends a thread apart from `exit_group`, and `clone(CLONE_THREAD)` is
  on `develop` running ferrousli's pthread test.
- Native process creation: `process_create` and `process_start`, `vmo_map`,
  process observers, and a start argument on entry; `devmgr`'s kernel half.
- `SA_RESTART`, pid 1 with orphans reparented, a program killed by its own
  fault no longer panics the kernel, `uname -s` says `Ferrix`.
- All three architectures take console input by interrupt (x86-64's COM1
  through the I/O APIC); ARMv7-A boots with 2 GiB; `ferrix.onexit=reset`
  returns the DK1 to U-Boot with no hand at it — proven on the board, with
  the flash-from-Windows procedure in the board guide.
- Every frame-counted self-check nets heap pages against free frames and
  names the route of any mismatch; the interrupt line frees at its last
  handle's close; the wake check counts wakes rather than timing them.
- ferrousli: its busybox links (from 154 undefined symbols to none) and is
  the primary busybox the gates run — `test-shell` and `test-vfs` pass with
  it on Ferrix, with the musl and glibc busyboxes kept as compatibility
  checks. `cargo xtask busybox` builds it; `--init ferrousli` runs it.
- Decisions of record: POSIX.1-2024 compatibility is a goal, Linux winning
  where they differ; the goal after `rustc` is a Hyprland-shaped compositor
  in Rust (stages 17–19); estimates are story points.
- Known limits: no `AF_UNIX` sockets yet (landing 1 of 3 is verified on
  `develop`), no `MAP_PRIVATE` file mappings, no `devmgr` program (the boot
  check starts the driver), one open lead on ARMv7-A at one processor.

What to test:

```
cargo xtask busybox
cargo xtask run --arch x86_64 --init ferrousli
cargo xtask test-vfs --arch x86_64 --init ferrousli
cargo xtask test-boot --arch armv7a --memory 2048 --smp 2
```

## stage-9.1-console-and-iommu — 2026-09-13, commit 5fb365f

Arm console input, translated IOMMU domains, and the day's intermittent
failures fixed.

- Typed input works on the Arm machines: the PL011 and STM32 USART receive,
  proven on the STM32MP157D-DK1 (the first keystroke read on hardware), and
  the console drains before power-off so the last line arrives whole.
- glibc programs run as init: `/proc/self/exe` is absolute, so Ubuntu's
  static busybox reaches its prompt
  (`FERRIX_INIT=/usr/bin/busybox cargo xtask run --arch x86_64`).
- Stage 10: VT-d on x86-64 and SMMUv3 on AArch64 translate every device's
  DMA, an out-of-domain write is caught by the unit's fault record, a driver
  can pin VMO pages for its device, and IOMMU waits run with interrupts on.
- Stage 11, host side: btrfs reads verify every data sector against the
  checksum tree, honour the default subvolume and parse `INODE_EXTREF`.
- Fixed: unmount left a dentry per mount cycle (stage 8 frame-count
  failures); frame counts raced the reaper (stages 6 and 8); the preemption
  count could be raised on one processor and lowered on another (FX-0503); a
  new task was not charged before an insert (FX-0701); frame 0 was handed out
  under OVMF (a root-table panic under KVM).
- ferrousli is on the build path: `cargo xtask check --ferrousli` runs its
  gates, CI runs them, and busybox builds against it from pinned sources (the
  link still wants 154 functions).
- Verified on the STM32MP157D-DK1 on 2026-09-13, 21:25–21:31, flashed by hand
  from Windows (U-Boot `ums`, the three files checked by hash): stages 1–9,
  the console receiving by interrupt 84, `test-shell`'s script typed line by
  line, a 1000-character paste arriving whole, and the final line draining
  whole before power-off.
- Known limits: no threads, no file-backed `mmap`, no ring-3 disk driver yet;
  the interrupt-driven console and reboot-to-U-Boot are on `develop`.

What to test:

```
cargo xtask run --arch armv7a --init '~/.local/share/ferrix/busybox/{arch}/bin/busybox.static'
FERRIX_INIT=/usr/bin/busybox cargo xtask run --arch x86_64
```

Type at the `ferrix#` prompt on the Arm machine; the second starts Ubuntu's
glibc busybox as init on x86-64.

## stage-9 — 2026-09-13, commit 80d0ea7

Stages 1–9 done, every known stage 5 and 6 intermittent failure fixed.

- Boots from UEFI firmware (EDK2, U-Boot) into a Rust kernel on x86-64,
  AArch64 and ARMv7-A; the boot marker reads `FERRIX-BOOT-OK stages 1-9`.
- Memory: buddy allocator, slab heap, guard-paged vmap arena, demand paging,
  copy-on-write fork, W^X sweep, boot memory reclaimed.
- Traps, interrupts and time on every architecture; SMP with every vCPU
  online, IPIs, TLB shootdown, grace periods.
- EEVDF fair scheduler with load tracking, placement, affinity and balancing.
- User mode and the Linux syscall ABI: `fork`, `vfork`, `execve`, `wait4`,
  pipes, `futex`, signal delivery with `sigreturn`, a terminal with job control
  and Ctrl-C, about 150 calls answered.
- Static musl busybox runs a script and an interactive shell on all three
  architectures.
- On hardware, verified after the tag at `fd4442e`: an STM32MP157D-DK1 at two
  cores reaches `FERRIX-BOOT-OK stages 1-9` and runs `test-shell`'s script.
  Known defect: the last console line before power-off is cut, because the
  STM32 USART driver does not wait for its transmitter to drain.
- VFS with tmpfs, devfs, procfs and initramfs; `ls -R /proc`,
  `cat /proc/self/maps` and a forking shell script pass; `top`, `ps`, `mpstat`
  run.
- Native ABI: handles, channels with handle passing, ports, jobs, VMOs,
  `Interrupt` and `IoMapping` objects for ring-3 drivers.
- Stage 10 under way: PCI enumeration, device nodes, MSI-X vectors, virtio-rng
  driven by DMA with an MSI-X completion, VT-d translating on x86-64.
- Known limits: no threads, no file-backed `mmap`, no disk yet, console input
  only on x86-64.

What to test:

```
cargo xtask test-boot  --arch all
cargo xtask test-shell --arch all --init '~/.local/share/ferrix/busybox/{arch}/bin/busybox.static'
cargo xtask test-vfs   --arch all --init '~/.local/share/ferrix/busybox/{arch}/bin/busybox.static'
FERRIX_INIT=~/.local/share/ferrix/busybox/x86_64/bin/busybox.static cargo xtask run --arch x86_64
```

The last gives an interactive busybox shell on the serial console; type after
the prompt appears.
