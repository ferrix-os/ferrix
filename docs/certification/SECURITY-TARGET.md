# Security Target

Common Criteria (ISO/IEC 15408) Security Target for the Ferrix certified item.
Claimed assurance level **EAL5 augmented with ALC_FLR.2**.

This document closes finding F-21. It is a Security Target in structure and in
content; it has not been evaluated by anyone, and §9 records exactly where it
would not survive one.

| | |
|---|---|
| ST title | Ferrix Kernel Core Security Target |
| TOE | Ferrix certified item, as defined by `tools/common/data/certification-item.json` |
| TOE version | `48d58fb0`, reference configuration in the manifest |
| CC version | 3.1 Revision 5 |
| Assurance level | EAL5+ (ALC_FLR.2) |
| PP conformance | None claimed — see §2.3 |

---

## 1. TOE description

### 1.1 TOE type

A general-purpose operating system kernel core providing memory isolation,
scheduling and capability-mediated access to resources, with device drivers
running as unprivileged user processes.

### 1.2 Physical and logical scope

The TOE is the `core` and `item` rings of [ITEM.md](ITEM.md): **79,079 lines of
Rust** (64,788 of the kernel and 14,291 of two library crates, re-measured
2026-10-02), built for x86-64, AArch64 and ARMv7-A from the reference configuration.
It comprises the memory manager (buddy allocator, VMOs, address spaces, page
tables), the scheduler, the capability object system (handles, channels, ports,
jobs, interrupts, I/O mappings), the trap and system-call entry paths, the
IOMMU drivers (VT-d, SMMUv3), SMP bring-up and TLB shootdown, firmware table
parsing, device enumeration, and the panic path. Since 2026-10-02 it also
comprises the btrfs reader and write path, the library crates `ferrix-btrfs`
and `ferrix-btrfs-write`: from the `Device`
and `WriteDevice` traits the kernel's block layer answers, up to `Volume` and
`WriteVolume`.

**Outside the TOE**, running on it without being trusted by it: the VFS, the
btrfs glue on it (`ferrix-btrfs-vfs` and `src/kernel/src/fs/btrfs*.rs`: the
page cache, the commit interval, what `fsync` commits), procfs, sysfs and
tmpfs; the TCP/IP stack; the Linux system-call personality,
from its dispatcher to its `mmap`, `futex`, rlimits and POSIX threads; the
ring-3 device drivers; and all user software. 64,036 lines of kernel code
are in this category and the boundary is enforced at build time by
`tools/common/check/check-item-boundary.py`.

The **loader** (`src/boot/common/uefi/`, 3,503 lines) is in the reference configuration but
outside the TOE; it is covered by A.FIRMWARE in §3.3.

### 1.3 TOE security functionality, in brief

* **Address space isolation.** Each process has a page table the TOE
  constructs; no mapping is simultaneously writable and executable, and every
  boot sweeps all mappings to prove it.
* **Capability-mediated access.** A process names a resource only through a
  handle it holds. Handles carry rights, are unforgeable, and are transferred
  only over channels.
* **Device containment.** A driver runs in ring 3 and reaches its device
  through an `IoMapping` and an IOMMU domain, so a compromised driver cannot
  DMA into memory it was not given.
* **Residual information protection.** A physical frame is zeroed before it is
  handed to a new owner.
* **Hostile volumes and stored data.** The btrfs reader and writer parse a
  volume as hostile input, refuse a node or data sector that fails its
  checksum or does not match what refers to it, and commit by copy-on-write
  with the superblock written last, after a flush.
* **Resource bounding.** Jobs are where the TOE's quotas attach: a job and
  the jobs beneath it are bounded in the tasks, the memory of their programs
  -- their frames, and the kernel heap the Linux personality holds for them
  -- and the kernel objects they hold at once, and share a contended
  processor by weight, not by task count (§9.7).

---

## 2. Conformance claims

### 2.1 CC conformance
CC Part 2 conformant, CC Part 3 conformant, EAL5 augmented with ALC_FLR.2.

### 2.2 Rationale for the assurance level
EAL5 is the highest level whose `ADV_IMP.1` (implementation representation of
the TSF, sampled) and `ADV_INT.2` (well-structured internals) are plausible for
a 49,431-line TOE with the evidence described in §8. EAL6 requires `ADV_SPM`, a
formal security policy model, and `ADV_IMP.2` over the complete implementation
representation; neither is available, and [ITEM.md](ITEM.md) §3 names the
`core` ring as where that would later be attempted.

### 2.3 PP conformance
None. The natural candidates do not fit: the OS Protection Profile assumes
identification, authentication and audit functions this TOE deliberately places
outside its boundary (§9.1), and the Separation Kernel Protection Profile
assumes time and space partitioning the TOE does not yet offer as a service.

---

## 3. Security problem definition

### 3.1 Assets

