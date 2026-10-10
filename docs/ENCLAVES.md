# Enclaves: a program runs in a clean system of its own, held there by the processor

The customer asked on 2026-10-01 how Ferrix could stop encryption malware.
The first answer was write domains: a per-process list of folders, checked in
the VFS (`docs/WRITE-DOMAINS.md`, last on `main` at f42c5fd23). On 2026-10-10
the customer replaced that plan:

> change the plan for WRITE-DOMAINS.md to instead a locked down enclave mode
> for programs to run in which shall use hardware features of the CPU to
> fully lock down these programs similar to e.g. iOS for their programs but
> more extreme, where you can run programs similar to a virtual machine or a
> docker container, but faster wind up / down and more secure, where there
> are permissions which can be given by the kernel in the program, where in
> the most strict mode it can basically only execute stand alone code with
> only print and input capabilities and nothing else, and in a more
> permissive mode it can use e.g. network, disk, etc. but it should look to
> the program as if it is working on a fully clean OS, where if it tries to
> encrypt files it will only shoot itself by making its own environment
> unusable, but the host itself runs like nothing happened. But like I said
> it shall not be a simple VM running, as a VM is too heavyweight and a
> docker container is not secure enough.

This document is the design for that. It replaces the write-domain design,
and §10 says what of it survives.

Status: **design, no code, not reviewed.** Written 2026-10-10 on `main` at
f42c5fd23. The certification consultant's design review is owed before any
code (§12), and §15 lists the decisions that are the customer's.

