# Layout

Where everything in this repository lives, and where a new thing goes. The
top level has four directories: the source of everything that runs on
Ferrix and the tests around it, the host tools that build and drive it, the
documents, and the data files the image ships.

```
src/         everything that runs on Ferrix, and its tests
  boot/        loaders: what runs before the kernel
  kernel/      the kernel
  lib/         host-testable logic, grouped by layer
  user/        programs that run in ring 3
    system/      first party: what Ferrix needs to be useful
      native/      on Ferrix's own ABI: runtime, devmgr, drivers
      linux/       on the Linux ABI, each its own cargo workspace
    apps/        optional programs, each a folder of its own that xtask finds
  tests/       test programs and fuzzing that live outside any one crate
tools/       everything that runs on the host
  common/      the build driver, gates, generators, fetchers
  vendor/      host tools for one vendor's hardware
docs/        design documents, the roadmap, the SysML model, certification,
             the brand, the website
assets/      data files that ship in the image
```

Hardware-specific parts go under `vendor/<vendor>/<device>/` wherever a
directory has them (`src/boot/`, `tools/`, `docs/`), and everything else
under `common/` beside it. The vendor is the prefix of the chip's
device-tree `compatible` (`google`, `st`). Drivers are the exception: they
are grouped by what they drive, as in Linux's `drivers/`, whoever made the
chip.

The root `Cargo.toml` is one workspace: `src/boot/`, `src/kernel/`,
`src/lib/`, `src/user/system/native/` and `tools/common/xtask/`. Everything under
`src/user/system/linux/`, `src/tests/` and `tools/vendor/` is a workspace of its own,
built by xtask from inside its directory.

## `src/boot/`

| Path | What |
|---|---|
| `src/boot/common/uefi/` | The UEFI loader (`ferrix-boot`) for x86-64, AArch64 and ARMv7-A. Reads the kernel, builds the address space, leaves firmware. |
| `src/boot/vendor/google/pixel7/` | The second-stage loader the Pixel 7's Android bootloader starts, and `mkbootimg.py`. |

Only `src/boot/*` and `src/kernel/` are freestanding;
`tools/common/xtask/src/workspace.rs` holds that list.

## `src/kernel/`

The kernel crate. Its source paths are load-bearing: the certification
evidence (`docs/certification/coverage-*.json`) and the baselines in
`tools/common/data/` anchor to `src/kernel/src/**` by file and line, so a file moved
inside `src/kernel/src` means regenerating that evidence, and every file has a
ring in `tools/common/data/certification-item.json`.