| Id | Asset |
|---|---|
| AS.MEMORY | The contents of each process's address space |
| AS.KERNEL | The TOE's own code, page tables and object tables |
| AS.HANDLE | The handle tables that name every capability a process holds |
| AS.DEVICE | Device registers and DMA-capable memory |
| AS.CPU | Processor time and the scheduling invariants that apportion it |
| AS.VOLUME | The contents of a btrfs volume the TOE reads and writes, as last committed |

### 3.2 Threats

The threat agent is **unprivileged code running on the TOE**: a user process, a
ring-3 device driver, or the Linux personality itself. All are outside the TSF
and all are assumed hostile, which is the central design claim being made.
Since btrfs joined the TOE there is one more: **whoever wrote the storage
medium** -- a volume made elsewhere, a disk plugged in, or the ring-3 block
driver answering the TOE's reads with bytes of its choosing.

| Id | Threat |
|---|---|
| T.MEMORY | A process reads or writes memory belonging to another process or to the TOE. |
| T.ESCALATE | Unprivileged code causes the TOE to execute attacker-chosen code in ring 0, e.g. by corrupting a page table or a return path. |
| T.FORGE | A process fabricates or guesses a handle to obtain a capability it was never granted. |
| T.DMA | A compromised ring-3 driver programs its device to read or write memory outside the region it was granted. |
| T.RESIDUAL | A process recovers data left in a physical frame by a previous owner. |
| T.EXHAUST | A process consumes memory, CPU or object-table capacity so as to deny service to others. |
| T.CONFUSE | A process induces the TOE to act on a user-supplied pointer or length without validation. |
| T.MEDIA | A volume crafted or damaged by whoever wrote the medium, or bytes a ring-3 block driver returns, induce the TOE's btrfs code to read or write outside its buffers, panic, loop without bound, or return damaged data as valid. |

### 3.3 Assumptions

| Id | Assumption |
|---|---|
| A.PHYSICAL | The platform is physically protected. No defence is claimed against an attacker with bus access, cold-boot or fault injection. |
| A.FIRMWARE | UEFI, TF-A and the loader behave as specified and deliver an unmodified TOE image. The TOE performs no secure or measured boot (§9.2). |
| A.ADMIN | Whoever composes the system image and selects which drivers run is trusted to do so competently. |
| A.HARDWARE | The MMU, IOMMU and interrupt controller behave as their specifications state. |
| A.STORAGE | The storage device under the TOE's `WriteDevice` keeps its flush and FUA promises: every write that returned before a flush is durable when the flush returns ([SAFETY-MANUAL.md](SAFETY-MANUAL.md) AoU-16). |
| A.PROCESSOR | The processor offers the speculation controls [SPECULATION.md](SPECULATION.md) builds on, and they behave as the vendor states: SAFETY-MANUAL AoU-11, checkable from the boot log. |
| A.AUTH | The authentication service of [docs/AUTH.md](../AUTH.md), `authd`, and the programs that act on its verdict (`login`, `su`, `sessiond`, the compositor) are competently built, as A.ADMIN has image composition. They rely on the TOE for O.ISOLATE, O.CAPABILITY and O.SCRUB, and on the Linux personality's uid model and its `SO_PEERCRED`, both in the uncertified load ring. |

### 3.4 Organisational security policies

| Id | Policy |
|---|---|
| P.ACCOUNTABILITY | The decisions the TSF makes for and against the subjects it attests -- a call refused, authority handed over, a process or job ended from outside, a device quiesced or refused an access, TSF data changed, and the configuration the TOE booted in -- are recorded so that they can be reviewed afterwards, and a review can tell a lost record from one never made. |

---

## 4. Security objectives

### 4.1 For the TOE

| Id | Objective | Counters |
|---|---|---|
| O.ISOLATE | Separate address spaces so that no process can name memory it was not granted. | T.MEMORY, T.ESCALATE |
| O.WXN | Ensure no mapping is both writable and executable. | T.ESCALATE |
| O.CAPABILITY | Mediate every access to a kernel object through an unforgeable handle carrying explicit rights. | T.FORGE, T.MEMORY |
| O.DMA | Confine every device's memory access to an IOMMU domain the TOE programmed. | T.DMA |
| O.SCRUB | Zero a physical frame before a new owner can read it. | T.RESIDUAL |
| O.QUOTA | Bound the memory, objects and CPU a job may consume. | T.EXHAUST |
| O.VALIDATE | Validate every user-supplied pointer, length and handle at the system-call boundary before use. | T.CONFUSE, T.MEMORY |
| O.FAILSAFE | On detecting an inconsistent internal state, halt rather than continue. | T.ESCALATE |
| O.MEDIA | Parse every structure read from a btrfs volume as hostile input, answering an error rather than reading or writing outside a buffer, panicking or looping without bound; and refuse, or read from its other copy, a node or data sector that fails its checksum or does not match what refers to it. | T.MEDIA |
| O.AUDIT | Record each decision P.ACCOUNTABILITY names where the TSF makes it, in storage only the TSF writes, numbered so that a gap shows, with the boot's configuration among the records; let only the holder of a read-only capability read them. | P.ACCOUNTABILITY |

