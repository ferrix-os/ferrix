# Ferrix — roadmap

`docs/ARCHITECTURE.md` says what is being built. This says in what order, and
how each stage knows it is finished.

Two rules govern the ordering:

1. **Every stage ends in something that runs.** Not "the VM subsystem
   compiles" — a QEMU boot that demonstrates the new capability and stays in CI
   forever after. A stage with no observable exit criterion is a stage nobody
   can tell is broken.
2. **Nothing is stubbed that a later stage has to unpick.** A fixed-size process
   table, an in-memory-only filesystem or a cooperative scheduler would each
   save a week now and cost a rewrite later, because the goal at the end needs
   the real version of all three.

Sizes are order-of-magnitude, in the sense of "a weekend / a week / a month /
longer". This is a long program of work: stages 1–8 are a conventional kernel
bring-up, 9–14 are the parts this design chose to do properly, and 15–17 are the
goal. Nobody should read the table as a schedule.

---

## Stage 0 — Foundation ✅

Workspace, the quality gates ported from Starling, CI, and the two host-testable
libraries.

**Exit:** `cargo xtask check` runs fmt, clippy on all four targets, the layering
check, the assembly allow-list, the unsafe audit and the panic audit, and all
pass on an empty tree.

---

## Stage 1 — Boot, both architectures ✅

UEFI loader in Rust: read the kernel from the ESP, parse ELF64, build page
tables, take the memory map, `ExitBootServices`, switch to our own tables, jump
to the kernel's Rust entry point. Kernel writes to a serial port and shuts the
machine down.

Zero assembly at boot on either architecture — firmware calls `efi_main` in
64-bit mode. The only assembly is the page-table switch itself.

**Exit, met:** `cargo xtask test-boot --arch both` boots firmware → loader →
kernel on each architecture, and the kernel verifies four things before it
reports success:

* the hand-off structure's magic, version and layout constants agree with what
  the kernel was built against;
* the memory map is sorted, non-overlapping, has usable RAM, and describes the
  loader's *own* allocations — without which the frame allocator would hand out
  the frames holding its own page tables;
* the direct map really does alias physical memory, checked by reading the
  kernel's own first bytes through both mappings;
* the kernel can walk the loader's page tables and *extend* them, which on
  AArch64 is not optional — the console is an `MMIO` register that has to be
  mapped as device memory before a byte can go out.

Two bugs this stage found, both of which would have been very hard to diagnose
later: firmware reports a reserved aperture at 1 TiB that must not be counted as
RAM, and `FERRIX` as a directory name is byte-identical to `FERRIX` as a FAT
volume label.

---

## Stage 2 — Physical and virtual memory ✅

The arithmetic lives in `libs/` and is unit-tested on the host; the parts that
touch `CR3` or `TTBR1` do not.

**Done.** The buddy allocator (`libs/frame`) and the kernel heap
(`libs/heap`), both wired up and running on both architectures:

* `libs/frame` — buddy allocator over the UEFI memory map, orders 0 to 10.
  Free-list links live in a per-frame side array rather than in the free pages,
  which makes the whole allocator index arithmetic and therefore
  `#![forbid(unsafe_code)]`, host-testable and fuzzable. The side array is not a
  concession to testing: a refcount per frame is what copy-on-write will need.
* `libs/heap` — segregated free lists over a page supply, behind a `Backing`
  trait so the loads and stores that a free-list allocator needs are the
  implementor's problem rather than the allocator's. `Box`, `Vec` and
  `BTreeMap` now work in the kernel.
* The chicken-and-egg — the per-frame array has to exist before there is an
  allocator to make it — is resolved by carving it from the front of the
  largest usable region and handing that region over with the carved part
  excluded.

**Exit criterion met, and in the boot test on both architectures:** 4096 blocks
of assorted orders allocated and freed in an order that forces coalescing, with
the free-frame count required to return exactly to where it started; a `Box`, a
`Vec` grown through several reallocations, and a `BTreeMap` of two thousand
entries, all verified to hold what was put in them.

