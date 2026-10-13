# Ferrix — architecture

Ferrix is an operating system written in Rust for x86-64, AArch64 and ARMv7-A,
whose acceptance test is that it compiles Rust. Not "has a shell", not "draws a
window": it hosts `rustc`, which is the hardest thing a general-purpose OS is
routinely asked to do and the only goal that forces every subsystem to be real.

This document is the design. The roadmap, `docs/roadmap/`, is the order it
gets built in, and `docs/LAYOUT.md` says where each part lives in the tree.

---

## 0. The goal, read as a specification

"Runs the Rust compiler" is not an aspiration, it is a requirements list.
A statically linked musl `rustc` needs, at minimum:

| It needs | Which forces |
|---|---|
| `clone(CLONE_THREAD\|CLONE_VM\|CLONE_SETTLS)`, the `futex` family, `set_tid_address`, robust lists | 1:1 kernel threads, a real futex implementation, per-thread TLS registers |
| `mmap`/`mprotect`/`munmap` with `MAP_FIXED`, `MAP_NORESERVE`, 2–8 GiB of address space | Demand paging, a VMA tree, lazy anonymous memory, overcommit |
| A `SIGSEGV` handler on an alternate stack | Real signal delivery, `sigaltstack`, `rt_sigreturn` |
| `fork`/`execve`/`wait4` to run the linker | Copy-on-write fork, an ELF loader, process groups, exit status plumbing |
| `openat`, `getdents64`, `statx`, `renameat2`, `pread64`, ~150 syscalls in total | A VFS with inode and dentry caches |
| `/proc/self/maps`, `/proc/self/exe`, `/proc/self/fd` | A procfs backed by the real VM and fd table |
| A writable filesystem with room for a ~2 GiB sysroot | A block stack and an on-disk filesystem that survives a crash |

Every one of those is load-bearing. This is why the system below is shaped the
way it is, and why "toy OS" decisions — a fixed process count, an in-memory-only
filesystem, cooperative threading — are ruled out at the start rather than
discovered to be dead ends at the end.

---

## 1. Shape of the system

> **Being replaced (customer, 2026-10-13).** Ferrix is to become a
> microkernel whose ring 0 is the certified item whole, with the Linux ABI
> served from user space by `linuxd` (`docs/MICROKERNEL.md`;
> `docs/BACKLOG.md`, Decisions). This section and §2 describe the system as
> built today and stay true until that plan's flip (its M5).

```
        ┌─────────────────────────────────────────────────────────────┐
        │ userspace                                                    │
        │   rustc, busybox, init      │ devmgr │ virtio-blk │ virtio-net│
        │   (static musl, Linux ABI)  │  (Ferrix native ABI, handles)  │
        └───────────┬─────────────────┴────────────┬───────────────────┘
                    │ syscall (Linux ABI)          │ channels, VMOs, IRQ objects
        ┌───────────┴──────────────────────────────┴───────────────────┐
        │ kernel                                                        │
        │  syscall layer (Linux ABI)   │  native ABI (capabilities)     │
        │  ── VFS · page cache · btrfs · tmpfs · procfs · devfs         │
        │  ── block core · net core                                     │
        │  ── scheduler (domains, classes) · futex · signals · IPC      │
        │  ── VM: VMOs, VMAs, buddy, slab, reclaim                      │
        │  ── namespaces · cgroups v2 · seccomp                         │
        │  ── arch: MMU, traps, timers, IRQ controller, IOMMU, SMP      │
        └───────────────────────────────────────────────────────────────┘
```

**Monolithic core, capability seams, userspace device drivers.** A syscall is a
function call, not four IPC hops — that is what a compiler workload needs. But
device drivers run as ordinary user processes holding capabilities, so a driver
fault is a process fault. The line is drawn at *devices*: filesystems and the
page cache stay in the kernel because `rustc` touches them on every path, while
the code that pokes a PCIe BAR does not.