### 4.2 For the operational environment

| Id | Objective |
|---|---|
| OE.PHYSICAL | The platform is physically protected (A.PHYSICAL). |
| OE.FIRMWARE | Firmware delivers an unmodified image (A.FIRMWARE). |
| OE.ADMIN | Image composition is performed competently (A.ADMIN). |
| OE.HARDWARE | MMU, IOMMU and interrupt controller conform to specification (A.HARDWARE). |
| OE.PROCESSOR | The TOE runs, built `--mitigations on`, only on a processor whose boot log reports no side-channel hazard uncovered (A.PROCESSOR). |
| OE.AUTH | People are identified and authenticated by `authd`, which alone holds credentials, in ring 3 and outside the TOE, and security-relevant events of that kind are recorded in its audit log ([docs/AUTH.md](../AUTH.md) §3.6) (A.AUTH). |
| OE.STORAGE | Storage keeps its flush and FUA promises, and a medium whose contents a safety or security function relies on is protected against deliberate change by the environment, which CRC-32C does not detect (A.STORAGE, §9.8). |
| OE.AUDIT_STORE | The audit records the TOE's reader has written to the root volume (`/var/log/audit/<id>.bin`) are protected, kept and reviewed by the operational environment: the TOE claims nothing for them once written ([AUDIT.md](AUDIT.md) §7), as A.ADMIN has image composition. |

---

## 5. Security functional requirements

Drawn from CC Part 2. Operations: **assignment**, *selection*, refinement.

### FDP — user data protection

**FDP_ACC.1** Subset access control. The TSF shall enforce the **Capability
Access Control SFP** on **subjects: processes and threads; objects: VMOs,
channels, ports, jobs, interrupts, I/O mappings; operations: all operations
named by the native ABI**.

**FDP_ACF.1** Security attribute based access control. The TSF shall enforce
the Capability Access Control SFP based on **the handle a subject presents and
the rights that handle carries**. A subject may perform an operation only if it
presents a handle naming the object and that handle carries the right the
operation requires. A handle is valid only in the handle table of the process
holding it.

**FDP_IFC.1 / FDP_IFF.1** Subset information flow control. The TSF shall
enforce the **Address Space Separation SFP**: information flows between two
processes only through an object both hold a handle to. No implicit flow
through memory is permitted, since no physical frame is mapped into two address
spaces unless a shared VMO says so. **The one exception is speculative: between two
processes of one speculation domain** -- a job marked at its creation, whose
members were born in it and have not left (`docs/OPAQUE-KERNEL.md` §9) -- **the
predictor invalidation at a switch is left out**, so a flow through shared
predictor state between them is not prevented. Every flow out of a domain is,
and a process outside every domain, the default, has no such exception
(AoU-14, V-07).

**FDP_RIP.2** Full residual information protection. The TSF shall ensure that
any previous information content is made unavailable upon **allocation** of a
physical frame to any object.

**FDP_SDI.2** Stored data integrity monitoring and action. The TSF shall
monitor user data stored in containers controlled by the TSF for **a
checksum mismatch, and a tree node whose address, level, generation or
filesystem differs from what its parent names,** on all objects, based on
the following attributes: **the CRC-32C btrfs keeps for every tree node and,
except in a file marked `nodatasum`, every data sector**. Upon detection of a
data integrity error, the TSF shall **read the other copy where the chunk
keeps one, and otherwise answer an error and return none of the damaged
bytes**.

*Refinement.* "Integrity error" is refined to accidental change. CRC-32C is
not a keyed check, and a volume written by an attacker can carry checksums
that match (§9.8).

### FMT — security management

**FMT_MSA.1** Management of security attributes. The TSF shall restrict the
ability to *reduce* the **rights carried by a handle** to **the process holding
it**. Rights may never be raised. And it shall restrict the ability to
*set* **a job's speculation-domain mark** to **the holder of MANAGE on its
parent, as the job is made**; the mark shall not be changed afterwards, and a
process's membership shall only ever be lost (O.ISOLATE).

**FMT_MSA.3** Static attribute initialisation. The TSF shall provide
*restrictive* default values: a newly created process holds no handles other
than those explicitly transferred to it. A new job is in no speculation domain unless
made marked, a child job of a marked one included, and a process is in one
only if born in its marked job (O.ISOLATE).

### FPT — protection of the TSF

**FPT_FLS.1** Failure with preservation of secure state. The TSF shall preserve
a secure state — halting with a diagnostic — when **an internal consistency
check fails**.