**Done — the virtual half, and the tidying the physical half was owed.**

* `kernel/src/vmap.rs` — an arena over `KERNEL_VMAP_BASE` that hands out
  virtual ranges with an unmapped guard page either side, and guard-paged
  kernel stacks on top of it. Free ranges are coalesced with their neighbours,
  so a stack allocated and freed a thousand times does not fragment the arena
  into a thousand holes.
* The loader's identity map is dropped, in the one order that works: the W^X
  sweep cannot pass while it is live — the loader mapped itself writable *and*
  executable, and correctly so, since it was executing out of pages it was
  still relocating — so the map goes first and the sweep runs after. The kernel
  then proves the map is gone by translating the physical address it used to
  run at and requiring nothing back.
* The loader's own memory and the ACPI-reclaim regions are handed to the buddy
  allocator once nothing points into them, which on a 512 MiB QEMU machine is
  3 MiB — small in absolute terms and the entire difference between a kernel
  that can reclaim boot memory and one that cannot.
* The W^X sweep walks the live tables through `Mapper::for_each_leaf` and
  asserts no leaf is both writable and executable. It reports what it swept as
  well as what it found, because a sweep that walks nothing also finds nothing.
* `libs/heap` returns empty slab pages to the buddy. The free-object count
  lives in the per-frame record, as the note here asked — not in a header
  stolen from the first object, which would have made the allocator's own
  metadata the thing a use-after-free corrupts first. The last page of a class
  stays, so a workload oscillating across a page boundary does not pay a buddy
  allocation per cycle.

**Exit criterion met, and in the boot test on both architectures.** On top of
stage 2's allocator hammering: a vmap allocation is written through and read
back, two allocations are required to be separated by their guard pages, the
pages either side of a range are required to translate to nothing, a kernel
stack is required to be 16-byte aligned and writable at both ends with guards
beyond each, and freeing it is required to return every frame it held. Then the
identity map is dropped and its absence checked, the sweep reports 318 mappings
on x86-64 and 822 on AArch64 with none writable-and-executable, and the reclaim
reports the frames it recovered.

**Deferred to stage 4, because it needs a second CPU to mean anything:**
per-CPU frame caches.

---

## Stage 3 — Traps, interrupts, time  ·  *exit criterion met*

x86-64: GDT, TSS, IST stacks, IDT, exception handlers, LAPIC, IOAPIC, HPET/TSC
deadline. AArch64: `VBAR_EL1` vector table, synchronous/IRQ/FIQ/SError
handlers, GICv2 and GICv3, the architected generic timer.

Both behind one facade: `irq::register`, `timer::after`, `trap::Frame`.

**Done — the trap half.** Vectors are installed on both architectures as the
first thing after the stage 1 self-check, before anything can deliberately
fault. That ordering is not fastidiousness: until it runs the CPU is still
pointing at firmware's handlers, and those stopped existing at
`exit_boot_services`. A fault in that window is a jump into reclaimed memory,
which on x86-64 is a triple fault and a silent reset with nothing on the wire to
say why.

* x86-64 — GDT, TSS and a 256-entry IDT, every gate naming the kernel code
  selector the GDT installed a moment earlier. The double-fault gate runs on an
  IST stack of its own, because it is the one fault whose cause may be that the
  stack is unusable.
* AArch64 — the `VBAR_EL1` vector table and its handlers.
* One dispatch path above both (`kernel/src/trap.rs`), reached only through the
  architecture facade — `TrapFrame`, `classify`, `report_trap` — so generic code
  still never names a CPU.

**Exit criterion, half met, and in the boot test on both architectures.** A
breakpoint returns to the instruction after it, twice, with a canary in a
register the trap frame saves and restores required to come back intact — which
is what proves the entry path *restores* rather than merely arrives. Then a page
fault is resolved rather than reported: three pages in an unmapped window are
touched out of order, the handler maps the faulting address, the instruction
retries, and the pages are required to read back what was written, to be zeroed,
and to have cost exactly the frames they should — one per page plus one per
table level for the first fault into a fresh region, and exactly one for a fault
into a region already tabled. That last bound is what makes the accounting a
measurement rather than a shrug.

