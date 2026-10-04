# The certified item

Four assurance ratings are being argued for in this directory. Every one of
them is a claim about a *scope*, and naming that scope precisely is the first
and most consequential decision in the whole exercise — because each artifact
here is written against it. The Security Target's TOE, the hazard analysis's
safety item, the traceability matrix and the coverage obligation all mean the
same thing, and that thing is defined in
[`tools/common/data/certification-item.json`](../../tools/common/data/certification-item.json) and
enforced by `tools/common/check/check-item-boundary.py`.

A boundary that lives only in a document is a boundary that has already moved.

---

## 1. Why the item is not Ferrix

Ferrix's acceptance test is that it hosts `rustc`, and since 2026-09-23 that it
builds itself. That goal is why the system is worth anything, and it is also
exactly why the whole of it cannot carry an assurance rating. A certified item
must be a frozen, analysable configuration with every line traced to a
requirement; a self-hosting general-purpose OS with a Wayland compositor, a
browser and a package of shell themes is the opposite of that, on purpose.

`docs/ARCHITECTURE.md` §1 argues for a monolithic core with capability seams
because that is what a compiler workload needs. That decision and a
certificate over the whole kernel are mutually exclusive. The way out is not to
abandon either one: it is to notice that the architecture already contains the
seam a certificate needs, and to say where it is.

**The item is therefore a subset of the kernel, and the rest of Ferrix is
uncertified load running on top of it.** This is the same shape as every
certified separation kernel, and the same shape 62304 §4.3(c) has in mind when
it permits software items of different classes in one system.

---

## 2. Three rings

The manifest puts every one of the kernel's source files into exactly one of
three nested rings, and, since 2026-10-02, two library crates outside the
kernel into the `item` ring whole (`crates` in the manifest; see *btrfs* below).
A file in no ring fails the build, and so does a file of a classified crate
that its module tree does not reach.

| Ring | Product lines | In-kernel test lines | Carries |
|---|---:|---:|---|
| `core` | 56,644 | 18,815 | EAL6+, ASIL D, SIL 3/4, DAL B — *aspirational* |
| `item` | 8,144 | 3,795 | EAL5+, DAL C, Class C, SIL 2 — *the present claim* |
| `load` | 64,036 | 37,745 | nothing |

| Library crate | Ring | Product lines | Host-test lines |
|---|---|---:|---:|
| `ferrix-btrfs` (`src/lib/fs/btrfs`), the reader | `item` | 8,272 | 6,031 |
| `ferrix-btrfs-write` (`src/lib/fs/btrfs-write`), the write path | `item` | 6,019 | 2,247 |
| `ferrix-btrfs-vfs` (`src/lib/fs/btrfs-vfs`), the VFS glue | `load` | 2,185 | 1,822 |

**The certified item is `core` + `item` and the item's crates: 79,079 lines of
product code** -- 64,788 in the kernel and 14,291 in the two btrfs crates --
against 64,036 lines of uncertified load in the kernel. The kernel's own part
is 50.3% of its product code. A crate's product lines are the files its module
tree reaches from `lib.rs`, other than through a `#[cfg(test)] mod`; those are
host tests, `cargo test` and Miri, never in the image, and are counted apart.
(Re-measured 2026-10-02 on main at 5625d22f with
`check-item-boundary.py --report`, when btrfs joined the item. The kernel's
rings moved with the work since 2026-09-27 -- the discovery relayout, the
namespaces, System V IPC -- which this table does not attribute line by line.
Measured 2026-09-26, after W-5 moved the Linux dispatcher's routing and
five of the personality's files out of the `item` ring, see below, and after
F-23 made the item's allocations fallible. That added 2,711 lines, most of
them in the `core` ring: `fallible.rs` and `mm/reserve.rs`, which the manifest
places there, and the conversions. F-10's x86-64 pass added 16 product lines,
wiring its checks in, and 387 lines of self-test. Re-measured after F-36,
whose table list and check added 305 lines to `core`, and F-23's signal-table
fix, 109 to the load, beside the coverage work's checks. Re-measured after
F-35's job quotas, which added 1,275 lines to `core` -- `object/quota.rs`
and the charging at each site -- 129 to `item`, the native calls that set and
read them and the boot's `quota` line, and 102 to the load: cgroupfs's
controllers and the personality's calls at fork, thread and reap.
Re-measured after F-37 charged the Linux personality's heap to the job:
the tree stood at 58,054 item lines -- 49,376 of them `core` -- and 48,596 of
load before it, the vDSO and the other work since F-35 included. F-37 added
153 lines to `core` -- `object/quota.rs`'s bytes and kernel-heap account,
and the charges on a VMO's mappers -- 30 to `item`, the boot's `kmem` line
and the quota line's heap, and 447 to the load, the charge at each of its
sites; and 445 lines of self-test. The charging in the libraries the load
calls, `src/lib/kernel/kmem` among them, is outside the kernel and not counted here.
Re-measured 2026-09-27 with F-10's coverage evidence, on main at 7db6e8e8: the
table above, after the checks F-10's passes wrote, F-38's quarantine, F-40's
limit right and the other work since F-37, and the kernel relayout, which put
the SoC code in `platform/` and the Arm peripherals in `arch/arm_common/`.)