**FPT_STM.1** Reliable time stamps. The TSF shall provide reliable time stamps
from the platform timer.

**FPT_TDC.1** Inter-TSF basic TSF data consistency. The TSF shall consistently
interpret **handles, VMO offsets and lengths supplied by untrusted subjects,
and the superblocks, chunk items, tree nodes, items and compressed extents of
a btrfs volume,** when shared with the TSF.

*Refinement.* For a volume, "consistently interpret" is: every structure is
bounds-checked against the buffer it was read into before any field is used,
and one that is malformed is answered with an error, never a panic or an
access outside the buffer (O.MEDIA).

### FRU — resource utilisation

**FRU_RSA.1** Maximum quotas. The TSF shall enforce maximum quotas of the
following resources: **the physical memory of a job's programs -- the frames
of their address spaces and those spaces' page tables, and the kernel memory
the TOE's Linux personality holds for them; the kernel objects its programs
make -- VMOs, channel ends, ports and jobs; and its tasks** that **a job,
together with every job beneath it,** can use **simultaneously**.

*Refinement.* "Physical memory" is refined to the memory of a job's
programs: their frames, and -- since 2026-09-26 (F-37) -- the kernel heap
the Linux personality holds for them, counted in bytes against the same
limit, as Linux counts kernel memory against a memory cgroup's. Kernel
memory held once per task (a kernel stack, a futex waiter) is bounded through
the task quota, and a few tables the machine shares carry fixed bounds of
their own (§9.7). "CPU time", which the claim named until 2026-09-26, is not quota'd as
a maximum: the TSF shares a contended processor between jobs in proportion to
a weight each job carries, whatever the number of tasks in it, and an idle
processor is given to whichever job can use it. A share bounds what one job
takes from another under contention, which is what T.EXHAUST is about, and a
maximum would additionally leave a processor idle while work waits.

### FIA — identification and authentication
**None claimed.** See §9.1.

### FAU — security audit

The design and its reasoning are [AUDIT.md](AUDIT.md); what follows is the
claim.

**FAU_GEN.1** Audit data generation. The TSF shall be able to generate an
audit record of the following auditable events: a) start-up of the audit
functions -- **the start-up record, and, as the last record before a power
action, the power action**; b) **the TOE's configuration at start-up:
`ferrix.checks`, `ferrix.devmgr`, the mitigations and KASLR**; c) **a
native call its handle's rights refused, a widening of rights refused, a
charge a job's limit refused, a native process made, a job handle given for
a cgroup, a device's control channel given, `devmgr` started through the
starter, the starter and the audit handle given to pid 1, a job made one
speculation domain, a job killed, a cgroup killed, an OOM kill, a device quiesced, a DMA fault an IOMMU
reported, a limit set through a job's handle or a cgroup's file, and the
switch of `/`**. The TSF shall record within each audit record: the date
and time of the event -- **nanoseconds since boot on the TSF's counter** --,
type of event, subject identity, and the outcome of the event; and **what
the decision was about and the event's detail**.

*Refinement.* A call's ordinary success is not an auditable event: authority
handed over and TSF data changed are (AUDIT.md §1). A refusal past its
budget's 64 a second is counted in one *suppressed n* record rather than
recorded itself (AUDIT.md §3). A DMA fault is recorded when the TSF reads
it from the unit -- the boot's checks and a domain's own reads do; after
boot the fault interrupt is masked, so a fault in the field is refused by the
unit and not recorded until something reads it
([VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md), "Faults after boot").

**FAU_GEN.2** User identity association. The TSF shall be able to associate
each auditable event with the identity of the **process and job, as the
TSF attests them,** that caused the event.

*Refinement.* "User" is refined to the TOE's own subjects. The uid the Linux
personality gives is carried beside them, marked as the personality's, and
is never the TSF's identity of the subject: people are identified outside
the TOE (OE.AUTH). FAU_GEN.2's dependency, FIA_UID.1, is unmet by design:
the subjects it names are the TSF's own process and job, which need no
identification, and people are identified under OE.AUTH and A.AUTH.

**FAU_SAR.1** Audit review. The TSF shall provide **the holder of the audit
handle, which the TSF gives pid 1 alone** with the capability to read **all
audit records, the boot's own pinned apart**, from the audit records. The
TSF shall provide the audit records in a manner suitable for the user to
interpret the information: **64 fixed bytes a record in
`src/lib/proto/audit`'s layout, which `svc audit` decodes.**

**FAU_SAR.2** Restricted audit review. The TSF shall prohibit all users read
access to the audit records, except those users that have been granted
explicit read-access: **the holder of the audit handle, which carries `READ`
alone and so can be neither duplicated nor sent.**

**FAU_STG.1** Protected audit trail storage. The TSF shall protect the
stored audit records in the audit trail from unauthorised deletion. The TSF
shall be able to *prevent* unauthorised modifications to the stored audit
records in the audit trail.