| Directory | Holds |
|---|---|
| `src/kernel/src/arch/<isa>/` | What one instruction set architecture defines: entry and traps, context switch, signal frames, bringing up the other processors, speculation defences, the architected timer, and the interrupt controller a core reaches through system registers (`aarch64/gic/`, x86's APIC) |
| `src/kernel/src/arch/arm_common/` | Drivers for Arm peripherals both Arm architectures can have: the GICv2, the PL011 and the STM32MP1's USART |
| `src/kernel/src/platform/<vendor>/<soc>/` | The kernel's part in one system on chip's devices, found in the device tree at boot: `google/gs201` (the Pixel 7) and `st/stm32mp1` (the DK1), the vendor being the prefix of the chip's device-tree `compatible`. Declared inline in `main.rs`, and each file has its own ring |
| `src/kernel/src/discovery/` | Finding what the machine has and handing it to ring 3: `acpi` and `fdt` reach the firmware's description of the machine, `pci` walks the buses it describes, and `devmgr` starts `devmgr` with what was found. The parsers are the host-testable crates in `src/lib/platform/`; what the kernel does for a running driver (device nodes, IOMMU domains, interrupts) stays outside |
| `src/kernel/src/interfaces/` | The kernel's end of each ring-3 driver's protocol, not a driver: `block_ring`, `net_ring`, `display`, `render`, `input`, `audio`, `logctl`. Each checks its channel against the protocol crate in `src/lib/proto/` and publishes what the driver serves (a disk, a card, an event node). The drivers themselves are processes under `src/user/system/native/drivers/` |
| `src/kernel/src/` otherwise | Everything the architecture does not change: memory, scheduling, objects, system calls, filesystems |

`#[cfg(target_arch)]` appears only under `src/kernel/src/arch/`
(`tools/common/check/check-crate-layering.sh`, rule 4). So a driver that only one
architecture can compile lives under that architecture, and one under
`platform/` compiles everywhere and does nothing on a machine without its
chip. `#[path]` is not used: a module lives where its `mod` line says.

## `src/lib/`

Architecture-neutral, host-testable logic — the only code `cargo test`, Miri
and the fuzzers can reach, which is why it is kept out of `src/kernel/`.
`tools/common/check/check-crate-layering.sh` enforces that no lib depends on the
kernel or a loader. Each crate sits in exactly one group:

| Group | Holds | Crates |
|---|---|---|
| `src/lib/proto/` | The interfaces between components: the ABIs the kernel offers, the rings and control protocols it shares with ring-3 drivers, the loader hand-off | `linux-abi` `native-abi` `native` `bootinfo` `devmgr-proto` `blkring` `netring` `displayctl` `renderctl` `inputctl` `sndctl` `logctl` `auth-proto` `pkg` |
| `src/lib/kernel/` | Kernel-internal cores: memory, scheduling, synchronisation, objects, randomness, the process stack image, the vDSO, the panic screen | `frame` `heap` `kmem` `paging` `vma` `sched` `sync` `objects` `fallible` `crng` `seccomp` `ustack` `vdso` `qr` `fbtext` |
| `src/lib/platform/` | Parsers for what firmware and the boot medium hand over, and which of the two descriptions a machine is read by | `acpi` `fdt` `description` `pci` `elf` |
| `src/lib/fs/` | Storage and filesystems, including the text of the pseudo-filesystems | `vfs` `block` `btrfs` `btrfs-vfs` `btrfs-write` `cpio` `procfs` `sysfs` `cgroupfs` |
| `src/lib/network/` | The network stack | `net` `netwire` `nettcp` `netlink` |
| `src/lib/drivers/` | Device logic over an abstract transport, and the serve loops ring-3 drivers run, grouped by function (see below) | `virtio` and one directory per function |
| `src/lib/init/` | The service manager's pure core, the restart policy it shares with `devmgr`, and its wire formats | `svc` `restart` `svc-proto` |
| `src/lib/crypto/` | Cryptography user space checks secrets with: the password hash `authd` stores credentials under (`docs/AUTH.md` §5.1). The kernel's random number generator is not here but in `src/lib/kernel/` | `argon2` |

**Drivers by function.** A driver is two crates: its logic in
`src/lib/drivers/<function>/<name>/` and its process in
`src/user/system/native/drivers/<function>/<name>/`, under the same function and,
where there is one of each, the same name. `virtio/` is the virtqueue core
every virtio driver shares.

| Function | Logic (`src/lib/drivers/…`) | Process (`src/user/system/native/drivers/…`) |
|---|---|---|
| `block/` | `virtio-blk`, `blkserve` | `virtio-blk` |
| `net/` | `virtio-net`, `netserve` | `virtio-net` |
| `display/` | `virtio-gpu`, `stm32-display` | `virtio-gpu`, `stm32-ltdc` |
| `gpu/` | `gc400` | `gc400` |
| `input/` | `virtio-input` | `virtio-input` |
| `usb/` | `usb-host`, `usb-device`, `dwc3` | `usbhid`, `usbdev` |
| `sound/` | `virtio-snd` | `virtio-snd` |
| `console/` | `virtio-console`, `vdagent` | `vport` |

`display/` puts pixels on a screen (`displayctl`), `gpu/` renders
(`renderctl`), as the protocols split them.

**Choosing a group for a new lib.** Ask what the crate *is*, not who uses it
first. A format two components agree on is `proto`, even if only the kernel
reads it today. Logic that drives a device is `drivers`. Something the
kernel alone needs and that is not storage, network or a device is `kernel`.
Add the crate to the root `Cargo.toml`'s `members` and
`[workspace.dependencies]`, and to the table above.

## `src/user/system/native/`

Programs that run in ring 3 on Ferrix's native ABI, built into the
initramfs by xtask:

| Path | What |
|---|---|
| `src/user/system/native/rt/` | The runtime every native program links: entry, the system-call instruction, exit, panic. |
| `src/user/system/native/driver/` | `ferrix-driver`, what every driver process shares: START, register blocks, DMA memory freed only after a reset, a bus's transport (`virtio`), and the protocol a subsystem speaks to its kernel interface (`input`, `block`). A driver implements its subsystem's trait and nothing else. virtio-input and virtio-blk are on it; the other drivers move as they are touched. |
| `src/user/system/native/devmgr/` | Matches devices to drivers and starts each in a job of its own. |
| `src/user/system/native/drivers/<function>/<name>/` | One process per driver, grouped by function as in the table above. The logic lives in `src/lib/drivers/`; the program is the thin shell around it. |
| `src/user/system/native/pong/`, `src/user/system/native/channel-echo/` | Small native test programs the boot gates start. |

`tools/common/xtask/src/native.rs` lists which of these go into the image.

## `src/user/system/linux/`

Programs that run on Ferrix's Linux ABI. Each is a separate cargo workspace
with its own `Cargo.lock` and lints, built by xtask from inside its
directory:

| Path | What |
|---|---|
| `src/user/system/linux/compositor/` | hyprix, the terminal and every Wayland piece. |
| `src/user/system/linux/init/` | `/sbin/init`, getty and the unit files. |
| `src/user/system/linux/pkg/` | `/bin/pkg`, the package manager: lists, installs and removes the apps' packages (`docs/APPS.md` §7). |
| `src/user/system/linux/zinc/` | The zsh-compatible shell. |
| `src/user/system/linux/media/` | The resampler and the playback through `/dev/snd` that the sound server and the `badapple` app share (`docs/MEDIA.md`), and the PulseAudio-protocol server and its client (`docs/AUDIO.md`, U2). ferrix-90's since 2026-09-27. Bad Apple!!'s player and its video format are the `badapple` app since 2026-10-01. |
| `src/user/system/linux/ferrousli/` | The C library written in Rust, its dynamic linker, and the toolkit programs are ported against it with (`tools/ports/`), with the libraries they link; the ported programs themselves are apps. |
| `src/user/system/linux/drivers/nvrm/` | NVIDIA's driver host (`docs/NVIDIA.md`). `src/` is `nvrm` itself, so far N1b's skeleton: a static ferrousli program, in C, that devmgr starts as its `Gpu` kind's driver, with a native entry (`src/start.c`) that builds the stack ferrousli starts from, and the native calls it makes (`src/native.h`). `test/hold.c` is the init `cargo xtask test-nvrm` boots beside it. `uvm-kpi/` is the Linux-compatible headers and runtime, in C, that NVIDIA's `nvidia-uvm` is built against from a fetched tree, and `uvm-selftest`, which runs UVM's own tests on it with no GPU (§11.3, C0a). Both are C and a Makefile, not a cargo workspace; `cargo xtask test-nvrm` and `test-uvm` build them against ferrousli and boot them on Ferrix. |

## `src/user/apps/`

Optional programs, native or Linux, one folder each: its `app.toml`, a
cargo workspace of its own and its README. xtask finds every folder here and
builds, gates, packages and installs it by what its `app.toml` says, so an
app is added by adding its folder and nothing else. No file outside a folder
names it, which `cargo xtask check` holds to, and this table has no row per
app: `cargo xtask apps` lists them. `docs/APPS.md` is the design.

## `src/tests/`

| Path | What |
|---|---|
| `src/tests/fuzz/` | `cargo fuzz` targets over `src/lib/`, and their committed corpus. |
| `src/tests/loom/` | `loom` models of the orderings the kernel's lock-free looks rest on (`docs/OPAQUE-KERNEL.md` §9.8), each with a control that must fail; `cargo xtask loom`, and a step of `check`. loom is a dev-dependency here only (the customer's decision, 2026-10-03). |
| `src/tests/threads/` | Stage 7's threads exit test: a static musl program using `std::thread`. |
| `src/tests/sem/`, `src/tests/shm/`, `src/tests/procfs/` | Static programs `test-sem`, `test-shm` and `test-procfs` run inside Ferrix. |