Resolving a fault by mapping a page and retrying *is* demand paging, arriving
here rather than at stage 6 because it is the only honest way to show the fault
path recovers. Stage 6 inherits it instead of writing it.

**Done — the interrupt and time halves.** Something now arrives that the
kernel did not ask for at the moment it arrives, which is the difference
between a program and an operating system.

* x86-64 — the local APIC is mapped from the MADT's address, enabled, and its
  task priority dropped to accept everything; the local APIC timer is
  calibrated against the HPET, whose period firmware states exactly in
  femtoseconds so that nothing has to be measured. A machine with no HPET
  table falls back to the TSC calibrated against the PIT, which is the only
  clock a PC is guaranteed to have. Every I/O APIC firmware described is
  mapped and every input masked — not configured, because nothing is wired to
  a device line until stage 10, but quiesced, because a line firmware left
  enabled would arrive at a vector chosen by whoever wrote the firmware.
* AArch64 — GICv2's distributor and CPU interface, from the MADT; the
  architected *virtual* timer, whose interrupt number comes from the GTDT.
  Virtual rather than physical because a kernel at EL1 is by definition below
  any hypervisor present, and on bare metal the two are the same counter.
* The other two thirds of the facade: `irq::register` and `timer::after`.
  One-shot is the primitive, because AArch64's timer has no periodic mode at
  all — it compares against an absolute instant — and stage 5's tickless
  scheduler wants one-shot anyway. Periodic is a re-arm inside the handler.

**Exit criterion met, and in the boot test on both architectures.** A
thousand timer interrupts are counted, and the time they took is measured
with the *counter* rather than by multiplying the tick count by the rate they
were programmed at — which would be arithmetic that cannot fail rather than a
measurement. x86-64 reports 990 Hz against a requested 1000 and AArch64 992,
the shortfall being one interrupt entry and exit per period under an emulator.

Before that, a one-shot is armed and the count required not to move for ten
further intervals. That check is there for one specific bug: AArch64's timer
interrupt is level triggered, so a handler that acknowledges the controller
without disarming the timer is re-entered immediately and forever. It does
not fail by producing a wrong number — it fails by never returning, with
nothing in the log after the line before it.

The boot marker reads `FERRIX-BOOT-OK stages 1-3`, and it is now the whole
truth.

**Still to do, and neither is reachable from a machine Ferrix boots on
today.** Both are hardware variants rather than gaps in the stage: the
facade above them does not change, and neither is on any later stage's path.

* GICv3 and its redistributors. `gic::init` reads the version out of the
  MADT and refuses anything that is not a GICv2, rather than writing GICv2
  layouts into GICv3 registers and producing a machine that takes no
  interrupts for reasons nothing explains. QEMU's `virt` gives a GICv2 unless
  asked otherwise, so this needs a second boot-test configuration as much as
  it needs code.
* TSC-deadline mode, which replaces the local APIC timer's countdown with a
  comparator against the TSC and is how a tickless kernel avoids
  reprogramming a divider on every reschedule. The calibration it needs is
  already here.
* Per-CPU anything. The controller is brought up for the boot CPU because
  there is only one; stage 4 is where a redistributor per core and a local
  APIC per core start to mean something.

## Stage 4 — SMP  ·  *week*

x86-64 AP bring-up via INIT–SIPI–SIPI and a real-mode trampoline (the one place
in the project with 16-bit assembly). AArch64 via PSCI `CPU_ON`. Per-CPU data
areas, IPIs, TLB shootdown, and the RCU-like grace period the scheduler mode
switch later depends on.

**Exit:** a boot test that brings every QEMU vCPU online, runs a contended
counter across all of them, and gets the right total.

---

## Stage 5 — Tasks and the scheduler  ·  *week*

`Task`, kernel stacks, context switch, per-CPU runqueues, the class stack, and
the EEVDF fair class. Scheduling domains exist from the start with one mode
(`Throughput`) implemented; the other two are stage 14, but the domain
abstraction is not retrofitted.