*Refinement.* The audit trail is the TSF's two rings: nothing but the TSF
writes them, and no call deletes or changes a record. The records once
written to the volume are OE.AUDIT_STORE's.

**FAU_STG.4** Prevention of audit data loss. The TSF shall *overwrite the
oldest stored audit records* and **count the records overwritten, and
report the gap in the numbers and its count to the reader,** if the audit
trail is full.

*Refinement.* A full ring is one ring of the two: a flood of refusals fills
the refusal ring alone and never overwrites a grant (AUDIT.md §3). The other
two selections are declined deliberately: *ignore audited events* or
*prevent audited events* when full would let an attacker fill the store
first and then act unrecorded, and a TSF that stopped instead would make
audit a lever for T.EXHAUST. FAU_STG.3 is not claimed: the TSF raises no
alarm on a potential loss, and the reader learns of one from the gap.

FPT_STM.1 (above) is the time stamp of each record.

---

## 6. Security assurance requirements

EAL5 as defined in CC Part 3, augmented with **ALC_FLR.2** (flaw reporting
procedures). No other augmentation is claimed; in particular `AVA_VAN.5` is not
claimed, and `AVA_VAN.4`'s moderate-attack-potential analysis has not been
performed (F-21 successor finding in §9.3).

---

## 7. TOE summary specification

How the TOE meets each objective, with the evidence that exists today.

| Objective | Implementation | Evidence |
|---|---|---|
| O.ISOLATE | Per-process page tables built by `src/kernel/src/user/space.rs`; higher-half kernel mapping; TLB shootdown on SMP. Against speculative reads, the defences in `src/kernel/src/arch/speculation.rs` and each architecture's `speculation.rs`: program-chosen indices clamped at the system call boundary, the processor's speculation controls, a predictor barrier at each switch of address space between programs not in one speculation domain (a job marked at its creation by MANAGE on its parent; `docs/OPAQUE-KERNEL.md` §9, AoU-14, V-07). | `user/check.rs`, `user/rmap_check.rs`; `arch/speculation_check.rs`, every boot: *"speculation defences read back on 4 processors"*; `object/domain_check.rs`, every boot: the `domain` line |
| O.WXN | Enforced at map time; `WXN`/`NX` set on all three architectures. The direct map's alias of the kernel's text and read-only data is mapped read only by both loaders (F-34), so no mapping of the text's frames is writable, not only none of the text's own. | Every boot sweeps all mappings: *"w^x 3485 mappings swept, 917 executable, none writable"*, then every mapping of the text's frames: *"sealed 4416 KiB of text and read-only data, 1697 mappings of it, none writable"* (x86-64, 2026-09-26). A write of text through the direct map faults on all three architectures (FX-9001) |
| O.CAPABILITY | `src/kernel/src/object/`: handle tables, rights masks, transfer only over channels. | `object/check.rs`, 3,318 lines; *"18 refusals as specified"* |
| O.DMA | `src/kernel/src/iommu/{vtd,smmuv3}.rs`; a driver receives an `IoMapping` and a domain. | `iommu/gate.rs`; `tools/common/check/check-device-access.py` holds the seam at build time |
| O.SCRUB | `mm::zero_frame` on every frame handed to a VMO. | `src/kernel/src/mm.rs:1140`, called from `user/vmo.rs` at three sites |
| O.QUOTA | A quota slot per job in `src/kernel/src/object/quota.rs`, charged hierarchically at every task, frame and native object a job's programs make and uncharged wherever each goes (the frame record keeps its slot); the kernel heap the Linux personality holds for its programs charged as memory, in bytes, by a `src/lib/kernel/kmem` token kept in each object (F-37); a per-job weight applied to each task's in `sched`. Set by `job_set_limit` or cgroupfs. What is bounded otherwise is in §9.7. | `object/quota_check.rs`, every boot: *"a fork loop refused at its job's 8 tasks; faults refused at 47 pages (3 of them page tables, beside 512 bytes of its regions' heap) while a sibling job faulted in 48; objects refused at 5; one task alone in its job kept 50.0% of a processor against eight in another; every counter back to zero and every quota slot given back"*; `fs/kmem_check.rs`, every boot: *"at a 32 KiB memory limit a job made 34 files, 14 pipes, 5 socket pairs, 70 descriptors in flight, 128 epoll registrations, 31 eventfds, 255 regions of one mapping and 454 record locks, and was refused one more of each -- ENOMEM, ENOLCK for a lock -- while a sibling made one; every byte of heap charged came back"* (x86-64, 2026-09-26); eight negative controls (W-13, W-15) |
| O.VALIDATE | `src/kernel/src/syscall/uaccess.rs`, backed on x86-64 by SMEP and SMAP since 2026-09-25, and on AArch64 by PAN where the CPU has it. The reference `cortex-a72` does not, and ARMv7-A cannot (V-01). | `syscall/check.rs`, 9,537 lines, 427 refusal assertions; the boot reports *SMEP on, SMAP on* |
| O.FAILSAFE | `src/kernel/src/panic.rs` with a catalogue of explanations. | `tools/common/check/check-panic-audit.py`; `gen-panic-catalog.py --check` |
| O.MEDIA | `src/lib/fs/btrfs` (`ferrix-btrfs`): `forbid(unsafe_code)`, every field read through `slice::get` and checked length arithmetic, a node or data sector read from the first copy that passes its checksum and its parent's expectations; `src/lib/fs/btrfs-write`, which edits only nodes the reader has parsed and checked. | host tests naming `H.STORE.1`, `H.STORE.2` and `L.btrfs.1` to `L.btrfs.11` (TRACEABILITY.md); the `btrfs_read` fuzz target over resealed real images; stage 11's and 12's boot checks |
| O.AUDIT | `src/kernel/src/audit.rs`, in the core ring: two rings of static storage under leaf `IrqSpinLock`s, fairness per budget, the boot's own records pinned; a record at each decision's site; `Object::Audit` and `audit_read`, the handle only pid 1 holds; init keeping the records in `/var/log/audit/<id>.bin` ([AUDIT.md](AUDIT.md)). | `audit/check.rs`, every boot: the store on stores of its own and the kernel's, and at the end of boot *"13 kinds of decision the boot's checks made are recorded, each with its outcome and the subject that decided it"* (x86-64, 2026-09-27), with a negative control for every recording site; `syscall/native_check.rs`: the handle reads, refuses without `READ` and cannot be sent; `test-init`: the record read back from the volume, the power-off's number, and a `ferrix.checks=skip` boot's record read from outside; `H.AUD.1` to `H.AUD.13` |