### `core` — the minimal trusted base

Memory protection, scheduling, capability objects, the trap and syscall entry
paths, the IOMMU, SMP, the MMU and CPU control for three architectures, the
firmware tables that say where the CPUs and timers are, the device registry
that makes an MMIO claim exclusive, and the panic path.

A device's claim by a driver's control channel (`claim.rs`) is exclusive
too, but for one caller: the input core may claim the STM32 USB host's node
once per keyboard or mouse, up to eight, because an input channel carries
only ports, never memory, and the node's `MANAGE` handle is one driver's,
so the extra claims open no DMA path; a core whose channel carries DMA
memory stays at one claim per device.

This is the code that must be correct for isolation to mean anything. Nothing
here may depend on a filesystem, a network stack or a device driver, and
`check-item-boundary.py` asserts that rather than trusting it.

### `item` — the core, plus what brings it up and dispatches into it

Bring-up (`main.rs`, `init.rs`), the system call dispatcher's way in
(`syscall/mod.rs`: the native range, and a Linux number decoded with its
Spectre clamp), the native ABI (`native`), the program file init starts
(`program`), PCI enumeration, `devmgr`, the entropy source and power. The pid
table was the item's `syscall/registry.rs` until W-1 moved it into the core
(`object/process.rs`); what that file keeps is the Linux personality's typed
lookup, and it is in `load` with the rest of the personality.

**What left the item ring on 2026-09-26, and why (W-5).** Until then this list
also held `memory`, `thread`, `limits`, `system` and `futex`, as "syscalls
that belong to the item rather than to the Linux personality", and the item
ring held the Linux dispatcher's routing. The resolving gate showed each of
them reaching into the personality for its state (F-09), and asked whether
the state belonged in the core or the file in the personality. Each is the
personality's:

| File | What it is | Why it is not the item's |
|---|---|---|
| `syscall/linux.rs` (was the body of `syscall/mod.rs`) | the Linux dispatcher's routing: `exit`, `clone`, `execve`, then every table | it names 21 of the personality's modules; the item keeps the decode and hands the call on through a `Personality` trait it defines, which `main.rs` composes with it |
| `syscall/memory.rs` | `mmap`, `munmap`, `mprotect`, `mremap`, `msync`, `madvise`, `brk` | argument decoding, by its own account, onto the core's `AddressSpace`, which is where a mapping is refused or made |
| `syscall/futex.rs` | `futex(2)` | Linux's operations and timeouts; the native ABI waits on objects and ports, never a futex |
| `syscall/limits.rs` | `getrlimit`, `setrlimit`, `prlimit64`, `sched_*` | the POSIX process's limits and credentials; the quota the ST claims (FRU_RSA.1) is the job's, in the core (`object/quota.rs`, F-35) |
| `syscall/system.rs` | `uname`, `sysinfo`, `sethostname`, `syslog`, `reboot` | checks over POSIX credentials, which the ST claims nothing about; `power`, which `reboot` reaches, stays |
| `syscall/thread.rs` | the POSIX thread | since W-1 the scheduler holds a `UserThread`; only the Linux dispatcher needed this |

Nothing the item's claims rest on moved: the Security Target names the Linux
personality a threat agent outside the TSF (§3.2), every call these files make
into the core is checked there, and the Spectre clamp stayed in the item, in
front of the table. What the move does change is honest scope: the item
ring's product code went from 10,578 lines to 8,002, and the coverage and
traceability evidence scoped to it has to be read against the new boundary.
Nothing in the core or the item names the five files, so the move needed no
code change and left no edge.

The native ABI is here rather than in `core` because it is the interface the
item *exports*, and an interface is evaluated with the thing that exports it.