The kernel drives two output devices of its own, both write-only, and both named
as exceptions here so that they stay the only ones. The serial port writer
carries early boot and panic output, when no userspace exists to talk to. The
firmware's framebuffer is drawn on by a panic, once, with the same text the
serial port carried, for a machine that has a screen and no cable. When the
command line asks for it with `ferrix.fbcon`, the kernel's own console lines are
drawn there as they are printed too, for a machine whose only log is read back
after a reset, like the Pixel 7. That boot console stops for good when a panic
draws or a display driver takes the screen, and it never draws a program's
output. Neither device is ever read, and neither is configured beyond what
firmware left.

---

## 2. ABI: Linux is the native ABI

Syscall 0 is `read`. Not a compatibility layer over something else — the actual
native interface.

This is the highest-leverage decision in the project. It means the existing
static-musl world is our userland from the first day there is a userland, that
POSIX conformance is inherited rather than reimplemented, and that the goal is
reachable by writing an OS rather than by also porting LLVM to a new target.
(That alternative is worth naming: a native target with its own `std` needs LLVM
built for it, which drags a C++ runtime into the tree — making the project
*less* Rust, not more.)

Two ABIs coexist:

* **Linux ABI**, syscall numbers 0.., each architecture's table as Linux
  defines it — on ARMv7-A, the EABI one. Everything about it is a compatibility obligation; we do not get
  to have opinions.
* **Ferrix native ABI**, syscall numbers from `0x1000`, capability-handle based.
  This is what device drivers, `devmgr` and anything else Ferrix-specific speaks.
  It is where the design opinions live.

A process may use both: a musl program can make native calls for the parts
POSIX cannot express. `devmgr` and the ring-3 drivers are native programs,
built on `src/user/system/native/rt`.

---

## 3. Kernel objects

The native ABI is built on typed, reference-counted kernel objects reached
through per-process handle tables. Nine of them:

| Object | What it is |
|---|---|
| `Vmo` | A pageable memory object: pages, not a mapping. Anonymous memory, page-cache pages, and DMA buffers are all VMOs. |
| `AddressSpace` | Page tables plus an interval tree of `Vma`s, each mapping a range of a VMO. |
| `Channel` | Bidirectional datagram pipe that carries bytes *and handles*. The basis of driver IPC. |
| `Port` | An event queue a thread waits on; how one driver thread services many sources. |
| `Interrupt` | A bindable hardware interrupt. A userspace driver waits on it via a `Port`. |
| `IoMapping` | An MMIO aperture, mappable into a driver's address space, with its IOMMU domain. |
| `Task` | A schedulable thread. |
| `Process` | A group of tasks sharing an `AddressSpace`, fd table, fs context and signal dispositions. |
| `Job` | A container of processes, and where resource limits and kill authority live. |

**The VMO unification is the load-bearing idea.** One page-list abstraction
serves the page cache, anonymous memory, shared memory, and driver DMA buffers.
A block driver reading into a page-cache page is not copying into a buffer and
then into the cache — it is filling the VMO the cache already holds, which is
what makes userspace drivers affordable for a compiler workload.

`Job`s exist because a userspace driver that wedges has to be killable as a
unit, together with anything it spawned.

---

## 4. Memory

**Physical.** Buddy allocator over the UEFI memory map, orders 0–10, with
per-CPU magazine caches so the common single-page allocation never takes the
global lock. Frames carry a `PageInfo` with refcount, owning VMO and flags —
this is what makes reclaim and copy-on-write tractable.

**Kernel heap.** Slab allocator over the buddy, with per-CPU free lists and
size-class caches for the general `alloc` path. Kernel object types get their
own slabs, so a `Task` allocation is a pop off a list.

**Virtual.** Tables of 512 eight-byte descriptors over a 4 KiB granule on all
three architectures: four levels over 48-bit addresses on the 64-bit pair, three
over 32 on ARMv7-A, whose Large Physical Address Extension is AArch64's
descriptor format with a narrower physical address. `src/lib/kernel/paging` is written
once, over a *geometry* and an *encoding* each architecture supplies.

The 64-bit pair share *identical layout constants*:

```
0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   user
0xFFFF_8000_0000_0000 .. 0xFFFF_FEFF_FFFF_FFFF   direct map of all physical RAM
0xFFFF_FF00_0000_0000 .. 0xFFFF_FFEF_FFFF_FFFF   kernel vmap (MMIO, guard-paged stacks)
0xFFFF_FFFF_8000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   the kernel image
```

x86-64 puts the image in the top -2 GiB because that is what the "kernel" code
model addresses; AArch64 does not need to, and does anyway, because one layout
is one set of bugs instead of two.

ARMv7-A cannot hold a single one of those constants, so its layout is argued
rather than merely different — a 2/2 split, `TTBR0` translating the lower half
and `TTBR1` the upper:

```
0x0000_0000 .. 0x7FFF_FFFF   user
0x8000_0000 .. 0x9FFF_FFFF   kernel vmap (MMIO, guard-paged stacks)
0xA000_0000 .. 0xEFFF_FFFF   direct map of RAM, 1.25 GiB
0xF000_0000 .. 0xFFFF_FFFF   the kernel image
```

The user half is the larger because a 32-bit process wants it. The direct map
gets what the vmap area and the image leave, and its size is the ceiling on the
RAM a 32-bit kernel can use. On every architecture the direct map begins at the
lowest RAM address rather than at zero — a gibibyte in on QEMU's Arm machines,
whose first gibibyte is flash and device registers — and `BootInfo` says where.
`src/lib/proto/bootinfo` holds both layouts and checks both at compile time on every
build, whichever the build is for.

The direct map is writable and never executable, except over the kernel
image's own text and read-only data, which it aliases read only: a writable
alias of the text would change the code the image mapping runs without ever
being writable and executable itself. Both loaders cut the direct map around
that span, and every boot walks each mapping of its frames and requires none
to be writable. No new mapping of the image can be made from outside `mm`
either: every interface that maps a physical address its caller names -- a
kernel device window, early boot's, a user space's device or GPU window --
refuses a range touching any part of the image (`mm::overlaps_image`).

These are regions, not addresses. Built `--mitigations on`, the default, the
loader puts the kernel image, the direct map and the top of the vmap arena
somewhere new in its region each boot (KASLR), and `BootInfo` says where;
the image's link address is the bottom of its region, where it runs only
built `off` or told `nokaslr`.
`docs/certification/SPECULATION.md` §6.1 has the steps and the entropy.

**Address spaces.** A red-black interval tree of `Vma`s, each naming a VMO, an
offset, a protection and a share mode. `mmap` inserts, `munmap` splits,
`mprotect` splits and re-permissions. Anonymous memory is lazy; `fork` marks
both sides read-only and copies on fault; file mappings fault from the page
cache VMO directly, so a mapped file and a `read` file are the same pages.

**Reclaim.** Two-list LRU (active/inactive) with a shrinker interface for the
caches. `rustc` will exhaust memory on a small machine, so this path is a
correctness requirement, not a nicety: an OOM kill scoped by `Job` and cgroup,
never a livelock.

---

## 5. Scheduling: selectable per domain, at runtime

The scheduler is not one policy. CPUs are partitioned into **scheduling
domains**, each of which runs in one of three modes, changeable at runtime:

| Mode | Classes, highest first | Preemption | Interrupts |
|---|---|---|---|
| `Throughput` | EEVDF fair → idle | Preemption points, deferred for cache locality | In-context |
| `SoftRt` | FIFO/RR (1–99) → EEVDF → idle | Fully preemptible kernel, PI mutexes | Threaded |
| `HardRt` | EDF with CBS admission control → nothing else | Fully preemptible, bounded sections | Threaded, bandwidth-reserved |

**Per domain, not global.** A four-core machine can run a `HardRt` partition on
one core and `Throughput` on the other three, with `rustc` on the latter. A
global switch would be less useful and much harder to make safe, since it would
have to quiesce a machine that is doing something.

**EEVDF, not CFS**, for the fair class: it gives each task an actual eligible
time and a deadline, so latency has a bound instead of only fairness having one.

