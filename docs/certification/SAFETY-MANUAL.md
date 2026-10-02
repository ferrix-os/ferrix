# Safety manual

The certified item in [ITEM.md](ITEM.md) is developed as a **safety element out
of context**: a component with no application of its own, certified against
*assumed* safety requirements and shipped with the conditions an integrator
must discharge to use the result.

This is how a general-purpose kernel is certified at all. QNX, PikeOS and
VxWorks 653 all ship a manual of this shape, because none of them knows what
their customers are building either. Every standard has a name for the route:

| Standard | Name |
|---|---|
| ISO 26262 | Safety Element out of Context (Part 10) |
| EN 50716 / EN 50128 | Generic software, with exported application conditions |
| DO-178C | Reusable Software Component (§12.1.4, FAA AC 20-148) |
| IEC 61508 | Certified element with a safety manual |
| IEC 62304 | Supplied component evidence; the device manufacturer still owns §7 |

**The obligation is not removed, it is relocated.** A system hazard analysis
still has to happen. It happens at the integrator, and this item's certificate
— were one ever issued — would be valid only inside the assumptions in §4.

---

## 1. What the element is

| | |
|---|---|
| Element | Ferrix certified item, `core` + `item` rings |
| Size | 79,079 lines of product code: 64,788 in the kernel, 56,644 of it in `core`, and 14,291 in the two btrfs crates ([ITEM.md](ITEM.md) §2, 2026-10-02) |
| Scope | memory protection, scheduling, capability objects, trap and syscall entry, IOMMU, SMP, device enumeration; since 2026-10-02 the btrfs reader and write path (`ferrix-btrfs`, `ferrix-btrfs-write`) |
| Not in scope | VFS, the btrfs glue on it (`ferrix-btrfs-vfs`, `src/kernel/src/fs/btrfs*.rs`), network stack, Linux personality, ring-3 drivers — 64,036 lines of uncertified load in the kernel |
| Reference configuration | x86-64, AArch64, ARMv7-A; release profile; rustc 1.97.1; zero Cargo features; built `--mitigations on`, the default |

The boundary is enforced on every build by `tools/common/check/check-item-boundary.py`, so
what this manual describes and what ships cannot drift apart silently. The
manual's own claims are held the same way: every requirement and failure mode
below names its evidence in `tools/common/data/safety-requirements.json`, and
`tools/common/check/check-safety-requirements.py` fails the build when a citation stops
resolving or when the manual and the register disagree about which ids exist.
It caught a wrong citation the first time it ran.

---

## 2. Assumed safety requirements

What the element assumes a safety application will need of an isolation kernel.
These are *assumed*, not derived — that is what out-of-context means. An
integrator whose system needs something else must say so (§4, AoU-1).

| Id | Assumed requirement | Implemented by | Evidence |
|---|---|---|---|
| ASR-1 | A partition shall not read or write memory belonging to another partition or to the element. | per-process page tables, `user/space.rs` | `spaces` check on every boot; SMEP+SMAP (x86-64), PAN (AArch64) |
| ASR-2 | No mapping shall be simultaneously writable and executable. | map-time enforcement | every boot sweeps all mappings: *1,874 swept, 387 executable, none writable* |
| ASR-3 | A partition shall reach a resource only through a capability it holds. | `object/`, handle tables | `object/check.rs`, 135 refusal assertions |
| ASR-4 | A device shall not access memory outside the region its driver was granted. | VT-d, SMMUv3 | *9 PCI functions behind one unit, 0 bypassing, 0 unresolved* |
| ASR-5 | Memory released by one partition shall not be readable by the next. | `mm::zero_frame` on allocation | called at three sites in `user/vmo.rs` |
| ASR-6 | On detecting an inconsistent internal state, the element shall enter its safe state rather than continue. | `panic.rs` with a catalogued explanation | `check-panic-audit.py`; the safe state is defined in §3 |
| ASR-7 | Data supplied by a partition shall be validated before use. | `syscall/uaccess.rs` | 427 refusal assertions in `syscall/check.rs` |
| ASR-8 | Admission of a real-time workload shall be refused when the set is unschedulable. | EDF with CBS admission | **partially met** — see AoU-4 |
| ASR-9 | Data the element has stored on a volume shall be returned as it was stored or refused with an error, never returned altered, and what a commit has reported durable shall survive a loss of power. | the btrfs crates: checksums on every node and data sector, copy-on-write, a flush before the superblock | host tests of both crates, with 100 power cuts rebuilt from a device's record of its writes — see AoU-15, AoU-16 |