A crate's own tests and test data stay beside it (`src/lib/fs/btrfs/testdata/`,
`src/user/system/linux/compositor/render/tests/data/`). `src/tests/` is for programs
that exercise the system from outside any one crate.

## `assets/`

| Path | What |
|---|---|
| `assets/fonts/` | Inter, Liberation and Noto Sans CJK, with their licences, and `fonts.conf`. xtask puts them into the image under `/usr/share/ferrix/fonts`. |
| `assets/start/` | The page Chrome opens on a desktop. xtask fills in the boot's keys and facts and puts it under `/usr/share/ferrix/start`. |

## `scripts/`

| Path | What |
|---|---|
| `scripts/skills/` | Agent skills: one directory per role, each with its `SKILL.md` and the helpers it runs (`AGENTS.md`). `.claude/skills` links here, so Claude Code finds them in every checkout. |

## `tools/`

Everything that runs on the host. A script is a tool; `scripts/` holds only
the agent skills.

| Path | What |
|---|---|
| `tools/common/xtask/` | The host build driver (`cargo xtask …`). |
| `tools/common/check/` | The quality gates `cargo xtask check` and CI run: layering, assembly budget, unsafe and panic audits, the certification item boundary, complexity, commit authors. `rustlex.py` is their shared Rust lexer. |
| `tools/common/gen/` | Generators and their `--check` modes: brand images and release notes, the architecture document (with its `sysml/` reader), the panic catalogue, fonts, Wayland protocol tables, XKB tables, SOUP, coverage justification, fuzz corpus seeds. |
| `tools/common/fetch/` | Fetch pinned downloads: the rustc sysroot, busybox, Chrome, Bad Apple!!, Steam's volumes, NVIDIA's driver. |
| `tools/common/test/` | Test drivers run by hand: the self-host matrix, the host `btrfs check` oracle. |
| `tools/common/steam/` | What `cargo xtask run-steam`, `test-steam-window` and `test-steam-store` carry into the guest: the scripts that start and watch Steam, its stand-ins, and in `workarounds/` the C shims for kernel gaps, each headed with its gap and the owner of the real fix (`docs/STEAM.md`). |
| `tools/common/data/` | The allow-lists, baselines and registers the checks read. |
| `tools/common/release/` | One-off GitHub repository settings, run by the owner (`github-setup.sh`). |
| `tools/vendor/google/pixel7/` | The Pixel 7 launcher app (`android/`), its host helper and the desktop monitor (`monitor/`). |