**Switching modes** takes the domain through a quiescent point: stop admitting,
wait out an RCU grace period, migrate tasks whose policy the new mode does not
offer, swap the class stack, resume. Tasks that cannot be represented in the new
mode are demoted with an errno the switching caller sees, never silently.

**What `HardRt` does and does not promise.** It promises EDF with admission
control that refuses an unschedulable set, bounded kernel critical sections on
the RT path, preallocated pools instead of dynamic allocation there, and
interrupts that cannot steal unaccounted time. It does not promise a certified
worst-case execution time for the whole kernel — no OS that also hosts LLVM can
offer that, and claiming it would be the kind of statement that gets believed.

Per-CPU runqueues; work stealing in `Throughput`, none in `HardRt` (partitioned
scheduling is what makes the admission test valid).

Linux's `sched_setscheduler` maps onto this: `SCHED_FIFO`/`SCHED_RR` to the
soft-RT classes, `SCHED_DEADLINE` to EDF, `SCHED_OTHER`/`BATCH`/`IDLE` to fair.

---

## 6. Processes, threads, isolation

**1:1 threading**, forced by the ABI. `clone` flags compose independently
shareable objects — address space, fd table, fs context, signal handlers,
namespace set — exactly as Linux does, because a process is precisely a
particular sharing arrangement of those.

**Namespaces**, all eight, designed in from the start rather than retrofitted:
pid, mount, uts, ipc, net, user, cgroup, time. Retrofitting namespaces means
finding every global table years later; designing them in means every such table
is reached through the task's `NsSet` from the first line. A `pid_t` is
meaningless without saying in which pid namespace.

**cgroups v2**, one unified hierarchy, with the `cpu`, `memory`, `io` and `pids`
controllers. `cpu` is not a separate mechanism — it is bandwidth and weight
handed to the scheduling classes above. `memory` is where reclaim and the OOM
killer are scoped.

**seccomp**, classic BPF filters evaluated on syscall entry, with an interpreter
that is itself a pure function over bytes and therefore fuzzable and Miri-able
in `src/lib/`.

**Credentials** are Unix: uid, gid, supplementary groups, POSIX capability sets,
no-new-privs. They sit on top of the handle system rather than beside it.

---

## 7. Device drivers in userspace

The kernel enumerates buses, because that needs ACPI (x86-64, AArch64) or a
device tree (ARMv7-A, and AArch64 firmware that offers one) and privileged
access. It does not drive devices.

**And that is gated, not merely meant.** `tools/common/check/check-device-access.py` reads
every volatile access and port instruction under `src/kernel/` against
`tools/common/data/device-access-allowlist.json`, which says for each file whether it
touches a device register, RAM a device also reads, or ordinary memory made
volatile so a boot check cannot be optimised away. A register access outside the
paths this section and §1 permit is refused, a file over its budget is refused,
and an entry whose file no longer makes one is refused too, so the list cannot
be kept warm. It runs in `cargo xtask check` and in CI. A script cannot tell the
three kinds apart -- they are the same call in Rust -- so what it enforces is
that somebody said which, in a diff, with a reason.

For each device found, the kernel creates a device node and hands `devmgr` a
handle (`docs/DEVMGR.md` is the protocol). `devmgr` matches a driver, spawns
it in its own `Job`, and gives it:

* an `IoMapping` for each BAR or MMIO window, and nothing outside it,
* an `Interrupt` object per vector, bound to a `Port`,
* a `Vmo` for DMA, whose device addresses come from an IOMMU domain scoped to
  that device — a driver cannot DMA over the kernel, or over another driver,
* a `Channel` to the kernel subsystem it serves (block, net, input).

**The data path is not per-request IPC.** Driver and kernel share a descriptor
ring in a VMO and ring a doorbell; requests batch. This is why userspace drivers
are affordable under a compiler's I/O load, and it is the same shape virtio and
NVMe already use, so the driver's own ring and ours line up.