---

## 8. Rationale

### 8.1 Threats to objectives
Each threat in §3.2 is countered by at least one objective in §4.1, as the
*Counters* column records. T.MEMORY and T.ESCALATE are each countered by more
than one, since they are the threats the TOE exists to address. T.MEDIA
is countered by O.MEDIA alone. O.AUDIT
counters no threat: it meets P.ACCOUNTABILITY, the one policy of §3.4, and
nothing else is claimed to meet it.

### 8.2 Objectives to SFRs

| Objective | SFRs |
|---|---|
| O.ISOLATE | FDP_IFC.1, FDP_IFF.1, FMT_MSA.1 and FMT_MSA.3 (the speculation-domain mark) |
| O.WXN | FDP_IFF.1 (refinement) |
| O.CAPABILITY | FDP_ACC.1, FDP_ACF.1, FMT_MSA.1, FMT_MSA.3 |
| O.DMA | FDP_ACF.1, FDP_IFF.1 |
| O.SCRUB | FDP_RIP.2 |
| O.QUOTA | FRU_RSA.1 |
| O.VALIDATE | FPT_TDC.1 |
| O.FAILSAFE | FPT_FLS.1 |
| O.MEDIA | FPT_TDC.1, FDP_SDI.2 |
| O.AUDIT | FAU_GEN.1, FAU_GEN.2, FAU_SAR.1, FAU_SAR.2, FAU_STG.1, FAU_STG.4, FPT_STM.1 |

### 8.3 Why EAL5 is the right claim
§2.2. The three arguments that carry it: the TOE is 51,525 lines and 100%
first-party source, so `ADV_IMP.1` is satisfiable; the reference configuration
has zero Cargo features and one two-valued build switch, `--mitigations`, of
which only `on` is evaluated, so the configuration space is enumerated in a
sentence; and
eleven build-time gates support `ADV_INT.2`'s well-structuredness in a way
review notes cannot.

### 8.4 SFR dependencies
Each SFR's dependencies as CC Part 2 states them, and whether this ST meets
them. Three are unmet, each by design.

| SFR | Depends on | Met |
|---|---|---|
| FAU_GEN.1 | FPT_STM.1 | yes |
| FAU_GEN.2 | FAU_GEN.1, FIA_UID.1 | FAU_GEN.1 yes; **FIA_UID.1 no** (1) |
| FAU_SAR.1 | FAU_GEN.1 | yes |
| FAU_SAR.2 | FAU_SAR.1 | yes |
| FAU_STG.1 | FAU_GEN.1 | yes |
| FAU_STG.4 | FAU_STG.1 | yes |
| FDP_ACC.1 | FDP_ACF.1 | yes |
| FDP_ACF.1 | FDP_ACC.1, FMT_MSA.3 | yes |
| FDP_IFC.1 | FDP_IFF.1 | yes |
| FDP_IFF.1 | FDP_IFC.1, FMT_MSA.3 | yes (3) |
| FDP_RIP.2 | none | -- |
| FDP_SDI.2 | none | -- |
| FMT_MSA.1 | FDP_ACC.1 or FDP_IFC.1, FMT_SMR.1, FMT_SMF.1 | FDP_ACC.1 yes; **FMT_SMR.1 no** (2); **FMT_SMF.1 no** (2) |
| FMT_MSA.3 | FMT_MSA.1, FMT_SMR.1 | FMT_MSA.1 yes; **FMT_SMR.1 no** (2) |
| FPT_FLS.1 | none | -- |
| FPT_STM.1 | none | -- |
| FPT_TDC.1 | none | -- |
| FRU_RSA.1 | none | -- |