Where the item has to act on the load -- power commits a filesystem before the
machine stops, init starts a program from one, `devmgr` reads its drivers
from one, device enumeration asks board support what it prepared, the native
ABI makes a ring's control channel or a native process, a Linux call is
answered -- the item defines the interface and the load registers into it
(`src/kernel/src/hooks.rs`, `syscall::native::serve`), or implements a trait the
item defines and `main.rs` composes the two (`syscall::Personality`). `main.rs` is the crate root: it declares every
module, and its `register_load` is the one place the load is told to
register, in bring-up order, with a boot check that it did.

Pid 1's inputs cross the same way (2026-10-04). The program init starts
when nothing is named, the script for its `sh -c` and the list of commands
were compiled into the kernel until then; an image carries them in its
initramfs under `.ferrix/init/` instead, so that one kernel serves every
test. The load reads the archive -- `fs::init`, over `ferrix-vfs`'s
`initramfs::init_entries`, which also keeps `ferrix-vfs`'s unpacker from
creating any of it in either root -- and hands each entry to
`init::set_inputs`, an interface `init.rs` defines and accepts once. `init.rs`
judges the entries itself (its refusals are `L.init.3`) and names no
filesystem or archive crate.

**`main.rs` is in the item, and it is the composition root.** The manifest
puts it in the `item` ring as bring-up, and nothing about that is changed
here. But it is also where the load is put together with the item. Since
2026-09-26 the gate reads its calls into the load -- 27 modules on
2026-10-03 -- and records them under `composition_root` in the manifest
rather than in the debt register: ratcheted the same way, filed against no
finding. Of the 27, 7 are the load's own boot self-checks (`fs::check`,
`net::check`, `fs::btrfs_write_check`, `fs::btrfs_powerfail`,
`fs::mmap_check`, `fs::procfs::check` and
`interfaces::block_ring::driver_check`), and 20 are product modules:
registration (`syscall::launch`, `syscall::linux`, `syscall::deliver`,
`syscall::seccomp`, `platform::st::stm32mp1`,
`platform::google::gs201::usb`, `fs`, `fs::procfs`, and since W-5
`block_ring`, `net_ring`, `render`, `input`, `audio` and `logctl`), and the
load's subsystems brought up in order (`fs::root_disk`, `fs::data_disk`,
`fs::home_disk`, `net`, `syscall::time`, `display`). The manifest's list is
the authority; this sentence is recounted from it.

That is a judgement an assessor has to accept, and it is only as good as the
claim that those edges carry composition and no item logic. It is plausible
from the list; it is not checked, because the exemption covers the file, and
`main.rs` is 3,399 lines. Making it checkable means reducing the root to
composition, with bring-up logic in item-ring modules that name nothing above
them, and the boot checks that drive the load in verification files.

A `mod` declaration is not counted as an edge anywhere. A parent declaring
its child says where the child sits in the module tree, not that the parent's
code runs it: `main.rs` declares 10 load-ring modules and `syscall/mod.rs`
declares 36. What either file's *code* then does with them is resolved and
counted like any other reference.

### btrfs — two crates in the item, and where it meets the VFS

On 2026-10-02 the customer decided that btrfs is certified for as long as it
runs inside the kernel. Its on-disk logic is two `no_std` crates outside
`src/kernel`, so the manifest classifies crates as well as files, and puts
both in `item`:

| Crate | Below it | Above it |
|---|---|---|
| `ferrix-btrfs`, the reader | the `Device` trait (`ferrix_btrfs::volume::Device`): sector reads, answered by the kernel's block layer through `Disk` in `src/kernel/src/fs/btrfs.rs`; and memory its caller lends, `ExtentBuffers` and `ChunkStorage` -- the crate allocates little of its own | `Volume`, and the format types it hands out: superblock, chunk map, keys, items, the readers of `fs` and `compress` |
| `ferrix-btrfs-write`, the write path | the `WriteDevice` trait (`ferrix_btrfs_write::WriteDevice`, a `Device` that also writes and flushes), implemented by the same `Disk`; and `ferrix-btrfs` | `WriteVolume`: its transactions and its inode, directory and extent operations |