**Bootstrap.** The root filesystem needs a block driver that lives in userspace
that lives on the root filesystem. The loader breaks the cycle: it loads an
initramfs into RAM containing `devmgr`, the virtio-blk driver and `init`. The
kernel mounts that as root, starts init, drivers come up, and the system pivots
onto btrfs. This is exactly Linux's answer, and it works for exactly the same
reason. On an image that boots `/sbin/init`, the kernel starts init alone and
init starts `devmgr` as a service (`docs/INIT.md`, L12): the kernel gives pid 1
a starter token that can start `devmgr` and nothing else, and the channel that
carries every device's authority goes from the kernel to `devmgr` without
passing through init. A driver that dies is started again by `devmgr` under
the service manager's restart policy (`src/lib/init/restart`), and its
device's pins are kept until the next driver has reset it.

**IOMMU is not optional here.** A userspace driver without an IOMMU is a
userspace process that can write to any physical address, which is worse than an
in-kernel driver, not better. VT-d/AMD-Vi on x86-64, SMMUv3 on AArch64. Where no
IOMMU exists, drivers run in a degraded trusted mode and the kernel says so
loudly at boot.

**Isolation, per platform.** Whether that degraded mode is the one a machine is
in is not a matter of opinion, so it is written down here and the kernel prints
the line that decides it. Every row below was read from a boot, not assumed.

| Machine or board | Bus and IOMMU found from | Driver DMA | The line that decides it |
|---|---|---|---|
| x86-64 `q35` | ACPI: MCFG, DMAR endpoint scopes | **translated**, VT-d at `0xfed90000` | `iommu 1 VT-d units and 0 SMMUv3s translating, 0 left alone`, and the domain check's `through a device's translated domain` |
| AArch64 `virt` | ACPI: MCFG, IORT root complex mapping | **translated**, `SMMUv3` at `0x9050000` | `iommu 0 VT-d units and 1 SMMUv3s translating, 0 left alone`, and `through a device's translated domain` |
| ARMv7-A `virt` | device tree: `iommu-map` to an `arm,smmu-v3` node | **untranslated — degraded trusted mode** | `iommu 0 VT-d units and 0 SMMUv3s translating`, then `iommu degraded trusted mode: no IOMMU domain is programmed, so device DMA reaches all of memory` |
| STM32MP157D-DK1 | device tree; no PCI host bridge on the board | **untranslated — degraded trusted mode** | the same degraded line; the stage 9 lines find no PCI and no IOMMU |

Read the unit counts and the domain check's one word, not the function counts:
how many functions a machine places behind a unit depends on which devices the
gate asked QEMU for (six on a plain x86-64 boot, eight with the network on), and
a machine can place every function behind a unit it never programs — ARMv7-A
does exactly that, which is why its row is decided by the degraded line and not
by `0 bypassing, 0 unresolved`.

The two untranslated rows are different failures and are not to be conflated:

* **ARMv7-A has an `SMMUv3`, and the kernel does not program it.** U-Boot
  2025.10's virtio-pci driver fails a heap assertion and resets when a device
  offers `VIRTIO_F_ACCESS_PLATFORM`, so the 32-bit machine's virtio devices are
  built without it and would bypass the unit whatever the kernel did. Stage 10's
  exit criterion accepts this and `xtask test-boot` does not ask that machine for
  an out-of-domain fault. It is a firmware limit, and it ends when the loader
  stops being the one to touch those devices.
* **The DK1 has no IOMMU at all.** Read on the board on 2026-09-13 under the
  `stage-9.1-console-and-iommu` tag, not inferred from the STM32MP157
  documentation: the boot finds no PCI host bridge and no unit. Any ring-3
  driver on that board can write to all of memory, and no amount of kernel work
  changes that.

So the safety claim §7 opens with — a driver cannot DMA over the kernel or over
another driver — holds on x86-64 and AArch64, and on the other two the kernel
says at every boot that it does not.

---

## 8. Storage

**Block core** in the kernel: request queues, merging, an I/O scheduler with
per-cgroup bandwidth, and the ring protocol to userspace drivers.

**VFS** in the kernel: inode cache, dentry cache with negative entries, mount
table per mount namespace, page cache unified with VMOs. `rustc` opens tens of
thousands of files during a build; this path is a performance requirement.