1. **FIA_UID.1.** People are identified outside the TOE (OE.AUTH, A.AUTH,
   §9.1). FAU_GEN.2 is refined to the TSF's own subjects, a process and its
   job as the TSF attests them, and those need no identification function:
   the TSF made them.
2. **FMT_SMR.1 and FMT_SMF.1.** The TOE has no roles. Authority is a handle
   and the rights it carries (FDP_ACF.1), so "who may manage an attribute" is
   "who holds a handle with the right to", not a role an identified user
   plays. The management functions FMT_SMF.1 would list exist all the same:
   duplicating a handle with fewer rights (FMT_MSA.1), setting a job's
   limits through its handle or its cgroup files (FRU_RSA.1's quotas, each
   change recorded under FAU_GEN.1), and giving a starter or the audit handle
   to pid 1. Claiming FMT_SMF.1 for them, and FMT_MTD.1 for the job limits
   as TSF data, is the ST owner's choice (`docs/BACKLOG.md`); until then they
   are the unmet dependency this note justifies.
3. **FMT_MSA.3 for the flow policy.** FMT_MSA.3's restrictive default -- a
   new process holds no handle it was not given -- is also the flow policy's:
   a flow between two processes needs an object both hold a handle to.

---

## 9. Limitations — where this ST would not survive evaluation

Stated here rather than discovered by an evaluator.

### 9.1 Identification and authentication are the environment's
There is **no identification or authentication** (FIA) inside the TOE —
POSIX credentials live in `syscall/credentials.rs`, which is in the
uncertified load ring. For a TOE of this type that is defensible: it is an
isolation kernel, and identity is a personality concern. It is also why no
OS Protection Profile can be claimed (§2.3).

Audit (FAU) is claimed since 2026-09-27 (§5, [AUDIT.md](AUDIT.md), F-21b),
for the TSF's own decisions and with the TSF's own subjects: a process and
a job, not a person. What a person did is `authd`'s to record. The TOE's
records, once its reader has written them to the volume, are
OE.AUDIT_STORE's; a boot with no reader keeps the rings' last records and
prints any a power action would lose.

Their counterparts are the operational environment's since 2026-09-26:
OE.AUTH and A.AUTH (§3.3, §4.2). `authd` identifies and authenticates
people and keeps the audit log of it, outside the TOE, and no
organisational security policy is claimed for it. On ARMv7-A and the DK1,
where no IOMMU confines a ring-3 driver (ARCHITECTURE.md §7), a driver can
read `authd`'s memory, and so its secrets while they are checked. The one
weakness found in what it relies on, E-01 (`SO_PEERCRED` taken when a
socket was made), is closed by K-E ([VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md),
"What this analysis does not cover").

### 9.2 No trusted boot path
The TOE does not verify its own integrity. A.FIRMWARE carries the whole of that
burden, which is a large assumption to place on the environment.

### 9.3 The vulnerability analysis found a single point of failure
[VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md) now covers `AVA_VAN.4`
over all seven threats. It found five residual vulnerabilities, of which V-01
bears on three: **no SMAP, SMEP or PAN was enabled**, so the software bound
check in `uaccess` was the only barrier between a user pointer and kernel
memory at kernel privilege.

F-32 has since enabled SMEP and SMAP on x86-64 and PAN on AArch64, and no
product-code path needed an access window. V-01 remains on Arm: the reference
`cortex-a72` is ARMv8.0 and lacks PAN, and the Cortex-A7 of ARMv7-A cannot
have it. There the bound check is still the only barrier.

An earlier draft of this ST claimed SMAP and PAN were enforced. That was wrong
— the tree's 56 apparent references to "smap" are `smap_base` and `smap_len`,
the system memory map — and §7 is corrected above. The mitigation is sound and
centralised; on Arm it still has no hardware defence in depth.

### 9.4 The design evidence does not yet reach the TOE's modules
`ADV_TDS.3` needs a semiformal design decomposing the TSF into subsystems and
modules. `docs/sysml/` is the right notation and describes Ferrix rather than
the TOE, at system granularity (F-15).