**What stays load, and why.** `ferrix-btrfs-vfs` and `src/kernel/src/fs/btrfs*.rs`
are not in the item. The first implements the VFS's `FileSystem` and `Inode`
over `Volume` and `WriteVolume`; the second mounts it, implements `Device` and
`WriteDevice` over the block core, and runs the boot checks. Both are written
against the VFS (`ferrix-vfs`, `src/kernel/src/fs/`), which is load, so neither
can be item without the VFS coming with it. **The interface between btrfs and
the VFS is therefore the two crates' public API**: `Volume` and `WriteVolume`
and the types they take and return, called from the load, and `Device` and
`WriteDevice`, implemented by the load. Nothing in the item names the VFS. What
the item is to vouch for at that interface is what the crates do with the bytes
a `Device` returns, and what a `WriteVolume` writes back: that a corrupt or
hostile volume gives an error and never stops the machine. What holds is that
every field is read through a checked slice access, in crates that are
`#![forbid(unsafe_code)]` and deny indexing and explicit panics
(`ferrix-btrfs`'s *Totality*), and since 2026-10-02 that no allocation in
either can stop it: the write path's allocations report failure, and an
allocation sized from the disk that exhausts the heap fails the mount or
aborts the transaction with `ENOMEM` (`H.STORE.7`; what that costs other
partitions is VULNERABILITY-ANALYSIS.md's T.EXHAUST path 6). **The target,
not yet the fact,** is the rest: the release profile has no overflow checks
and the workspace no arithmetic lint, so "every length is checked
arithmetic" rests on care rather than on a gate (`L.btrfs.23`, F-56, the
crates' missing evidence; `FINDINGS.md`). What it does not vouch for is that the
load calls it correctly: the VFS's locking, its caching of what btrfs returned,
and the Linux calls that reach it.

The boundary gate holds the crates at their manifests: an item crate may
depend only on item and core crates or on an allowlisted piece of `no_std`
infrastructure the core already links (`crates.infrastructure_allowlist`:
`ferrix-fallible` and `ferrix-sync`, each with its reason; an allowlisted
crate's own dependencies are held to the same rule once an item crate
depends on it, which `ferrix-btrfs-write` does on both since the allocation
conversion). `ferrix-btrfs-write` on `ferrix-btrfs` passes; a load
crate as a dependency fails, which a negative control showed when the rule was
written. A kernel file naming one of the crates is an edge to the `item` ring
like a `crate::` path, so the core cannot name btrfs. The item-scoped gates --
complexity and recursion, fallible allocation, the unsafe trace, and the
panic-exemption count -- read the crates' product code from 2026-10-02. What
they found then was recorded as debt, not waived: 166 allocations in the
write path that could not report failure, converted the same day, and 19
calls in the reader the gate counts by name that allocate nothing, argued
`NOALLOC:` (both crates are at 0 since); and 40 functions over the
complexity floors (18 and 22), none recursive, 43 since the conversion
re-recorded them. Neither crate has an `unsafe` site or a panic
exemption.

The DMA and interrupt decisions the item makes about PCI rest on three more
crates the manifest does not classify, `ferrix-pci`, `ferrix-acpi` and
`ferrix-paging`: the DMAR's scopes are parsed by `ferrix_acpi::dmar`, and
since 2026-10-02 whether a function is its own requester -- the rule that
decides whether it gets an IOMMU domain (`L.iommu.45`) -- and which of an MSI
capability's registers mask it (`L.device.23`) are `ferrix_pci::topology` and
`ferrix_pci::msi`, and since NVIDIA's N0d which bytes of its configuration
space a driver may write (`L.device.26`) is `ferrix_pci::window`. Also since
2026-10-02 (F-58), which VT-d table writes are
cleaned to memory before a unit that does not snoop is told of them is
`ferrix_paging::coherence`: `Unpublished`, the record of writes not yet
cleaned, and `Walked`, the `unsafe impl PhysMem` that puts the mapper's own
writes and fresh tables through it (`L.iommu.56`, `L.iommu.57`). And since
NVIDIA's N0g, the descriptors of the VT-d invalidation queue every
invalidation goes through, and the queue registers' encodings, are
`ferrix_paging::vtd::queue` (`L.iommu.47`, `L.iommu.48`); and the
interrupt remapping entries, the remappable messages and I/O APIC entries
that name them, the fault reasons and the isolation rule are
`ferrix_paging::vtd::remap` (`L.iommu.50`, `L.iommu.53` to `L.iommu.55`,
`L.x86_64.129`, `L.x86_64.130`, `L.x86_64.132`), with the I/O APIC's source
ID matched by its identifier in `ferrix_acpi::dmar` (`L.x86_64.130`). Their host
tests trace to those requirements, but the item-scoped gates do not read
them; `docs/BACKLOG.md` has the row that classifies them.

Two open points are for the assessor rather than settled here. The two
allowlisted crates are linked by the core today without being classified at
all, which this change makes visible rather than causes; classifying them
`core` is the natural next step (F-56; `docs/BACKLOG.md`). And the coverage evidence, which is measured on
the kernel image, does not yet reach the crates, whose tests run on the host;
the traceability matrix reaches them since the same landing (`L.btrfs.*` and
`H.STORE.*`, traced to the crates' host tests), with the requirements no test
verifies yet in its baseline.

### `load` — everything it runs and does not vouch for

The VFS, btrfs's glue into it (`ferrix-btrfs-vfs` and the kernel's
`fs/btrfs*.rs`), procfs, sysfs, cgroupfs and tmpfs; the network stack; the
Linux personality's syscall surface; the display, render, input and ring
drivers' kernel halves; STM32MP1 board support.

This is not a list of code that matters less — it is most of what makes Ferrix
useful. It is excluded because a defect in it is bounded by the item's own
enforcement, and because a claim over 103,397 lines is one nobody can afford to
substantiate.

---

## 3. Why the rings nest

The obvious alternative was to draw one boundary for the four present ratings
and a second, tighter one later if EAL6+ or ASIL D were ever wanted. That is
the expensive mistake. Every artifact in this directory is scoped to the item;
re-scoping later means rewriting the Security Target, the hazard analysis, the
trace matrix and the coverage evidence, because none of them mean anything
detached from a boundary.

Nesting makes the boundary a **ratchet**. Raising the target becomes a matter
of moving modules from `item` to `core` and paying down the findings in §4 —
not of starting the paperwork again. `core` is named now, while naming it is
free, precisely because the cost of discovering the right boundary later is
every document written against the wrong one.

The same nesting is what makes the ratings honestly *ordered*. `core` at
46,891 lines is in the size range where EAL6-grade work has actually been done
(INTEGRITY-178B, ~10k SLOC, is the benchmark and is still nearly five times
smaller).
It is not there yet. Saying which ring carries which target keeps that gap
visible instead of letting "Ferrix is certified" absorb it.

---

## 4. What the measurement found

The boundary above is a claim about dependencies, so the gate measures it.
Today the item contains **no upward references** -- no place where a ring
names something in a ring above it -- beside the composition root's 27 (§2).
When the audit began it had 94, in 28 files, by today's measure; each was
recorded in the manifest against a finding id and analysed in
[FINDINGS.md](FINDINGS.md), and each finding closed when the build said so.

**The counts this section gave before 2026-09-26 — 29, and 62 when the audit
began — were lower bounds.** The gate matched only the literal text
`crate::a::b`, so it missed nested `use` groups, paths through a name bound by
`use` or declared by `mod`, and every path in code its string pattern had taken
for a literal. It now resolves names as the compiler does
(`tools/common/check/check-item-boundary.py`, whose docstring says how, and what it still
cannot see: an edge that is a type flowing through a value rather than a name
written in the file). Re-measured the same day, the tree had 56 where 29 were
reported, and the audit's starting tree 94 where 62 were.

They were not a reason to move the boundary. They were the reason the boundary
is worth having: each was a specific, addressable piece of coupling that was
invisible while the architecture was described in prose. They were paid down
by F-02, F-02a, F-03, F-04, F-05 and F-08; by F-01 and F-06, splitting the
process (W-1); and last by F-07, F-09 and F-33 (W-5):

* **F-07** — the native ABI named ten load-ring modules, and `devmgr` two. The
  six calls about a subsystem above the item are a table the subsystems
  register into, a native process is made through what the personality lends,
  and every boot checks the table is full.
* **F-09** — the Linux personality in the item ring. The trap entries reach the
  dispatcher through the core; the Linux dispatcher's routing is the load's,
  behind one registered pointer; and five personality files moved to the load
  ring, argued in §2. The item ring shrank by 2,576 lines for it.
* **F-33** — the x86-64 paranoid entry's boot check moved to a verification
  file.

`object/` and `sched/` name nothing above the core since W-1: a task holds a
`sched::UserThread` and the core holds a process whole only as an
`object::process::Host`, a trait the personality implements. `trap.rs` — the
most trusted file in the kernel — names nothing above the core, which it did in
three places when the audit began, and since W-5 neither do the architectures'
system call entries.

The gate's debt register is empty and stays in the manifest: a new upward
reference fails the build until somebody adds it against a finding, which is a
diff somebody has to argue for. What the gate cannot see is still what its
docstring says -- a load-ring value reaching the item through a type rather
than a name -- and the composition root, which is exempt by file and argued in
§2 rather than checked.

---

## 5. The reference configuration

A certificate attaches to a configuration, not to a repository.

| | |
|---|---|
| Architectures | x86-64, AArch64, ARMv7-A |
| Profile | release |
| Toolchain | rustc 1.97.1, pinned exactly in `rust-toolchain.toml` |
| Unstable features | none in `src/kernel/` or `src/boot/common/uefi/` |
| Cargo features | 7 in the workspace, **0** in `src/kernel/` or `src/boot/common/uefi/` |
| Build settings | **one**, `cargo xtask --mitigations on\|off`; the reference is `on`, the default |
| Boot options that change the item's work | **one**, `ferrix.devmgr=kernel\|init`; the reference is `kernel`, the default |
| Test platform | QEMU; on x86-64 the patched 10.2.1 `(ferrix-cfi)`, whose VT-d blocks compatibility-format interrupts, with `intel-iommu,intremap=on,eim=off` and, under KVM, the split interrupt controller ([TOOLS.md](TOOLS.md)) |
| Kernel link (`on`) | a static PIE on x86-64 (PIC code model, every x86-64 crate) and AArch64 (static code model, `-pie -z notext`); on ARMv7-A a fixed-address link that keeps its relocations (`--emit-relocs`). The loader moves it each boot (KASLR). `off`: the static fixed-address image |
| External crates | 21, listed in [SOUP.md](SOUP.md) |
| Assembly | 500 lines across 22 allow-listed sites outside the Pixel 7 loader, ~99.76% Rust (`check-asm-budget.py`) |

The feature count is the line worth pausing on. A certified item must be one
configuration with all dead and deactivated code justified; Linux's ~18,000
Kconfig symbols are why that objective is unmeetable there at any budget. Here
the configuration space is three architectures and one switch with two
settings, which is most of why this item is analysable at all.

The switch is the side-channel defences and KASLR
([SPECULATION.md](SPECULATION.md)). `off` builds with
`--cfg ferrix_mitigations_off` and the static relocation model, set only by
`xtask`, compiles every defence out and links the kernel at its fixed address; it exists to measure what they cost and for owners
who have decided they need none. It is not a Cargo feature, and the claims
here are made of `on` alone (SAFETY-MANUAL AoU-8). `cargo xtask check` builds
the kernel in both settings on all three architectures so that `off` cannot
stop compiling unnoticed, and the running kernel says which it is.

The boot option is who starts `devmgr` (`docs/INIT.md` §7.3, landing L12).
Under `kernel`, the reference, the kernel starts it at bring-up, runs the
disk checks of stages 10 to 12 through its drivers, and switches `/` to the
root disk before pid 1 exists. Under `init`, which the images that boot
`/sbin/init` use, the kernel gives pid 1 a one-shot starter (`Object::Starter`,
`MANAGE` alone, so it can never leave pid 1's table) and starts `devmgr`
itself when pid 1 asks with `devmgr_start`, in the job pid 1 names: the
DEVICES channel, which carries every device's authority, goes between the
kernel and `devmgr` alone, and pid 1 gets a process handle that reaches
nothing inside it. The disk checks of stages 10 to 12 are not run on those
boots, and `/` is switched after `devmgr` has reported; the switch then
**re-roots pid 1**: its root and working directory become the volume's in
the step that publishes the new root, under pid 1's own filesystem lock.
That is the one process the kernel changes the root of from outside, once,
and only under this option (`fs::root_disk`). Every start is checked as it
is made (FX-1009), and so is the re-root (FX-1202). No claim is made of
`init` (SAFETY-MANUAL AoU-8, AoU-13); the boots that carry the evidence keep
`kernel`.

---

## 6. What this item deliberately does not claim

* **It is not a separation kernel.** It does not yet offer time or space
  partitioning as a service; stage 13's namespaces and stage 14's real-time
  domains are unbuilt. `docs/ARCHITECTURE.md` §5 is explicit that no certified
  worst-case execution time is promised for a kernel that also hosts LLVM.
* **It carries no field history.** Every rating here is argued from
  construction and verification evidence. The proven-in-use route that IEC
  61508 route 2s and EN 50716's prior-use provisions open to Linux is closed to
  a kernel this young, and nothing in this directory pretends otherwise.
* **It has not been assessed by anyone independent.** See
  [FINDINGS.md](FINDINGS.md) §Organisational, where that is recorded as the
  finding it is rather than omitted.