**Filesystems.** tmpfs, devfs, procfs, sysfs and cgroupfs in-kernel and small.
sysfs and cgroupfs are views that store nothing: cgroupfs of the job tree,
sysfs of the devices as their owners describe them -- enumeration, the cores
the ring-3 drivers publish into, and `devmgr`, which alone says which driver
drives which device and to which a write to `bind` or `unbind` is sent to
decide (`docs/SYSFS.md`).
The real one is **btrfs, read and write**, staged:

* **Stage A — read.** Superblock, chunk tree (logical→physical), root tree, fs
  trees, extent data both inline and regular, directory and inode items, crc32c
  verification, zstd/zlib/lzo decompression. Enough to mount what `mkfs.btrfs`
  produced and read a sysroot out of it.
* **Stage B — write.** Copy-on-write block allocation through the extent tree,
  delayed refs, transaction commit against both superblock copies with the right
  flush/FUA ordering, free-space tree, log tree and its replay.
* **Stage C — subvolumes and snapshots**, then the rest.

Single device to begin with; no RAID 5/6, which is where btrfs itself is
weakest. This is the largest single piece of work in the project and the
schedule should say so.

**How it is verified**, which matters more than the feature: every CI run builds
an image with real `mkfs.btrfs`, mounts it under Ferrix in QEMU, runs a
workload, and then hands the resulting image to `btrfs check` on the host. A
filesystem that only our own reader can read is not a filesystem. Power-fail
injection — killing QEMU mid-transaction and checking what survives — is the
test that a CoW filesystem actually has to pass.

---

## 9. What is written where, and why

| Layer | Lives in | Reachable by tests |
|---|---|---|
| Byte-level logic: ELF, cpio, btrfs item parsing, seccomp BPF, page-table arithmetic, allocators | `src/lib/` | `cargo test`, Miri, fuzzers |
| Loader | `src/boot/common/uefi/` | QEMU boot test |
| Kernel | `src/kernel/` | QEMU boot test, in-kernel test harness |
| Ring-3 programs on Ferrix's own ABI: the native runtime, `devmgr`, drivers, test programs | `src/user/system/native/` | Built and linted with clippy per kernel target; xtask checks each program's ELF shape; run under the QEMU boot test |
| Ring-3 programs on the Linux ABI: init, `authd`, the compositor and its clients, ferrousli, zinc, the installer, the media player and sound server, `statd`, `adbd` | `src/user/system/linux/`, each its own cargo workspace | `cargo test` on the host, and the `test-*` boots on Ferrix |
| Test programs and fuzzing outside any one crate | `src/tests/` | the boots, and `cargo fuzz` |
| Host tooling | `tools/common/`, and `tools/vendor/` for one vendor's hardware | `cargo test`, and the gates that run them |

The split is not cosmetic. Nothing in `src/kernel/` can be run by `cargo test`,
Miri cannot interpret a privileged instruction, and a fuzzer cannot drive a
page fault handler. So anything expressible as a pure function of bytes is
written as one, in `src/lib/`, where all three tools reach it.
`tools/common/check/check-crate-layering.sh` keeps that from eroding.

Architecture-specific code lives under `arch/` and is reached through one
facade. Generic code never names an architecture, and no
`#[cfg(target_arch)]` appears outside those directories — both enforced by the
same script, because a facade maintained by convention is a facade for about six
weeks. One system on chip's devices are not an architecture: they go under
`platform/<vendor>/<soc>/`, which compiles everywhere and does nothing on a
machine whose device tree does not name that chip (`docs/LAYOUT.md`).

---

## 10. Assembly

There is none at boot on any architecture: UEFI firmware — EDK2 on the 64-bit
pair, U-Boot on ARMv7-A — calls a Rust `efi_main` with a stack and the MMU
already set up. What assembly exists is confined to constructs the machine defines
before a Rust function could run — exception vectors, the syscall trampoline,
context switch, and the CPU primitives with no Rust spelling. `docs/ASSEMBLY.md`
is the list, `tools/common/data/asm-allowlist.json` is its machine-readable form, and CI
fails on an assembly site that is not in it.