ASR-1 to ASR-7 are met on the reference configuration, with the architecture
exceptions in §4. **ASR-8 is partially met** and is the one an integrator must
read most carefully. **ASR-9 is met at the btrfs crates' interface**,
`Volume` and `WriteVolume`, for a single-device volume of `SINGLE` or `DUP`
chunks with CRC-32C checksums, under AoU-15 and AoU-16. *Altered* means
changed by accident -- a flipped bit, a torn or misdirected write, a stale
block: CRC-32C is not a cryptographic check, and a volume written by an
attacker can carry checksums that match (AoU-15). When a program's write
becomes a commit is the VFS's choice, which is load (AoU-3): ASR-9 promises
what `WriteVolume::commit` and `commit_log` keep, not what `fsync` through a
mount does.

### Why the element uses `unsafe`

A kernel cannot be written without `unsafe`: storing to a page table or a
device register is the program. So every `unsafe` block, `unsafe impl` and
`unsafe fn` in the element says which of the obligations below it discharges,
and through it which requirement, failure mode or assumption of use above it
serves (finding F-26). The set is closed and was derived from what the
element's unsafe sites do, not written first: a site that fits none of them is
a reason to look at the site before it is a reason to add an id.

The id opens the site's `SAFETY:` comment, `// SAFETY: (DEVICE) ...`, or an
`unsafe fn`'s `# Safety` section, `/// (TRANSLATE) The caller ...`.
`tools/common/check/check-unsafe-audit.py` refuses an id that is not in this table
and holds the untagged remainder to a baseline that may only shrink; this table
and `unsafe_obligations` in `tools/common/data/safety-requirements.json` are held
to each other by `check-safety-requirements.py`.