### 9.5 The TSF randomises its layout but neither hides nor partitions
The side-channel defences (F-31, [SPECULATION.md](SPECULATION.md)) stop a
program steering a speculative read, on processors A.PROCESSOR admits. KASLR
(§6.1 there) moves the kernel image, the direct map and the vmap arena each
boot, so an exploit needs a disclosure as well as a corruption. The TSF does
not unmap the kernel from a program's tables (no KPTI — not needed on the
reference processors against Meltdown), so a program with a timer can still
find the kernel, and it does not partition a cache, so two processes sharing
one can time each other (V-06). No security objective rests on KASLR. An
evaluator at `AVA_VAN.4` would accept KPTI's absence as argued and press on
the cache for any deployment where processes share one.

### 9.6 The TSF's independence of the load is checked by name, not by type
No reference reaches by name from the TOE into the uncertified load ring, down
from 94 when the audit began (F-07, F-09 and F-33 closed 2026-09-26; the 29 and
62 given here before that day were lower bounds, FINDINGS.md §A). The trap
entry and return paths reach the personality only through what it registers
(F-02, F-02a, F-09); board support, bring-up, power, the native ABI's
subsystems and native process creation register with the TOE rather than
being named by it (F-04, F-07, F-08); the core no longer names the Linux
personality's process or thread (F-01, F-06).

What an evaluator would still press on is what the gate cannot see. It reads
names, so a load-ring value reaching the TOE through a trait object or a
function pointer -- which is exactly how every registration above works -- is
not an edge to it; the interfaces are the TOE's types, but what runs behind
them is not the TOE's code. The crate root, `main.rs`, composes the load with
the TOE and is exempt by file, with 38 edges argued in ITEM.md §2 rather than
checked. And the load ring runs in ring 0, in the same address space and heap:
the boundary is one of dependency and assurance, not of protection, which is
why A.ADMIN and the SAFETY-MANUAL's assumptions of use carry the rest.

### 9.7 What FRU_RSA.1 does not bound
Until 2026-09-26 this read that the quotas FRU_RSA.1 claims were not built
(F-35); they are, and the claim is refined in §5 to what they bound. The
kernel heap the Linux personality allocates for a job -- the regions of its
address spaces, the inodes and names of files it makes in a memory
filesystem, descriptors in flight in a socket's queue, and the rest F-37's
audit found -- was charged to no job until the same day, and is now charged
to the job's memory quota (F-37, W-15). What an evaluator would still press
on: kernel memory held once per task, which only the task quota bounds; the
machine-wide tables with fixed bounds rather than per-job shares -- 256
pseudoterminals, the neighbour and reassembly caches, the routing tables,
which only root may change, and a btrfs transaction's changed tree nodes
(V-05, low); the dentry cache, whose unused entries stay charged to whoever
looked them up until evicted, where Linux would reclaim them; and a job with
no limit above it, which the TOE bounds by nothing but the machine (A.ADMIN,
AoU-5). And the processor is shared by weight, not capped: a job alone on an
idle machine may use all of it, which is a choice, stated in §5, rather than
a gap.

### 9.8 btrfs: what is claimed for a volume, and what is not
btrfs joined the TOE on 2026-10-02 (the customer's decision). What an
evaluator would press on:

* **CRC-32C is not authentication.** FDP_SDI.2 finds accidental damage.
  Whoever can write the medium can write a volume whose checksums match and
  whose files say anything; O.MEDIA promises only that the TOE parses such a
  volume safely, not that it can tell it is false. OE.STORAGE carries the
  rest.
* **The TOE does not decide when data becomes durable.** The VFS glue above
  `WriteVolume` -- its page cache, its 32 MiB commit threshold and its commit
  interval, what `fsync` commits -- is load. The TOE promises what a commit
  and a log commit keep once they return (ASR-9), not when a program's write
  reaches one.
* **The write path is not fuzzed.** `btrfs_read` drives the reader; nothing
  drives `WriteVolume` over hostile images
  ([VULNERABILITY-ANALYSIS.md](VULNERABILITY-ANALYSIS.md), T.MEDIA).
* **Allocation failure in the write path** is still fatal while it is being
  made fallible (`H.STORE.7`, [MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md)
  §1.8, F-56), and an allocation sized from the disk can be driven to it.
* **Arithmetic is not linted.** The release kernel has no overflow checks,
  so an unchecked sum of two fields of a hostile volume wraps and only the
  bounds check after it stands; clippy's `arithmetic_side_effects` reports
  112 sites in the two crates (`L.btrfs.23`, F-56).
* **It runs on the load's behalf, under the load's locks.** The time a
  commit or a read takes is spent inside the VFS's calls and under its
  locks, which is (c) of [CLAIM.md](CLAIM.md) §3.1; no bound on it is
  claimed ([MEMORY-AND-TIMING.md](MEMORY-AND-TIMING.md) §2.2d).