**Exit:** a boot test that runs a thousand kernel threads doing bounded work,
verifies fair distribution against EEVDF's own guarantee, and shuts down clean.

---

## Stage 6 — User mode  ·  *week*

`AddressSpace`, VMOs, the VMA interval tree, demand paging, copy-on-write, the
ELF loader, and the ring-3/EL0 transition. The first user process is a
hand-written static binary that makes one syscall.

**Exit:** a boot test that runs a user binary which writes to fd 1 and exits,
with a page fault serviced along the way.

---

## Stage 7 — The Linux syscall ABI  ·  *month*

The syscall entry path on both architectures, the dispatch table, and the core
of the surface: memory (`mmap`, `mprotect`, `brk`), files, process
(`clone`, `execve`, `wait4`, `exit_group`), threads and `futex`, signals with
`sigaltstack` and `rt_sigreturn`, time, and identity.

**Exit:** a static musl `busybox sh` starts, runs a script, and exits — the
first time somebody else's binary runs on Ferrix.

---

## Stage 8 — VFS, initramfs, the pseudo-filesystems  ·  *month*

Inode and dentry caches, the mount table, file descriptors and their sharing
rules, tmpfs, devfs, procfs (`self/maps`, `self/exe`, `self/fd`, `cpuinfo`,
`meminfo`), and cpio initramfs unpacking.

**Exit:** `busybox ls -R /proc`, `cat /proc/self/maps` and a shell script that
manipulates files under tmpfs, all under the boot test.

---

## Stage 9 — The native ABI: handles, channels, ports, VMOs  ·  *week*

Handle tables, `Channel` with handle passing, `Port` event queues, `Interrupt`
objects, `IoMapping`, and `Job`. The syscalls in the `0x1000` range. This is
what stage 10 is written against.

**Exit:** two user processes exchange messages and a handle over a channel, and
a `Job` kill takes down a process tree.

---

## Stage 10 — Userspace drivers  ·  *month*

ACPI and device-tree enumeration in the kernel; IOMMU domains (VT-d, AMD-Vi,
SMMUv3); `devmgr`; the shared-ring block protocol; and the first driver —
virtio-blk — as a user process.

**Exit:** a boot test that reads a sector from a virtio disk through a driver
running in ring 3, with the IOMMU on and a deliberate out-of-domain DMA
attempt faulting.

---

## Stage 11 — Block core and btrfs, read  ·  *month*

Request queues, merging, the I/O scheduler. Then btrfs stage A: superblock,
chunk tree, root tree, fs trees, extents inline and regular, crc32c, and
zstd/zlib/lzo.

The item parsing is `libs/` code — pure functions over bytes, fuzzed against
images `mkfs.btrfs` produced.

**Exit:** Ferrix mounts an image made by real `mkfs.btrfs`, and reads a file
tree out of it that byte-for-byte matches what the host wrote.

---

## Stage 12 — btrfs, write  ·  *longer*

Copy-on-write allocation through the extent tree, delayed refs, transaction
commit against both superblocks with correct flush/FUA ordering, the free-space
tree, and log-tree replay.

**Exit, and it is a strict one:** Ferrix writes a tree, and host `btrfs check`
finds nothing. Then the power-fail test — kill QEMU at a random point inside a
transaction, remount, replay, `btrfs check` again — over hundreds of seeds.

---

## Stage 13 — Namespaces, cgroups v2, seccomp  ·  *month*

All eight namespaces, the unified hierarchy with `cpu`/`memory`/`io`/`pids`,
cgroupfs, and classic-BPF seccomp with the interpreter in `libs/`.

**Exit:** an unprivileged user namespace runs a process whose pid is 1 inside
it, under a memory limit that triggers scoped reclaim and a scoped OOM kill,
with a seccomp filter that blocks a syscall.

---

## Stage 14 — Real-time domains  ·  *month*