| Id | Obligation | What it covers | Serves |
|---|---|---|---|
| `(TRANSLATE)` | address translation | installing or removing a translation root, TLB invalidation, dropping the identity map, a page-table descriptor read or written in its frame | ASR-1, ASR-2, FM-1, FM-2 |
| `(PROTECT)` | protection and speculation controls | SMEP, SMAP, UMIP, PAN, the user-access window; speculation-control registers, predictor and buffer flushes, return-stack filling, SSBS | ASR-1, FM-1, AoU-11 |
| `(USER-COPY)` | a partition's memory | a system call's copy to or from a partition, through the frame its own tables name, with its space's lock held (V-01) | ASR-1, ASR-7, FM-1 |
| `(FRAME)` | frame contents through the direct map | a memory object's pages; a frame cleared before it is handed out; a copy-on-write copy; a start block or trampoline another processor starts from | ASR-1, ASR-5, FM-1, FM-5 |
| `(DMA)` | memory a device reads or writes | IOMMU tables and queues, interrupt translation tables, virtqueue rings and buffers, a pinned or quarantined frame | ASR-4, FM-4, AoU-12 |
| `(DEVICE)` | device registers | a register window or I/O port of a device the element drives itself -- console, timer, interrupt controller, IOMMU unit, framebuffer -- claimed by it alone | ASR-4, ASR-6, FM-4, FM-6 |
| `(CONTEXT)` | execution context | preparing and switching kernel stacks; saving and restoring a program's registers, floating-point, thread-pointer, segment and TLS state; entering and resuming user mode, a signal frame included | ASR-1, FM-1, FM-9 |
| `(ENTRY)` | trap and system-call entry | the GDT, IDT and TSS, the vector base, the system-call registers, the privilege and interrupt stacks | ASR-6, ASR-7, FM-6, FM-9 |
| `(SYSREG)` | processor registers | the running processor's own state that is neither translation nor protection: interrupt masks, barriers, cache maintenance, idle hints, identification, counter, timer, debug and scratch registers, the interrupt controller's CPU interface | ASR-6, FM-6 |
| `(FIRMWARE)` | firmware and platform calls | PSCI and SMCCC through `hvc` or `smc` -- power, reset, starting a processor, workarounds, the TRNG -- a reset port, the triple fault, the emulator's exit port | ASR-6, AoU-2 |
| `(SHARED)` | shared kernel state | an `UnsafeCell`, raw pointer or `Sync` claim whose exclusion or lifetime is argued: single-threaded early boot, masked interrupts, a lock taken or released by hand, a processor's own record, a `'static` published once, the interrupt and preemption controls the locks rest on | ASR-6, FM-6 |
| `(KMEM)` | kernel memory ownership | the global allocator and the heap's pages, the page array, vmap buffers, kernel stacks and their release, a box taken back from a raw pointer, the loader's memory given back | FM-6, FM-7, FM-9 |
| `(BOOT-DATA)` | what the loader and the image hand over | the boot information, the ACPI tables and the device tree, the kernel's own text, the vDSO's bytes | AoU-8, FM-6 |
| `(PROBE)` | deliberate faults in self-checks | a breakpoint, a debug-register trap, a write that must fault and be mapped on demand, an access through a space under test | ASR-1, ASR-6 |

Measured 2026-09-27: all 663 of the element's unsafe sites carry an id -- 123
`CONTEXT`, 118 `SYSREG`, 87 `SHARED`, 66 `ENTRY`, 60 `TRANSLATE`, 55 `DEVICE`,
29 `FIRMWARE`, 29 `PROTECT`, 28 `KMEM`, 24 `FRAME`, 22 `PROBE`, 11 `DMA`, 7
`BOOT-DATA`, 4 `USER-COPY` -- and the gate prints the current counts on every
run.

The obligation says what a site must get right, not that it does: the prose
after the id is the argument, and a reviewer reads it. What the id adds is
the direction an assessor needs -- from a requirement to every site whose
soundness it rests on, `grep 'SAFETY: (TRANSLATE)'` -- which the prose alone
could not give.

---

## 3. Safe state

**The element's safe state is a halted processor with a diagnostic on the
serial console and, where firmware left a framebuffer, on the screen.**

It is entered on any detected internal inconsistency: a failed boot self-check,
a failed invariant, an allocation failure at bring-up or in the uncertified
load (§4, AoU-5), or an unhandled kernel fault. The report names the condition
and carries a catalogued explanation.

**AoU-2 below is the obligation this creates.** A halt is only a *safe* state in
a system where stopping is safe. In a system where the controlled process must
keep being controlled — a moving train, an infusion in progress — the
integrator must provide an external mechanism: a watchdog, a hardware
interlock, a redundant channel. The element does not fail over, does not
restart itself, and does not degrade gracefully.

---

## 4. Assumptions of use

Every one of these is an obligation on the integrator. A certificate over this
element would be void outside them.

### AoU-1 — the system hazard analysis is the integrator's
The element assumes the requirements in §2. The integrator shall perform the
system-level hazard analysis (ISO 14971, EN 50126, ARP4761 as applicable),
apportion safety requirements to software, and **verify that the apportioned
requirements are a subset of §2**. Where they are not, the element does not
cover the difference.

### AoU-2 — halt must be safe, or be made safe
See §3. The integrator shall ensure that a halted processor is a safe outcome
in the system, or provide external means to reach a safe outcome from it.

### AoU-3 — the uncertified load is untrusted
The VFS, the network stack, the Linux personality and all ring-3 drivers are
outside the element and carry no assurance claim. So is the btrfs glue the
VFS calls -- `ferrix-btrfs-vfs` and `src/kernel/src/fs/btrfs*.rs`, its page
cache, its commit interval and when an `fsync` commits -- though the btrfs
reader and write path beneath it are in the element since 2026-10-02 (ASR-9).
The integrator shall not place a safety function in the load, and shall treat
its output as untrusted input. The element's own enforcement is what bounds
their failure.

### AoU-4 — no worst-case execution time is provided
The element provides admission control, partitioned scheduling, bounded
critical sections on the real-time path and interrupts that cannot steal
unaccounted time. It does **not** provide a certified WCET, and
`docs/ARCHITECTURE.md` §5 is explicit that no OS which also hosts a compiler
can. An integrator whose safety requirement depends on a proven response time
shall establish it by measurement on their own configuration and workload, and
shall treat ASR-8 as unmet until they have. (Finding F-24.)

### AoU-5 — the heap is not bounded per partition, and exhaustion outside the element is fatal
The element allocates dynamically, and reports allocation failure at every
site in its own source. A native call answers `NO_MEMORY`, a Linux call
`ENOMEM` (`EAGAIN` from `madvise`), and the element carries on. The
exception since 2026-10-02 is the btrfs write path, which joined the element
with its allocations still infallible; they are being converted (`H.STORE.7`,
F-56), and until then a refusal there stops the element with FX-0008, as one
in the load does.
`tools/common/check/check-fallible-alloc.py` fails the build on an allocation that does
not report failure, and every boot proves the handling by failing allocations
under the native calls ([MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §1). Two
cases still reach the safe state of §3. An allocation failure during bring-up,
before the first program runs, stops the element with FX-0007. One in the
uncertified load after boot, whose allocations are not fallible and which
shares the element's heap, stops it with FX-0008 -- including the load code
two native calls run, `process_create` and `process_start`, to make a
process and its first thread (MEMORY-AND-TIMING.md §1.3 lists it).

Since 2026-09-26 the element bounds what a partition's programs hold, when
the partition is a job with limits set: the frames of their memory and their
page tables, the kernel heap the Linux personality holds for them (counted
against the same memory limit since F-37), the native objects they make and
their tasks, each refused at its limit while the other partitions go on
(F-35, F-37, `FRU_RSA.1`). It does not bound its own working set, kernel
memory held once per task beyond what the task limit implies, nor a few
machine-wide tables with fixed bounds of their own (V-05, low). The
integrator shall put each partition in a job of its own, with memory,
object and task limits whose sum -- heap and frames together, with the
per-task kernel stacks the task limits imply -- the machine can hold, shall
provision the heap so that exhaustion does not occur in
normal operation, and shall treat FX-0007 and FX-0008 as transitions to the
safe state. An application that runs on the element shall handle `NO_MEMORY`
and `ENOMEM` as an outcome of any call that allocates, and of a job at its
limit, not as an impossibility. (Finding F-23, closed for the element's own
allocations; F-35; F-37; V-05.)

### AoU-6 — ARMv7-A carries reduced claims
On ARMv7-A the element provides **no ASR-4** (the reference board has no IOMMU)
and no hardware backstop for ASR-1 (PAN is an ARMv8.1 feature; the Cortex-A7 is
ARMv7-A, so the software bound check in `uaccess` is the only barrier). An
integrator requiring either on that architecture shall not use it.

### AoU-7 — no authentication, and an audit of the element's own decisions only
The element provides no identification or authentication; POSIX credentials
live in the uncertified load. It records its own security decisions
([AUDIT.md](AUDIT.md)) against its processes and jobs, not people, and keeps
them only where its reader, pid 1, writes them. An integrator needing
authentication, or an audit of what people did, shall provide it above the
element. (Finding F-21b.)

### AoU-8 — the configuration is the one in §1
The claims hold for the reference configuration and no other. Changing the
toolchain, enabling a Cargo feature, building with `--mitigations off`,
booting with `ferrix.devmgr=init` (AoU-13), or moving a file between rings
changes what is claimed. The boundary gate makes
the last of these visible, and the boot log says which build it is — *"speculation
defences off: built with --mitigations off"* — so the third is visible on the
running system; the first two are the integrator's to control.

### AoU-9 — no independent assessment has been performed
No accredited laboratory, notified body or independent assessor has examined
this element. Every analysis in `docs/certification` was produced by the same
process that wrote the code. **This is the assumption most likely to be
unacceptable to an integrator**, and it is stated first among the residuals for
that reason. (Finding F-27.)

### AoU-10 — no field history
The element has no operational history. Proven-in-use and prior-use credit
(IEC 61508 route 2s, EN 50716's equivalent) are unavailable. (Finding F-30.)

### AoU-11 — the processor is one the side-channel defences cover
ASR-1's separation holds against speculative reads only on a processor that
offers what [SPECULATION.md](SPECULATION.md) builds on, and the element cannot
supply what the processor lacks. The integrator shall run the element, built
`--mitigations on`, only on processors whose boot log lines
`cpu      speculation exposure:` (on AArch64, one for each kind of core the
machine has) name nothing **NOT covered** and nothing **EXPOSED** — which on x86-64 means an IBRS form (enhanced, automatic, or
always-on), `IBPB`, `SSBD` unless the part says `SSB_NO`, and a part not
affected by Meltdown; on AArch64, for every core, `CSV2`, a core Arm lists
as unaffected by Spectre v2 (Cortex-A35, A53, A55), or firmware implementing
SMCCC `ARCH_WORKAROUND_1`, `SSBS` or `ARCH_WORKAROUND_2`, and a core Arm lists as
unaffected by Meltdown or reporting `CSV3`; on ARMv7-A, a core Arm lists as
unaffected, or firmware that set `ACTLR.IBE` on one that is not. On an
MDS-affected x86-64 part the integrator shall disable SMT. Partitions that
must not learn each other's cache access patterns shall not share a cache: no
cache is partitioned (V-06). QEMU's TCG, on which CI's gates and every Arm gate run, offers no
speculation controls and executes no speculation, so its log lines are not a
counter-example; the gate under KVM is where the controls are exercised.
On x86-64 the boot line `cpu      program register state:` must not name
Zenbleed or GDS as holding AVX back unless the integrator accepts programs
without AVX; and in a guest, whose verdict rests on the CPU model and
`IA32_ARCH_CAPABILITIES` the hypervisor presents, the integrator shall run
the element only on a host that mitigates Zenbleed and GDS itself
([SPECULATION.md](SPECULATION.md) §9).
For KASLR the integrator shall provide firmware offering `EFI_RNG_PROTOCOL`,
or on x86-64 a processor with `RDRAND`. The boot line `kaslr    image, direct
map and arena moved` must name one of the two, not the cycle counter. On
x86-64 the processor shall offer UMIP (`UMIP on`), without which `SIDT` reads
the image's slide. The integrator shall not rely on KASLR for separation: without
KPTI a program with a timer can locate the kernel, and no ASR rests on it; and
shall treat a program that can read the framebuffer the boot console drew on
as able to read the slide the log printed there. (Finding F-31.)

### AoU-12 — no device translates for itself
An unpin takes a page out of its device's domain and waits for the IOMMU's
invalidation to complete before the frame goes back (`object/pin.rs`), which
stops a device that asks the IOMMU on every access. A device with address
translation services (PCIe ATS) keeps translations of its own, which that
invalidation does not reach. The element never enables ATS: VT-d context
entries are written with `TT=00`, SMMUv3 stream table entries with `EATS=0`,
and PCI enumeration switches off any ATS capability firmware left on, and
refuses to boot a function that keeps it on (`pci.rs`, `keep_ats_off`). The
integrator shall not enable ATS on any device, nor configure its IOMMU to
accept translated requests, nor add a device-TLB invalidation path without
the element's.

A dead driver's pins on a translated domain are not given back until its
device's next driver has reset it and been accepted; until then they stay
mapped, their frames held and charged to no job. devmgr starts no driver again
after one that died before publishing, or once a device's restart budget is
spent, so a device keeps at most two drivers' pins that way. An explicit rebind
of a device whose drivers die before publishing keeps one more driver's pins
each time, up to the element's own cap: once a device's quarantine holds
133120 pages (520 MiB, two drivers' worst case), a new pin for it is refused
with `QUARANTINE_FULL` and the device stops working until a driver of it is
accepted or the element restarts. The integrator shall not rebind such a
device in a loop, and shall treat `QUARANTINE_FULL` as a device that has
failed. (Finding F-38.)

---

### AoU-13 — the disk checks of stages 10 to 12 run only when the kernel starts devmgr
Under `ferrix.devmgr=init` pid 1 starts `devmgr` (`docs/INIT.md` §7.3), so
the kernel has no driver to read a disk through before pid 1 runs, and the
boot checks of stages 10 to 12 that do -- `devmgr`'s own REPORT at bring-up,
the block driver, btrfs read and btrfs write -- are not run; the kernel says
so on the console (*"left to pid 1: the disk checks of stages 10 to 12 are
the kernel path's"*). The evidence for those stages comes from boots with the
reference setting, `ferrix.devmgr=kernel`. The integrator shall use the
reference setting, or accept that a boot with `init` does not prove those
stages itself.

### AoU-14 — one speculation domain holds only programs that may read each other
A job made with `JOB_SPECULATION_DOMAIN` is one speculation domain
(`docs/OPAQUE-KERNEL.md` §9). Between two programs born in it, a switch
leaves out the predictor invalidation, so either program may read the
other's memory speculatively (VULNERABILITY-ANALYSIS V-07). ASR-1's
separation holds against speculative reads only *between* domains, and
between a domain and every program outside one, the default for every job.

The integrator shall place in one domain only programs that may read each
other's memory: a driver and the client it serves, two halves of one service.
MANAGE on a marked job is the authority to start programs in it, and the
integrator shall hand that right only to a program trusted to place others
there. A program that moves between jobs, or stops being dumpable (a set-id
`execve`, a change of credentials, `PR_SET_DUMPABLE`), leaves its domain for
good, so a privileged program is never in one by accident. Partitions of
different criticality shall not share a domain. A marking is recorded as a
`DOMAIN` audit event, which is how an assessor finds every domain on a
running system.

### AoU-15 — a volume is untrusted input, and only as authentic as its medium
The element parses a btrfs volume as hostile input in ring 0: whatever its
bytes, the reader and writer answer an error rather than read or write
outside a buffer, panic or loop without bound (`H.STORE.1`), and a block or
sector whose checksum, address, level or generation is wrong is refused or
read from its other copy (`H.STORE.2`). A damaged volume is not an internal
inconsistency: it is answered with an error, and a failed write turns the
mount read-only at its last commit, never the safe state of §3. Two limits
follow, and the integrator shall design for both. CRC-32C detects accident,
not intent: whoever can write the medium, or a ring-3 block driver
answering reads, can supply a volume whose checksums match and whose
contents are false. The integrator shall not rely on stored data for a
safety function unless the medium is protected against deliberate change by
means of its own, and shall treat an I/O error from the volume as an outcome
of any read. And only volumes the writer maintains are written: one device,
`SINGLE` or `DUP` chunks, CRC-32C, skinny metadata, the free-space tree,
`NO_HOLES`, no subvolumes, quotas or shared blocks; any other is read only,
or refused.

### AoU-16 — the storage device keeps its flush and FUA promises
ASR-9's half about power loss rests on one property of the device below the
element's `WriteDevice` interface: every write that returned before a flush
is durable when the flush returns, and a write with FUA is durable when it
returns. The commit writes its nodes, flushes, and only then writes the
superblock (`H.STORE.3`). A device, controller, emulator or host cache mode
that acknowledges a flush it has not done can make the superblock durable
before a node it names, and the volume may then not mount after a power cut.
The integrator shall use storage, and a virtual disk configuration, that
honours flush and FUA, and shall not run the element over a write cache
that is volatile and claims otherwise.

## 5. Element failure analysis

The hazard analysis the element *can* do: not what harm the system causes —
that is AoU-1 — but how the element itself can fail to deliver §2. This is the
safety counterpart to [VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md),
which asks the same questions with an attacker rather than a fault as the
cause.

| Id | Failure mode | Effect at the element boundary | Detection | Mitigation | Residual |
|---|---|---|---|---|---|
| FM-1 | Separation lost: one partition reaches another's memory | ASR-1 violated silently | boot-time sweep; SMEP/SMAP/PAN fault on the wrong access; stage 4 checks that a page table an unmap empties is freed only by its shootdown (F-36) | per-process tables; hardware backstop; nothing a translation reached -- frame or table -- is given back until every processor that may cache it has flushed | ARMv7-A has no backstop (AoU-6); a walk through a freed table cannot be provoked under emulation, so the table order rests on the check and the rule |
| FM-2 | A mapping becomes writable and executable | ASR-2 violated | every boot sweeps all mappings and fails | enforced at map time | detection is per boot, not continuous |
| FM-3 | A capability is honoured that was never granted | ASR-3 violated | 135 refusal assertions | per-process handle tables, unforgeable | none identified |
| FM-4 | A device writes outside its granted region | ASR-4 violated, arbitrary corruption | IOMMU fault | VT-d / SMMUv3 domains; a domain's emptied tables are freed only after the unit's invalidation completes (F-36) | no IOMMU on ARMv7-A (AoU-6) |
| FM-5 | A frame is reused without being cleared | ASR-5 violated, data disclosure | none at runtime | zeroed on allocation | zeroing is on allocation, not free (V-04) |
| FM-6 | The element continues in a corrupt state | any ASR may be violated silently | invariant checks | safe state on detection | detection is not exhaustive |
| FM-7 | A partition exhausts memory | calls that allocate fail with `NO_MEMORY` or `ENOMEM` for every partition; the safe state if the load's allocation fails | allocation failure, reported at every site in the element (gate and boot check) | a job's memory, object and task limits, refused at the limit while other jobs go on (`quota` boot line), the Linux personality's heap within the memory limit (`kmem` boot line, F-37); job limits on depth and descendants; capped queues | a partition in no limited job; per-task kernel memory and machine-wide tables with fixed bounds (V-05, low); the load's allocations are fatal (AoU-5) |
| FM-8 | A partition is starved of processor time | ASR-8 violated | none at runtime; the `quota` boot line checks one job's share against another's | EEVDF eligibility, EDF admission; a job's share of a contended processor is its weight's, whatever its task count | no WCET, so no bound is provable (AoU-4) |
| FM-9 | Kernel stack overflow | page fault at the instruction that overflowed | **guard page below every kernel stack**, and a boot check that the guard is unmapped | `vmap` reserves an unmapped page on each side of every allocation; no recursion in the element | the loader-provided boot stack is not guarded (early boot only) |
| FM-10 | A processor stops answering a TLB shootdown or grace period (x86-64) | none while the wait lasts: nothing is freed and no narrowed permission relied on until every processor answers; then the safe state (FX-0001, FX-0002, FX-0003) | `smp::wait_for` and `take_turn`: a wall-clock floor (1 s, 5 s) **and** a count of the waiter's own polls, which stretches with the emulator's slowness | the count is in guest units, so a slow machine is not called stuck; a stuck processor answers no count and is still found (negative control: 1.8 s under KVM, 5.1 s under `tcg`, 32 s under the coverage plugin) | a host that stops running one virtual processor and keeps running the waiter can still end the wait early: availability lost, never integrity |
| FM-11 | Stored data is altered or lost: a torn commit, a write the device reordered, damage on the medium, a hostile image | ASR-9 violated: wrong bytes returned as a file's, or a volume that no longer mounts | a CRC-32C on every node and data sector, and each node's address, level, generation and filesystem checked against its parent; the second copy read where the chunk keeps one | copy-on-write: nothing the last commit reaches is overwritten, and the superblock is written after a flush; a failed transaction is aborted and the volume reloaded read only at its last commit | deliberate alteration with matching checksums (AoU-15); a device that does not honour flush (AoU-16); log replay and orphan cleanup at mount are not measured against a full volume, and can still fail the mount there (`docs/BACKLOG.md`) |

**FM-9 was recorded as the worst entry in this table and that was wrong.**
Every kernel stack is guard-paged at both ends: `crate::vmap` reserves an
unmapped page on each side of every allocation, *inside* the range the arena
hands out so the guard cannot be handed to anybody else, and the module's own
documentation says the guard below a stack is why it exists. `check_stacks`
asserts it on every boot — it writes the first and last usable words, then
requires that `stack.base - PAGE_SIZE` and `stack.top` translate to nothing,
failing with *"a kernel stack has no guard page below it, so an overflow would
be silent"*. So an overflow is a page fault at the instruction that caused it,
not silent corruption, and that is verified rather than intended.

The narrow residual: the **boot stack the loader allocates**
(`MemKind::BootStack`) is an ordinary pool allocation with no guard. It carries
early boot, before the arena the guarded stacks come from exists. An overflow
there would be silent, and the window is bounded by bring-up rather than open
for the life of the system.

With FM-9 corrected, the least-defended modes are **FM-5** (a frame is zeroed
on allocation rather than on free, so contents persist until reuse) and
**FM-8** (no bound on scheduling latency is provable, because no WCET is
claimed — AoU-4).

---

## 6. What an integrator receives

| Artifact | Purpose |
|---|---|
| This manual | assumed requirements, safe state, assumptions of use, failure analysis |
| [ITEM.md](ITEM.md) | exactly what is and is not in the element |
| [SECURITY-TARGET.md](SECURITY-TARGET.md) | the security counterpart, CC EAL5+ |
| [VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md) | AVA_VAN.4, six residual vulnerabilities |
| [SPECULATION.md](SPECULATION.md) | the side-channel defences behind AoU-11, per architecture, and what they cost |
| [VERIFICATION.md](VERIFICATION.md) | what exercises the element; 74.7% statement coverage on x86-64, 73.7% AArch64, 70.9% ARMv7-A |
| [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) | the determinism arguments behind AoU-4 and AoU-5 |
| [TOOLS.md](TOOLS.md) | tool classification and operational requirements |
| [SOUP.md](SOUP.md) | generated; the element contains none |
| [FINDINGS.md](FINDINGS.md) | every open finding, including the ones this manual exports as assumptions |

**The findings register is shipped deliberately.** An integrator who is told
only what works cannot judge the element. Several assumptions above exist
precisely because a finding is open, and each says which.

---

## 7. What this manual does not make true

It does not make the element certified. Nobody has assessed it (AoU-9).

It does not let an integrator skip their hazard analysis (AoU-1) — it tells
them what to check theirs against.

And it does not raise any of the four target ratings by itself. What it changes
is that [FINDINGS.md](FINDINGS.md) F-20 and F-22 are no longer blocked on a
device that does not exist: the element-level analysis is §5, the element-level
safety argument is §2 to §4, and what remains is the integrator's half, which
is exported rather than missing.