**In one paragraph.** An *enclave* is a new kernel object: a second-level
address space the processor enforces (AMD-V with nested paging first, then
Intel VT-x with EPT and Arm's stage 2), one or more virtual processors, and a
handle table. A program inside reaches the host through a table of at most
16 calls, and at the strictest level through four: write output, read input,
wait for input, exit. It cannot make a Linux system call to the host at all.
For a program that needs an operating system, Ferrix's own kernel runs
*inside* the enclave, deprivileged, and serves the program's Linux calls
there without leaving. The program sees a complete Ferrix of its own: its
own root, pids, users, mounts and network. Its root disk is a copy-on-write
layer over a read-only base image, so encrypting every file it can reach,
the raw disk included, destroys that layer and nothing else. Everything
beyond that is a capability the launcher puts into the enclave's handle
table before it starts: a folder, a network destination, a window. No guest
boots. An enclave starts as a copy-on-write clone of a template that is
already running, and stops by dropping one address space.

There are sixteen landings (§14).
* E0 and E1, **24 points**, show the property end to end at the strictest
  level on x86-64: a static program prints and reads input, and a test
  program that tries everything else is stopped each time.
* E0 to E5, **76 to 89 points**, give the answer to encryption malware for a
  program that needs an operating system and a disk.
* The whole set is about **205 to 235 points** with one piece unsized (the
  GPU). That is about four times the write-domain plan's 47 to 60, and
  the size of stage 23's first guess.

What in this document is checked against the tree, and what is assumed, is
marked where it is said and collected in §16.

---

## 1. The property

The write-domain design argued that detection cannot be the answer
(entropy, rate, extensions and partial encryption each have a way around,
and each fires on a package upgrade). That argument stands and is not
repeated here. That design took away one authority, the right to change
files. This one takes away the *system*.

**The property.** A program in an enclave can affect only:

1. the memory, processors and private disk of its own enclave, each bounded
   by its job's quotas; and
2. the objects behind the handles in its enclave's handle table, with the
   rights those handles carry.

Nothing else on the machine has a name the program can say. There is no
path, pid, device, port number or system call number that reaches the host,
because the host's Linux system call layer is not reachable from inside.

**What that means for encryption malware.** A program that encrypts every
file it can open, as root, and then the raw block device under them, has
encrypted its own enclave's disk layer. The enclave is unusable. The host,
the user's home, every other enclave and the base image are byte-identical
(§11).

**What it cannot mean** is in §4, and the cases the property does not cover
are in §11.

## 2. Ferrix today

Each row is checked against the tree at f42c5fd23 unless it says otherwise.

| What | Where | What it means here |
|---|---|---|
| The kernel has no hardware virtualisation of its own | no VMCS, VMCB, EPT, NPT or stage-2 code under `src/kernel/src` | The enclave object is new item code |
| A capability-native ABI under the Linux one: handles with rights that only shrink, channels that pass handles, ports, VMOs, jobs | `docs/roadmap/stage-09-native-abi.md`; `src/lib/proto/native-abi`, `src/lib/kernel/objects` | An enclave's whole authority can be a handle table, and the exits can be a subset of calls that exist |
| The Security Target already names the Linux personality a hostile threat agent outside the TSF | `docs/certification/SECURITY-TARGET.md` §3.2 | The claim is ready for a personality that is deprivileged; today it runs in ring 0 |
| 64,036 lines of uncertified load run in ring 0: the VFS, the network stack, the Linux system call surface | `docs/certification/ITEM.md` §2 | This is the surface a container shares. An enclave does not reach it |
| Jobs carry the quotas: tasks, memory, kernel heap, objects, CPU weight; every cgroup is a job | `SECURITY-TARGET.md` §1.3; `docs/CGROUPS.md` | An enclave is charged and killed as a job. Nothing new is needed for exhaustion |
| Driver rings: a ring VMO, a data VMO and two doorbell ports, with indices that never trust the other side | `docs/BLOCK-RING.md` §2, `docs/NET-RING.md` §2 | The enclave's disk and its other bulk I/O use the same shape, so steady I/O makes no exit |
| Speculation domains, and one switch barrier rule on all three architectures | `docs/OPAQUE-KERNEL.md` §9.2; `docs/certification/SPECULATION.md` | The world switch needs its place in that rule (§4) |
| Mount, user, pid, network, IPC, UTS and cgroup namespaces; seccomp filters (S3); cgroup controllers | `docs/roadmap/README.md`, stage 13 | They make a container. §8 says where an enclave still uses them |
| init's sandboxing keys, a bootstrap channel per service, `Offers=` and `Uses=` | `docs/INIT.md` §4.5, §6 | Units ask for an enclave the same way (§9.3) |
| `app.toml` declares files, not permissions | `docs/APPS.md` §3 | The manifest gains a table (§9.3) |
| The desktop session runs as the user `ferrix` on `--everything` | `docs/AUTH.md` §6.1 | Apps start with the user's whole authority today |
| No overlay filesystem and no packet filter | no match for an overlay in the kernel's or the libraries' filesystem code, nor for a filter under `src/` | §7.2 and §9.2 choose designs that need neither |
| btrfs has a write path, in the certified item; it refuses a volume with snapshots | `ITEM.md` §2; the snapshot refusal as the write-domain design recorded it on 2026-10-01, not re-read | An enclave can keep a real filesystem on its private disk. Undo for shared folders still waits on Stage C (§10) |
| x86-64 gates boot `qemu64` plus named features, with neither `svm` nor `vmx` | `tools/common/xtask/src/qemu.rs`, `x86_cpu` | An enclave gate adds `+svm,+npt` to that model |
| AArch64 secondaries that arrive at EL2 are dropped to EL1; the Pixel 7 loader does the same for the boot core | `src/kernel/src/arch/aarch64/smp.rs`, `ferrix_secondary_entry` | On a native Pixel 7 boot, EL2 is ours at entry and given away |
| The ARMv7-A loader refuses any mode but SVC | `docs/arm32.md` (`prepare_cpu`) | HYP mode is never entered today |

Probed on the gate host on 2026-10-10 (nazuna, Ryzen 9 9900X, QEMU 10.2.1):
`kvm_amd`'s `nested` parameter is 1; QEMU accepts `+svm,+npt` under KVM and
under TCG; it refuses `+vmx` under both ("host doesn't support", "TCG
doesn't support"). So AMD-V can be gated on that host both ways, and Intel
VT-x cannot be gated there at all.

## 3. The hardware mechanisms, and the choice

### 3.1 What each mechanism is for

| Mechanism | What it gives | Use here |
|---|---|---|
| **Hardware virtualisation for one program**: AMD-V with nested page tables, Intel VT-x with EPT, Arm EL2 with stage-2 translation | A second address translation the program cannot see or change, its own privileged mode beneath the host's, and a fixed set of events that return control to the host | **The boundary.** Chosen |
| Memory protection keys, PKU for user pages and PKS for supervisor pages | Sixteen domains inside one address space, switched without a system call | Not the boundary: the switch instruction is unprivileged for PKU, so it needs a verified binary. Possible later inside the host kernel, for the consultant's ring-0 compartments (`OPAQUE-KERNEL.md` §8) |
| SMEP, SMAP, PAN | The kernel cannot run or read user pages by accident | Already on in the host. The kernel inside an enclave turns them on for itself |
| CET shadow stacks and IBT; Arm PAC and BTI | Return and branch targets are checked | Hardening for the world switch and the exit handlers. Not a boundary. A separate landing, not in §14 |
| Arm MTE | Memory tagging | No hardware Ferrix runs on is known to have it (the Pixel 7's cores are ARMv8.2, which predates it; not checked on the phone) |
| Intel TDX, AMD SEV-SNP, Intel SGX, Arm CCA | They protect a guest from its *host* | The opposite direction (§3.3) |

### 3.2 Prior art, and what each teaches

Sources are in §17. Each line is what this design takes from it.

* **Dune** (Stanford, 2012) ran an ordinary Linux process in VT-x non-root
  ring 0, with the host kernel reached by hypercall. It showed the shape:
  virtualisation hardware used for a process, not a machine, with no
  emulated devices. It kept the host's whole system call interface behind
  the hypercall, so it sandboxed nothing by itself. *Lesson: the mechanism
  is cheap; the boundary is whatever the hypercall table is.*
* **gVisor's KVM platform** runs its own kernel, written in Go, as the guest
  kernel of each sandbox, and the workload in guest ring 3. System calls are
  served in the sandbox. Its documentation says the KVM platform does poorly
  under nested virtualisation and offers a second platform for hosts that
  are themselves virtual machines. *Lessons: put the personality inside; and
  expect nested hosts to be slow.*
* **Hyperlight** (Microsoft) creates a virtual machine with no guest kernel
  and no devices in 1 to 2 ms, for one function call. *Lesson: the strictest
  level needs no operating system inside, and start time can be the time of
  building a page table.*
* **Firecracker** and **Cloud Hypervisor** are the heavy end: a real Linux
  guest and a handful of virtio devices, about 125 ms to the guest's init
  and a few MiB of overhead for each. They restore snapshots in
  milliseconds. *Lessons: the guest boot is the cost, and a snapshot removes
  it; a clone of a snapshot must get fresh randomness.*
* **Nabla containers** run a unikernel on Solo5, whose host interface is
  seven calls, against the more than 300 Linux system calls a container can
  make. *Lesson: count the host calls; that count is the security argument.*
* **seL4** as a hypervisor, and Android's **pKVM**, keep the part that runs
  at the highest privilege to the world switch and the stage-2 tables, and
  put everything else outside it. *Lesson: the same split fits the certified
  item: the core holds the switch and the tables, and nothing else moves in.*
* **iOS**, the model the customer named. Every app runs in a sandbox with a
  container directory of its own, gets other files only through a picker or
  an entitlement signed into the app, and reaches hardware only through
  entitlements. Underneath, PAC signs pointers, and PPL and now SPTM keep
  the kernel's page tables out of the kernel's own reach. *Lessons: a
  private container per app, a manifest of entitlements, and a picker are
  the right user model, and §9 copies all three. But every iOS app still
  makes system calls into the one shared kernel, filtered by a profile.
  "More extreme" has a checkable meaning here: an enclave makes none.*

### 3.3 Why the confidential-computing features do not help

TDX, SEV-SNP and SGX keep a tenant's memory secret from the machine's owner.
The customer's problem is the reverse: the machine's owner distrusts the
program. A program in an SEV-SNP guest can encrypt the files it was given as
freely as any other. Two parts are still worth knowing. SEV-SNP's reverse
map table and TDX's module show how to keep a page from being mapped twice,
which E1's sweep checks in software (§13). And if Ferrix is one day run *as*
a confidential guest, its enclaves need nested virtualisation from that
platform, which those platforms mostly do not offer. Nothing here depends on
them.

### 3.4 The choice

**Hardware virtualisation for a process, implemented in Ferrix's own kernel,
with the exits a subset of the native ABI.** Ferrix is the hypervisor. No
KVM, no QEMU and no emulated machine are involved when Ferrix runs on bare
metal.

Alternatives, and why each was set aside:

| Alternative | Why not |
|---|---|
| Write domains, alone (the old design) | Kernel bookkeeping in the VFS, which is load. The customer asked for the processor to enforce it, and for more than files |
| A container: namespaces, seccomp and a cgroup | Everything landed for it. It shares the host's Linux system call layer, 64,036 lines in ring 0. This is the customer's "not secure enough". It survives only as the weaker mode of §5.2, if the customer allows one |
| A full virtual machine: Ferrix or Linux booted as a guest with virtio devices | Boots a kernel, emulates a machine, holds fixed memory. The customer's "too heavyweight" |
| The program alone in the enclave, every Linux call forwarded to the host | Each call becomes an exit of about a microsecond or more, and the host's personality is still the surface, now behind a forwarder. Dune's weakness |
| Protection keys with a verified binary | Needs binary rewriting or verification of unmodified Linux programs, and x86-64 only |
| A language sandbox (WebAssembly) | Does not run Linux programs |

## 4. What "fully locked down" can and cannot mean

**The measure** is the code that runs at the host's privilege on input an
enclave chooses. Two numbers state it, and from E2 on the gate prints both
on every run:

* **The exit table.** At most 16 calls (§6.2), four at the strictest level,
  each a native call on a handle or an operation on the enclave's own memory.
  A container on Ferrix can make every Linux call the personality serves.
* **The enclave core.** The world switch, the second-level tables, the
  enclave object and the exit dispatch, for each architecture. The budget is
  4,000 lines for x86-64 with AMD-V, a guess that E1 replaces with a count.
  Beneath it lies the item's object code those calls reach, which every
  native process already reaches.

**What the boundary does not stop.** None of these is hidden behind the
word "fully".

| Limit | What is done about it | What is left |
|---|---|---|
| **Speculative side channels** across the world switch (Spectre v2, return-stack and store-bypass forms, and the microarchitectural data-sampling family) | An enclave is never a member of a speculation domain, so a switch between it and anything else on a processor pays the barrier `SPECULATION.md` already defines. The exit path is written with the same clamps as the system call entry. The section of `SPECULATION.md` for the world switch is owed before E1 lands | What `A.PROCESSOR` leaves on a given processor |
| **A sibling hardware thread** shared with another enclave or the host | Open: either an enclave's virtual processor never shares a core with a thread outside its job, or the hazard is stated. The consultant's and the customer's (§15, Q8) | Until decided, not claimed |
| **Cache and timing channels** in the shared last-level cache | At the strictest level the enclave has one virtual processor and no clock: time-stamp reads are intercepted where the processor allows it (§6.1). A program with neither a clock nor a second thread to count with cannot time a cache | Higher levels have clocks and threads. Nothing is claimed for them |
| **Covert channels** between two enclaves that cooperate | Not addressed | Not claimed |
| **Resource exhaustion** | The enclave's job bounds memory, kernel heap, objects and CPU. Its private disk has a size cap, enforced by the server that holds its layer | A program can use all of what it was given |
| **A bug in an exit handler or the world switch** | The handlers are item code under the item's rules: every `unsafe` traced, every countermeasure with a check and a negative control, coverage carried. E1's fuzzer drives the exit table as the handle table is fuzzed today | This is the trusted computing base. Its size is the measure above |
| **A bug in a host service that serves a grant** (the folder server, the network proxy, the compositor bridge) | Each is a ring-3 process that holds only the one grant it serves | A flaw gives the enclave that grant's authority in full, and no more |
| **A flaw in the processor's virtualisation** | `A.HARDWARE` widens to say the second-level translation conforms | Out of Ferrix's hands |
| **What was granted** | A read-write folder can be encrypted. A network grant can carry out anything the enclave can read | §11 |
| **The terminal** that shows a sealed program's output | Output is bytes on a channel. The launcher may strip escape sequences (`--plain`) | A terminal emulator bug is the terminal's |

## 5. Where it runs

### 5.1 Each deployment

| Deployment | Hardware feature | Enclaves |
|---|---|---|
| x86-64 as a KVM guest on an AMD host (the gate host) | Nested AMD-V with nested paging, when QEMU is given `+svm,+npt` (probed, §2) | **Yes, and gated.** Every exit is relayed by the outer hypervisor, so exits cost tens of microseconds. Functional evidence, not performance evidence |
| x86-64 under TCG | QEMU emulates AMD-V (probed) | Yes, slowly. Lets the gate run without KVM |
| x86-64 on bare metal, AMD | AMD-V with nested paging | Yes. The figures of §12 are for this case and are owed a run |
| x86-64 on bare metal or under KVM, Intel | VT-x with EPT | After E9. No Intel machine is in the fleet's gate pool (§15, Q6) |
| x86-64 under WHPX on the Windows PC | Assumed absent: the Windows hypervisor does not pass virtualisation through to a QEMU guest. Not probed | No |
| AArch64 under QEMU `virt`, TCG | EL2 when the machine is started with `virtualization=on`. The gate's `cortex-a72` has no VHE, so the kernel cannot simply move to EL2 | After E10, with a small EL2 part and the kernel still at EL1 |
| AArch64 as a crosvm guest on the Pixel 7 | None. The phone's hypervisor keeps EL2 and offers no nested virtualisation | **No hardware mode.** This is today's main AArch64 deployment |
| AArch64 booted natively on the Pixel 7 | EL2 at entry, dropped today (§2) | Possible after E10 by keeping EL2. Phone use is the product owner's to grant |
| ARMv7-A on the STM32MP157 board | The Cortex-A7 has the virtualisation extensions and stage 2. Whether the board's U-Boot hands off in HYP mode is not verified, and the loader refuses HYP today | After E11, if the hand-off allows it. 512 MiB bounds what fits |

### 5.2 A host without the feature

Two answers are possible, and which one is the customer's decision (§15, Q2).

* **Refuse.** `enclave run` and the launcher say "this machine cannot run
  enclaves" and name the missing feature. A unit that asks for an enclave
  fails to start. This is the default in this design.
* **A weaker mode, named as weaker.** The same manifest, the same private
  root and the same grants, carried out with what landed in stage 13: fresh
  mount, pid, network, user, IPC and UTS namespaces, a seccomp filter and a
  cgroup. It is a container. It is called **container mode**, never an
  enclave, in every message, log line and document. It shares the host's
  Linux system call layer, which is the weakness the customer named. It is
  off unless the machine's owner turns it on (`ferrix.enclave=container`),
  and the launcher marks every window and prompt of a program that runs
  under it.

A boot line says which the machine has, in the manner of the `kaslr` and
`cpu` lines: the feature found, or the reason there is none.

## 6. The enclave

### 6.1 The object

An **enclave** is a kernel object in the `core` ring, made with a native
call by a holder of `MANAGE` on a job. It has:

* **A second-level address space.** Guest-physical pages map to frames the
  enclave's job is charged for, or to shared read-only frames of a template
  (§7.3). The host kernel's memory, other processes' frames and device
  memory are never in it. No mapping is writable and executable at the
  second level unless the template marked the page so for the kernel
  inside.
* **Virtual processors.** Each is a task in the enclave's job, so the
  scheduler, CPU weight, `cpu.max`, freezing and the job's kill all apply
  unchanged. One at the strictest level.
* **A handle table**, like a process's. The launcher fills it before the
  first instruction runs. The enclave can pass handles out over a channel it
  holds and receive handles over one, under the rules stage 9 already has.
* **A call mask**: which entries of the exit table (§6.2) this enclave may
  use. Set when the enclave is made, never widened.

What the processor forces back to the host, besides the calls:

| Event | Answer |
|---|---|
| A second-level fault on a page of the template, or on a page not yet backed | Map it, or copy it on a write, charged to the job (§7.3) |
| A second-level fault anywhere else | The enclave is killed, with an audit record |
| A host interrupt | The host handles it and resumes the enclave. The enclave sees nothing |
| `CPUID`, and reads of model-specific registers | Fixed answers from a table. The enclave learns no more of the processor than the table says |
| Writes to model-specific registers, I/O ports, the virtualisation instructions themselves | Killed. An enclave has no ports and no devices |
| A halt | Treated as a wait (§6.2) |
| A time-stamp read, at the strictest level | Killed, on x86-64. On Arm, trapping the virtual counter from EL2 needs a processor feature older cores lack (from the architecture manual as recalled; to verify at E10). Where it cannot be trapped, the boot line says the strictest level has a clock on this machine |
| A triple fault or shutdown | Killed |

A killed enclave is a killed job: its tasks end, its second-level tables and
private frames are freed and uncharged, its handles close, and the servers
on the other ends of its channels see a closed peer.

**Rules.** Each has a number so that §13's checks can name the rule they
test. These are this document's own labels, not requirement ids.

| Rule | Statement |
|---|---|
| E-R1 | An enclave reaches the host only through the calls its mask allows. A call outside the mask kills it. |
| E-R2 | No Linux system call number is accepted from an enclave, at any level. |
| E-R3 | Every frame in an enclave's second-level tables is charged to its job or is a read-only frame of its template. A boot check sweeps the tables to prove it, as the W^X sweep does for first-level tables. |
| E-R4 | The mask and the handle table are filled before the first instruction, and the mask never widens. A handle arrives later only over a channel the enclave already holds. |
| E-R5 | An enclave is never a member of a speculation domain. |
| E-R6 | Every value an exit handler reads from enclave memory is read once, validated, then used, as the rings require of indices. |
| E-R7 | A clone of a template gets fresh entropy before its first instruction (§7.3). |
| E-R8 | Making an enclave, each kill and its reason are audit records. |

### 6.2 The exit table

The whole table. No level adds a call; levels differ in the mask and in
which handles exist.

| Call | Strictest level | Other levels | What it is |
|---|---|---|---|
| `exit` | yes | yes | End the enclave with a status |
| `write` | yes | yes | Bytes to the output channel. The existing `channel_write`, on the one handle the enclave has |
| `read` | yes | yes | Bytes from the input channel (`channel_read`) |
| `wait` | for input only | yes, with a deadline | Sleep until a handle is ready (`port_wait`) |
| `channel_write_read` | | yes | The round trip of `ipc-round-trip.md`, for request and reply services |
| `handle_close`, `handle_duplicate` | | yes | As stage 9 defines them |
| `port_queue` | | yes | Ring a doorbell |
| `map`, `unmap` | | yes | Place a VMO the enclave holds a handle to, a ring or a shared buffer, into its guest-physical space |
| `grow`, `release` | | yes | Ask for more memory up to the job's limit; give pages back |
| `cpu_start`, `cpu_kick` | | yes | Start another virtual processor; wake one |
| `clock_page` | | yes | Map the read-only page the vDSO's clock already uses. After this, reading time makes no exit |

That is 4 at the strictest level and 15 in all. The rule is that the table
does not pass 16 without a new design review.

Each call that takes a handle is the existing native call, entered from the
exit dispatch in place of the system call entry, so the capability check is
the one the Security Target already claims (FDP_ACC.1's Capability Access
Control SFP). The enclave object and its own calls are new objects and
operations in that policy.

### 6.3 What runs inside

Three ways to fill an enclave were weighed.

| | What is inside | Host surface | Cost |
|---|---|---|---|
| **A. The program and a shim** that forwards its Linux calls to the host's personality | Little | The whole personality, behind a forwarder: a container with slower calls | An exit for every system call |
| **B. Ferrix's kernel, deprivileged**, serving the program's Linux calls inside | A whole kernel: VFS, tmpfs, btrfs, processes, signals, its own network stack | The exit table and nothing else | Memory for the kernel's data in each enclave; a template to avoid its boot |
| **C. A split**: processes and memory inside, files and sockets forwarded | Half a kernel, newly written | The host's VFS and sockets, call by call | A new personality to write and keep equal to the first |

**B is chosen, with a second, tiny inside for the strictest level.** The
more of the personality runs inside, the less of the host is exposed and the
closer the view is to a clean operating system, because it *is* one. B also
reuses what exists: a program that runs on Ferrix runs in an enclave,
because the same code serves it. C would write the Linux personality twice.

The host does not know or care what is inside. Two images ship:

* **`bare`**, for the strictest level. A few hundred lines that set up the
  enclave's own page tables, start a static program in the enclave's user
  mode, and answer its Linux calls themselves: `write` and `read` on
  descriptors 0 to 2 become the two calls, `exit` becomes `exit`, anonymous
  memory comes from a fixed arena, thread-pointer setup is done in place,
  and every other call returns `ENOSYS`. An unmodified static Linux program
  that only computes, prints and reads runs as it is. No file system, no
  clock, no second thread.
* **`os`**, for every other level: the Ferrix kernel built for a new
  platform, `enclave`, beside the SoC platforms under
  `src/kernel/src/platform/`. It has no firmware tables, no device
  discovery, no `devmgr` and no drivers. Its memory map, its entropy and its
  bootstrap handles arrive in a boot record. Its console is a channel, its
  timer is the host's wake-up, its disk is a block ring whose other end is a
  host service, and its processors start with `cpu_start`. How much of
  bring-up assumes firmware today is not surveyed; that survey is E4's first
  task and the reason E4 is a range.

Neither image is trusted by the host. The item code inside an `os` enclave
is ordinary software there: it protects the enclave's own programs from one
another and is relied on for nothing outside.

## 7. The clean system

### 7.1 What the program sees

A complete Ferrix: `/` with the base image's files, itself as any uid it
likes including 0, pid numbers from 1, its own mount table, hostname,
loopback and `/proc`. It may mount, `chroot`, make namespaces and install
seccomp filters, because it does so in a kernel of its own. Programs that
sandbox themselves, such as Chromium, find what they find on Ferrix.

An enclave holds a process tree, not one process. A shell pipeline or a
build runs inside one enclave.

### 7.2 The root, and why encrypting it harms nothing else

The enclave's root is a btrfs volume on a block ring. The host service on
the other end, `enclave-disk`, serves **a read-only base image plus a
private copy-on-write layer**: a read falls through to the base unless the
layer holds the block, and a write goes to the layer. The layer is a sparse
file with a size cap.

* The enclave may write, delete or encrypt every file, and the block device
  itself. All of it lands in the layer.
* The base image is opened read-only by a service that holds nothing else.
* Discarding the layer returns the enclave to a clean system.

This was chosen over an overlay filesystem, which Ferrix does not have and
which would protect files but not the block device, and over a tmpfs root
with read-only binds, which leaves most of `/` unwritable and so does not
look like a clean system.

The base image is a plain Ferrix userland image built by xtask, the same
packages a desktop image carries. An enclave reads it through its own page
cache; §7.3 says how the common part of that is shared.

### 7.3 Start and stop at the speed of a process

No enclave boots. For each base image the host keeps a **template**: an
`os` enclave that was started once, mounted its root, ran to the point where
a small program inside waits on its bootstrap channel for "run this", and
was then frozen into an immutable memory image with its processor state.

**Start** is:

1. Make an enclave object and an empty second-level table.
2. Copy the template's processor state. Fill the handle table and the mask.
3. Write the boot record's fresh entropy and the launch message: the
   program, its arguments and environment, and what each handle is.
4. Run. Each first touch of a page faults at the second level and maps the
   template's frame read-only. Each first write copies the frame and charges
   the job.

So start costs a table, a few faults and one message. It does not depend on
the template's size. A later refinement records which pages a start touches
and maps them in one pass.

**Stop** is the job's kill: the tables are dropped, the private frames
freed, the handles closed. The layer file is deleted, or kept (§7.4).

**Clones must not share secrets.** Every clone of a template begins with
the same memory, so the same kernel layout inside, the same random pool and
the same hash seeds. E-R7: the boot record carries fresh entropy, and the
kernel inside reseeds from it before it runs anything. Its own address
layout stays the template's, which matters only inside the enclave; the host
relies on nothing in there. Templates are remade at every host boot.

The `bare` image needs no template: it is small enough to build in place.

### 7.4 Throwing away, and keeping

| Kind | The layer | Use |
|---|---|---|
| **Ephemeral** | Deleted at exit | `enclave run` on a command the user does not trust; a build; the default for anything not installed |
| **Kept** | Stays under `/var/lib/enclaves/<name>/`, owned by root, outside every enclave | An installed app: its settings and its own data live in its own disk |

A kept enclave's files are *in the enclave*, not in the user's home. That is
iOS's model, and it has iOS's consequence: a document made in an app is not
in `~/Documents` until the user saves it there through the chooser or the
app has that folder as a grant (§9.2). Removing an app removes its
directory. The host can copy a layer for backup, and can reset an app by
deleting it.

## 8. Namespaces, seccomp and cgroups under this design

* **Not needed for the boundary.** An enclave has its own pid table, mounts,
  users and network because it has its own kernel. No host namespace is made
  for it.
* **Still used inside.** bubblewrap, Chromium's sandbox and Steam's
  container make namespaces and filters in the enclave's kernel as they do on
  Ferrix.
* **Still used on the host**, for the services that serve grants: the folder
  server runs in a mount namespace that holds only the folders it serves,
  with a seccomp filter, as an init unit with L13's keys.
* **cgroups, unchanged**, because an enclave is a job.
* **The whole of container mode** (§5.2), if the customer allows it.

## 9. Permissions

### 9.1 The model

Deny by default. An enclave has what its handle table holds and nothing
else. There is no ambient authority on the host to fall back on: no uid, no
path and no capability bit means anything across the boundary.

A grant is made in one of three ways, and in no other:

1. **At launch**, by the launcher, from the manifest (§9.3).
2. **Later, by the user**, through a trusted prompt: the chooser (§9.4), or
   a permission prompt the launcher shows in its own window when the manifest
   marked a grant `ask`.
3. **By another program that holds the capability** and sends it over a
   channel the enclave already has, as any native process may.

### 9.2 Levels and grants

The grants are the truth. A level is a name for the largest kind of grant an
enclave has, so that a person can read it at a glance.

| Level | Inside | What it adds | Served by |
|---|---|---|---|
| **0 `sealed`** | `bare` | Output and input. One processor, no clock, memory fixed at launch | The launcher relays bytes between the two channels and its own terminal |
| **1 `clean`** | `os` | A clean system with a private disk, ephemeral or kept; a clock; more processors and memory up to the job's limits | `enclave-disk` |
| **2 `files`** | `os` | Named folders, each read-only or read-write; files handed over by the chooser | `enclave-files` |
| **3 `net`** | `os` | Connections to named destinations, or to any | `enclave-net` |
| **4 `desktop`** | `os` | A window, sound, later the GPU and other devices | Bridges to the compositor and the sound server |

**Folders.** `enclave-files` holds a directory and serves it over a channel
and a ring: open, read, write, list, rename, remove, all resolved beneath
that directory and never above it. Inside, a small client filesystem mounts
it at its usual place, `~/Downloads` for the grant `downloads`. A read-only
grant is enforced by the server, which opened the directory read-only. The
host's VFS is reached only by that server, with that one directory's
authority.

**Network.** The enclave asks for connections, not packets. Its kernel
forwards `connect`, `send` and `recv` on outside sockets to `enclave-net`,
which checks the destination against the grant, by name or address, and
makes the connection with an ordinary host socket. Loopback stays inside the
enclave's own stack. This needs no packet filter, which Ferrix does not
have, and it puts the destination check where the name is known. Raw sockets
and listening on the host's addresses are not offered at first. The kernel's
sockets are a closed `enum` today (`OPAQUE-KERNEL.md` §2 names the trait
that would open it); E7 owes that refactor.

**A window and sound.** Both are Unix-socket protocols that pass
descriptors. One mechanism serves both: a **bridge** that carries a socket's
bytes across the boundary and turns each passed buffer into a VMO mapped on
both sides. The compositor's bridge refuses an enclave client the
virtual-keyboard, virtual-pointer and screen-capture protocols, so a program
in an enclave cannot type into or read another program's window.

**The GPU and other devices** are the far end. A device handed whole to an
enclave needs the IOMMU, which the item has for VT-d and SMMUv3 and, as far
as this design has checked, not for AMD's. A shared GPU needs a forwarding
protocol in the manner of virtio-gpu's. Neither is designed here; E8 carries
a window with software rendering, and the GPU is unsized.

### 9.3 Who asks: the manifest, the unit, the command

**`app.toml`** gains a table under `[package]`, the part that travels into
the installed record:

```toml
[package.enclave]
level   = "files"                  # sealed | clean | files | net | desktop
keep    = true                     # the private disk outlives the run
folders = ["downloads:rw", "pictures:ro"]   # named places, never paths
net     = ["api.example.org:443"]  # or ["any"], shown as a warning
window  = true
ask     = ["microphone"]           # prompted for at first use
memory  = "512M"
```

The named places are the write-domain design's: `downloads`, `documents`,
`music`, `pictures`, `videos`, `desktop`, and `home` for the whole home. The
installer shows the list, and shows `home:rw` and `net = ["any"]` as
warnings. A package with no table gets the default the customer chooses
(§15, Q3).

**init** reads the same grants from a unit:

```ini
[Service]
Enclave=clean
EnclaveKeep=yes
EnclaveFolders=/srv/www:ro
EnclaveNet=any
Uses=file-chooser
```

`Uses=` names from init's directory (`INIT.md` §6) become handles in the
enclave's table directly, since both sides are native channels. L13's
existing keys keep their meaning for services that run on the host.

**The command line.** `enclave run` is the write-domain design's `confine`:

```sh
enclave run -- ./a.out                       # sealed
enclave run --clean -- ./build.sh            # a clean system, thrown away
enclave run --clean --folder .:rw -- make    # and this directory, read-write
```

**One launcher for apps**, as the old design had it: the desktop starts an
app through one program that reads the installed record and builds the
enclave. A program started any other way runs wherever its parent runs.

### 9.4 Handing a file in and taking a result out: the chooser

Kept from the write-domain design, with the boundary making it stronger.
`ferrix-chooser` is a user service on the host that `Offers=` the name
`file-chooser`. An enclave that `Uses=` it holds a channel to it.

1. The program asks to open or save a file.
2. The chooser shows its own window on the host, and the user picks.
3. The chooser opens that one file and sends the enclave a handle that
   serves it: read for "open", write for "save as".
4. Inside, the file appears under `/run/handed/`, and a portal front-end
   gives its path to the program that asked.

The user's click is the grant, so the click must be real. The three
conditions on hyprix stand: no virtual keyboard or pointer input reaches the
chooser's window, nothing captures or covers it, and it keeps focus while it
waits. Under this design an enclave's programs reach the compositor only
through the bridge, which refuses them those protocols outright.

Linux GUI programs ask for a chooser through xdg-desktop-portal over D-Bus,
and Ferrix has no D-Bus. That open question carries over (§15, Q10).

### 9.5 Existing Linux programs and ambient authority

A Linux program expects to open any path its uid allows. In an enclave it
can: the paths are the enclave's. It is root if it wants to be. What it
cannot find is the user's data, unless a grant put a folder at the place the
program expects. So nothing is ported, and nothing is refused with an error
the program does not understand. The cost is on the user's side: files move
in and out by a chooser or a grant, as on a phone.

## 10. What survives from the write-domain design

| Old part | Fate |
|---|---|
| The argument against detection (old §1) | **Kept**, by reference (§1) |
| `WriteDomain` in the VFS, `may_change` at every changing operation, rules W-R1 to W-R7, `SET_ATTR`, per-file tags | **Dropped** as the answer. The boundary is the enclave, not a check in load code |
| Landlock in the Linux personality (old W2) | **Kept as a separate, smaller thing**: a compatibility feature for programs that sandbox themselves, since Chrome asks for it and gets `ENOSYS`. It is no longer how Ferrix protects the user from a program. Its own backlog row, P3, about 13 points with the VFS part it needs. Inside an enclave it would run in the enclave's kernel like everything else |
| The launchers: init for services, one launcher for apps (old §3.3) | **Kept** (§9.3). They build enclaves, not domains |
| `[package.access]` and the named places | **Kept, renamed** `[package.enclave]`, with levels and more kinds of grant |
| `confine` | **Kept, renamed** `enclave run` |
| The chooser, and hyprix's three conditions (old §3.4) | **Kept** (§9.4) |
| Read-only btrfs snapshots as undo (old §4) | **Kept, for a narrower job**: undo for folders shared read-write into an enclave, and for what an unconfined shell does (§11). Still waits on btrfs-write's Stage C. Landing E13 |
| The tripwire (old §6) | **Kept as optional and last**, moved: `enclave-files` sees every change an enclave makes in a read-write folder, so it can count them and pause the enclave. No VFS hook. Landing E14 |
| The table of ways around a domain (old §5) | **Dropped.** Hard links, bind mounts, `/proc/<pid>/fd`, set-id programs, raw block devices and `ptrace` are routes inside one shared kernel. An enclave shares none. §4 is the new table |
| The session off root as "not a prerequisite" | **Still true**, for a different reason: no uid crosses the boundary |

## 11. Encryption malware, restated

**Covered.** A program in an enclave, at any level, with any uid inside,
can encrypt its own private disk and nothing else of the machine's. This
holds for a packaged app, a service with `Enclave=`, and any command given
to `enclave run`. It holds against a kernel exploit of the Linux
personality, because the personality the program can attack is the copy in
its own enclave.

**Not covered, and what covers each.**

| Case | Why the enclave does not help | What does |
|---|---|---|
| A folder granted read-write | The grant is the authority | Snapshots (E13): hourly, read-only, outside every enclave, never deleted to make room. The installer's warning for `home:rw`. Optionally the tripwire (E14) |
| A file handed over by the chooser for writing | The same, for one file | The same snapshots. The damage is one file |
| A program run unconfined: a shell with the user's authority, and whatever it starts | It is not in an enclave | Snapshots. And `enclave run`, which makes confining one command as short as running it |
| The desktop's own programs: the compositor, the bar, the launcher | They are the trusted side | The ordinary care a system's own programs get; not this design |
| Data copied out before it is encrypted, to extort by publishing | Reading is covered: an enclave reads only what it was granted. Sending needs a network grant as well | The installer shows the two together as a warning |
| Until E5 lands | Nothing is built | A backup off the machine |

## 12. Performance

### 12.1 What is known

Ferrix's figures, each from the document named, on x86-64 under KVM on
nazuna unless it says otherwise:

| What | Figure | Source |
|---|---|---|
| A native channel round trip between two processes in one speculation domain, mitigations on | 873 ns in a quiet window on the fast path; 1,308 to 1,378 ns on the general path | `docs/roadmap/ipc-round-trip.md` |
| The same outside a speculation domain: each of the two switches pays the barrier | about 2 us more for each switch (6.5 us against 2.6 us with mitigations off, on the branch of the day) | `OPAQUE-KERNEL.md` §9.1 |
| seL4's round trip on the same machine | 440 ns | `OPAQUE-KERNEL.md` §9.6 |
| A 4 KiB read through the block ring and a ring-3 driver | p50 230 us, p99 417 us; 13 switches and 3.2 IPIs a read | `OPAQUE-KERNEL.md` §8 |
| A warm `rustc` compile | 4,345 system calls, one trip to ring 3 | `OPAQUE-KERNEL.md`, *The verdict of S0* |
| Chrome | about 8,000 system calls a second | `OPAQUE-KERNEL.md` §5 |

No figure for a null Linux system call on Ferrix is recorded anywhere this
design looked. E0 measures one.

From outside Ferrix, as orders of magnitude and not re-measured: a system
call costs tens to a few hundred nanoseconds; an exit from a virtual
processor and the return cost about a microsecond on current x86-64 before
any speculation barrier; under nested virtualisation each exit is relayed by
the outer hypervisor and costs tens of microseconds.

### 12.2 What the opaque-kernel work teaches

That plan moved kernel services to ring-3 servers and was shelved when S0
measured the seam: a trip to ring 3 cost ten to twenty times Linux's
in-kernel path, and the plan would have put one under every socket call
(`OPAQUE-KERNEL.md`, *The verdict of S0*). The same arithmetic rules out
design A of §6.3. At about 2 us an exit with its barrier, Chrome's 8,000
calls a second would cost 1.6% of a processor, which is tolerable; a file
tree walk at several hundred thousand calls a second would cost all of one.

Design B avoids the seam where S0 found it. A system call made inside an
enclave is served inside, in the enclave's own privileged mode, at the cost
of a system call. S0's own finding says which path matters: the warm compile
made 4,345 calls and left the kernel once. In an enclave those 4,345 calls
make no exit.

Exits happen on:

* **A doorbell**, when a ring's other side is asleep. The rings' want-bell
  handshake already suppresses a bell to a peer that is awake, so a busy
  disk or folder makes few. A cold read still pays what §12.1's 230 us read
  pays today, plus one exit each way. S0's trace showed that trip is made of
  wake-ups, not of crossings.
* **A request and reply to a service**: one `channel_write_read`, about a
  round trip plus the exit and its return.
* **A first touch of a page** (§7.3).
* **A wait**, when the enclave has nothing to run.
* **An interrupt between virtual processors**, until the processor's own
  interrupt virtualisation is used. Not in the first landings.

Two costs are not exits. Address translation is two-dimensional, so a miss
in the translation cache walks more levels; large second-level pages for the
template reduce it. And the barrier a world switch owes is the speculation
section's to decide: if it is the full barrier on every exit, an exit costs
over 2 us on nazuna's processor by §9.1's figure, and the design leans
harder on making exits rare.

### 12.3 Targets, and how each is measured

Every target is a guess until E0 and E3 put a number beside it.
`cargo xtask bench-enclave` prints one line for each, under KVM with
`+svm,+npt` and under TCG. The gate host is itself nested, so its figures
bound nothing; each target is for bare metal, and the bare-metal run is a
`docs/BACKLOG.md` row from the day E0 lands.

| What | Target on bare metal | How it is measured |
|---|---|---|
| A null call: enclave to host and back | under 3 us with every mitigation on | 20,000 calls, p50, the counter read as `bench-ipc` reads it |
| A system call inside an `os` enclave against the same call on the host | within 1.1 times | `getpid` in a loop, both places, alternated |
| Start, `sealed`: from the launcher's call to the program's first instruction | under 1 ms | 1,000 starts, p50 and p99 |
| Start, `clean`: to the program's `main`, from a warm template | under 20 ms | the same |
| Stop: from the kill to the last frame freed | under 5 ms for a 64 MiB enclave | the same |
| Host memory for a `sealed` enclave, beyond the program's own pages | under 64 KiB | the job's charge |
| Private memory of a `clean` enclave that ran `hello` and waits | under 8 MiB | the job's charge after the run |
| A warm `rustc` compile inside against outside | within 1.05 times | `test-rustc`'s second compile, both places |
| A cold 4 KiB read inside against outside | within 1.5 times | S0's read bench, both places |

For comparison: Hyperlight reports 1 to 2 ms to create its micro VM and run
a function; Firecracker about 125 ms to boot a Linux guest to its init.

## 13. Checks

* **Host tests** for the second-level table code (map, copy on write, unmap,
  the sweep), the exit decode and the call mask, with Miri; a fuzz target
  that drives the exit table against a model, as `handle_table` does.
* **Boot checks**, under an `enclave` line on a boot started with the
  feature: an enclave is made, runs a few instructions and exits; E-R3's
  sweep; a call outside the mask kills; a write to a template page copies it
  and leaves the template's frame unchanged.
* **`cargo xtask test-enclave`**, the gate. It grows with the landings:
  * **The escape test** (E1). A program, never shipped, that tries, each
    attempt in a fresh `sealed` enclave: every call number outside the mask;
    every Linux system call number through the `syscall` instruction and
    through the hypercall instruction; every I/O port class; writes to
    model-specific registers; the virtualisation instructions; reads and
    writes of guest-physical addresses outside its memory; call arguments
    that point outside its memory, cross a page end or carry absurd lengths;
    and a handle it was not given. The gate requires each attempt to end in a
    kill with the expected reason in the audit, and the host's check line to
    follow.
  * **The encryption test** (E5), kept from the write-domain design. A
    program walks a populated system as root, XORs every file it can open,
    then overwrites its block device. Run in a `clean` enclave beside a
    populated home on the host: the home, the base image and a second
    enclave's layer must be byte-identical by hash, the enclave must fail to
    start its next program, and a new enclave from the same template must
    start clean.
  * **The grant tests** (E6, E7): a read-only folder refuses every changing
    operation; a path that climbs above the folder is refused; a connection
    to a destination outside the grant is refused, by name and by address.
* **Negative controls**, each a one-line sabotage shown firing by the
  check's own message and then restored:
  * the mask check answers "allowed": the escape test must report the first
    call that got through;
  * one host frame is added to an enclave's table: E-R3's sweep must stop
    the boot;
  * the copy on write is skipped: the template check must report the changed
    frame;
  * the encryption test's own control: run unconfined on a scratch home, it
    must change every file, which shows the comparison can see damage;
  * the folder server resolves without the "beneath" rule: the climbing path
    must be reported.

## 14. The landings

Points are the roadmap's story points. "Proves" names the gate and its
negative control from §13.

| Landing | What | Points | Proves | Needs |
|---|---|---|---|---|
| E0 | Probe and measure. The boot line for the feature on all three architectures; `--enclave` on xtask's boots adds `+svm,+npt`; a throwaway world switch times a null exit under KVM and TCG, and a null Linux system call | 3 | The line on every boot; two figures recorded | the consultant's design review of this document |
| E1 | **The enclave object on x86-64 with AMD-V**: second-level tables, one virtual processor, the world switch, the four calls and the mask, the `bare` image, `enclave run` relaying a terminal, audit records | 21 | `test-enclave`: a static program prints and reads; the escape test; controls 1 and 2 | E0; `SPECULATION.md`'s section; ids reserved |
| E2 | The launchers' first half: `[package.enclave]` read by `ferrix-pkg`, `Enclave=sealed` in init, the job and its quotas, the size and call counts of §4 printed by the gate | 5 | A unit and an installed package each start sealed; a quota kill | E1 |
| E3 | Templates: freeze, clone, lazy mapping, copy on write, fresh entropy, the rest of the exit table (`map`, `grow`, `cpu_start`, the clock page), `bench-enclave` | 13 | The template boot check; control 3; start and stop figures | E1 |
| E4 | **The `os` image**: the kernel's `enclave` platform, console and timer over the exit table, a root in memory, several processors; a shell runs inside | 21 to 34 | `test-enclave --clean`: busybox's shell runs a script inside; the escape test again from inside an `os` enclave as root | E3 |
| E5 | **The private disk**: `enclave-disk` with the base image and the layer, the root on btrfs, ephemeral and kept, the size cap | 13 | The encryption test and its control. **The customer's property, for a program with a system and a disk** | E4 |
| E6 | Folders and the chooser: `enclave-files`, the client filesystem inside, `ferrix-chooser`, hyprix's three conditions, `/run/handed` | 21 | The folder grant tests and control 5; a compositor test that sends virtual input to the chooser and sees none arrive | E5 |
| E7 | Network by destination: the socket trait, forwarded sockets, `enclave-net` | 13 | The network grant tests; `test-net`'s applets inside an enclave | E5 |
| E8 | The desktop: the socket bridge, a Wayland client and sound from inside, the one app launcher, defaults per §15 Q3 | 21 | A terminal and a browser window from inside enclaves on `run-compositor`; the bridge refuses the three protocols | E6, E7 |
| E8g | The GPU inside an enclave | unsized | | E8; stage 19's and stage 21's GPU work |
| E9 | Intel VT-x with EPT | 13 | `test-enclave` whole, on an Intel machine | E3; a machine (§15, Q6) |
| E10 | AArch64: a small EL2 part, stage 2, the world switch; native Pixel 7 boot keeps EL2 | 21 | `test-enclave --arch aarch64` under TCG with `virtualization=on` | E3 |
| E11 | ARMv7-A: HYP mode and stage 2 | 13 to 21 | `test-enclave --arch armv7a` under TCG; the board when its hand-off is known | E10 |
| E12 | Container mode, **only if the customer allows it** (§15, Q2) | 8 | The same manifests start under namespaces and a filter; every message says "container" | E2; can come any time after it |
| E13 | Undo: btrfs-write's Stage C, subvolumes and snapshots (13 to 21), then the snapshot unit and its policy (3) | 16 to 24 | A read-write folder encrypted from an enclave is restored from the last snapshot and compared | the btrfs stream; independent |
| E14 | The tripwire in `enclave-files` | 3 | A burst of renames over existing files pauses the enclave | E6, E13 |

* **E0 and E1, 24 points,** are the smallest thing that shows the property
  end to end: the strictest level, on x86-64, under KVM with nested
  virtualisation, with a test that tries everything else.
* **E0 to E5, 76 to 89 points,** answer the customer's question for programs
  that need a system and a disk.
* **E6 to E8, 55 points,** make it usable for desktop apps.
* **E9 to E11, 47 to 55 points,** are the other processors.
* **The whole set** is about 205 to 235 points, the GPU aside. E13 can run
  beside everything; E9, E10 and E12 beside E4 onward.

The order of the views is: output and input (E1), a clean system (E4), disk
(E5), folders (E6), network (E7), windows and sound (E8).

If the customer allows container mode and wants protection sooner, E12
with E2's manifest and E5's disk service gives the clean system and the
private disk in the weaker form for about 30 points, without waiting for E4, and the same manifests move to
enclaves as E4 and E5 land.

## 15. Decisions for the customer

Each is stated as a decision, with a recommendation.

* **Q1. The name.** "Enclave" is the customer's word and this document's.
  Elsewhere it usually means SGX's or TrustZone's enclave, which protects a
  program *from* its host. *Recommendation:* keep the word and the file name
  `docs/ENCLAVES.md`, and say the direction in the first paragraph of
  anything public.
* **Q2. May a weaker mode exist on machines without the feature?** Refuse,
  or allow container mode (§5.2). The Pixel 7 as a crosvm guest and the
  Windows PC can have nothing else. *Recommendation:* allow it, off by
  default, turned on by the machine's owner, named "container mode"
  everywhere and marked on screen. The risk is that the weaker thing becomes
  the thing people run.
* **Q3. Defaults.**
  * A packaged app with no `[package.enclave]` table. *Recommendation:*
    `clean` with a kept disk and no network.
  * The terminal. *Recommendation:* unconfined, since what the user types is
    the user's authority, with `enclave run` for one command. The
    alternative is a terminal that opens inside an enclave with the current
    directory granted.
  * Steam and games. *Recommendation:* unconfined until an enclave can have
    the GPU (E8g), then `desktop` with the library folder read-write and
    `net = ["any"]`. Anti-cheat may refuse a virtualised processor; stage
    23's step 0 will say.
* **Q4. Is nested virtualisation required of the hosts Ferrix runs on as a
  guest?** *Recommendation:* required of the gate host, which has it, so
  that enclaves are gated on every landing; not required of users' hosts,
  where Q2's answer applies.
* **Q5. To the processor, an enclave is a virtual machine.** There is no
  other hardware feature that gives a second address space the program
  cannot touch. What is removed is what makes a virtual machine heavy: the
  emulated machine, the firmware, the boot, the fixed memory. Is that the
  meaning of "not a simple VM"? *Recommendation:* yes; §17 says it plainly
  under its last heading.
* **Q6. Intel.** The gate host is AMD, and QEMU there offers no VT-x under
  either accelerator, so E9 cannot be gated in the pool. *Recommendation:*
  AMD first; E9 when the customer names an Intel machine.
* **Q7. Where an app's own files live.** In its kept disk, away from the
  home (§7.4), or in folders of the home granted by default.
  *Recommendation:* in its own disk, with named places asked for in the
  manifest and shown at install.
* **Q8. Certification.** Whether the enclave core joins the `core` ring,
  whether the claim's wording (`docs/certification/CLAIM.md`, open decision
  1) names enclaves as its non-safety partition, and the rule for sibling
  hardware threads (§4). The customer's, with the consultant.
  *Recommendation:* the design review first; code in the `core` ring; no
  claim until E1 to E3 have their evidence.
* **Q9. The roadmap.** This is stage-sized. A stage of its own, or a
  backlog item; and whether its points enter the burndown.
  *Recommendation:* E0 and E1 from the backlog as a proof, then a stage once
  their figures are read.
* **Q10. Linux GUI programs and the chooser.** A portal front-end without
  D-Bus, or a D-Bus. Carried over from the old design. *Recommendation:*
  decide at E6.
* **Q11. Landlock.** Keep it as a separate compatibility row for programs
  that sandbox themselves (§10), or drop it. *Recommendation:* keep the row
  at P3; build it when Chrome's sandbox work reaches it.
* **Q12. The strictest level has no clock and one processor** (§4).
  Programs that time themselves will not run at it. *Recommendation:* yes,
  with `clean` one flag away.

## 16. Certification

**Where it sits.** The enclave object, the second-level tables, the world
switch and the exit dispatch are memory protection and processor control.
They belong in the `core` ring, on each architecture, and they are a new
interface into the item. Everything else is outside it: the `bare` image,
the `os` image (the same kernel, running as load inside), `enclave-disk`,
`enclave-files`, `enclave-net`, the bridges, the chooser and the launchers
are ring-3 programs. The item boundary gate would have to say that the
kernel built for the `enclave` platform is not the item.

**What it could support.**

* `CLAIM.md` §1 words the claim as mixed criticality: unmodified Linux
  software in non-safety partitions beside safety functions, kept apart by
  the item. Today that Linux software is served by 64,036 lines of load in
  the host's ring 0, and the argument that the load cannot interfere is work
  still to do (the roadmap's status page counts 90 to 160 points for it). An
  enclave is a partition whose Linux layer is not in the host's ring 0 at
  all. For software run in enclaves, freedom from interference would rest on
  the enclave core and the processor, not on an argument about the load.
* The Security Target's capability policy extends to the new object without
  a new kind of policy: an enclave is a subject with a handle table.
* `ITEM.md` §6 says the item "is not a separation kernel" and does not offer
  space partitioning as a service. Enclaves are the space half of that.

**What it could not support.**

* Any statement about what runs inside an enclave.
* Confidentiality against the side channels of §4 beyond what
  `SPECULATION.md` argues for a given processor.
* Time partitioning. That is stage 14's.
* The correctness of the processor's virtualisation. `A.HARDWARE` would be
  widened to assume it.
* Container mode. No claim is made of it, as none is made of
  `--mitigations off`.

**What it owes.**

* **A design review by the certification consultant before E0's code**: a
  new interface into the item and a moved boundary (`AGENTS.md`). Its
  verdict goes into §18.
* **The reference configuration.** `ITEM.md` §5 counts one build switch and
  one boot option. Enclave code is either in every build and dormant on a
  machine without the feature, which DO-178C calls deactivated code and
  wants justified, or behind a second switch. The consultant's call.
* **`SPECULATION.md`**: a section for the world switch, before E1.
* **The Security Target**: the new object and its operations, the widened
  hardware assumption, and whether a threat of escape from an enclave is
  named apart from T.MEMORY and T.ESCALATE.
* **For every landing that touches the item**: requirements for the new
  behaviour, every `unsafe` traced, a check and a negative control for each
  rule of §6.1, coverage carried after the last rebase.
* **Requirement ids.** This document writes none. E1 reserves its ids on
  `main` first (`docs/CONVENTIONS.md`, *Requirement ids are reserved before
  they are written*).
* **The audit**: new events for making and killing an enclave, in
  `docs/certification/AUDIT.md`.

## 17. Sources, and what was checked

**Checked against the tree at f42c5fd23 on 2026-10-10:** every row of §2
that does not say otherwise; the figures of §12.1 against the documents
named; `SECURITY-TARGET.md` §1.2, §1.3, §3.1, §3.2 and FDP_ACC.1;
`ITEM.md` §2, §5 and §6; `CLAIM.md` §1; `INIT.md` §4.5 and §6; `APPS.md` §3;
`AUTH.md` §6.1; the two ring documents' object tables.

**Probed on the gate host on 2026-10-10:** the QEMU feature probe and
`kvm_amd`'s `nested` parameter (§2).

**Assumed, not verified:**
* how much of the kernel's bring-up depends on firmware tables (E4);
* that WHPX offers a QEMU guest no virtualisation (§5.1);
* how the STM32MP157 board's U-Boot hands off, and which Arm cores can trap
  the virtual counter from EL2 (§5.1, §6.1);
* that the item has no AMD IOMMU driver (§9.2: the Security Target names
  VT-d and SMMUv3);
* btrfs-write's refusal of a volume with snapshots, taken from the
  write-domain design (§2);
* every target in §12.3, and the 4,000-line budget of §4;
* the cost of an exit in §12.1, which is the literature's order of magnitude
  and not a measurement.

**External sources.** Those marked *read today* were read on 2026-10-10;
the others are cited from the author's knowledge of them and were not
re-read.

* Belay et al., "Dune: Safe User-level Access to Privileged CPU Features",
  OSDI 2012:
  <https://www.usenix.org/conference/osdi12/technical-sessions/presentation/belay>
* gVisor, *Platform Guide* and *Introduction to gVisor security* (*read
  today*, in summary): <https://gvisor.dev/docs/architecture_guide/platforms/>
* Hyperlight's README, and DevClass's report of 1 to 2 ms from its
  engineers (*read today*, in summary):
  <https://github.com/hyperlight-dev/hyperlight>,
  <https://www.devclass.com/development/2025/03/27/microsoft-releases-experimental-hyperlight-wasm-micro-vms-that-run-wasm-apps/1623450>
* Agache et al., "Firecracker: Lightweight Virtualization for Serverless
  Applications", NSDI 2020, and LWN's summary (*read today*, in summary;
  secondary sources disagree on what the 125 ms and the 5 MiB bound):
  <https://www.usenix.org/conference/nsdi20/presentation/agache>,
  <https://lwn.net/Articles/775736/>
* Nabla containers and Solo5: <https://nabla-containers.github.io/>
* seL4's virtualisation support: <https://docs.sel4.systems/>; Android's
  pKVM: <https://source.android.com/docs/core/virtualization/architecture>
* Apple Platform Security, for the app sandbox, entitlements, pointer
  authentication, PPL and SPTM:
  <https://support.apple.com/guide/security/welcome/web>
* AMD64 Architecture Programmer's Manual, volume 2, chapter 15 (SVM); Intel
  SDM, volume 3C (VMX); the Arm Architecture Reference Manual (EL2, stage 2,
  the counter traps).
* Landlock: <https://docs.kernel.org/userspace-api/landlock.html>

**Said plainly, where the request cannot be met as worded.**

* *"Not a VM."* The only processor feature that gives a program an address
  space it cannot escape and a host it cannot call is the one virtual
  machines are built from. An enclave uses it. It is not a virtual machine
  in the sense the customer objected to: no machine is emulated, nothing
  boots, memory is not set aside, and start is a clone.
* *"Fully locked down."* §4 lists what remains. No design removes side
  channels, covert channels or the chance of a bug in the trusted base. What
  can be done is to make that base small and to count it.
* *"A fully clean OS" and "only print and input".* These are two levels, not
  one. At the strictest level there is no operating system to look at: a
  call other than the four is answered `ENOSYS` inside.
* *"Similar to iOS but more extreme."* The checkable sense is the host
  interface: an iOS app makes filtered system calls into a shared kernel,
  and an enclave makes none. No claim is made that Ferrix is more secure
  than iOS as a whole.
* *"Faster wind up and down" than a container.* Plausible at the strictest
  level and with templates, and unmeasured. §12.3's targets are guesses
  until E0 and E3.
* *On every machine.* The Pixel 7 as a crosvm guest, Ferrix's main AArch64
  deployment today, cannot have the hardware mode at all (§5.1).

## 18. Where it stands (2026-10-10)

**State.** A design only: no code. Written on branch `enclave-design` from
`main` at f42c5fd23. `docs/WRITE-DOMAINS.md` is removed by the same change;
its last text is at f42c5fd23, and §10 says what of it is kept.

**Review.** Nobody has reviewed this document. The certification
consultant's design review is owed before E0 (§16), and the product owner's
word on the order of the landings. Neither is required to land a document
outside `docs/certification/`.

**Decisions.** The customer's instruction of 2026-10-10 is recorded in
`docs/BACKLOG.md`, Decisions. §15's twelve questions are listed there under
*Waiting on the customer*.

**Requirement ids.** None written, none reserved.

**Its row** is in `docs/BACKLOG.md` P2, where the write-domain row was.
