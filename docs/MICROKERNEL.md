# Ferrix as a certifiable microkernel

**Revision 10, 2026-10-13.** The customer asked for a plan to switch Ferrix
to a microkernel whose ring 0 can be certified whole. This document is that
plan. **The customer answered most of §11 on 2026-10-13** (recorded under
*Decisions* in `docs/BACKLOG.md` on this branch; §11 has the words and the
plan's readings of them apart): route C with the Linux ABI in user space,
maximum performance with both builds gated, a certified `btrfsd`, maximum
certification (the top level of each standard, IEC 61508 at SIL 4, for the
new kernel from M7), a build switch for speculation inside a Linux partition,
ARMv7-A as a native-only safety target, timing claims on AArch64 safety
processors only, feature work allowed with growth reported, user-space disk
checks, the consultant's certified-component set, IEC 61508 SIL 4 as the
first standard, nazuna as the measurement host with seL4's methods to reach
the targets, a proof toolchain and a safety board. §11 lists what is still
open. It reopens three
earlier positions:

* the 2026-09-16 decision (a monolithic core, capability seams, device
  drivers in ring 3);
* the 2026-09-27 shelving of `docs/OPAQUE-KERNEL.md`'s option B;
* `docs/ARCHITECTURE.md` §2, which says the Linux ABI is Ferrix's own and
  "not a compatibility layer over something else". Under this plan it is
  served by a user-space process on top of the native kernel (§2.2, §11 Q2).

It takes as inputs the two decisions `docs/certification/CLAIM.md` leaves
open: the claim's wording (§1 there) and the first standard (§2 there).

Figures marked *guess* are estimates, not measurements. Points are in the
backlog's units, for the product owner to convert.

## 1. Why

Today ring 0 holds the certified item and the uncertified load together
(`docs/certification/ITEM.md` §2). Measured with
`check-item-boundary.py --report` on `main` at 5450ee1fe:

| Ring | Product lines | In-kernel test lines |
|---|---:|---:|
| `core` | 70,158 | 32,432 |
| `item` | 9,382 | 4,274 |
| `load` | 72,228 | 47,319 |

The item is 52.4% of the kernel's product code. The other 47.6% runs with the
item's privileges, so every claim about the item is argued *around* the load:
28 composition-root edges from `main.rs` into the load, 57 `mod`
declarations of a higher ring, and AoU-3 forbidding a safety function in the
Linux layer. `CLAIM.md` §3.1 names the four arguments this forces and which
are not yet written: (a) memory, held only by safe Rust and an audit of the
load's `unsafe`; (b) resources, since the load's allocations share the item's
heap and are not fallible (FX-0008, AoU-5); (c) time, since load code holds
spin locks with preemption off (FX-0503); (d) failure, since a panic in the
load is a kernel stop.

The shape a certificate wants is the separation kernel's: **all product code
in ring 0 is the certified item, and everything else is a process the item
isolates** (§2.1 says what else stays in ring 0: the item's own boot checks).
Then the safety element's boundary is the privilege boundary (the TOE and
the element's certified user-space items are stated in §6.1), and §3.1's (a) to (d) stop being arguments about code in the same
address space (§6.4 says how far that goes, and where it does not).

## 2. What the switch is

### 2.1 The target

**Ring 0 holds no `load` ring.** The manifest keeps two rings in the kernel:
`core`, with its dependency rule, and `item`. Under the customer's
maximum-certification answer (§6.5) **the targets move from the rings to the
item**: one item, one set of targets. `core` and `item` stay as two
**layers**, and the rule that `core` names nothing in `item` stays checked
(§11 Q11, the consultant's recommendation, ledger 886): a layered kernel with
an enforced dependency direction is the architecture evidence CC ADV_INT.3,
ISO 26262-6 and IEC 61508-3 ask for, `core` is F2's natural proof scope (no
firmware parsing or bring-up), and keeping it costs one manifest field.
Coverage, MC/DC and requirements are reported per item, with per-layer sizes
for information.

Ring 0 also holds the in-kernel boot checks, 84,025 lines of verification
code today (36,706 of them the item's and the core's). Those that test the
kernel stay: they are verification code run in ring 0 by the reference
images (`ferrix.checks`), held to the fallible-allocation, `unsafe`-trace and
panic rules, and argued as executed code, not deactivated code. Those that
test the load leave with it, after the evidence they carry has been re-homed
(§6.2, B5).

**What stays in ring 0**, the kernel:

* trap, system call and exception entry on x86-64, AArch64 and ARMv7-A;
* address spaces, VMOs (anonymous, physical, contiguous, pager-backed),
  mapping into a space named by a handle;
* threads, processes, jobs, quotas, the scheduler with time budgets;
* handles and rights, channels, ports, events, timers;
* interrupts as objects, the IOMMU, I/O mappings, pins and pin quarantine;
* the device registry and its kernel-owned configuration space (§3, K8):
  enumeration, BAR placement, MSI and MSI-X, ATS, PRI and ACS, bus mastering,
  device quiesce, limits and isolation;
* SMP, speculation domains, the speculation defences (`SPECULATION.md`);
* **restricted mode** (§3, K1) and **shared processes** (K11);
* audit (`audit.rs`, `AUDIT_READ`, the FAU claims) for the events the kernel
  still sees (§6.1);
* power-off and reset, and their ordering with the servers that hold disks
  (`power.rs`; the hook list of the bm-nvme-stop review, ledger 792);
* clocks, and the time page that Linux's vDSO reads (FPT_STM);
* the kernel's entropy and an entropy call for user space (`random.rs` serves
  `getrandom` from the load today);
* the loader of the first processes and the initramfs they come from
  (L.init.2, AoU-24; §5);
* a write-only debug log, the panic path (serial only), the gs201 watchdog;
* the firmware tables needed to find memory, processors, interrupt
  controllers, timers, IOMMUs and PCI host bridges. The rest of ACPI or the
  device tree is handed to user space as a read-only VMO.

**What leaves ring 0**: the Linux personality, the VFS and page-cache policy,
every filesystem, the network stack, the device cores (display, render,
audio, input, chardev, block ring, net ring, logctl), the console's screen
and input, the panic screen's drawing, the board glue
(`platform/st/stm32mp1.rs`, `platform/google/gs201/usb.rs`), and the
in-kernel checks of these.

**The core's Linux-specific code** goes one of two ways:

| Code | Where it goes |
|---|---|
| `signal_frame.rs` and the arch signal frames (`setup_signal_frame`, `restore_signal_frame`) | `linuxd`, which writes frames into the program's memory itself; the kernel keeps only K1's check of the context `linuxd` hands back |
| `decode_syscall`, `decode_compat_syscall` | the kernel keeps only "native range or not" (§2.4); the Linux decode goes to `linuxd` |
| `vdso_spec`, `sigpage_code` | the vDSO's code and the ARMv7-A sigpage are `linuxd`'s images; the time page they read stays the kernel's |
| `set_thread_area`, `thread_area`, FS/GS bases, TPIDRURO | stay, as K1's sanitiser: the kernel sets the TLS registers from a context `linuxd` supplies, after checking it |

### 2.2 The shape: option C, with restricted mode

`docs/OPAQUE-KERNEL.md` §1 drew three shapes. B (servers behind an in-kernel
page cache) leaves the VFS, the page cache, the fd table, pipes, epoll and
procfs in ring 0, which keeps a large part of today's load in ring 0. **This
plan is option C**, with the one change that answers §1's objection to C
("the compiler's hot path becomes IPC round trips"):

* A Linux program's threads run in **restricted mode** (K1). A Linux system
  call or a fault leaves restricted mode and resumes the same kernel thread in
  `linuxd`'s code, with no switch of thread, no scheduler decision and no
  channel message. Zircon built this for starnix.
* **Each Linux process is a kernel process** (K11, Zircon's shared
  processes): its own restricted address space, its own handle table, its
  own job (§2.3). `linuxd`'s code and data are one *shared region* mapped
  into each of them. The kernel keeps every Linux process's restricted
  memory apart from every other's, as it does today.
* `linuxd` holds the VFS, the page cache's policy, the fd table, pipes,
  epoll, signals, credentials, procfs, sysfs, cgroupfs, devfs and the pty.
  A cached `read` is a restricted-mode exit, a function call in `linuxd` and a
  copy into the program's restricted memory. It never leaves the process.
* The page cache's **pages** are kernel VMOs with `linuxd` as their pager
  (K3). A file `mmap` maps the VMO, and a fault on a resident page is resolved
  by the kernel without `linuxd`. Only a miss goes to the pager.
* System-wide servers (§2.3) talk to `linuxd` over channels.

The hot path the 2026-09-16 decision protected (warm `open`, `stat`, `read`,
`mmap` on cached files) costs one restricted-mode round trip more than a
system call does today, and no IPC. §7 gates that.

### 2.3 Partitions, and who isolates what

**A Linux partition** is one `linuxd` shared region, the Linux processes that
run on it, and a job above them that carries its quota and time budget.
**cgroups stay kernel jobs**: `linuxd` creates a child job for each cgroup
and puts each Linux process in its cgroup's job, as C8 (2026-09-23) built
them "so a future microkernel keeps working" (`OPAQUE-KERNEL.md` §0). `JOB_FOR_CGROUP`,
`JOB_SET_LIMIT` and `JOB_GET_QUOTA` stay. So within a partition:

| Isolation between Linux processes | Today (the load in ring 0) | After |
|---|---|---|
| Memory (T.MEMORY, ASR-1, FDP_IFC's address-space separation) | the item, by address space | the item, by restricted address space (K11) |
| Memory against a fault in the Linux layer | none: a load bug is a kernel compromise | `linuxd`'s shared region holds every process's fd table, page cache and credentials, so a compromised `linuxd` reaches its own partition's processes, and nothing outside it |
| Quotas (F-35, F-37, F-40) per cgroup | the item, by job | the item, by job; server-held memory charged by K7 |
| The switch barrier between two Linux processes | the item (SPECULATION §3) | §11 Q5: either the item on every restricted exit and entry, or one partition is one speculation domain |
| Audit: cgroup kill, cgroup limit, OOM kill | the item | the item (jobs) |
| Audit: switch of `/` | the load in ring 0 | `linuxd`, outside the TSF (§6.1) |
| Permissions on files, credentials, `setuid` | the load in ring 0, never claimed by the ST | `linuxd`, outside the TSF |

That the kernel no longer sees credentials or files is stated in ST §1.3 and
§3.2, ASR-1 and AoU-3 (§6.1), not left to be inferred.

**System-wide services cannot live in a partition's personality.** The
network stack, the device cores, the console, btrfs and the block and net
rings serve every partition and the native processes. So the lift (§4) has
two outcomes:

* **Until M8 there is exactly one Linux partition.** The lift puts every
  system-wide service inside that one `linuxd`, except `btrfsd`,
  a certified item of its own (§11 Q4). The mixed-criticality claim until then is *native safety partitions
  beside one Linux partition*: it is what `CLAIM.md` §1 proposes, and it is
  true from M5. A safety partition uses devices only through native drivers
  devmgr assigns to it directly, never through `linuxd`'s device cores.
* **Several Linux partitions come after M8**, when `netd`, the device servers
  and the console server are their own processes. A claim with more than one
  Linux partition is dated after M8.

### 2.4 Programs that use both ABIs

`ARCHITECTURE.md` §2 lets one process make native calls beside Linux ones.
init does (`DEVMGR_START` under `ferrix.devmgr=init`, the audit handle,
`JOB_FOR_CGROUP`), and so does `nvrm` (`CHARDEV_*`). From restricted mode:

* **A native call goes straight to the kernel**, against the calling Linux
  process's own handle table (K11 gives it one). The kernel tells the two
  ABIs apart by number, as `syscall/mod.rs` does today: 0x1000 to 0x1FFF is
  native. `linuxd` is not on the path, so it never acts with a program's
  authority (no confused deputy). It holds a program's handle only through
  the two deliberate, rights-checked moves of K11: `PORT_FD`'s port (below)
  and the audit-read handle it passes to init at start (§5, §6.1).
* **`PORT_FD` (0x101B)** makes an fd, which is Linux state the kernel no
  longer holds. It exits to `linuxd` like a Linux call; the port moves from
  the program's private handle table to the shared one through K11's
  explicit, rights-checked move. It is listed in the native ABI as such.
* **The chardev calls (0x105A to 0x1064) are retired** (§3, *Retired
  calls*): no native call answers them under any mode. `nvrm`, which serves
  `/dev/nvidia*` through them today, talks to `linuxd`'s `/dev` service over
  a channel instead, and is re-planned for it (§10).
* Every other Linux call exits to `linuxd`.

### 2.5 One program, parts at different levels

The customer asked (2026-10-13): "would it be possible to run different
parts of programs with different security levels?" **Yes, as separate
partitions, and not inside one address space.**

* **The certifiable way: one application, several partitions.** An
  application is split into parts, each a native process in its own
  partition with its own level, its own K9 window or budget and its own K12
  reservation: an ASIL D control loop, an ASIL B supervisor, a QM user
  interface, and a Linux program (QM) beside them. The parts talk over
  channels (the round trip's target is under 400 ns, §7) and share memory
  through VMOs. The partition manager starts the group from its description,
  which names each part's level, image digest (K14) and every channel and
  shared VMO between parts. A Linux program reaches a native safety part the
  same way, through a native call on a channel (§2.4).
* **What the kernel enforces between levels (K17).** Each job carries a
  **criticality level** set by the partition manager at creation and never
  raised afterwards. The kernel refuses any handle transfer that would give a
  lower-level job (a) a handle to a higher-level job's processes, threads,
  jobs or address spaces, (b) a write right to a VMO a higher-level job maps
  other than one the description declares as a channel buffer from lower to
  higher, or (c) a right to change a higher-level job's budget or
  reservation. Data a higher level reads from a declared buffer is validated
  by the reader (ASR-7's rule, in the reader's own code). Freedom from
  interference between levels then rests on the kernel's spatial (ASR-1,
  K17), temporal (K9) and resource (K12, K16) isolation, which F1 models.
* **Not inside one address space at high levels.** Compartments within one
  process (x86's protection keys, Arm's MTE or POE, language-level
  boundaries) cannot carry an ASIL D or SC 4 freedom-from-interference
  argument: protection keys are switched by an unprivileged instruction the
  low part can execute itself, MTE is probabilistic, and Rust's type safety
  is not accepted alone as isolation at those levels. They may serve as
  defence in depth inside one level. CHERI (Arm's Morello) is the one
  hardware route to certifiable in-process compartments, and is research
  today.

## 3. Kernel mechanisms the switch needs

Each is a landing with its own design review before code (CONVENTIONS,
*Changes to the certified item go through review*), its ids reserved in a
docs-only landing first, and its checks and negative controls. **Each lands
with a boot check that exercises it in the reference configuration** (for K1
and K11, a native test program running a context in restricted mode), so
none is deactivated code in the reference (`ITEM.md` §5) and each is useful
before any code leaves ring 0.

| # | Mechanism | Today | What is new, and what it must hold |
|---|---|---|---|
| K1 | **Restricted mode**, on x86-64 and AArch64 (ARMv7-A is native-only, §11 Q7): enter a saved user context; return on system call, fault or kick, with the context saved; a kick from another thread, for signals | nothing | Every context `linuxd` hands in is checked as sigreturn's is (the I2a/I2b rules, ledger 7, 15, 23): `cs`/`ss` pinned, IOPL, VM, VIF and VIP refused, selectors through `gdt::loadable`, descriptors DPL 3, S=1, L=0; forged contexts as checks with negative controls. It covers the FS and GS bases, the i386 TLS descriptors and ARMv7-A's TPIDRURO (user read-only, so the kernel sets it). It saves the program's whole vector state across an exit: §9.8 3a's contract and AoU-25/27 bind `linuxd`'s own native calls, never a Linux program that made none. **Nothing of the shared region is reachable from restricted mode**, neither readable, writable nor executable: it holds every process's fd table, page cache and credentials, and a readable region would undo §2.3's first row. The restricted context runs on a root that maps none of it; the vDSO, the ARMv7-A sigpage and the time page are VMOs of their own, mapped read and execute (or read only) into the restricted space. The boot's mapping sweep proves it, with a negative control that maps one shared page into a restricted root. **Lazy vector state** (§7) holds only under four rules: (a) the program never sees `linuxd`'s register values: on re-entry, if `linuxd` touched vector state, the program's is restored at once, or the registers are cleared and the program's first use traps and restores; (b) the 3a contract (AoU-25/27) never applies while the program's state is still in the registers: a per-thread "unsaved" mark forces the save before any switch away (a blocking native call by `linuxd`, preemption, migration), with a check and a negative control that another thread never reads the program's values; (c) `SPECULATION.md` gains an entry for LazyFP (CVE-2018-3665), argued, not assumed; (d) "`linuxd`'s call path uses no vector registers" is a compiler-setting rule with a check. **The fastpath** (§7) is valid only when the context the kernel holds is unchanged apart from the result registers; any write by `linuxd` to the context (signals, sigreturn, `exec`, `ptrace`) takes the checked path, and a check with a negative control shows a forged context offered through the fastpath refused |
| K2 | **Address space by handle**: map, unmap, protect in another process's restricted space | `VMO_MAP` into the caller only | ASR-2 (W^X) holds on every space mapped by handle; an executable mapping only through a handle with the EXECUTE right (PO, 2026-09-13). **`fork`, `exec` and `ptrace` need three more operations**: a copy-on-write clone of a restricted space (or copy-on-write child VMOs); reads and writes of another process's space by handle, each with its own right (`ptrace`, `process_vm_readv`, core dumps); and a fresh restricted space for `exec`. A check, with a control, that a copy-on-write child never sees a write its parent makes after the clone (the F-41 lesson, ledger 29 and 31) |
| K3 | **Pager-backed VMOs** | none (`fs/pages.rs` fills from the block ring in ring 0) | The pager protocol over a port; dirty tracking and write-back requests. The faulting job pays for the page, by K7's token. A fault waiting on a dead or slow pager returns `SIGBUS`/`EIO` to the faulting process after a bounded wait, never the safe state. **One fault-forwarding mechanism**: K3 and `NVIDIA.md` §11.4's fault windows (OK IF 2026-10-02, L.user.111-124 reserved) are reconciled into one design, not built twice. An AoU: a native safety partition maps no pager-backed VMO it did not choose |
| K4 | **Exception channels**: faults of native processes delivered to a supervisor | panic or kill | needed for supervised servers and for `linuxd`'s own faults |
| K5 | **Wait on a user address** | the Linux `futex(2)` in the load | a generic primitive; `linuxd` maps Linux's operations onto it |
| K6 | **Kernel-attested sender identity** on a channel message | none | `OPAQUE-KERNEL.md` §2 point 4: the job and process written by the kernel, never taken from the body |
| K7 | **Charging that follows the client** | F-35/F-37 charge the caller's job | `OPAQUE-KERNEL.md` §2 point 5 and ledger 35-36: a token tied to one outstanding request, valid until the reply, so a server never names a job to charge |
| K8 | **Device authority for devmgr**, by handle | the kernel enumerates PCI and claims for devmgr | devmgr gets the right to bind a function to an IOMMU domain, never an IOMMU unit: **IOMMU registers and tables are never mapped to ring 3.** Configuration writes stay kernel-mediated (`DEVICE_CONFIG_WRITE`'s rule); BAR placement, MSI/MSI-X, ATS/PRI/ACS and bus mastering stay kernel-owned, bus mastering only with a domain attached (AoU-12, AoU-21/22/23, `device.rs`'s `verify_config`, `keep_ats_off`). PCI enumeration stays in the kernel, so `ITEM.md`'s "device enumeration is inside" holds; what changes is that devmgr, not the kernel's own matching, decides which driver gets a function |
| K9 | **Time budgets per job** | `cpu.max` throttling (a cap only); `ferrix-sched` answers `ModeUnavailable` for SoftRt and HardRt | **Stage 14's HardRt** (`ARCHITECTURE.md` §5: partitioned EDF with CBS admission; `CLAIM.md` M3), not a second model. Plus the two halves stage 14 lacks: a server's time on a client's request is charged to the client's budget (K7's temporal twin, as seL4 MCS donates a scheduling context), and interrupt time is charged to the line's holder or bounded per partition (`MEMORY-AND-TIMING.md` §2.2: today the victim pays). Under maximum certification (§6.5) a **static time-partition schedule** is added beside EDF (fixed windows per partition in a major frame, the ARINC 653 shape avionics integrators expect), and AoU-4/F-24's "no WCET claimed" is replaced by §6.5's timing work |
| K10 | **Timer objects** | the Linux personality's timers in the load | a deadline object signalled to a port, with slack |
| K11 | **Shared processes**: a process created with another's shared region mapped, and its own restricted space | none | **Two handle tables per process.** The *shared* table is the partition's (`linuxd`'s server channels, the cgroup jobs, the pager ports) and serves calls made in normal mode, from `linuxd`'s code; the *private* table is the Linux program's and serves calls made from restricted mode. The kernel picks the table from the mode the call came from, never from an argument. A handle moves between the two only through an explicit call that checks its rights (the way `PORT_FD`'s port reaches `linuxd`, §2.4). A check with a negative control shows that neither side can name the other's handles. The creating `linuxd` gets no handle into the new process's restricted space beyond K2's rights on it |
| K12 | **Kernel-memory reservation**: a job's reserved frames and kernel heap, guaranteed, beside its limit | limits only (F-35, F-37); the frame watermark of the endfield-oom review (ledger 797) is the first step | a QM partition cannot take the kernel memory a certified partition was promised |
| K16 | **No kernel heap after boot**: every kernel object made from memory its job has given the kernel (K12's reservation, retyped on creation), never from a shared heap | F-23 made every allocation fallible; F-37 charges them; the heap stays shared | `CLAIM.md` §2: dynamic memory "justified rather than forbidden" does not survive SIL 3 and above, and ISO 26262-6 highly recommends no dynamic objects from ASIL B (§6.5's table). seL4's untyped memory is the reference. The kernel's own boot-time structures are sized at boot and never grow. This retires AoU-5's dynamic-memory justification. **Evidence**: from M6 no global allocator is linked into the kernel (or the one linked refuses every request after boot), held by a check with a negative control (a planted allocation after boot fails it). The retype and revocation rules, and how F-35, F-37 and K12 map onto them, are for K16's own design review; M6's points are re-sized after it |
| K17 | **Criticality levels on jobs** (§2.5) | jobs and quotas, no level | a level per job, set once by the partition manager and never raised; the three refusals of §2.5 on handle transfer and VMO mapping; each with a check and a negative control (a lower-level job offered a write mapping of a higher-level job's private VMO, then refused) |
| K13 | **The rebind contract**: a server's restart without its clients' loss | T0's park and take-up, per kind, in ring 0 | `OPAQUE-KERNEL.md` §2 point 6, generalised to user space: a pager VMO handed to the next instance of its server, parked state charged to its clients' jobs (ledger 36). `linuxd` itself is not rebound: **a `linuxd` that dies takes its partition's programs with it** |
| K14 | **Image integrity for certified user-space items, by kernel-held measurement** | `A.FIRMWARE` covers the kernel image | Whoever starts a certified user-space item (`btrfsd`, a safety partition), with which capabilities, and how its image is shown to be the certified one (ST §9.2; the AoU-26 precedent). Since K15 the creator maps the segments itself, so the kernel measures: **an image VMO is sealed** (immutable from then on) and the kernel computes its digest at sealing; **a process's executable mappings come only from sealed, measured VMOs**, through K2's EXECUTE right; **the process carries the digests of its images**, and a grant that needs a certified driver is checked against the certified list (`A.FIRMWARE` extended over that list and the initramfs). A check with a negative control: an unsealed or unlisted executable mapping, then the grant refused |
| K15 | **The native ELF loader leaves the kernel** | `PROCESS_CREATE`/`PROCESS_START` run the load ring's loader (`syscall/exec.rs`, `syscall/launch.rs`) in ring 0, an untrusted-input parser | `PROCESS_START` takes an entry point, a stack and a bootstrap channel; the creator maps the image's segments itself through K2, with a user-space loader library (Zircon's process builder is the reference). The native ABI changes, and every native starter (devmgr, `linuxd`, the test programs) moves to the library. The kernel keeps one loader only: for the two first images of §5, read from the initramfs, which K14 vouches for, held to the item's rules with its own requirements |

### Retired calls

Each retired call's requirements are withdrawn, never reused.

| Number | Call | After the switch |
|---|---|---|
| 0x1048 | `BLOCK_RING_CREATE` | retired: the filesystem server and the block driver share a ring over channels, VMOs and ports, a protocol in user space |
| 0x1049 | `DEVICE_INFO` | stays |
| 0x104A | `DEVICE_QUIESCE` | stays |
| 0x104B | `NET_RING_CREATE` | retired, as 0x1048 |
| 0x104C | `DISPLAY_CONTROL_CREATE` | retired: a channel between the display server and its driver |
| 0x104D | `INPUT_CONTROL_CREATE` | retired, as 0x104C |
| 0x104E | `RENDER_CONTROL_CREATE` | retired, as 0x104C |
| 0x104F | `DEVICE_CLOCK` | stays |
| 0x1050 | `SOUND_CONTROL_CREATE` | retired, as 0x104C |
| 0x1051 | `LOG_CONTROL_CREATE` | retired: drivers log to the log server over a channel, and to the kernel's debug log before it exists |
| 0x1052 | `DEVMGR_START` | retired with `ferrix.devmgr=init`: the kernel starts devmgr (§5), so device authority never passes through init or `linuxd` |
| 0x1053 | `AUDIT_READ` | stays |
| 0x1054 | `DEVICE_APERTURE` | stays |
| 0x1055 | `DEVICE_CONFIG_READ` | stays |
| 0x1056 | `DEVICE_CONFIG_WRITE` | stays, kernel-mediated (K8) |
| 0x1057 | `DEVICE_SET_LIMIT` | stays |
| 0x1058 | `DEVICE_GET_LIMIT` | stays |
| 0x1059 | `DEVICE_ISOLATION` | stays |
| 0x105A | `CHARDEV_CONTROL_CREATE` | retired: `linuxd` serves `/dev` files and talks to the driver over a channel |
| 0x105B to 0x105E | `CHARDEV_REPLY`, `CHARDEV_COPY_IN`, `CHARDEV_COPY_OUT`, `CHARDEV_FILE` | retired: the copies are `linuxd`'s, into restricted memory it can address |
| 0x1060 to 0x1064 | `CHARDEV_DMABUF_INSTALL`, `_RESOLVE`, `CHARDEV_SYNC_INSTALL`, `_SIGNAL`, `_RESOLVE` | retired: dma-bufs and sync files are `linuxd` objects over VMO and event handles |

A device server is built from channels, VMOs, interrupts, I/O mappings and
pins, which the kernel already has.

## 4. The move: lift the load out, then split it

`OPAQUE-KERNEL.md` §4 took subsystems out one at a time, each behind an
in-kernel forwarding layer (a served inode, a forwarding socket). Under
option C that layer would be written, gated, certified and deleted when the
VFS itself leaves. This plan does not build it:

1. **Lift.** Build the `load` ring as one user program, `linuxd`, against a
   shim that replaces each core and item interface the load calls with a
   native call or a library. **The shim's size**, measured with the item
   gate's own resolver by `tools/common/gen/microkernel-surface.py surface`
   on 5450ee1fe: 102 load product files reach the core and item through
   1,859 references, to 401 distinct items in 56 modules. By sites, `syscall`
   leads (363: the item's `syscall/mod.rs`, `native` and `program`, and the
   core's `uaccess`), then `object` (307), `sched` (210), `sync` (192),
   `user` (112), `timer` (101), `arch` (99), `trap` (85) and `console` (68).
   K1 to K15 are what the shim needs from the kernel. The `Personality`
   trait (W-5) is a seam only for the item-to-load direction; the
   load-to-core direction is these 401 items. That the rest is "mostly
   mechanical" is a *guess*, and M5's points carry that uncertainty.
2. **Run both.** The in-kernel load and `linuxd` build from one source
   behind `--personality kernel|user`, until `user` passes every gate the
   load passes today (§8, M5). The switch is a `--cfg` that xtask sets, never
   a Cargo feature (`kernel_cargo_features` stays 0). It is recorded in
   `certification-item.json`'s `reference_configuration` as `personality`,
   `personality_settings` and `personality_why` (the devmgr precedent) and in
   `ITEM.md` §5. While both exist, `xtask check` builds both on three
   architectures, and the item's code (`main.rs` included) under the two is
   shown identical or its differences listed.
3. **Flip and delete.** `user` becomes the default and the reference, and the
   in-kernel load is deleted in the same stage. At the flip, coverage, rows
   and boot-line comparisons are **measured afresh** on the new reference,
   not carried, and F-10's residual is re-enumerated.
4. **Split.** Inside user space, `linuxd` is cut along the lines B drew:
   `netd`, the device servers, the console server (M8). These change no kernel
   code, except where a split needs a new mechanism.

## 5. Boot and authority

init is a Linux program (std on musl, `INIT.md`), so it cannot be the first
process of a kernel without a Linux personality. **The kernel starts three
processes from the initramfs**: the **partition manager** (below), devmgr, with the device authority it holds
today (K8's handles, never through init or `linuxd`: the L12 rulings, ledger
33-34; `OPAQUE-KERNEL.md` §3), and `linuxd`, whose first program is init.
That is the reference configuration's shape today (the kernel starts devmgr;
`ferrix.devmgr=init` is outside it, `ITEM.md` §5, AoU-8, AoU-13), so the
reference does not move to a devmgr the kernel does not start. `btrfsd` and
any certified user-space item are started under K14.

**The partition manager** (§6.3; the consultant's recommendation, ledger
886, accepted by the customer 2026-10-13) is a small certified native
program. It reads a static description of the safety partitions (their
images by K14 digest, K9 windows and budgets, K12 reservations, handles and
statically assigned devices), starts them, holds their authority and
supervises them through K4's exception channels. So a safety partition's
authority never passes through init, `linuxd` or devmgr. The description is
security-policy data: for Common Criteria the manager is in the TOE and the
description is set by an administrator (A.ADMIN). The tool that generates
the description is classed by how its output is used: **T3 / TQL-1** if its
output is not independently verified, or a qualified independent checker of
the description at **T2 / TQL-5** with the generator then uncertified; the
plan proposes the second. Devices go to safety partitions
statically, from the description; devmgr serves only the Linux side.

**What the kernel hands each at start:**

| To | Handles |
|---|---|
| the partition manager | the safety partitions' devices (K8 handles, statically, as the description names them), their images' sealed VMOs, the K9 windows and K12 reservations to hand out, the debug log's write handle |
| devmgr | the device authority it holds today, less the safety partitions' devices, as K8's handles (the right to bind a function to a domain, apertures, interrupts, I/O mappings, configuration reads and the mediated writes, quiesce, limits, isolation); the debug log's write handle |
| `linuxd` | **init's three inputs as read-only VMOs**, read by the kernel from `.ferrix/init/program`, `.ferrix/init/script` and `.ferrix/init/commands` (`init.rs`), so L.init.2, choosing pid 1's inputs, stays the kernel's; the initramfs as a read-only VMO for the rest of the root; the Linux partition's job; the audit-read handle pid 1 holds today, which `linuxd` moves into init's private table (K11) and holds no use of itself; a channel to devmgr; the time page's VMO; the debug log's write handle |

AoU-24 is about the initramfs unpacker in `ferrix-vfs`, which leaves ring 0
with `linuxd`; it becomes an assumption on `linuxd`, outside the TSF, and
the kernel no longer discharges it.

What does move: the root mount becomes `linuxd`'s, and the kernel's re-root
of pid 1 (`INIT.md` §7.3) goes. Stages 10 to 12's in-kernel disk checks,
which mount and read through ring 0 at boot, become user-space checks. That
loses a property of the reference boot today, a kernel that proves its own
disk path, and §11 Q9 puts it to the customer.

## 6. Certification on the new boundary

### 6.1 The kernel as the item

* `ITEM.md` is rewritten around a kernel with no `load` ring: the
  composition-root edges and the higher-ring `mod` declarations go to zero,
  and `check-item-boundary.py` fails on any load-ring file in the kernel,
  with a planted file as its negative control.
* **The TOE is the kernel plus the partition manager and its loader
  library** (§5, §6.3). **The safety element is the kernel**, with those two
  as certified items of the minimum set the claim names. The Linux personality is already
  a threat agent outside the TSF (ST §3.2). What leaves the TSF with the load
  is stated in ST §1.3 and §3.2: the audit of the switch of `/`; any
  file-level event. The audit records change as follows: FAU_GEN.1 b) loses
  the `ferrix.devmgr` record with the option (§3, 0x1052); in c), the
  cgroup-kill and cgroup-file-limit records become job records whose subject
  is the Linux process the job holds; the OOM-kill record stays the kernel's.
  **Who may read the audit log (FAU_SAR.2):** the audit-read handle reaches
  init through `linuxd`'s shared table (§5), so the authorised reader is the
  Linux partition, `linuxd` and init together, not pid 1 alone. That is
  stated in the ST. It is no wider in practice than today, where the load in
  ring 0 runs pid 1's calls and a compromised `linuxd` runs init's. FDP_ACC is unchanged: it claims capabilities only, and
  file permissions were never in it.
* AoU-3 changes from "the Linux layer is excluded" to "no user-space
  component is part of the safety element unless it is certified as one
  (§6.3)". ASR-1 is restated per §2.3's table. The integrator places safety
  functions in native partitions with K9 budgets and K12 reservations.
* `MEMORY-AND-TIMING.md` gains the temporal half: every system call bounded
  by argument and measured, the kernel's longest non-preemptible section
  stated and measured per architecture, K9's guarantee argued. AoU-4 and F-24
  stay: no WCET is claimed.
* The "no kernel heap" design, out of scope in earlier revisions, is in
  scope since the customer chose maximum certification: K16 and §6.5.

### 6.2 What the kernel's code must meet

The item's existing rules, which today apply to half of ring 0:

* every kernel function traced to a low-level requirement, one check each,
  ids reserved before writing, retired ids never reused;
* 100% of statements covered or individually justified on every architecture
  (F-10's rule), decision coverage as recorded today;
* every allocation fallible (F-23's rule, held by an `xtask check` step);
* every `unsafe` site traced (F-26's rule);
* no panic reachable from a system call except the documented fatal ones;
* static stack depth bounded and checked in the build, per architecture;
* a qualified toolchain for every target the kernel builds for (F-17).

**B5, the evidence the load carries for the item.** 91 of the 531 ids that
kernel files verify are verified *only* by checks in load-ring files
(`tools/common/gen/microkernel-surface.py orphans --list` on 5450ee1fe):
H.DEV.2/4, H.MEM.8/16, H.QUOTA.6/7/10-12, H.SCHED.9, H.TRAP.9/11/12/16, and
L.aarch64, L.armv7a, L.device, L.object, L.quiesce, L.sched, L.syscall,
L.trap, L.user and L.x86_64 ids. The most are in `syscall/check.rs` (31,
which also holds ASR-7's 427 refusal assertions), `fs/mmap_check.rs` (9),
`syscall/seccomp_check.rs` (7), `interfaces/block_ring/check.rs` (6) and
the cgroupfs checks (22 together). In M2 and M3, before M5, each is
re-homed to a check in a core or item file with its negative control fired
again (`gate.sh control --expect`, FIRED). A new `xtask check` step fails when
a core or item id is verified only from a load-ring or user-space check.
It is an evidence gate, so it has a negative control: a planted orphan id
(a `Verifies:` tag for a kernel id moved into a load-ring check) makes it
fail. Its scope is stated where it runs: it counts `Verifies:` tags in
kernel source files only, not host tests or gate-level checks, so an id
whose only check is a host test or an xtask row is outside it and argued
apart. It gets a `TOOLS.md` entry when it joins `xtask check` (F-19).

**Moves.** Core and item code that leaves or moves in M6 (the console's
screen and input, the panic screen, the board glue) goes through
`carry-coverage.py` then `gen-coverage-justification.py --check`, dropped
anchors listed, with byte-identical bodies and identical boot lines on four
boots for what stays.

### 6.3 Certified user-space components

A user-space component can be certified on its own: a separate item with its
own manifest entry, requirements and coverage, resting on the kernel's
isolation as an assumption. Each adds a document set of its own.

**btrfs.** The 2026-10-02 rule is that btrfs is certified "for as long as it
runs inside the kernel" (`ITEM.md` §2). It does not make a `btrfsd`
certified: that is a new decision (§11 Q4). **This plan proposes** a
certified `btrfsd` split out at the lift, so that btrfs never spends a
release uncertified. If the customer agrees:

* the two crates move byte-identical, so their host tests and L.btrfs
  evidence carry;
* ASR-9, T.MEDIA, O.MEDIA, FPT_TDC's volume refinement, FM-11 and
  AoU-15/16 move to `btrfsd`'s item;
* its runtime and the `Device`/`WriteDevice` glue (load today) are in that
  item or argued;
* BACKLOG row 398 (b), the derived commit reserve, gates it as it gates
  btrfs today;
* it is started under K14.

**The certified set** (the consultant's recommendation, ledger 886; the
customer: "Accept as recommended", 2026-10-13):

| Component | Status | Why |
|---|---|---|
| The partition manager (§5) | **required** | without it a safety partition is started by a Linux program, against the L12 rule; a few thousand lines with the loader (*guess*) |
| K15's loader library, as the partition manager links it | **required** | K14 measures the code, but the layout, relocations and initial data are the loader's |
| The native runtime library (`rt`, 1,166 lines today), with no heap | **certified** (strongly recommended) | every safety application links it; Ferrocene covers `core`, not `rt` |
| `btrfsd` | certified (decided) | §6.3 above |
| One block driver per safety storage medium | certified, as used | virtio-blk about 1.6k lines, stm32-sdmmc about 2k, NVMe about 3.6k; an NVMe controller's firmware stays an AoU |
| ARMv7-A's DMA-capable drivers | certified, only those a safety configuration uses | X1's rule; none if the safety partitions use no DMA device |
| devmgr | **not certified** | safety devices are assigned statically by the partition manager; devmgr serves the Linux side, confined by the IOMMU and X1's rule. If devices are ever to reach safety partitions at run time, devmgr joins the set |
| The log and audit server, the console server, init, `linuxd`, authd, sessiond | not certified | audit stays the kernel's; an AoU says no safety partition calls any of them |

Each certified component carries the kernel's targets: its requirements,
MC/DC coverage, an FMEA and a safety-manual section.

### 6.4 What it gains, and what it does not

Gains, against `CLAIM.md` §3.1:

* **(a) memory**: held by the hardware, by the privilege boundary and the
  restricted address spaces, instead of by Rust and an audit of the load;
* **(b) resources**: FX-0008 and AoU-5's second case go; a Linux partition
  exhausts its own job (K12);
* **(c) time**: FX-0503's spin locks in load code go; ring-0 time is the
  kernel's, and K9 charges servers' time to their clients;
* **(d) failure**: a Linux-layer panic is a process fault, not a kernel stop.

Not gained:

* **DMA on ARMv7-A, and behind a refused IOMMU unit**, is untranslated (V-03,
  degraded trusted mode). A server that drives a DMA device there stays
  trusted, so "a boundary the hardware enforces" is qualified on those
  configurations. ARMv7-A is native-only (§11 Q7): no Linux programs, and a
  DMA-capable function there goes only to a certified driver (§6.5).
* **The kernel does not shrink much.** K1 to K15 add code; M6 removes the
  console's screen and input, the panic screen and the board glue. *Guess*:
  the kernel's product code ends near today's core plus item, 75,000 to
  90,000 lines across three architectures, against 79,540 today. The gain is
  that it is *all* of ring 0, not that it is small.
* **A second item's document set** for each certified user-space component.
* **The claims that leave the TSF** (§6.1) and the isolation between Linux
  processes that moves to `linuxd` or to a speculation-domain decision
  (§2.3, §11 Q5).
* **The steps outside the repository** still block every target: the
  accredited pre-assessment (`CLAIM.md` M2), a quality management system
  (F-28), independent reviewers (F-27), a position on AI-authored code (F-29)
  and a qualified toolchain (F-17). The consultant's reviews remain internal
  and are not independent verification.

### 6.5 Maximum certification

The customer's words (2026-10-13): "cert level asil d etc so max
certification", and, asked for IEC 61508, "SIL 4". **The plan reads this as**
the whole new kernel argued at the top level of each standard, from M7
(§11 Q8). Until the flip, today's item keeps the four present targets of
2026-10-01 (EAL5+, DAL C, IEC 62304 Class C, EN 50716 SIL 2); `README.md`,
`CLAIM.md` and the manifest's ring targets change in M7.

| Standard | Present (2026-10-01) | From M7, the new kernel |
|---|---|---|
| ISO 26262 | (`core`: ASIL D aspirational) | **ASIL D**, as a SEooC |
| IEC 61508 | (`item`: SIL 2; `core`: SIL 3/4 aspirational) | **SIL 4** (the customer) |
| EN 50716 | SIL 2 | **SIL 4**, aligned with IEC 61508 |
| DO-178C | DAL C | **developed to DAL A objectives**: DO-178C issues no certificate on its own, so the kernel is offered for approval inside an aircraft system (AC 20-148's reusable software component, DO-297's IMA platform) |
| IEC 62304 | Class C | Class C (already the top class) |
| Common Criteria | EAL5+ | **EAL6+**, with EAL7's formal design as a further step |

`CLAIM.md` §2 called SIL 3 and ASIL C/D "not a near target"; that text
changes in M7.

**The first standard is IEC 61508 at SIL 4**, with ISO 26262 ASIL D pursued
alongside (the customer, 2026-10-13: "SIL 4 first"). The first assessment's
reference platform is the AArch64 safety processor class below, because
x86-64 carries no timing claim. **The claim**, for `CLAIM.md` §1, for the
new kernel from M7 (the consultant's draft, ledger 886, fitted to SIL 4
first; until the flip `CLAIM.md`'s present sentence stands):

> Ferrix is a Rust microkernel being developed as a compliant item to IEC
> 61508-3 with systematic capability SC 4, with a safety manual for compliant
> items (IEC 61508-3 Annex D), and as a Safety Element out of Context to ISO
> 26262 ASIL D. Everything that runs in the processor's privileged mode is
> the element. It keeps native safety partitions apart from one another and
> from a partition running unmodified Linux software (QM, carrying no safety
> claim) in memory, devices, failure and processor time on Arm Cortex-A78AE
> processors in Lock-mode. x86-64 and ARMv7-A are outside the first
> assessment. It has not been assessed.

61508 gives SILs to safety functions; a software element is claimed with a
systematic capability, hence SC 4. "Out of context" is ISO 26262's term
(SEooC), used here only for that standard. **x86-64 and ARMv7-A are outside
the first assessment** because no qualified compiler offers their targets
(below; Z1, ledger 902). When x86-64 enters, once the vendor offers
`x86_64-unknown-none` (F-17, §11 Q17), the claim adds "and in memory,
devices and failure on x86-64 with an IOMMU, without a timing claim". When
ARMv7-A enters, it names it as native partitions only, isolation without
timing, user-memory protection by software alone.

Its rules: never "certified", "is compliant", "compliant with", "a SIL 4
kernel" or "safe Linux" before an assessment says so ("being developed as a
compliant item" is allowed); "not assessed" in the same sentence as any
standard named; the Linux software never in a sentence that says "assured"
or "safety"; the hardware assumptions (an IOMMU on x86-64 and AArch64, the
processor class for timing) named wherever the claim is given in full.

**The toolchain falls short of SIL 4 today** (research, 2026-10-13, sources at
the end of this document; F-17): Ferrocene 26.05 (Rust 1.95)
is qualified for ISO 26262 ASIL D, IEC 61508 SIL 3 (it "supports" SIL 4) and
IEC 62304 Class C, and offers no DO-178C qualification. Of the kernel's
targets it qualifies `aarch64-unknown-none` only: `x86_64-unknown-none` and
`armv7a-none-eabi` are not offered. A SIL 4 qualification of the compiler
for the reference platform's target, and a route for the other two targets,
are items for the vendor (F-17) before any SIL 4 claim. Consequences:

* **The compiler that builds the certified kernel is Ferrocene's**, on the
  reference platform's target. Today Ferrocene is at Rust 1.95, the tree
  pins 1.97.1 and Verus pins 1.98.1. From M0, CI builds `core` and `item`
  with Ferrocene's compiler, so no language feature newer than it creeps in.
* **Whether a compiler qualified to SIL 3 suffices for SC 4** is a question
  for the pre-assessment (`CLAIM.md` M2), with V2's object-code verification
  named as the compensating measure.
* **x86-64's and ARMv7-A's places in the claim wait on a compiler route**
  for `x86_64-unknown-none` and `armv7a-none-eabi` (F-17, §11 Q17); until
  then both are outside the first assessment (§11 Q7). Both stay built,
  booted and gated as today; only the claim waits.

**What the levels ask, and where.** The standards' texts are not in the
tree. The clauses below are the plan's and the consultant's reading, marked
by the standards' own grades (*required*; *HR* highly recommended; *R*
recommended), and **M0 checks each against the purchased text** and corrects
this table before any stream is sized on it.

| Technique | ISO 26262-6:2018 | IEC 61508-3:2010 | EN 50716 | DO-178C | Common Criteria |
|---|---|---|---|---|---|
| No dynamic objects (K16) | Table 6, 1b: HR from ASIL B | Table B.1, items 2 and 3a: HR from SIL 2, so a justified deviation is needed at every SIL; that SIL 3 and 4 make the justification unrealistic is `CLAIM.md`'s judgement, not the text's | Annex A (as EN 50128's): to check | — | — |
| MC/DC (V1) | Table 9, 1c: HR at ASIL D | Table B.2, item 7c: HR at SIL 4 | Annex A: HR at SIL 3/4, to check | Table A-7, objective 5: required at Level A, with independence | — |
| Object code not traceable to source (V2) | — | — | — | 6.4.4.2b, Table A-7 objective 9: required at Level A | — |
| Formal methods (F1, F2) | Table 2, 1d formal notations and Table 7 formal verification: R at ASIL D; semi-formal notations HR at C and D | Tables A.1, A.2, A.4: formal methods HR at SIL 4 | Annex A: HR at SIL 3/4 | DO-333, as an alternative means | **EAL6 requires** a formal security policy model (ADV_SPM.1); EAL7 adds formal functional specification and design (ADV_FSP.6, ADV_TDS.6) |
| Timing analysis (T1) | Table 8 resource-usage analysis: HR at ASIL D | Table A.2/A.5 timing behaviour: HR at SIL 3/4 | Annex A: to check | 6.3.4f, worst-case timing: required; multicore interference: AMC 20-193 / CAST-32A | — |
| Independence | Part 2 confirmation measures: I3 at ASIL D | Part 1 §8, assessment: independent organisation at SIL 3/4 | independent assessor | verification independence at Level A | ALC_DVS.2, ALC_CMC.5, ATE_DPT.3, AVA_VAN.5 at EAL6 |

**What each adds to the plan:**

* **K16**, no kernel heap after boot (§3).
* **V1, MC/DC** on the whole kernel. A *condition* in Rust is a boolean
  operand of `&&` or `||`, and a *decision* is every `if`, `while`, `match`
  arm and guard, `if let`, `let`-`else` and `?`; V1's design makes this
  definition precise and the consultant reviews it. MC/DC is measured on
  source, with the source-to-object correspondence argued by V2. The tool is
  a verification tool, qualified at DO-330 TQL-5 (F-19); the compiler's
  qualification is F-17. This plan knows of no qualified MC/DC tool for Rust
  on the kernel's targets, so V1 finds or builds one.
* **V2, object-code verification** (DO-178C 6.4.4.2b): what monomorphisation,
  inlining and the compiler's generated code add beyond the source,
  identified and verified. A work item of its own.
* **F1, the formal isolation model**, an EAL6 deliverable (ADV_SPM.1), not
  only an EAL7 one. Its properties, each mapped to the ST's policies and the
  safety requirements: spatial isolation, no partition reads or writes
  another's memory except through a VMO granted by handle (the Address Space
  Separation SFP, ASR-1, ASR-5); authority confinement, a partition reaches a
  resource only through a handle it holds and rights never grow (the
  Capability Access Control SFP, ASR-3); resource isolation, a partition's
  kernel memory and quota are its own (FRU_RSA.1, K12, K16); temporal
  isolation, a partition's budget and its static window are honoured
  (ASR-8, K9). **Speculation and timing channels are outside the model**;
  they are argued in `SPECULATION.md` and by T1, and the model says so.
* **F2, refinement**: proofs that the core's object, handle, IPC and
  address-space code implements F1's model. Kani gives bounded proofs of
  test harnesses, not refinement, so F2 needs Verus, Creusot or a proof
  assistant. F2's design states the proofs' trusted base, the toolchain's
  impact (Verus brings its own compiler driver, which must agree with the
  pinned rustc and with Ferrocene), and lists every proof tool in
  `TOOLS.md`.
* **The proof tools and who does the proofs** (the customer: "find a
  proofer"; research of 2026-10-13, sources at the end of this document):
  * **F1 in Isabelle/HOL.** It has the deepest separation-kernel precedent:
    seL4's integrity and confidentiality proofs (AArch64's completed in
    2026), PikeOS's formal API and its noninterference model, and AWS's
    Nitro Isolation Engine. Its LCF kernel is the easiest to defend to CC
    and DO-333 assessors. Under CC:2022, ADV_SPM.1 requires a formal model
    of the **entire TSF**, so F1 covers every kernel interface, not only
    `core`.
  * **F2 in Verus.** It is the only Rust verifier with an end-to-end verified
    microkernel close to Ferrix's shape (Atmosphere, SOSP 2025: capabilities,
    IPC, address spaces, IOMMU, with refinement and noninterference proofs,
    about 3.3 lines of proof per line of code), it handles the raw pointers
    and interior mutability the core needs, and the verified source builds
    with a plain rustc because ghost code is erased. The alternative is all
    of it in Isabelle with AWS's AutoCorrode (one prover, Nitro's
    precedent), at the price of µRust's lack of traits and dynamic dispatch.
  * **The seam**: Verus's top-level specification is shown equal to F1's
    Isabelle model, by a translation argument or by mirroring under review;
    EAL7's formal correspondence may push toward the all-Isabelle option,
    which F1's first model decides.
  * **Gaps**: no Rust verifier has a DO-330 or ISO 26262-8 qualification or
    has served as certification evidence, so F2 builds a TQL-5 case around a
    frozen Verus and Z3; Verus pins rustc 1.98.1 against Ferrocene's 1.95,
    so the plan argues that the two front ends agree on the verified source;
    Verus's trusted base includes about 700 lines of axioms.
  * **Who**: **Proofcraft** (the seL4 proof engineers; they did seL4's
    AArch64 security proofs and work with Verus in DARPA INSPECTA) for F1,
    the proof architecture and certification-grade proof engineering; for
    F2, the Verus kernel groups (Utah's Atmosphere team, Kansas State with
    Collins Aerospace, who helped write DO-333), or **CertiK** as a
    commercial Verus contractor. None publishes prices; contacting them is
    the customer's.
  * **Effort** (*guess*, from seL4's about 25 person-years for 8,700 lines
    of C and Atmosphere's 1.5 person-years for 6,000 lines): F1 2 to 4
    person-years, F2 6 to 15, so **8 to 20 person-years** before the
    certification paperwork, the same order as seL4. The core's size after
    M6 decides it.
* **T1, timing**, claimed on **AArch64 safety processors only** (the
  customer, 2026-10-13). **The named class: Arm Cortex-A78AE (Armv8.2-A, no
  SMT) in DSU-AE Lock-mode** (CEMODE 0b11, set at cluster cold reset; each
  core pair seen by the OS as one core), which Arm rates ASIL D / SIL 3 for
  hardware metrics. Hybrid mode is outside the claim (Arm rates it ASIL B).
  Software facts from Arm's TRM that the kernel must honour: MPIDR_EL1.MT
  reads 1 although the core is not multithreaded, so SMT is never inferred
  from it; there is no MPAM, so the shared L3 is partitioned only through
  CLUSTERPARTCR_EL1; a lockstep mismatch is reported to the SoC, which owns
  the fault reaction. Cortex-A720AE and Neoverse V3AE are a later extension.
  **The board**: AMD's VEK385 (Versal AI Edge Gen 2: 8 Cortex-A78AE in four
  two-core clusters with documented per-cluster lock-step, SMMUv3, GICv3
  with ITS, upstream TF-A, U-Boot with UEFI, a public TRM and a QEMU model;
  about $16,000, 10 to 16 weeks' lead). The runner-up, NVIDIA's Jetson AGX
  Orin developer kit (12 Cortex-A78AE, EDK2 UEFI, about $3,500), has the same
  core and is a cheap porting platform, but Lock-mode is unsupported by
  NVIDIA's own software, its IOMMU is an SMMUv2 with quirks and the SoC is
  rated ASIL B for random faults, so it is not the evidence platform. NXP's
  S32G is excluded (no SMMU). Buying a board is the customer's decision
  (§11 Q18). x86-64 stays certified for isolation, with **no
  timing claim**; AoU-4 says so. A bound is a **measured high-water mark plus
  a stated margin**, on the named processors, with multicore interference
  channels identified and bounded per AMC 20-193 / CAST-32A (shared caches,
  interconnect, memory controller), and static analysis where a qualified
  tool exists for the core. F-24 changes from "no WCET claimed" to these
  bounds on these processors.
* **Independence.** F-27 (independent reviewers), F-28 (a QMS) and F-29 (a
  position on AI-authored code, which ALC_DVS.2 makes pressing) block more at
  these levels, not less. The consultant's reviews stay internal.
* **Hardware assumptions.** ASIL D's hardware metrics, ECC and lockstep
  belong to the integrator's hardware, as AoUs of the SEooC. An IOMMU is
  required for the certified configuration on x86-64 and AArch64.
* **ARMv7-A as a native-only safety target** (P7). Without an IOMMU, an
  assumption of use cannot make an uncertified DMA-capable driver trusted at
  SIL 4 or ASIL D: it would be part of the safety element. So on a
  configuration with no IOMMU (or a refused unit), **devmgr may grant a
  DMA-capable function only to a driver whose image digests K14 finds on the
  certified list**; any other grant is refused by the kernel, with a check
  and a negative control. **"DMA-capable" is classified per platform in the
  kernel's device registry**: every bus-mastering function, and on SoCs the
  DMA controllers' MMIO (the STM32MP1's DMA and MDMA among them), since a
  driver that reaches a DMA controller does DMA whatever its own device is.
  **ARMv7-A's claims stay reduced in two ways**, which AoU-6's rewrite, §11
  Q7 and the customer read together: ASR-1 has no hardware backstop on the
  Cortex-A7 (no PAN; LPAE has no domains), so the software check in
  `uaccess` is the only barrier and is argued as such at SIL 4 / ASIL D; and
  by the customer's timing answer (§11 Q8) ARMv7-A carries **isolation
  claims without timing claims**. What AoU-6 no longer reduces is DMA, which
  only certified drivers may do there.
* **The speculation switch** (§11 Q5): the certified reference builds with the
  barrier; the performance build is outside the certified configuration.

## 7. Performance rows

The switch is gated by numbers, as S0 gated B. Every row is a perf row whose
method is written before the work, and every row can fail:

* measured on nazuna under the gate's KVM with the CPU model and the
  mitigations stated, and the same on both sides (the matched rule,
  2026-10-02);
* five boots a side (G8), alternated against the baseline;
* the baseline's hash and its number written into the row before work starts;
* a fail rule (1.10x the target, the round trip's precedent, unless the row
  says otherwise);
* a negative control: an injected delay of known size makes the row fail.

nazuna has no PCID, and no row runs on hardware; both are stated beside each
figure.

**Maximum performance** (the customer, 2026-10-13). Every Linux-facing row's
target is **no slower than today's in-kernel personality** (1.00x of
`--personality kernel` on the same tree), failing above 1.10x; the native
rows keep the customer's existing targets. The means in scope: PCIDs and
ASIDs, ERAPS, batching, and a **cheaper generic restricted-mode exit**. Two
are excluded: global pages for `linuxd`'s text, because a global TLB entry
survives the switch into a restricted root and would let a program read and
run `linuxd`'s text (R1; the consultant ruled global user pages NOT YET
until a design of their own, ledger 413, U10 to U14); and answering any
Linux call in ring 0, which would put Linux personality code back in the
item.

**The measurement host is nazuna** (AMD Ryzen 9 9900X, Zen 5; the customer:
"use amd instead"), which has no PCID (CPUID.1:ECX[17] clear), has INVPCID,
ERAPS, AutoIBRS and SVM with 32,768 ASIDs (the consultant's measurement,
ledger 886). **Tagging restricted mode with SVM ASIDs is rejected**: a
VMRUN world switch is not cheaper than the root switch it removes on Zen,
every Linux call would become an exit, it would put a hypervisor into the
x86-64 item only, its evidence would come from nested SVM under KVM, and it
would buy speed on the platform with no timing claim (ledger 886). K1's
design records it as rejected.

**seL4's methods, to reach seL4's level** (the customer, 2026-10-13: "Use
the same methods seL4 uses, it must be possible to reach the same level,
they did it, so can we"). The targets stay as they are, and the stop rule
stays on the bare-call rows. The evidence that they are in reach is seL4's
own figure on this machine: **440 ns for a round trip with two page-table
switches and no PCID** (`OPAQUE-KERNEL.md` §9.6). A Linux call through
restricted mode has the same shape as a seL4 call and reply: out of the
program, into `linuxd`, and back. The (ii) row's fail line, 1.10 times
today's 357 ns, is 393 ns, which is the customer's existing round-trip goal
of under 400 ns (2026-10-06). The methods, each a design for K1's review:

* **A fastpath** for the common restricted exit and re-entry, as seL4's
  fastpath for Call and ReplyRecv: one hand-tuned path with no scheduler
  decision, no lock, no allocation and no general dispatch, falling back to
  the general path on any unusual state.
* **Lazy vector state**, under K1's four rules, as seL4 switches the FPU
  lazily. Calls that copy data (`read`, `write`) will trap anyway, so the
  saving is for calls like `getppid`: `linuxd`'s call
  path is built to use no vector registers, so an exit saves none; the first
  use traps (CR0.TS, CPACR_EL1) and saves the program's state then. K1's
  rule that the program's whole vector state survives is kept; only when it
  is saved changes.
* **A small, hot path**: the entry, the check of the context (C6), the root
  switch and the return in as few cache lines and TLB entries as possible;
  `linuxd`'s dispatch reached by a direct call.
* **Nothing global**: no global pages (P1), no TLB tricks the R1 rule
  forbids; the root switch is paid, as seL4 pays it.
* **ERAPS** on nazuna in place of the return-stack refill (about 20 ns).

**The certified build under seL4's methods.** seL4 sets no speculation
barrier per call by default; it offers an optional IBPB on every context
switch, and its time-protection research flushes at domain switches. With
IBPB at every switch, seL4's own round trip on nazuna is about 900 ns
(§9.6's figures: 440 ns plus about 230 ns each way). §11 Q5's certified
build (i) has a barrier on every restricted exit and entry, so at seL4's
level it lands near the same 900 ns, over its 714 ns limit (2.00 times
357). That is §11 Q16, for the customer. On the AArch64 class the picture
differs: restricted mode and `linuxd` run under different ASIDs, and
FEAT_CSV2 keeps branch-predictor targets from crossing ASIDs, so the barrier
may cost little there; K1's design measures it on the board.

**Two builds, both gated** (§11 Q3 and Q5, the customer, 2026-10-13: "gate
both"):

* **(i), the certified reference build**, with the barrier on every
  restricted exit and entry. It is the reference configuration: every gate
  whose result counts as evidence (coverage, boot checks, controls, any row a
  claim cites) runs on it. Its rows are gated at **2.00x of today's in-kernel
  figure, failing above 2.00x**: the customer's chosen answer read "within
  2x of today ... and also fails if missed", so no tolerance is laid on it.
* **(ii), the performance build**, with one Linux partition as one
  speculation domain. Its rows are gated at **1.00x, failing above 1.10x**,
  and labelled as `--mitigations off`'s figures are: measured and gated, not
  evidence. (ii)'s rows compare a weaker build with today's stronger baseline
  and are unmatched in the 2026-10-02 sense; (i)'s are the matched ones.
* The switch is a compile-time `--cfg` recorded in `reference_configuration`,
  so (i)'s image carries no (ii) code, and nothing is deactivated code.
  Under (ii), the partition's job becomes a speculation domain by the
  2026-10-01 rules: marked by its MANAGE holder at creation, with an audit
  record.
* **The stop rule** is driven by (ii)'s restricted row **on nazuna**. A failing (i)
  row blocks the flip (M5) until it passes or the customer decides again.
  The pricing below expects (i) near 900 ns even at seL4's level, because of
  the barrier, over its 714 ns limit: §11 Q16.

| Row | Today (hash, date) | Target (the customer: maximum performance) |
|---|---|---|
| Native channel round trip, `domain-call` p50 | about 820 ns (523fc3d50, 2026-10-07) | under 400 ns (the customer's target, 2026-10-06) |
| Linux `getppid`, `--personality kernel` | 357 ns, `qemu64` with controls; Linux 53 ns (2026-10-10) | **the baseline, frozen**: M0 re-measures it on a named commit and that figure is the baseline for the next row from then on |
| Linux `getppid` through restricted mode, K1's vector-state save included; the performance build gated, the certified build beside | not built | 1.00x of the frozen baseline above, failing above 1.10x. The baseline does not follow the tree: M4 makes the in-kernel call faster, and a moving baseline would push this target below its floor (two root switches, about 260 ns on nazuna). A target in multiples of Linux would be below today's in-kernel call (3x Linux is 159 ns) and is not used |
| `fork` + `exec` + `wait` of a static program | to measure in M0 | 1.00x of `--personality kernel` |
| `mmap`/`munmap` churn, 4 KiB anonymous | to measure in M0 | 1.00x |
| A page-cache miss end to end (through `linuxd`, `btrfsd` if split, and the ring-3 driver) | to measure in M0 | 1.00x |
| 4 KiB block read, ring and ring-3 driver | 300 to 844 us (2026-09-27; to re-measure in M0) | under Redox's 80 us p50 (the customer's target, 2026-10-03) |
| Signal delivery to a running thread | to measure in M0 | 1.00x |
| `futex` wake to a waiting thread | to measure in M0 | 1.00x |
| Warm `rustc hello.rs` | 0.14 s (to re-measure in M0) | 1.00x |
| Cold `test-rustc` and the self-host build | to measure in M0 | 1.00x |
| Chrome network bench, compositor frame time, `test-unity` | to measure in M0 | 1.00x |

**What the restricted-mode row is expected to cost** (x86-64 on nazuna; M3
measures each). The first estimate, by parts, was 0.7 to 1.2 us for (ii) and
1.2 to 1.7 us for (i), with the root switch priced at 100 to 300 ns each
way. seL4's measured round trip on the same machine, both switches
included, is 440 ns, so the root switch costs much less than that estimate
took (about 130 ns each way, §9.6) and the estimate is replaced by seL4's
figure as the target to reach:

| Build | At seL4's level | Fail line | Expected |
|---|---|---|---|
| (ii), the performance build | about 440 ns | 393 ns (1.10 times 357) | **reachable by beating seL4 by about 11%**, which is the customer's existing goal (under 400 ns) |
| (i), the certified build | about 900 ns (seL4's 440 plus the barrier, about 460) | 714 ns (2.00 times 357) | **fails** unless the barrier moves to partition switches (§11 Q16) |

A Linux program making 10,000 calls a second (Chrome makes about 8,000)
would spend under 0.1% of a processor more than today at (ii)'s target; a
warm `rustc` compile (4,345 calls, `OPAQUE-KERNEL.md` S0) well under 1 ms
more. The cold run's call count is measured in M0.

**The stop rule.** If the restricted-mode row cannot be met, the plan stops
after M4, with every mechanism landed and nothing lost, and the customer
decides again on the numbers, as on 2026-09-27.

## 8. Stages

Points are *guesses*. A stage's gate passes before the next stage's code
lands; designs may be written and reviewed ahead. **"Unchanged"** in a gate
means: PASSED, the same check counts, on all four boots (x86-64 under KVM and
TCG, AArch64, ARMv7-A; after the flip, ARMv7-A boots native programs only). A consultant's OK on a document is a review, recorded
in the ledger, and not a gate.

| Stage | What | Points | Gate |
|---|---|---|---|
| **M0** | §11's decisions written in BACKLOG; §7's rows with methods, baselines and controls; `--personality` recorded as a reference-configuration setting (C11); a ratchet in `xtask check` on the `load` ring's size, set to §11 Q10's answer (it reports growth if feature work stays allowed as now, and fails on it if not), with a planted file as its control; **§6.5's clause table checked against the purchased standards' texts** and corrected before V1, V2, T1, F1 or F2 is sized; **the research of 2026-10-13 checked with it**, each item against its source in the appendix (CC:2022's ADV_SPM.1 "entire TSF" reading, Arm's Lock-mode ratings and the A78AE manual's facts, seL4's AArch64 proofs, Ferrocene's qualifications and targets, Verus's compiler pin), confirmed or corrected before any stream or purchase is sized on it; the CI build of `core` and `item` with Ferrocene's compiler | 15 | the customer's answers; baselines on `main`; the clause table's check recorded (each row's clause, table, grade and level confirmed or corrected), and no stream's points stated before it |
| **M1** | K5, K6, K7, K10, K12 (small and independent), each with its ids reserved first | 25 | each mechanism's boot check and controls FIRED |
| **M2** | K2, K3, K4, K13, K15; the in-kernel page cache moved onto K3 **while still in ring 0**; B5's re-homing of the 91 ids, first half | 45 | `test-btrfs`, `test-powerfail`, `test-rustc` unchanged; F-37's charging tests; the orphan step of §6.2 passes for the ids re-homed |
| **M3** | K1, K11 on x86-64, then AArch64; seL4's methods for the restricted exit (fastpath, lazy vector state, a small hot path); ASIDs on AArch64; K9 on stage 14's HardRt with the static time-partition schedule; K14; B5's second half | 75, plus stage 14's own size | the restricted-mode row; a K9 interference row (a QM job at full load does not push a budgeted job past its deadline, with a control that removes the budget); the orphan step passes for all 91 |
| **M4** | §7's rows that are not K-items: the round trip, the bare system call, the block read | on the backlog already | §7's rows |
| **M5** | **The lift**: `linuxd` behind `--personality user`, with one Linux partition and every system-wide service inside it; `btrfsd` split out as a certified item (§11 Q4, yes); devmgr started by the kernel with K8's handles; the retired calls removed | 130 to 200 | every gate of `docs/BACKLOG.md`'s gate table under `--personality user`, unchanged; §7's rows; the Steam and Chrome rows; then the flip, with coverage and rows measured afresh, and the in-kernel load deleted |
| **M6** | **Shrink**: the console's screen and input, the panic screen's drawing and the board glue out; discovery cut to §2.1's list; the retired calls' requirements withdrawn; K16, the kernel heap retired | 70 (a *guess*, re-sized after K16's design) | `check-item-boundary.py` fails on a planted load file; coverage carried (§6.2, *Moves*); boot lines identical on four boots where the code did not change |
| **M7** | **Evidence**: `ITEM.md`, ST, SAFETY-MANUAL, MEMORY-AND-TIMING, TRACEABILITY, VULNERABILITY-ANALYSIS and CLAIM rewritten for the new boundary; the stack-depth check; coverage at 100%-or-justified on the whole kernel; the register recounted | 40 | the stack-depth and coverage checks in `xtask check`, each with a control; the consultant's review of each document recorded |
| **M8** | **Split** `linuxd`: `netd`, the device servers, the console server; then several Linux partitions | 45 | `test-net`, compositor, audio and card rows unchanged; a kill-and-restart row per server on K13; a two-partition row (one partition's `linuxd` killed, the other's programs unaffected) |

**Maximum-certification streams** (§6.5), run beside M1 to M8 and finished
before the claim:

| Stream | What | Points | Gate |
|---|---|---|---|
| **V1** | MC/DC: a tool for Rust on the kernel's targets, found or built, qualified (F-17); MC/DC on the whole kernel, 100% or justified | 60 to 120 | an `xtask` MC/DC step with a control (a planted untested condition fails it) |
| **T1** | Timing on Cortex-A78AE in Lock-mode, on the VEK385: the method (high-water mark plus margin), bounds for every system call and non-preemptible section, the multicore interference channels (AMC 20-193), the static schedule's interference test on hardware | 60 to 100 | a timing row per processor on hardware, with an injected-delay control |
| **V2** | Object-code verification (DO-178C 6.4.4.2b) for what the compiler adds beyond the source | 30 to 80 | a check over the image's functions against the source's, with a control |
| **F1** | The formal isolation model of the whole TSF, in Isabelle/HOL, and its proofs (spatial, temporal, resource isolation between partitions) | 150 to 300 (2 to 4 person-years, *guess*) | the proofs checked in `xtask check`; a control that breaks one rule of the model and fails a proof |
| **F2** | Code-level proofs in Verus that the core's object, handle, IPC and address-space code refines F1's model; the TQL-5 case for frozen Verus and Z3 | 300 to 700 (6 to 15 person-years, *guess*) | the same, per module; its scope recorded |

Total *guess*: about 470 to 560 points for M0 to M8, plus stage 14's HardRt,
plus 600 to 1,300 for V1, V2, T1, F1 and F2: **about 1,050 to 1,900 points**,
most of it the formal work. That is many times option B's 117, and the
largest piece of work Ferrix has planned. M1 to M4 and the four streams can
run as parallel streams split by file ownership (CONVENTIONS, *Splitting
one piece of work across several agents*). M5 is one stream with one owner;
its shim is one interface. F2 should start only after M6, on the code that
stays, so it is not proving code the lift moves.

## 9. The alternatives

`CLAIM.md` §3.2 and §3.3 keep other routes on record. Q1 of §11 is a choice
between these, not a yes or no:

| Route | What it gives | Cost (*guess*) | What "Linux software" means |
|---|---|---|---|
| **C, this plan**: the kernel is the item whole; Ferrix's own Linux layer in user space | all four isolations of `CLAIM.md` §3.1 by the privilege boundary; restartable services; SIL 2 within reach, SIL 3 on a further ratchet | 375 to 445, plus stage 14 | Ferrix's own Linux layer, as today |
| **2b**: the item as a separation kernel, real Linux as a guest | all four isolations; SIL 3 with industry precedent | 80 to 150 | a Linux VM; Ferrix's Linux layer becomes unnecessary |
| **5 + 4 + 3 (btrfs)**, `CLAIM.md` §3.3's SIL 2 proposal: software compartments in ring 0, the IPC fast path, btrfs in ring 3 | resources, memory by language, time if the load is made preemptible; isolation by hardware only for btrfs | 90 to 160, plus stage 14 | Ferrix's own Linux layer, as today |

**The customer chose C on 2026-10-13** ("Linux =use space => ok").
C is the only route that both keeps Ferrix's own Linux layer and takes it out
of ring 0. 2b is cheaper and proven, and gives up the Linux layer as a
product. 5 is cheapest and leaves the load in ring 0, with §3.1's arguments
still to write.

## 10. Risks

* **The lift stalls half way** and leaves two personality builds for months.
  Mitigation: M5 starts only after M1 to M4 have landed, and the switch lives
  one stage.
* **Performance.** If restricted mode costs more than §7 allows, Linux
  software gets slower everywhere at once. Mitigation: M3 measures it before
  M5 starts, and the stop rule.
* **Feature work keeps landing in the load** during M1 to M5 (Steam, NVIDIA,
  the desktop, the installer), each adding to what the lift moves and to the
  shim. Mitigation: M0's ratchet makes growth visible; §11 Q10 asks what is
  allowed.
* **The NVIDIA and card work** reaches the chardev core, which M5 retires.
  The card VM protocol and the N stages are re-planned against `linuxd`'s
  `/dev` service before M5, not broken by it.
* **Certification evidence moves under a moving tree.** Mitigation: ids of
  code that does not change keep their checks; B5 re-homes evidence before the
  lift; the boundary documents are rewritten once, in M7.
* **The formal work (F1, F2) is open-ended.** Its size is the least certain
  figure here. Mitigation: F1's model and its first proofs come before F2 is
  sized, and F2 is scoped module by module; the claim's level can be decided
  again on F1's result.
* **Maximum performance and maximum certification pull apart** on the
  barrier, the kernel heap (K16 replaces a fast shared heap with per-job
  memory) and the static schedule. Mitigation: the two builds of §7, and the
  rows that gate both.
* **The plan competes** with NVIDIA, Steam, the installer and the
  certification backlog for the same sessions.

## 11. Decisions for the customer

**Answered 2026-10-13** (the customer's words in quotes; *Decisions* in
`docs/BACKLOG.md` on this branch):

* **Q1, The route**: C, this plan ("Linux =use space => ok"). The 2026-09-16
  decision and the 2026-09-27 shelving are replaced.
* **Q2, `ARCHITECTURE.md` §2**: the plan reads the same answer as yes: the Linux
  ABI served by `linuxd` in user space, and mixed-ABI programs as §2.4 says.
* **Q3, The rows**: "performance: maximum performance", and, asked how to gate
  the two builds, "gate both". The plan reads these as §7's: (ii) at 1.00x,
  failing above 1.10x; (i) within 2.00x, failing if missed (the chosen
  answer's own words); (i) the evidence. §7's pricing expects (i) near 3x
  even on a PCID host, because of the barrier, so this gate is expected to
  hold the flip (M5) until the barrier gets cheaper.
* **Q4, btrfs**: "btrfs server: ok". The plan reads it, with Q8, as `btrfsd` a
  certified user-space item, split out at the lift.
* **Q5, Speculation inside a Linux partition**: the **build switch**. The
  certified reference builds with the barrier on every restricted exit and
  entry (i); a performance build treats one Linux partition as one
  speculation domain (ii) and is outside the certified configuration. The
  switch is a `--cfg` xtask sets, recorded in `reference_configuration` and
  `ITEM.md` §5 like `--personality`; `xtask check` builds both. Under (ii)
  AoU-14, SPECULATION §3, ST FDP_IFC's exception and V-07 are amended for
  that build. The 2026-10-11 game-mode decision (privileged trades of
  protections for frame rate) is a separate mechanism and is not decided by
  this.
* **Q7, ARMv7-A**: a native-only safety target. No restricted mode and no
  `linuxd` there. The plan reads this, under maximum certification, as
  §6.5's rule: a DMA-capable function goes only to a certified driver,
  enforced by the kernel. The ARMv7-A Linux programs' gates retire at the
  flip. ARMv7-A's safety role waits on a qualified compiler for
  `armv7a-none-eabi` (§6.5), so it is outside the first assessment.
* **Q8, Certification level**: "cert level asil d etc so max certification";
  asked for IEC 61508, "SIL 4"; asked when, "new kernel, from M7". The plan
  reads these as §6.5's table, which amends the 2026-10-01 targets for the
  new kernel from M7. Asked which processors the timing claim covers:
  "AArch64 safety chips".
* **Q9, The reference boot's disk path**: proven by user-space checks; every
  certified id keeps a kernel check (§6.2, B5).
* **Q10, Feature work in the load during M1 to M5**: allowed as now; M0's
  ratchet reports growth and does not fail on it.
* **Q6, Certified user-space components**: asked "which other components to
  certify; which one would the cert agent recommend?", then, offered the
  consultant's set, "Accept as recommended". §6.3's table: the partition
  manager, the loader library and `rt` certified; `btrfsd` and block
  drivers as used; devmgr not certified, safety devices static.
* **Q11, `core`**: "whether to keep the core ring as a separate structure; ask
  the cert agent". The consultant recommends keeping it as a layer with
  one set of targets for the item (§2.1); the plan follows it.
* **Q12, The first standard and the claim**: "the SIL D / ASIL D", then, asked,
  "SIL 4 first". IEC 61508 SIL 4 first, ASIL D alongside; the claim's
  wording in §6.5.
* **Q13, The proofs**: "how to do the formal proofs; find a proofer". The plan
  reads this as asking for a recommendation: F1 in Isabelle/HOL, F2 in
  Verus, Proofcraft and the Verus kernel groups or CertiK as providers
  (§6.5).
* **Q14, The measurement host**: "i don't have one, so use amd instead";
  nazuna. Asked what the stop rule should use: "Use the same methods seL4
  uses, it must be possible to reach the same level, they did it, so can
  we". The plan reads this as: the bare-call targets and the stop rule
  stay, reached by seL4's methods (§7).
* **Q15, The AArch64 safety processor class**: "find one". Cortex-A78AE in
  Lock-mode, the VEK385 to buy (§6.5).

**Still open:**

* **Q16, The certified build's barrier under seL4's methods** (§7). Three
  options:
  * **(a) Gate (i) at what its barrier costs** (recommended by the
    consultant, ledger 892): keep the barrier on every restricted exit and
    entry and set (i)'s fail line near 1 us, seL4's own figure with IBPB at
    every switch on nazuna. This revisits the earlier "within 2x" answer.
    The reason: (i)'s speed on x86-64 is not a certification property, and
    x86-64 carries no timing claim.
  * **(b) Keep the barrier on every exit and the 2x limit**: expected to miss
    at about 900 ns, so (i)'s gate holds the flip (M5) until the barrier
    gets cheaper.
  * **(c) Move the barrier to partition switches**, as seL4's
    time-protection work does: meets the limit, and gives the certified
    build (ii)'s claim inside a Linux partition, with AoU-14 amended: the
    certified build no longer protects one Linux partition's programs from
    each other against speculation.
* **Q17, Approaching the proof providers** (Proofcraft; the Verus groups or
  CertiK), and the compiler vendor for a SIL 4 qualification of the
  reference target (F-17): the customer's, since both cost money.
* **Q19, Which profiles carry a claim** (§12): `max` (SC 4 / ASIL D) and
  `fast` (SC 2 / ASIL B), as proposed, or `max` only, with `fast` an
  uncertified speed build.
* **Q20, The in-kernel Linux layer for `desktop`** (§12): keep
  `personality=kernel` permanently for the fastest uncertified kernel, at the
  cost of building and gating both forms of the Linux layer, or delete it at
  the flip as §4 planned.
* **Q18, Buying the VEK385** (about $16,000, 10 to 16 weeks' lead), or the Jetson
  AGX Orin kit first as a cheaper porting platform (about $3,500).


## 12. Build profiles: one source, several kernels

The customer asked (2026-10-13): "can we make it so that some features are
compile time flaggable similar to Kconfig, so that we can move certain parts
in and out of the kernel by compile flags and enable or disable certain
parts of security by flags, so that we have one high speed kernel with
minimal certifications e.g. ASIL B and one with the highest possible SIL D".
**Yes, with one rule: only named profiles carry a claim, and nothing below a
profile's level runs in its ring 0.** (The plan reads "SIL D" as the top
level the customer chose earlier: SC 4 / ASIL D.)

### 12.1 The options

Each option is a compile-time `--cfg` that xtask sets from one configuration
file, never a Cargo feature (`kernel_cargo_features` stays 0) and never a
boot option. Code a profile does not select is not in its image, so it is
neither dead nor deactivated code in that profile, which is why compile time
is the right place for this, as `ITEM.md` §5 already argues for
`--mitigations`.

| Option | Values | What it moves or relaxes | Highest level it allows |
|---|---|---|---|
| `personality` | `user`, `kernel` | `kernel` links `linuxd` (QM) into ring 0 for speed: the cached-file path loses the restricted-mode round trip | `user`: any; `kernel`: **no claim** |
| `fs_in_kernel` | `off`, `btrfs` | `btrfs` links the certified btrfs crates into ring 0 instead of `btrfsd` | as the crates' own level |
| `partition_speculation` | `barrier`, `domain` | §11 Q5's build switch | `barrier`: any; `domain`: as §11 Q16 decides |
| `mitigations` | `on`, `off` | F-31's switch | `on`: any; `off`: no claim |
| `kernel_memory` | `retyped`, `heap` | K16's no-heap model, or today's fallible, charged shared heap (F-23, F-37) | `retyped`: any; `heap`: up to SC 2 / ASIL B, with AoU-5's justification |
| `schedule` | `static+edf`, `edf` | K9's static windows, or EDF with CBS admission only | `edf`: up to SC 2 / ASIL B |
| `fastpath` | `off`, `on` | the 0x1013 fast path (off in the certified configuration, customer 2026-10-02) and K1's fastpath | per each fast path's own review |
| `checks` | `on`, `off` | the in-kernel boot checks | `on` in every certified profile |

Each option value declares the **highest level it allows** in the
configuration file, and its dependencies and conflicts, as Kconfig's
`depends on` does. A component may be linked into ring 0 only if it is
certified at or above the profile's level: the item-boundary gate reads the
profile and each component's level from `certification-item.json` and fails
a profile that breaks the rule, with a planted QM component in a certified
profile as its negative control.

### 12.2 The profiles

| Profile | Claim | Options |
|---|---|---|
| **`max`** | IEC 61508-3 SC 4 / ISO 26262 ASIL D, the first assessment (§6.5) | `personality=user`, `fs_in_kernel=off`, `partition_speculation` per Q16, `mitigations=on`, `kernel_memory=retyped`, `schedule=static+edf`, `fastpath` per its reviews, `checks=on` |
| **`fast`** | IEC 61508-3 SC 2 / ISO 26262 ASIL B | `personality=user`, `fs_in_kernel=btrfs` (if the crates are certified at ASIL B or above), `partition_speculation=domain`, `mitigations=on`, `kernel_memory=heap`, `schedule=edf`, `fastpath=on`, `checks=on` |
| **`desktop`** | **none** | `personality=kernel`, `fs_in_kernel=btrfs`, the rest as the customer's machines want, `mitigations` per F-31 and game mode |

**Why `fast` keeps the Linux layer out of ring 0.** ASIL B still needs
freedom from interference between the kernel and the QM Linux layer, and
with `linuxd` in ring 0 that is `CLAIM.md` §3.1's unwritten argument again.
So moving the Linux layer into the kernel is a `desktop` choice, with no
claim; `fast` gets its speed from the relaxed options instead.

### 12.3 What it costs

* **Evidence per certified profile.** Coverage, rows, boot-line comparisons
  and the safety manual's configuration section are produced per profile
  and per architecture. Two certified profiles (`max`, `fast`) roughly
  double M7's evidence work and the gates' run time for the kernel's rows;
  MC/DC (V1), the proofs (F1, F2) and T1 apply to `max` only. *Guess*: 40 to
  80 points for the configuration system and the second profile's evidence.
* **Two memory models and two schedules** in one kernel (`retyped` and
  `heap`, `static+edf` and `edf`) are code that F2 proves in one form only.
  The plan proposes that F2 covers `max`, and `fast`'s other forms carry
  ASIL B's evidence (requirements, statement and branch coverage).
* **`personality=kernel` lives on.** §4 deleted the in-kernel Linux layer at
  the flip; with `desktop` it stays, built from the same source through the
  shim, and `xtask check` builds both forever. That keeps the shim a
  permanent interface and doubles the Linux layer's build and boot rows.
* **The configuration tool is a tool.** xtask turns the configuration into
  `--cfg` flags that select code, so its output is verified independently:
  the image carries its resolved option set in a read-only section, the
  kernel prints it in a boot line, and the gate compares it with the
  profile's expected set. That keeps xtask out of tool qualification beyond
  T2 / TQL-5 for the comparison.
* **Arbitrary combinations carry no claim.** Any option set that is not a
  named profile builds and boots, and its boot line says "no claim".

### 12.4 Where it lands in the stages

M0 adds the configuration file, the resolver, the boot line and the
profile rule in the item-boundary gate (with today's options:
`mitigations`, `fastpath`, `checks`); each K-row that adds an option adds
its line. M5 keeps `personality=kernel` for `desktop` instead of deleting
it. M7 produces `max`'s and `fast`'s evidence. §11 Q19 and Q20 ask which
profiles to certify and whether `desktop` keeps the in-kernel Linux layer.

## Where it stands

Draft on branch `microkernel-plan`.

* Revision 1 (060d59ce0): the certification consultant's design review,
  **NOT YET** (B1 to B7, C1 to C16), ledger line 812 of
  `~/.local/share/ferrix/cert-consultant/reviews.md`, 2026-10-13. It accepted
  the direction.
* Revision 2 (2ae2d74f1) folded in B1 to B7 and C1 to C16 and added
  `tools/common/gen/microkernel-surface.py`, which reproduces the
  consultant's surface and orphan figures. Second round: **OK IF** R1 to R8,
  ledger line 837.
* Revision 3 (this text) folds in R1 to R8: nothing of the shared region
  reachable from restricted mode (K1), two handle tables (K11), the native
  loader out (K15), `fork`/`exec`/`ptrace` on K2, what the kernel hands
  devmgr and `linuxd` (§5), the restricted row priced (§7), the corrections,
  and the orphan step's control and scope (§6.2).
* Third round: **OK IF** S1 to S4 (text fixes, no further round), ledger
  line 847. Revision 4 folds in S1 (the K2 row's columns), S2 (init's
  `commands` input), S3 (the audit log's reader is the Linux partition;
  `linuxd`'s two handle moves) and S4 (the warm compile's call count). The
  consultant who reviews this document's landing on `main` checks S1 to S4
  against the diff. A note for K1's own review: under Q5 (ii), the restricted
  and shared roots of one partition must count as one domain in
  `speculation::entered_space`, which is keyed on the root, or (ii) pays
  (i)'s barrier.
* Revision 5 (this text): the customer's answers of 2026-10-13 (§11), and
  what maximum certification adds (§6.5, K16, the static schedule in K9, the
  V1, T1, F1 and F2 streams, the points). Sent to the consultant for review.
* Fourth round (revisions 4 and 5): **OK IF** P1 to P9, ledger line 858.
  Revision 6 (this text) folds them in with the customer's answers to P2,
  P4, P5(1) and P6: global pages and ring-0 Linux calls out of §7's means
  (P1); both builds gated, (i) the reference and the evidence, the stop rule
  on the PCID host (P2); the customer's words quoted verbatim and the plan's
  readings apart (P3); the new targets from M7, today's four until then (P4);
  §6.5's clause table, DAL A as objectives, V2, EAL6's ADV_SPM.1, IEC 61508 and
  EN 50716 both SIL 4 (P5); T1 on AArch64 safety processors with interference
  channels (P6); DMA grants on ARMv7-A only to certified drivers (P7); K16's
  evidence (P8); V1, F1 and F2 defined (P9). **The review is still open**:
  sent for a fifth round.
* Fifth round (revision 6, a0349c819): **OK IF** X1 to X5, ledger line 869,
  no sixth round. Revision 7 (this text) folds them in: K14's kernel-held
  measurement (sealed image VMOs, executable mappings only from them, the
  process's digests, a grant checked against the certified list) and
  "DMA-capable" classified per platform, DMA controllers included (X1);
  ARMv7-A's two remaining reductions, no ASR-1 backstop and no timing claim
  (X2); the certified build failing if it misses 2x, as the customer's
  answer read (X3); the duplicated heading, one verbatim quote, K16's
  ISO 26262 grade (X4); M0 owning the clause check (X5). To be checked
  against the diff by the consultant who reviews the docs landing.
* Sixth round (revision 7): **OK**, no conditions, ledger line 886, with
  the consultant's answers to Q6, Q11, Q12 and Q14. Revision 8 (this text)
  writes in the customer's answers to them (§11), the partition manager
  (§5, §6.3), the core layer (§2.1), SIL 4 first and the claim (§6.5), the
  proof tools and providers and the safety processor and board from the
  research of 2026-10-13 (§6.5), nazuna as the host with SVM ASIDs rejected
  and seL4's methods (§7), and three new questions (§11 Q16 to Q18).
* Seventh round (revision 8, fe3aad1fb): **OK IF** Y1 to Y8, ledger line
  892. Revision 9 (this text) folds them in: K1's four rules for lazy vector
  state and the fastpath's rule (Y1, Y2, for K1's own review); the frozen
  baseline and the under-400 ns round-trip target (Y3); Q16 posed in full
  with the consultant's recommended option, seL4 described accurately, the
  AArch64 note (Y4); the claim as SC 4 under 61508-3 Annex D, the usage
  rule narrowed, ARMv7-A outside the first assessment (Y5); the TOE and the
  element stated, the generator's tool class (Y6); the toolchain's
  consequences (Y7); the research in M0's check (Y8). To be checked against
  the diff.
* Eighth round (revision 9): **OK IF** Z1, ledger line 902; with Z1 the
  design review of the plan itself is complete. Revision 10 (this text)
  folds in Z1 (x86-64 outside the first assessment with ARMv7-A) and adds
  the customer's two new requests of 2026-10-13: programs with parts at
  different levels (§2.5, K17) and build profiles (§12, Q19, Q20). Sent to
  the consultant for review of the additions.
* Each K-row still needs its own design review before code. This plan
  approves no code. The reviews are internal, not independent verification
  (F-27).

## Sources (research of 2026-10-13)

Read on 2026-10-13; each is the source of a figure or claim in §6.5 or §7.

* **Ferrocene**: [ferrocene.dev](https://ferrocene.dev/) (qualifications),
  [26.05.0](https://ferrous-systems.com/blog/ferrocene-26-05-0/) (Rust 1.95),
  [targets](https://public-docs.ferrocene.dev/main/user-manual/targets/index.html).
* **Verus**: [repository](https://github.com/verus-lang/verus),
  [toolchain pin](https://raw.githubusercontent.com/verus-lang/verus/main/rust-toolchain.toml),
  [supported features](https://verus-lang.github.io/verus/guide/features.html);
  **Atmosphere** (SOSP 2025): [paper](https://mars-research.github.io/doc/2025-sosp-atmo.pdf).
* **seL4**: [proof effort](https://trustworthy.systems/projects/OLD/l4.verified/numbers),
  [AArch64 security proofs](https://sel4.discourse.group/t/sel4-security-proofs-now-complete-on-aarch64/1074),
  [certification](https://sel4.systems/Verification/certification.html).
* **Proofcraft**: [proofcraft.systems](https://proofcraft.systems/);
  **Kansas State / Collins (HAMR, DO-333)**: [seL4 Summit 2025](https://sel4.systems/Summit/2025/abstracts2025.html);
  **CertiK on Asterinas**: [LWN](https://lwn.net/Articles/1022920/).
* **AWS Nitro Isolation Engine and AutoCorrode**:
  [Amazon Science](https://www.amazon.science/blog/ec2s-formally-verified-isolation-engine-provides-mathematical-assurance-of-virtual-machine-isolation),
  [AutoCorrode](https://github.com/awslabs/AutoCorrode).
* **CC:2022 ADV_SPM.1 covers the entire TSF**:
  [JIL interpretation](https://www.ipa.go.jp/en/security/jisec/g6ovkg00000067p3-att/5_JIL-SPM-Interpretation-v1.0.pdf).
* **DO-330 Criterion 3, TQL-5**:
  [LDRA briefing](https://ldra.com/wp-content/uploads/ldra/DO-330-Test-Tool-Qualification-Technical-Briefing-v2.0.pdf).
* **Cortex-A78AE Lock-mode** (CEMODE, MPIDR_EL1.MT, no MPAM): Arm's
  [TRM 101779_0002_07](https://documentation-service.arm.com/static/62502fb164e3265f7c90b120);
  hybrid mode's rating: [Arm blog](https://developer.arm.com/community/arm-community-blogs/b/automotive-blog/posts/functional-safety-compute).
* **AMD Versal AI Edge Gen 2 / VEK385**:
  [UG1304](https://docs.amd.com/r/en-US/ug1304-versal-acap-ssdg/Key-Hardware-Components),
  [TRM AM026](https://docs.amd.com/r/en-US/am026-versal-ai-edge-prime-gen2-trm),
  [board](https://www.amd.com/en/products/adaptive-socs-and-fpgas/evaluation-boards/vek385.html),
  [TF-A](https://trustedfirmware-a.readthedocs.io/en/latest/plat/amd-versal2.html).
* **NVIDIA Orin Lock-mode**:
  [Jetson adaptation guide](https://docs.nvidia.com/jetson/archives/r35.2.1/DeveloperGuide/_sources/text/HR/JetsonModuleAdaptationAndBringUp/JetsonAgxOrinSeries.rst.txt);
  **NXP S32G without an SMMU**: [NXP community](https://community.nxp.com/t5/S32G/S32G274A-SMMU-Support-and-Configuration/m-p/2230972).

Not verified by the research and to be confirmed before relying on them: how
lock-step is configured in AMD's boot tools; AMD's ASIL D status for the
Versal APU (its briefs say "certified" in one place and "targeted" in
another); whether Cortex-A720AE and Neoverse V3AE lack SMT; ProvenCore's
EAL7 certificate.