## `docs/`

| Path | What |
|---|---|
| `docs/*.md` | Design documents, one per subsystem, and `GUIDE.md`, the technical guide. |
| `docs/roadmap/`, `docs/sysml/`, `docs/certification/`, `docs/generated/` | The roadmap, the SysML model, the certification evidence, and what is generated from them. |
| `docs/vendor/<vendor>/<device>/` | Notes on one board or phone. |
| `docs/brand/`, `docs/marketing/` | The brand kit and the press copy. |
| `docs/website/` | The public website, deployed to GitHub Pages. |

## Not in the repository

| Where | What |
|---|---|
| `build/<arch>/` | Images, initramfs and serial logs from xtask. Git-ignored. A running VM may hold these open. |
| `target/`, `*/target/` | Cargo output. Git-ignored. Worktrees each use their own `CARGO_TARGET_DIR`. |
| `.claude/worktrees/` | Worktrees of the sessions working on this repository. Git-ignored. Remove a worktree with `git worktree remove` once its branch is on `main`. |
| `~/.local/share/ferrix/` | Downloaded reference sources, busybox builds, firmware, per-stream target directories and logs. |

Scratch files belong in `$TMPDIR` or a session's scratch directory, never at
the top of a checkout: a file there is either committed by the next `git add
-A` or left for someone else to wonder about.