The `SoftRt` and `HardRt` modes: FIFO/RR classes, threaded interrupts,
priority-inheritance mutexes, EDF with constant-bandwidth admission control,
and the runtime domain-mode switch with its quiescence protocol.

**Exit:** a cyclictest-shaped boot test measuring wake-up latency on a `HardRt`
domain while a `Throughput` domain on other cores is saturated, reporting a
maximum inside the stated bound; plus an admission test that refuses an
unschedulable set instead of missing deadlines.

---

## Stage 15 — A real userland  ·  *week*

Static musl busybox as `/bin`, a working init, job control, ttys, pipes. Enough
of a system to be used rather than demonstrated.

**Exit:** an interactive shell over the serial console that a person can use.

---

## Stage 16 — `rustc`  ·  *the goal*

The remaining syscall surface, the memory scale, the process spawn path for
`rust-lld`, and a sysroot on btrfs. Then run it.

**Exit:** `rustc hello.rs && ./hello` on Ferrix, in CI.

---

## Stage 17 — Self-hosting

Build Ferrix on Ferrix. At that point the acceptance test writes itself: the
image produced by the Ferrix-hosted compiler boots and passes every test above.

---

## Written ahead of their stage

The rule at the bottom of this file — anything expressible as a pure function of
bytes goes to `libs/` *before* it is called from the kernel — has a consequence
worth stating plainly, because otherwise the tree looks further along than it is:
several crates for stages that have not started are already written and tested.

They are parsers and data structures, not subsystems. None of them counts
towards the stage that will consume it, and all but one are still unreachable
from the kernel — `libs/acpi` is the exception, as of stage 3, which is what
the rule was for: the MADT walk the interrupt controller needed was already
written, tested and fuzz-shaped before a line of controller code existed. What they buy is that the stage in question begins with its
byte-handling already fuzz-shaped, host-testable and argued about, rather than
being written at three in the morning against a machine that reboots on a
mistake.

| Crate | Waiting for | Tests |
|---|---|---|
| `libs/acpi` | 3, 10 — RSDP, XSDT/RSDT, MADT, FADT fixed fields, GTDT, HPET. No AML, and there will be none. | 58 |
| `libs/fdt` | 3, 10 — flattened device tree reader; on AArch64 the only description of the machine there is. | 49 |
| `libs/sync` | 4 — ticket lock, rwlock, `Once`. Fair by construction, because an unfair lock on a starved core is a stage-14 latency bug nobody will find. | 19 |
| `libs/vma` | 6 — the VMA interval tree and the three calls that reshape it (`mmap MAP_FIXED`, `munmap`, `mprotect`). | 60 |
| `libs/linux-abi` | 7 — syscall numbers, `errno`, `repr(C)` layouts. Constants only; nothing executes. | 42 |
| `libs/cpio` | 8 — the "newc" reader an initramfs is unpacked from. Borrows, copies nothing, allocates nothing. | 45 |
| `libs/virtio` | 10 — the split virtqueue as logic over an abstract shared memory. | 50 |
| `libs/btrfs` | 11, 12 — superblock, chunk tree, B-tree nodes, item payloads. Parsing only: no device, no cache, no transactions. | 38 |

With the five crates the kernel already uses — `bootinfo`, `elf`, `frame`,
`heap`, `paging` — that is **444 host unit tests, all passing**, over code the
kernel cannot reach yet.

**The gap this opens, stated rather than hidden.** The continuous rule below
asks for a fuzz target *and* a Miri run per crate, and `fuzz/` currently has two
targets: `elf_parse` and `frame_alloc`. Every crate in the table above parses
bytes that came from outside the system — a disk, a firmware table, an archive a
stranger built — which is precisely the population the rule was written for. The
fuzz targets are owed, and are owed *before* the consuming stage starts, not
when it ships.

---

## Continuously, from stage 1

* Every stage's exit criterion joins the CI boot test and stays there.
* The assembly allow-list is not added to without an argument in the diff.
* Anything expressible as a pure function of bytes goes to `libs/` and gets a
  fuzz target and a Miri run — before it is called from the kernel, not after.
