# A small ring 0: what "about 20k" would take

*mk-tiny, 2026-11-05, on nazuna, develop bc617532d. Design only: no kernel code, nothing built here
ran as a boot or a row. The customer asked whether the microkernel is around 20k lines and asked for
one design agent. Every figure says **measured** or **estimate**. Builds were by hand at `nice -n 19`,
debug-optimised unless a row says release; no gate, no evidence. Scripts, per-file data and the budget's
generator: `~/.local/share/ferrix/mk-tiny/` (`budget.py`, `lines.py`, `scen.py`, `cc.py`).
**Reviewed: OK IF TK.1-TK.14, ledger 7256**, folded in here; §10 says what is still NOT YET.*

**Two names.** A **placed space** is a space with no regions: `SPACE_WINDOW`, `feature/mk-ibpb`'s window and
`schedule=windows` are three other things already. **Cut 0 to Cut 8** are this design's steps; ledger 7256
calls them T0 to T8, which collide with DEVMGR's T0, stream T1 and the tool classes.

## The answer

**No: not today, and not at 20k all-in.** Measured today, x86-64 is 64,158 lines of the kernel's own
crates and AArch64 58,592, with about 40,000 more in library files that are linked, and 16,000 to 18,000
of checks that run in ring 0 on top. The target I can support is about 36k, or 33k on a static board.

| Count, `--personality user` | x86-64 today | AArch64 today | seL4 (x86-64, nearest, this machine's build) | Budget, x86-64 `fast` | Budget, AArch64 `max` |
|---|---:|---:|---:|---:|---:|
| **A. Ring 0's product lines**: kernel crates + library files with linked code, **+ checks, printed beside and run in the image**. **Proposed definition** | **105,089** + 17,726 | **97,631** + 16,334 | 18,627 + 0 | **43,600** + about 8,000 | **36,300** + about 7,000; **32,900** + 7,000 with a static board |
| B. Kernel crates alone: the customer's figure, `test-kernel-shape` | 64,158 | 58,592 | same | 31,700 | 25,000 |
| C. Lines that carry machine code, check functions out of the call graph, opt-level 1 | 21,991 | not computed | 8,326 | | 9,000 to 11,000 |
| Text, opt-level 1: own code, then `core` and `alloc` | 0.74 MB, 0.76 MB | | 0.08 MB | | perhaps 0.6 to 0.8 MB in all |

Today's columns are measured. **The two budget columns are estimates**: ±30% a file, ±15% a total, and that ±15%
assumes independent errors, which one author in one direction does not give. Expect the high side: 31k to 41k by A.

* **Recommended target: count A under 36k on AArch64 `max`**, the only architecture either certified profile
  claims; under 33k if D2's static board is chosen, on a platform not chosen yet; count B under 25k. By A that is
  about 37%, a little over a third of today. **By lines with code and by bytes it is about half**, and those are
  closer to what coverage and object-code work pay for. Count C would land at 1.1 to 1.3 times seL4 and text
  at about 8 times: estimated, on another architecture, against measured.
* **20k by count A is the wrong target.** seL4 itself reads 18,627 here, one processor, no restricted mode, no audit,
  no job quotas, no boot checks. §4.4 prices every further cut I can find and stops at about 23,600, having removed
  SMP, Linux on `max` and every dynamic device. I found no honest way to the last 3,600.
* **20k by count B exists**: static board, one processor, static devices give about 20,200. It leaves libraries
  uncounted and makes `max` a one-processor kernel. A customer decision, §8 D3; not recommended yet.
* **Speed: no §7 row rises, by estimate.** Every fault of a Linux program already crosses to linuxd, whose shadow
  already runs `user/space.rs` in ring 3, so the crossing is in today's figures. §5 says what is not priced: linuxd's
  own arena, native eager maps, job paths. No step is built. **Every limit row in §5 misses today and still misses.**
* **What it does not do.** A third of A's fall removes no executed product code (§4.4), and the TSF shrinks partly
  because the claim shrinks (§6). Moving a line changes the count more than the risk.
* **First step:** Cut 0, the ratchet, tools only: the review allows it on ledger 7256. Then Cut 1's `cfg` of IOMMU
  units by architecture, under its own review. §7.

## 1. How to count

The brief's figures and this tree's differ, so both are here. Measured with
`cargo xtask test-kernel-shape --arch <a> --personality user` on bc617532d:

| | Product lines | Not under a cfg this shape rules out | Checks |
|---|---:|---:|---:|
| x86-64, brief | 64,777 | 62,303 | 17,125 |
| **x86-64, this tree** | **64,158** | **61,702** | **17,726** |
| x86-64, `max`'s values less `windows` | 64,227 | 61,771 | 17,787 |
| x86-64, `fast`'s values, fast path off | 64,470 | 62,014 | 17,975 |
| AArch64, brief | 58,822 | 56,479 | 15,733 |
| **AArch64, this tree** | **58,592** | **56,267** | **16,334** |
| AArch64, `max`'s values less `windows` | 58,661 | 56,336 | 16,395 |
| AArch64, `fast`'s values | 58,695 | 56,370 | 16,583 |

About 600 lines changed column between the two readings, `region_check`'s host and its neighbours by
inference; I did not bisect it. Everything below is against the bold rows. No profile builds, so `fast`
and `max` here mean ad-hoc `--set` builds of their values, and the differences above are small because
the flags choose little at file level.

**The proposed definition, count A, "ring 0's product lines".** For one architecture and one profile: non-blank,
non-comment lines of

1. every file rustc's dep-info lists for `ferrix_kernel_core`, `ferrix_kernel_item` and `ferrix_kernel`,
   as `check-kernel-shape.py` does today, less the files it calls checks, **and any generated file rustc reads
   outside `src/kernel/src`, which the tool skips today and Cut 0 counts**; and
2. **every library file with at least one line of machine code in the image**, by the image's own `.debug_line`,
   whole file. A library in ring 0 is ring 0. Whole files, so that an `errno` does not bring 5,387 lines of
   `ferrix-linux-abi` in free: the cheap way down is to split the crate. Which files have code moves with opt-level:
   the ratchet reads **release** (M.7), and prints opt-level 1 beside it.

**One column.** A counts whole files. A line under a `cfg` inside a read file stays counted: the tool's brace walk is
"no evidence" in its own words. So the budget's "compiled out" falls only by a `#[path]` or a file split, which §3.1
requires, and the second column of the table above is printed and is not the count.

**Checks are not in the headline figure, on one condition: the same row prints them.** `max.toml` has `checks = "on"` and
`"ferrix.checks" = ["run"]`, and §2.1 argues them as executed code, so "all of ring 0" would be false: the image holds A +
checks. Also printed: count C; text bytes; `core` and `alloc` by bytes, which count A leaves to the toolchain's
qualification (F-17). The headline is **AArch64 `max`**: both `max.toml` and `fast.toml` have
`claim_architectures = ["aarch64"]`, so x86-64 is shape evidence and the speed rows' host, and no line of its `mce*`,
VT-d or AMD-Vi is in any claim.

**Why not count C, though it reads 22k today.** It is the tempting one and I reject it as the headline. Lines
with code are what coverage and MC/DC pay for, and the coverage reports do read these line tables
(`Cargo.toml`'s `[profile.dev]` comment). But "22k, so we are there" would be flattery: seL4 by the same
measure is 8,326 at `-O1`, 1,529 of them generated, so Ferrix today is 2.6 times seL4 on it, and 22k is
not the same quantity as anyone's "20k lines". It is printed beside A. On AArch64 it stays "not computed" until the method
sees `adrp`; "check functions out" is an edit of the call graph, since a kernel without them cannot be built (§2's fourth).

**What seL4's 8,700 is, and is not.** `MICROKERNEL.md`'s figure is the proof paper's (`RESEARCH-CHECK.md` line
49). seL4 at `c6ce4d2a`, 16.0.0-dev, on nazuna, read and counted:

| seL4, x86-64, non-MCS, one node, IOMMU on, VT-x off, no printing, the `matched-nopcid` build | Measured |
|---|---:|
| All of `src/`, every architecture, no comments | 41,655 |
| **What the compiler reads for this configuration** (`kernel_all_pp_prune.c`, directives and blanks out) | **18,627** |
| Lines with code, `gcc -O1 -g` of `kernel_all.c` / `-O2` | 8,326 / 7,643 |
| `kernel.elf` at `-O2`: `.text` + `.boot` + `.rodata` | 64,807 + 28,157 + 2,767 bytes |

So **the nearest thing to count A is 18,627, not 8,700**: pruned preprocessed C is not whole files, so it is
near, not like for like. By origin: `src/object` 3,412, `include/arch` 2,531, `src/arch/x86/kernel` 2,509,
`src/arch/x86/64` 1,740, `src/plat` 1,233, `src/arch/x86/object` 1,156, `boot.c` 845. I built no AArch64 seL4 here;
its `src/arch/arm` is 11,491 against `src/arch/x86`'s 13,135, all modes, so it would be near.

**Bytes do not flatter either.** Release `.text`: Ferrix x86-64 1,866,452, AArch64 1,597,496, seL4 92,964 with
`.boot`: **20 times**. At opt-level 1 with check functions out, x86-64: kernel 540,659, libraries 202,814, `core` 535,359,
`alloc` 222,356, by line-table attribution, so half of Ferrix's text is inlined standard library. Count A does
not see that half. V2's object-code verification does. The budget's bytes are perhaps 600 to 800 KB (estimate),
still about 8 times seL4.

## 2. What reading the tree changed

Six things the earlier list did not have.

1. **The VM has already left, for Linux programs.** `M5-LIFT.md` §3: "`user::space`, `user::vmo`, `object::*`:
   the kernel's own files, by position" inside linuxd, and "`AddressSpace`'s page tables are the shadow". §3a,
   agreed by the customer on 2026-11-04: one arena VMO, and "linuxd does" resolve a fault on a resident
   page. `lift/core/fill.rs` is the path: a restricted fault exits to linuxd, `fill` walks the shadow, `SPACE_PLACE`
   maps it. A placed space has no region, no rmap and no fault resolution in ring 0 (`user/space.rs` `place`, ledger
   6411). So in a user kernel, **ring 0's 3,529 lines of `space.rs` serve native processes and linuxd's own
   space, and nothing else**. The hard part of "user-level VM" is built and running; the old copy in ring 0 was
   never compiled out.
2. **Mechanisms with no ring-3 caller.** By name, every `pub fn` of `ferrix_native` against `src/user`, `src/lib`,
   `src/tests` and `lift/`, by grep, so it can miss a caller and any test-only one is out of it: none for
   `pager_create`, `pager_supply`, `pager_op`, `pager_keep`, `pager_take_up` (K3, K13), `issue_ticket`, `adopt_ticket`
   (K7), `timer_create`, `timer_set`, `timer_cancel` (K10), `read_attested` (K6), `read_state` (K4), `clone_space` (K2),
   `region_create`, `process_create_over`, `job_declare_buffer` (K17). They have kernel checks and no client: the
   arena took K3's. Having no caller does not by itself let one go: §3.2's rule. `partmgr`'s header says it supervises; its source uses `notify_on_exit`.
3. **Half the library lines are not product.** Linked: x86-64 40,931, AArch64 39,039, whole files. Reachable with
   the checks removed, x86-64, by the image's own call and address references: **27,140 lines of files, 4,389 lines
   with code**. `ferrix-vfs` (6,044 lines, 47 KB) is not on that list at all: the product names it for an `Errno`
   type in `space.rs` and `vmo.rs`, and the rest is reached from checks. `ferrix-virtio`, `ferrix-blkring`,
   `ferrix-cgroupfs` and `ferrix-vdso` contribute under 400 bytes of product between them.
4. **`checks=off` removes almost nothing here**: 6,664 bytes of x86-64's text, 1,932 of AArch64's. About 209 KB of
   check code stays linked and rustc reads the same files. That is not a defect of the flag's stated purpose, BF7's "so it
   cannot rot", but no count may rely on it, and a kernel with its checks out cannot be measured today.
5. **Link-level pruning frees little.** With the check functions and the pager's handlers gone from the image's graph,
   1.4 KB more goes; with `SPACE_CLONE`, `oom`, and every region and fault path too, 49 KB of 1.51 MB. Objects
   name each other through `Object`'s drop and the handlers' shared helpers. So a smaller kernel needs smaller
   *types*, not fewer entry points. Conservative method: every address found in data is a root.
6. **Scale of ring 0's checks.** 17,726 lines is about all of seL4 again. Those that test what goes, go with it.

## 3. The smaller mechanism

### 3.1 New small files, chosen by flag

Every replacement is a **new, small file beside the old one**, selected by `#[path]` under a compile-time flag,
so `present`, `desktop` and linuxd's by-position copies compile today's text untouched: the same symbols with the
same sizes, `test-kernel-symbols` (M.8). No `cfg` through `space.rs`. **The price: two implementations in one
tree, for good**, since `desktop` stays hybrid. Count B falls by about 33,600 lines; the tree grows by about 15,000 (estimate),
and a fix in shared logic can be owed twice.

| New flag | Values | `max`, `fast` | `desktop`, `present` | Requires |
|---|---|---|---|---|
| `kernel_vm` | `placed`, `regions` | `placed` | `regions` | `placed` needs `personality=user` |
| `device_bus` | `devmgr`, `kernel` | `devmgr`, once its design returns (§3.6) | `kernel` | `devmgr` needs `kernel_vm=placed`; the kernel refuses an untranslated node under it |
| `first_process` | `one`, `three` | `one` | not applicable | K14's measured start |
| `firmware` | `static`, `tables` | `static` on `max`'s board (D2), `tables` on `fast` | `tables` | a board's generated description |

Each has `built`, levels and rows as `BUILD-FLAGS.md` asks, and each joins FLIP's refusals when it enters a profile. None is a boot option. Cuts 1 and 2 need no new flag:
`ferrix_personality="user"` and `target_arch` are enough.

### 3.2 Objects and calls that remain

| Object | Made from | Calls | Today's | Gone from ring 0 |
|---|---|---|---|---|
| Handle table, one or two a process | `ferrix-objects` | close, duplicate, replace; wait one, wait async; id | `object/mod.rs`, K11 | |
| **Memory** | a job's reservation, K12 | create, read, write, size; seal, digest (K14); hold, pin, pin addresses | `user/vmo.rs`, 2,178 | copy-on-write children, pager backing, `decommit`, rmap, retire lists, `ShadowCopies`, `Sharing` |
| **Space** (placed) | root and tables from the reservation | create, place, unmap, protect, read, write; a process's own by `VMO_MAP`, in `rt` over place | `user/space.rs`, 3,529 | regions, `Vma` tree, lazy resolve, fork, remap, file mapping, advise, fault-around |
| Process | bare | make, run, status, space, images, give and take bootstrap | `process.rs` + `bare.rs`, 1,135 | `Host`, the personality seam |
| Thread | task | create, self, exit, yield, affinity | K11 | |
| Region, restricted | K1, K11 | enter, resume, continue, kick, context and vectors, copy window, shared process, handle moves | kept whole | |
| Job | reservation | create, kill, limit, read; level (K17), speculation domain | `job.rs` + `quota.rs`, 2,179 | slot table, cgroup link, per-resource quotas, oom |
| Channel | message, sender's job | create, write, read, write-read, **attested read (K6 stays: §2.5's masquerade)** | `channel.rs`, 766 | tickets, if K7 goes (below) |
| Port | | create, queue, wait | 334 | |
| Address wait | | wait, wake, wait any | `address_wait.rs`, 666 | keys by VMO |
| Interrupt, IoMapping, device grant, domain, pin | K8 | interrupt, map, info, aperture, quiesce, clock, limit, isolation, bind, served; config read, **filtered** write | `device.rs`, `iommu.rs`, `pin.rs` | traversal, matching: §3.6 |
| Misc | | clock, random, debug write, console read, audit read | | |

**About 75 numbers of today's 135** (estimate). seL4's non-MCS `syscall.xml` has 8 system calls, and this build's
generated headers about 70 invocation labels.

**What may leave with no caller (ledger 7256 TK.5).** No ring-3 caller **and** no claim rests on it **and** a stated client that would bring it back. So:

| Mechanism | Standing | Condition |
|---|---|---|
| K2's clone | Out | Nothing forks a space in this shape |
| K3 | Out, with D11 | AoU-41 scoped. The lift's arena took its client; it also means restoring the plan's separation is given up: D11 |
| K10 | Out | §2.5's "no higher part blocks without a timeout" is shown by a wait's deadline in the kept set |
| K13 | Out | `BOOT-AUTHORITY.md` §4 and BA.8's "until K13" read "never in `fast` and `max`": devmgr's death is a line and linuxd's EIO. AoU-43 scoped |
| K6, K17's declare | **Stay**, though no caller | A claim rests on each |
| **K4** | **NOT YET, one named choice, D8** | §5 and ledger 886, accepted by the customer, say `partmgr` supervises through K4; its source uses `notify_on_exit` alone. Whole; or report and kill; or out, with §5 and 886's wording rewritten to supervision by end notice and a fault meaning death. The budget assumes report and kill, 180 lines; whole adds about 500 |
| **K7** | **NOT YET, a named replacement** | §2.3's "server-held memory charged by K7" and T.EXHAUST are the issue: no caller means `btrfsd` and `netd` have that hole today, not that it is closed. Out only with a per-client bound in the server (`btrfsd`'s item, a requirement, a control) or a checked rule that no shared server spans levels. §2.5's "charged to the sender, K7" is a loose citation: per-message charge is F-37's on the running job, unaffected. The budget assumes out; if not, add about 230 |

Each is proved by compiling it out: M6's `ENOSYS` check extended to each number, stage 9's lines leaving a user boot said, `level_check`, `sealing_check`, `grant_check` and `check-level-doors.py` scoped, requirements *under `personality=kernel`*.

### 3.3 Memory, and what "retype" means here

**Retype at page level only.** A frame is, by a per-frame tag K12 already keeps: free in a reservation, user page
of one Memory object, table of one space, or slab page of one job (K16). The holder decides what lives where in its
own Memory: linuxd's shadow, for a partition; `rt` and the process builder (K15), for a native process. **The kernel
never again decides what a virtual address should hold**; it maps what it is told, under W^X, K14's ceiling and
K2's rights, and refuses what is not.

`Space::place` is that kernel, as it stands today: range check, ceiling check, commit, `map_in`, shootdown. Its review is ledger 6411
(W1-W8: "the first hardware mapping in the kernel with no region, rmap or reference behind it"), and its unmap's 6421 U1-U9,
6462 and 6537 C1-C3. Cut 3 makes it the only way. **Those were arguments for restricted spaces, and W6 refused the rest**: measured spaces, the region's
own space, and protect, read, write and clone on a window. So W6 is reversed, not restated, and §3.2 keeps protect, read and write. What
Cut 3 owes, before code, by space class (program, native QM, measured or safety, linuxd's region): W1-W8 and U1-U9 restated in a table; what a measured space may place, by K14's sealed execute and ceiling (L.mm.120), the first-image exception; who writes leaves (place, protect, a demand fault, each under `inner`, W4 at return); W3's 4,096 bound against a native process, refused by name; W5's hold with commit and table allocation, in MEMORY-AND-TIMING; L.user.181's sweep; by-handle read and write, and `ptrace`'s answer (TK.7).

| Today in ring 0 | With a placed space |
|---|---|
| `Vma` tree, `new_region`, merge and split, `map_anywhere` | A cursor and a page-table walk. `VMO_MAP`'s ABI stays for native programs: `rt`'s bump allocator picks the address |
| `fault_with`, `resolve`, `permitted`, zero page, copy-on-write | A **demand range**, below; else no fault resolution |
| `fork`, `attach_child`, `SPACE_CLONE`, shadow chains | Gone. linuxd's fork copies by its shadow. K2's F-41 control stays where the shadow is, and by R6 no requirement gains from that run |
| `remap`, `advise`, `map_file`, `fill_file_pages`, `fault_around`, `write_ahead` | Gone from ring 0; linuxd's by position |
| rmap (`mapper list`), `forget_pages` | Gone. A placed Memory's pages never change (W2, sticky) and a space holds what it placed (W3) |
| `user/cache.rs`, `drop_poisoned` | **W7 stands**: fail closed, the frame stays out, the partition's job killed, audited; built nowhere yet. Handing it to linuxd by K1's exit would be a new mechanism, NOT YET. Teardown keeps a retired frame out, L.object.262's `is_retired` stays for pins, `test-mce` under user or NOT RUN. **AoU-36, H.FAIL.8 and H.FAIL.11's rows are x86-64: for `max`'s only claimed architecture, what a synchronous abort or an SError on a placed frame does, and a scrub retirement, are unwritten and owed (TK.10)** |

**A demand range.** A placed space holds up to eight fixed `(start, length, Memory, offset, rights)` ranges, set once, refused twice. A fault in one commits and places that page in ring 0, no tree, no split, no crossing; about 80 lines, a guess. It is a ring-0 fault path, and owes: W2 and W3 hold its Memory; no EXECUTE; a failed commit is that thread's fault, never the kernel's; what `uaccess` does on hitting one; charged to the faulting job's reservation (K16.4); K16.3 lists the new payers; K16.7's rule for a hold across jobs: a restart that does not fit refuses, named. **The kernel refuses a range in a job above QM**, with a check and a control; K16.2 applies: a safety partition's place is done by READY.

**The region's half has no design, and §3.4 and §5 turn on it: NOT YET (TK.9).** `place` refuses at `SHARED_BASE` (`space.rs:914`), and linuxd's region holds its 4 GiB arena, mapped once with "untouched pages cost nothing" (`lift/core/mm/mod.rs`), foreign windows, heap and stacks. Eager is all of it committed. A place per first touch is a new call on the fill's path. **My candidate, unreviewed:** a demand range has no size limit, so the arena is one range and the windows another, resolved in ring 0 as today's region fault is, with K11's shared tables and shootdown unchanged. It keeps a Linux program's fresh page about where it is. It must come back to review before Cut 3's code.

**True retype, for the record.** seL4's whole form, objects placed by the user in Untyped with a derivation tree:
`src/object/untyped.c` 224, `cnode.c` 746, `objecttype.c` 824, `kernel/cspace.c` 149, **1,943 lines measured**, against
what it removes here, `ferrix-heap`, `ferrix-kmem`, `fallible`, some of `mm.rs`: about 1,200 (estimate). **No line
gain**, a rewrite of every `Arc` in the object layer, and `max` keeps per-job heaps by the customer's order. It remains
`max_plus`'s, `kernel_memory=none`, unchanged by this design. Its real gain is the proof, not the count.

### 3.4 Faults and pagers

| Fault | Today | With Cut 3 |
|---|---|---|
| A Linux program, any page | K1 exit, linuxd's `fill::resolve`, `SPACE_PLACE`, resume | **The same** |
| linuxd touching its arena or a stack | Region fault in ring 0, lazy commit | NOT YET, above. Candidate: ring 0, by a range |
| A native QM process, first touch | Region fault in ring 0, lazy commit: **the common case** | Native programs are changed to map before use, or a range for stacks. **A change to every native program, not a fact**: `devmgr`, `rt`, `netd`, `btrfsd`, drivers to be read, footprint stated |
| A native process, anywhere else | Region says no: K4 or death | By D8's K4 choice; else death |
| A safety partition | Region fault in ring 0 | **No range is allowed**: mapped by READY, so any fault is an error. Good for stream T1's bounds |
| A pager | K3, in ring 0, no client | Compiled out, §3.2 |

`SPACE_READ`, `SPACE_WRITE`, `RESTRICTED_COPY_WINDOW` and `uaccess` stay, with `ENTER_COPY_MISS_EXITS`.

### 3.5 Jobs and memory limits

Ring 0 has few jobs in this shape: `linux`, `drivers`, each partition's. R3 (ledger 6012) moved cgroups into linuxd, whose
`object/job.rs` runs by position. So **a ring-0 job needs no tree worth the name and no per-resource table**: it is a kill
group, a level, a speculation mark, one reservation and, under `windows`, a window. Under `retyped` "kernel objects" and "memory" are
already one number, since the slab is on the reservation's pages. `FRU_RSA.1`'s three quotas become: memory and objects
by the reservation; processor by the window or budget. `quota.rs` is 1,304 lines of slots, periods and bursts, most of it
`cpu.max`'s, which is linuxd's sum by §3a. Depends on `retyped` being `built`: R.1 to R.5. K16.6's nearest-ancestor rule survives: children still draw on a parent's entry.

### 3.6 Devices

**The first draft of this section claimed a hardware-fed parser leaves the TCB. The review refused it (TK.11) and it was wrong.** What is true: **traversal, matching, DEVICES and REPORT can leave; a smaller parser stays.** This is NOT YET, and returns as Cut 7's own design before D7 is asked.

| Why it stays in ring 0 | |
|---|---|
| The kept set reads BARs (`bar::size`, which writes and drops decoding), mints MSI and MSI-X, filters writes, keeps ATS off | `device.rs` imports `ferrix_pci`'s `bar`, `capability`, `header`, `msi`, `msix`; `iommu.rs` its `Address` and `topology` |
| The ECAM base | The kernel's mediated read needs it, and ledger 818 (B6) forbids ring-3 ECAM. A base from ring 3 is authority. MCFG's reader stays |
| Requester IDs | `learn_bridges` feeds `iommu`'s `BRIDGES` and `topology::agrees`. A lie there decides what a domain translates, so K8.3 cannot rest on it: the kernel reads the path itself |
| "Every function" | AoU-12's ATS off, K8.6's `silence_found_masters`, MSI-X held off, `decoding_refusal`, BA §2's stop all say every function. In ring 3 that becomes "every function asked about". A function devmgr omits keeps firmware's mastering and ATS. So a bounded read-only walk stays for these, or each gets a per-unit argument by default-deny |
| Not per architecture | K8.2's Tree and VirtioMmio nodes get `Domain::untranslated()`, and §6.4 names a refused unit: `device_bus=devmgr` refuses such a node or the boot, by the kernel, with a control |
| BA.3, ledger 6343 | The filter is the kernel's "whatever devmgr does". A bus handle reads every BDF unless the kernel refuses the description's and their closure. A parent-is-a-root-port read stays, ACS with it. K14's `flag_nodes` (7166) at mint |
| New exposure | A QM server now chooses when ring 0 touches hardware: an absent BDF on a host with no clean ECAM may abort; re-sizing a function a driver runs is refused. `widen` and `withhold` decide what an aperture holds: not policy |

**So the budget now keeps** about 450 of `discovery/pci.rs`, about 1,500 of `ferrix-pci` and `device.rs`'s 800 (the review's estimate: 1,000 to 1,900 and 400 to 500). What goes to devmgr: `hosts` as policy, traversal, matching, `discovery/devmgr.rs` and `ferrix-devmgr-proto`. K8's closing sentence, `ITEM.md`'s "device enumeration is inside" and §2.1 stand until that design is reviewed. **With `firmware=static`, ask first whether `max` needs a run-time bus at all**: §4.4.

**IOMMU.** Units by architecture: AArch64 `smmuv3.rs` alone, x86-64 VT-d, AMD-Vi, IVRS, remapping. `iommu.rs`'s 1,864 is half policy and
report: `plan_rmrrs`, `announce_intel_gpu`, `say_unit`, `report`. About 900 remains: domains, pin, `map_all`, fault record. K8's rule stands.

### 3.7 The scheduler

`schedule=windows`, as FLIP and ledger 7011 S.1-S.6 require and the customer's C3 and C6 say: a static major frame, fixed
priorities per processor inside a window, a bitmap, **no fair class**. So `max` drops `vtime`, EEVDF, `ferrix-sched`'s tree, `steal_from`,
`balance`, `pull`, `leftovers`, `borrow`; keeps switch, wake, block, sleepers, preemption, reaper, `direct` if `ipc_fastpath` comes on. Budget: about
3,100 with K9's `windows` file at 350 and no code exists, so **this row is the softest in the budget**.

`schedule=edf` keeps the fair class under `OverFair`, as K9 §2a has it: about 4,400 plus 2,000 of crates. **Policy stays in ring 0.** A ring-3
scheduler would add a round trip per block and wake; `futex-thread-pingpong` is 4 crossings already, so it doubles. Rejected. D5 asks whether
`fast` needs EEVDF or a plain round-robin under its CBS: about 1,500 lines (estimate), and a row of its own.

### 3.8 How much of `arch/` can leave

Less than asked for: **`arch/` is the code that can only be ring 0.**

| | Measured today | Can leave | Stays, and why |
|---|---:|---|---|
| Machine checks, x86-64 | 1,570 | `decode.rs` 313 and CMCI's policy, to `lode` through `CPU_SERVICE`'s ring; about 1,020 | The `#MC` entry, bank read, recover-or-stop, the retire: an NMI-class path, MSRs. About 550 |
| Speculation | x86-64 885 + 397, AArch64 486 + 397 | Reporting, and what a fixed profile cannot need: about 230 and 180 | Every barrier stays. IBPB on restricted exits under `partition_speculation=exit` is fixed |
| IOMMU | §3.6 | | K8 |
| Linux-shaped | `signal.rs` x86-64 500, AArch64 188; `vdso.rs` 200; `signal_frame.rs` 102 | About 630 and 190, by M6's table: K1 keeps only the context's check | |
| Diagnostics | `msrlog*` 321, `inspect*` 56, `framebuffer.rs` 96 | All | |
| AArch64 GIC | `gicv2` 239 | Out on a v3 board. ITS 401 only if devices need MSI | |

**Budget: AArch64 4,500, x86-64 7,700.** Restricted mode, trap entry, switch and `smp` are not on any list.

### 3.9 One first process

BOOT-AUTHORITY §1 has the kernel start three: manager, devmgr, linuxd. Under `first_process=one` the kernel starts `partmgr` alone with a
fixed table, as seL4 starts its root task; `partmgr` starts devmgr and linuxd with the same handles. `first_rows.rs`, `boot_authority.rs`,
`linuxd_start.rs`, `discovery/first_image.rs`: 1,409 lines to about 570. L12's rule holds, since `partmgr` is certified. About 800 lines move **into the item**, which a count flatters. Standalone small; D6.

**Owed (TK.12).** H.DEV.12 ("pass from the kernel to their holders directly"), H.DEV.13, L.object.320-321 rewritten: kernel-judged rows become `partmgr`'s. The table excludes the root job (BA.12). BA.2 and §2's filter go to `partmgr`, and BA.13's kernel parser goes, a gain, but K16.9's `max_processes` must still reach the kernel: how is owed. BA.8: the kernel's watch is on `partmgr` alone, and `disable_dma` on devmgr's death is reached how. L.init.2, read by whom. K14's `grant_starter` of devmgr and linuxd, L.object.334's exception narrowed. `partmgr` starts on every boot, so its death stops a Linux-only `fast` machine, FX-0954.

### 3.10 Against others

Same counter throughout: `cc.py`, no blanks, no comments, tests dropped by directory and name. **Read means read:** seL4's `untyped.c` to its first
200 lines, `faulthandler.c`, and the build; for the others, directory listings and heads. I did not read Zircon's VM, nor Redox's scheme code. Fetched
shallow on 2026-11-05 to `~/.local/share/ferrix/mk-tiny/ref/`.

| Kernel | Revision | Lines | What it keeps in ring 0 |
|---|---|---:|---|
| seL4 | c6ce4d2a | 18,627 for one x86-64 configuration, 41,655 all | Untyped, CNode, TCB, Endpoint, Notification, frames and tables as caps, IOSpace. No VM policy, no allocator. Faults by `sendFaultIPC` to a handler endpoint: read |
| Zircon | fuchsia 62795b11 | 295,226 `zircon/kernel` all architectures: `object` 34,317, `vm` 49,593, `arch` 44,178 (x86 21,655), `kernel` 31,157, `lib` 74,652, `dev` 26,187. **Now mostly Rust** | VMOs, VMARs, page faults, pager, evictor and compression in ring 0: `vm/`'s listing. K1's restricted mode is `kernel/restricted.rs` |
| Hubris | 59c9a07 | 3,248 Rust, `sys/kern/src`, Arm M only | Static task table, no dynamic memory, 14 calls, read from `syscalls.rs`: no MMU, no SMP. Shows Rust does not force bulk; not the same job |
| Redox | b030c6f | 34,827 Rust, all architectures | Schemes, including memory and processes, and `context` in ring 0, listing only |
| Ferrix target | | 36,300 count A, 25,000 B | Zircon's object names, seL4's lack of VM policy, K1's restricted mode |

Nearest in spirit is seL4. Ferrix differs by: restricted mode (K1, K11) so a Linux call involves no IPC, SMP as the default, the barrier rules, audit,
jobs and windows. Those are why 18,627 is a floor.

## 4. The budget

The tree's modules, as `test-kernel-shape` groups them, with every line accounted for. **Today is measured; every other column is an estimate.** The generator
has a row per file, `budget.py`, and refuses a total that does not add up. Each line goes one way:

* **Moved**: to ring 3, table below. **Replaced**: gone because a smaller mechanism does the job: the Target column holds the replacement.
* **Compiled out**: in this shape, kept for `present`, `desktop` or another architecture. Since count A counts whole files, this needs a `#[path]` or a split, not a `cfg` inside a file (§1). It includes 2,325 lines (AArch64) and 2,456 (x86-64) already under a `cfg` today, most of it `panic/catalog.rs`: they fall only when the file is split.
* **Deleted**: no profile uses it. Almost nothing qualifies: nearly every line is used by the hybrid, which stays.

K4 and K7 are NOT YET, and the tables assume §3.2's assumptions. The whole of Cut 7 is NOT YET, and is re-priced as §3.6 says.

### 4.1 AArch64 `max`, kernel crates

| Module | Today | Target | Moved | Replaced | Compiled out | Deleted | New |
|---|---:|---:|---:|---:|---:|---:|---:|
| `object` | 7,726 | 2,948 | 0 | 3,193 | 1,585 | 0 | 0 |
| `user` | 6,893 | 1,932 | 2,679 | 1,263 | 1,019 | 0 | 0 |
| `sched` | 6,276 | 3,104 | 0 | 2,886 | 636 | 0 | 350 |
| `arch` | 6,213 | 4,484 | 0 | 920 | 809 | 0 | 0 |
| `syscall` | 5,634 | 2,650 | 892 | 1,485 | 607 | 0 | 0 |
| `iommu` | 4,745 | 1,374 | 0 | 675 | 2,696 | 0 | 0 |
| `panic` | 4,226 | 599 | 1,959 | 0 | 1,668 | 0 | 0 |
| `discovery` | 3,187 | 1,091 | 1,846 | 148 | 102 | 0 | 0 |
| `mm` | 2,571 | 1,331 | 0 | 865 | 375 | 0 | 0 |
| `device` | 2,298 | 800 | 1,198 | 300 | 0 | 0 | 0 |
| `main` | 1,596 | 700 | 0 | 403 | 493 | 0 | 0 |
| `console` | 1,404 | 570 | 209 | 625 | 0 | 0 | 0 |
| `smp` | 1,177 | 730 | 0 | 447 | 0 | 0 | 0 |
| `audit` | 625 | 300 | 0 | 325 | 0 | 0 | 0 |
| `trap` | 598 | 350 | 0 | 248 | 0 | 0 | 0 |
| `boot_authority` | 512 | 250 | 0 | 262 | 0 | 0 | 0 |
| `linuxd_start` | 483 | 100 | 383 | 0 | 0 | 0 | 0 |
| `vmap` | 400 | 200 | 0 | 200 | 0 | 0 | 0 |
| 19 small files: first_rows, build_flags, irq, fallible, early, kernel_core, claim, timer, signal_frame, power, platform, random, backtrace, fastpath, mmio, kernel_item, hooks, checks, sync | 2,028 | 1,462 | 0 | 308 | 150 | 108 | 0 |
| **Total** | **58,592** | **24,975** | 9,166 | 14,553 | 10,140 | 108 | 350 |

### 4.2 x86-64 `fast`, kernel crates

| Module | Today | Target | Moved | Replaced | Compiled out | Deleted | New |
|---|---:|---:|---:|---:|---:|---:|---:|
| `arch` | 11,779 | 7,680 | 1,020 | 1,486 | 1,593 | 0 | 0 |
| `object` | 7,726 | 2,968 | 0 | 3,181 | 1,577 | 0 | 0 |
| `user` | 6,893 | 1,932 | 2,679 | 1,263 | 1,019 | 0 | 0 |
| `sched` | 6,276 | 4,364 | 0 | 1,836 | 226 | 0 | 150 |
| `syscall` | 5,634 | 2,850 | 822 | 1,395 | 567 | 0 | 0 |
| `iommu` | 4,745 | 3,258 | 0 | 605 | 882 | 0 | 0 |
| `panic` | 4,226 | 599 | 1,959 | 0 | 1,668 | 0 | 0 |
| `discovery` | 3,187 | 892 | 2,026 | 148 | 121 | 0 | 0 |
| `mm` | 2,571 | 1,706 | 0 | 865 | 0 | 0 | 0 |
| `device` | 2,298 | 800 | 1,198 | 300 | 0 | 0 | 0 |
| `main` | 1,596 | 700 | 0 | 403 | 493 | 0 | 0 |
| `console` | 1,404 | 570 | 209 | 625 | 0 | 0 | 0 |
| `smp` | 1,177 | 730 | 0 | 447 | 0 | 0 | 0 |
| `audit` | 625 | 300 | 0 | 325 | 0 | 0 | 0 |
| `trap` | 598 | 350 | 0 | 248 | 0 | 0 | 0 |
| `boot_authority` | 512 | 250 | 0 | 262 | 0 | 0 | 0 |
| `linuxd_start` | 483 | 100 | 383 | 0 | 0 | 0 | 0 |
| `vmap` | 400 | 200 | 0 | 200 | 0 | 0 | 0 |
| 19 small files: first_rows, build_flags, irq, fallible, early, kernel_core, claim, timer, signal_frame, power, platform, random, backtrace, fastpath, mmio, kernel_item, hooks, checks, sync | 2,028 | 1,462 | 0 | 308 | 150 | 108 | 0 |
| **Total** | **64,158** | **31,711** | 10,296 | 13,897 | 8,296 | 108 | 150 |

AArch64 `fast` is the first table plus `edf`'s scheduler and the two fast paths: about 26,500.

**Where the moved lines go, and who gains an authority.** devmgr gains a bus handle, `partmgr` the start of two more processes, `lode` a ring and perhaps a grant. Nothing else does.

| From | Lines | To | Under what authority | Is it in the certified item? |
|---|---:|---|---|---|
| `user/space.rs`'s regions, fork, remap, file, advise | 2,679 | **linuxd's existing by-position copy.** Native layout: `rt`, process builder | Its own Memory and space handles | **linuxd: no, and the claim went with it at R3**: a smaller claim, not a free gain, §6, D11. **`rt` and the builder: yes**, ledger 886's set |
| `device.rs`, part of `discovery/pci.rs`, `discovery/tree.rs`'s bus half, native handlers | about 2,800, and `ferrix-pci`'s 1,200 | devmgr | A bus handle: mediated read, ask for a node; the kernel checks | No. NOT YET, §3.6 |
| `discovery/devmgr.rs`, `linuxd_start.rs` | 1,493 | `partmgr` | Its boot table | **Yes**: about 800 lines into a certified item |
| `panic/catalog.rs`'s prose | 1,959, with 1,668 already under a `cfg` | A host tool, by code | None | **Data, no logic. It flatters this table more than anything in it and takes no risk out** |
| `mce/decode.rs`, CMCI's policy, x86-64 | 1,020 | `lode` | `CPU_SERVICE`'s ring, read-only | No claim on x86-64 at all |
| `console/pstore.rs` | 209 | `lode`, if pstore lives on | A Memory grant over the region | To read: not checked that lode can |

### 4.3 Libraries

Linked whole files, today, measured; "without checks" is x86-64's image with the check functions out of the call graph, measured. Targets are estimates.

| Crate | x86-64 | AArch64 | Without checks | Target, AArch64 `max` | Target, x86-64 `fast` | Step | How |
|---|---:|---:|---:|---:|---:|---|---|
| `fs/common/vfs` | 6,044 | 6,044 | 0 | 0 | 0 | Cut 1 | Compiled out: `Errno` from the status crate; a check that needs it moves or goes |
| `proto/linux-abi` | 5,387 | 5,387 | 2,705 | 120 | 120 | Cut 1 | Split the crate: ring 0 wants statuses |
| `platform/pci` | 2,700 | 2,700 | 2,700 | 1,500 | 1,500 | Cut 7 | §3.6: the review's estimate. Traversal to devmgr |
| `platform/acpi` | 2,582 | 2,582 | 2,436 | 1,000 | 1,400 | Cut 7 | Tables a kernel needs, MCFG's reader kept. With a static board, 0 |
| `platform/fdt` | 2,571 | 2,571 | 2,571 | 1,500 | 0 | Cuts 1, 7 | Out of x86-64; why `iommu.rs` and `device.rs` reach it there is to be read. With a static board, 0 |
| `proto/bootinfo` | 2,568 | 2,568 | 2,568 | 700 | 700 | Cut 1 | `pstore.rs` 762 and `kaslr.rs` 350 with their users; layouts per width |
| `proto/native-abi` | 2,238 | 2,238 | 2,063 | 1,100 | 1,100 | Cuts 2, 3, 4, 7 | Fewer calls |
| `kernel/paging` | 2,176 | 2,176 | 1,887 | 1,300 | 1,300 | Cut 1 | Formats of one architecture |
| `proto/blkring` | 2,038 | 146 | 146 | 0 | 0 | Cut 1 | Compiled out |
| `kernel/sched`, `sched-edf` | 1,603 | 1,603 | 1,603 | 300 | 2,000 | Cut 5 | `windows` is its own crate; `edf` adds `sched-edf`'s 866 |
| `drivers/virtio` | 1,504 | 1,504 | 594 | 0 | 0 | Cut 1 | Compiled out |
| `platform/elf` | 1,087 | 1,087 | 604 | 350 | 350 | Cut 8 | `partmgr`'s image alone |
| `proto/devmgr-proto` | 1,028 | 1,028 | 565 | 0 | 0 | Cut 7 | devmgr |
| `kernel/sync` | 857 | 857 | 857 | 600 | 600 | Cut 6 | |
| `fs/common/cgroupfs` | 833 | 833 | 348 | 0 | 0 | Cut 1 | |
| `kernel/vma` | 775 | 775 | 775 | 0 | 0 | Cut 3 | No regions |
| `kernel/vdso` | 654 | 654 | 654 | 0 | 0 | Cut 1 | |
| `platform/smbios` | 553 | 553 | 553 | 0 | 0 | Cut 1 | The loader hands over type 16's answer: L.mm.106 moves (ledger 5942 M.1) |
| `kernel/frame` | 538 | 538 | 538 | 500 | 500 | Cut 6 | |
| `fs/common/cpio` | 524 | 524 | 524 | 300 | 300 | Cut 8 | |
| `kernel/heap`, `objects`, `kmem`, `fallible` | 1,617 | 1,617 | 1,395 | 1,150 | 1,150 | Cuts 4, 6 | |
| `proto/partition-desc`, `audit`, `description` | 659 | 659 | 659 | 472 | 472 | Cuts 6, 8 | |
| `crypto/sha256`, `kernel/crng` | 395 | 395 | 395 | 395 | 395 | | K14's digest, entropy |
| **Total** | **40,931** | **39,039** | **27,140** | **11,287** | **11,887** | | |

### 4.4 The totals, the split, and the last 10k

| | x86-64 `fast` | AArch64 `fast` | AArch64 `max` |
|---|---:|---:|---:|
| Today, count A + checks | 105,089 + 17,726 | 97,631 + 16,334 | 97,631 + 16,334 |
| **Budget, count A + checks** | **43,598** + about 8,000 | **about 39,400** + 7,500 | **36,262** + about 7,000 |
| Budget, count B, kernel crates | 31,711 | about 26,500 | 24,975 |
| With `firmware=static` (D2): -3,384, FDT, ACPI, `tree.rs`, ITS, `discovery`'s rest | not applicable | not applicable | **32,878** |
| Count C, a conservative formula, 0.30 × crates + 0.20 × libraries | | | 9,000 to 11,000. It gives 27,434 for today against 21,991 measured, so it is not a measured ratio |

**So the honest figure is about 33k to 36k, not 20k.**

**How much of the fall is real.** A falls by 61,369 on AArch64 `max`. Of that, on x86-64's reach figures: library files no product code reaches, 13,791; panic prose, 3,627; already under a `cfg`, 2,325. **So about 19,700, between a quarter and a third of the fall, removes no executed product code.** Only 4,389 library lines carry product code today. The kernel's budget fall is the part that changes what runs and what must be argued. Cut 1's "best value" is by count.

**What the last 10k would cost**, from 33k, AArch64 `max`, each by estimate:

| Cut | Lines, A | What it costs |
|---|---:|---|
| One processor per kernel image (`smp=one`). seL4's verified configurations are this | -2,000 | No SMP inside `max`: `smp.rs`, shootdown, wake placement, locks. Simpler, and `linuxd` on one core. Reverses nothing ruled, but a product decision |
| Static devices only: no run-time bus, no devmgr, no dynamic node, no driver restart | -3,350 | `max` has a fixed device list, and the kept PCI parser (§3.6) goes. DEVMGR's T0 restart rows leave `max` |
| `kernel_memory=none`, no allocator after boot | -900 | `max_plus`'s, not `max`'s by the customer's order; a rewrite of every `Vec` |
| Native ABI under 40 calls: ports to notification words | -1,000 | Every driver's event loop rewritten; no row reason |
| **No restricted mode in `max`: no Linux beside a safety partition** | -2,000 | §2's whole plan. A Linux partition would need a hypervisor or a second machine. I do not recommend it; it is listed because it is the honest size |
| **All of those** | **-9,250** | **23,628**, and `max` is seL4's shape with audit and windows |

I cannot name the last 3,600. IOMMU out (1,400) breaks G4's translation claim; audit out (500) breaks FAU. **20k by count A, for this kernel's job, is not a cost I can quote: it means not doing some of the job.** By count B, the static board, one processor and static devices alone reach about 20,200 (24,975, less 884, 2,000 and 1,850), since libraries do not count there: that is D3.

## 5. The price: section 7's rows

**Baseline**, all x86-64 under KVM on nazuna, by hand and not gated. `exit`: `OPEN-QUESTIONS.md` §1, "near", 2026-11-05, `fs=server kernel_memory=retyped fault_ahead=off smt=off`, flag on. `partition`:
`PERFORMANCE.md`, 2026-11-04, an older tree, `fs=lift`. **Nothing here was re-measured, and no step has been built.** AArch64 has no row at all: `OPEN-QUESTIONS.md` §1.

**Every limit row below misses today, in both columns. So "no row newly misses" would be empty, and the claim is narrower: no row rises, by estimate, for what is priced.**

| Row | `partition` | `exit` | Limit | Crossings | What Cuts 0 to 8 do | Estimate |
|---|---:|---:|---|---:|---|---|
| `null-getppid` | 570 | 670 to 690; 1,480 with `max`'s `restricted_fastpath=off` | `max`: 660. `fast`: 1.10 × frozen 357 = 393 | 1 | Not on its path: entry, K1, linuxd | No rise |
| `null-general` | | 1,010; 1,880 flag off | Beside | 1 | linuxd's general path | No rise |
| Channel, `domain-call` p50 | **No figure in the user shape**: `PERFORMANCE.md` says it does not finish there | | Under 400. Hybrid about 820 (§7, 2026-10-07) | 0 | Cuts 2 to 4 change exactly this path | Not priced until it runs |
| `mmap-munmap-4K` | 1,870 | 6,090 | Beside | 2 | `place`, `unmap` smaller | No rise |
| `mmap-touch-munmap-4K` (the fault row) | 4,220 | 13,310 | 1.10 × 1,880 | 3 | **The fault already crosses**: this row is Cut 3's Linux path today. Its first touch of the arena is the region's half | No rise if §3.3's candidate holds. **Not priced until TK.9 returns** |
| `signal-self` | 3,810 | 10,280 | 1.10 × 2,070 | 2 | Not on its path | No rise |
| `futex-thread-pingpong` | 6,411 | 22,470 | 1.10 × 2,210 | 4 | Address wait's kernel half smaller | No rise |
| `fork-exit-wait` | 166 us | 201.7 us | Beside | 25 | No `SPACE_CLONE` used today either; job paths change at Cut 4 | Not priced |
| `fork-exec-wait` | 335 us | 461.5 us | 1.10 × 248 us | 54 | As above | Not priced |
| `pagecache-miss-4K` | 90 us | 96.7 us | 1.10 × 74 us | | Not on its path | No rise; noisy |
| Warm `rustc hello.rs` | 60 to 64 ms | | 1.10 × 48 ms | | 70% of its 20,000 faults are fresh memory on a cold arena: the region's half | **Not priced until TK.9** |
| Block read 4K | not measured, no `seam` line | | Under 80 us | | | Not measured |
| Cold `test-rustc`, self-host, Chrome, compositor, `test-unity` | not measured | | 1.10 | | | Not measured |
| Boot to first process | | | None | | Eager native maps, Cut 7's enumeration by call: under 5 ms by estimate | Slightly worse. No row |

**Why the reasoning holds for what it covers.** The crossing the brief asks about, about 500 ns under the barrier, is in today's figures for a Linux program's fault (`M5-LIFT.md` §3a, `PERFORMANCE.md`'s table). Ring-0 fault code Cut 3 removes does not run on a Linux program's page. **Not priced:** linuxd's own faults on its arena, native eager maps, Cut 4's job paths. Each of Cuts 3, 4, 5 and 7 lands with its rows before and after on `exit` and `partition`, by M0's method, thresholds unmoved.

**A possible gain, no claim.** After the exit's IBPB every taken branch runs cold at 3 to 4 ns (`RESTRICTED-EXIT.md`), and the native-call path has about 95 taken a call. A smaller dispatch might lose 30 to 50 of them: 100 to 200 ns per native call under `exit`. No prototype is behind this: it is why a smaller kernel might be faster here, and it is not in the table.

**What the design refuses, because it would cost.** Each is what "move VM out" would mean with no same-thread exit:

| Alternative | Extra | What misses more |
|---|---|---|
| A separate `memd` process for all of VM, Mach or seL4 CAmkES style | A channel round trip per fault, across a program barrier: about 2,000 ns plus cold misses | `mmap-touch` +15% to +50%; `fork-exec` +15% to +25%; warm `rustc`'s 20,000 faults (`PERFORMANCE.md`, before `fault_ahead`) about +40 ms on 60. Estimate |
| Native lazy faults answered by a ring-3 pager | About 2,100 ns a native fault | No row, but a safety partition's timing |
| Scheduler policy in ring 3 | A round trip per block and wake | `futex` doubles. Rejected |
| Fair class out, ring-3 balancing | | Throughput on `fast`, unmeasured |
| `SPACE_PLACE` per page with no batch | A call a page | Today's batch stays |

**Speed work is elsewhere.** The levers for `max`'s row are `OPEN-QUESTIONS.md` §1's: the unnamed 30 ns and the window (W2-W12). This design neither helps nor hinders them, except that `restricted.rs` stays as it is.

## 6. Certification

**What a smaller ring 0 buys**, in `MICROKERNEL.md` §8's own *guessed* streams:

| Stream | §8's guess | Effect of about half the code, estimate |
|---|---:|---|
| F2, code-level proofs | 300 to 700 points | Scales with code that must be refined: perhaps 150 to 350 saved. A guess halved linearly over a guess: against this design's own cost, §7, it may pay for itself, and I cannot show more |
| F1, the isolation model | 150 to 300 | No VM policy, no quota tree, one fault rule: simpler, not half |
| V1, MC/DC | 60 to 120 | Lines with code from 22k to about 10k; the tool's cost does not fall |
| Stream T1, timing | 60 to 100 | About 75 calls to bound, not 135. No range or fault resolution in a safety partition |
| V2, object code | 30 to 80 | By bytes: still about 8 times seL4 (§1) |
| FMEA, `unsafe` trace, requirements | in M7 | 3,369 named functions and 1,501 low-level ids today, about half, for this shape |

**What it does not buy, plainly.**

* **The TSF shrinks because the claim shrinks.** Separation between Linux processes is linuxd's (R3, ledger 6012: "separation between Linux processes rests on the shadow in linuxd, outside the TSF"). **Ledger 5714 (18) and `M5-LIFT.md` §3a say no M5 flip until the kernel's charging and visible separation are restored, or the ST, ASR-1 and §2.3 are rewritten and F-35, F-37 and F-40 re-homed. W8 and R3 say `place` goes if K3 comes back. Cuts 2 and 3 make `place` stay and K3 go, for `fast` and `max`: they take the rewrite arm. That is D11, by name**, not a detail. A smaller kernel that promises less is a fair bargain, but it is a bargain.
* **Moved into the certified set**: `partmgr` about 800 lines, `rt` and the process builder take native layout (both in ledger 886's set), with their MC/DC and FMEA. A fair "certified lines" figure adds about 1,000 to 2,000 (estimate) to the target. `btrfsd` is untouched.
* **Panic prose is data** (§4.2). Taking it out changes the count by about 3,600 and the risk by nothing.
* **Compiled-out lines still exist** for `desktop` and `present`; their evidence is theirs. Only `fast` and `max` benefit.
* **The two-file rule (§3.1) means a defect in shared reasoning may be fixed twice**, a standing review cost.
* **`core` and `alloc`** are half the text. Ferrocene covers `core`, not `alloc`, as `MICROKERNEL.md` §6.3 notes of `rt`. Under K16 ring 0 still uses `alloc` everywhere. This design does not address it.
* **A favourable shape is not a certifiable kernel.** T3, in the review's words, "is a favourable shape for sc4, not a certifiable kernel by this line": sc4 stays refused, the review is internal (F-27), and the rewrite's evidence is all owed.

**What it costs in redone evidence.**

* **Requirements.** `Verifies:` tags in `src/kernel/src` name 1,034 ids (by grep today): `object` 309, `syscall` 174, `arch` 168, `user` 128, `mm` 56, `device` 46, `iommu` 42, `sched` 41. Estimate: 450 to 600 of those stop
  applying to this shape, scoped *under `personality=kernel`* by `BUILD-FLAGS.md` rule 7, never reused; 250 to 350 new ones.
* **Checks.** 16,300 to 17,700 lines in ring 0 today; about half go with their code and new ones are written: 3,000 to 4,000 lines (estimate). New ids, new controls FIRED.
* **Coverage.** Nothing carries: a new file is new. M7 owes the whole item afresh, **so doing this before M7's evidence wastes no evidence**; doing it after would discard all of it. F2 "should start only after M6, on the code that stays": this is that.
* **Documents.** `ITEM.md`, ST (FDP_IFC, FRU_RSA.1, FAU_GEN), SAFETY-MANUAL (AoU-5, 12, 21-24, 36, 38, 41, 43, 45, 47), VULNERABILITY-ANALYSIS (V-20, V-03), MEMORY-AND-TIMING, `MICROKERNEL.md` §2.1, §2.2, §2.3, §3, §5, §6.4 ("The kernel does not shrink much"), `M6.md`'s "Not cut", `BOOT-AUTHORITY.md` §4.

**Rulings it touches**, by ledger line, corrected by the review. None is overturned by this document; each needs its own review.

| Ledger | Ruling | Effect |
|---|---|---|
| 6411 W1-W8 | SPACE_PLACE's safety argument, for restricted spaces | **Restated by space class.** W2's sticky PLACED and W3's held object become universal. **W6 is reversed**: it refused measured spaces, the region's own, and protect, read, write, clone. W7 stands. W8's "goes if K3 comes back" is reversed, D11 |
| 6421 U1-U9, 6462, 6537 C1-C3 | Place's unmap and shootdown | Universal too. (6416 is withdrawn, 6420) |
| 6012 R3, 5714 (18) | The claim moves to linuxd; restore or rewrite before the flip | **The rewrite arm is taken: D11.** Extended to native layout |
| 818 (B6), 5745 K8.1, K8.4-K8.8 | No ECAM or IOMMU in ring 3; a filtered write | **Stand**, rewritten and not dropped. K8's closing sentence and `ITEM.md`'s stay until Cut 7's design. K8.3 does not rest on devmgr |
| 6343, 5896 BA.3, AoU-45 | The kernel's filter; a parent-is-a-root-port read | Stand, §3.6 |
| 7166 | `would_translate`, `flag_nodes` | At mint |
| 3906 | G4, translation alone until ITS rows | Stands; why IOMMU cannot go |
| 886 | The certified set; devmgr not in it; **`partmgr` supervises through K4** | The set stands, `rt` and the builder taking more. **The K4 wording is D8's** |
| 2858, 5856, 5968, 2538, 2773 | K3 (1)-(9), K13, K10 (design 2538, code 2773) | Scoped to `personality=kernel` by Cut 2, under §3.2's rule. K3's "one fault-forwarding mechanism" with `NVIDIA.md` §11.4 is `desktop`'s |
| 2525, 6510 | K7 (design, landing) | **NOT YET**, §3.2 |
| 2871 | K4, `SUPERVISE`, resume checked as sigreturn's | **NOT YET**, §3.2, D8 |
| 2845, K14.6 (5745) | K2's clone, the F-41 lesson | `SPACE_CLONE` out; the lesson's check is in linuxd, and by R6 no requirement gains from that run |
| 2550, 2461 M3, 5942 K16.1-K16.11 | K12, K16 | Stand and apply to ranges: §3.3. Slot chunks go; K16.9 gets simpler |
| 5942 M.1, 5057 (2), 5140 (1) | SMBIOS type 16, L.mm.106, AoU-38 | The loader answers, or `max`'s board states ECC: scrubber compiled out, a new AoU |
| 5312-5326, 5449 | Machine-check retirement, H.FAIL.11, AoU-36 | W7's kill stands; H.FAIL.11 scoped; **AArch64's path owed**, §3.3 |
| 5896 BA.1-BA.13, 6316 P1.3 | First processes, L.object.334's exception | §3.9 lists what is owed |
| 5942 M.1-M.10, 6933 M6.1-M6.6 | M6's list and gate | Extended: Cut 0 is M6 §3.4, unbuilt, by M.7's rule: release, checks apart, a ledger line on growth. M.2 and M.9 re-read |
| 6080, 6296 | K17 | Stands; clause (d)'s pager limb is vacuous here |
| 6238 N.12 | Console input in ring 0 | Stands, 120 lines kept |
| 7011 S.1-S.8, C3, C6 | `windows` | Stands: §3.7 only budgets it |
| 941, 6747-6755, 6787, 6826 | IBPB, the window | **Untouched** |

## 7. Steps

Each lands dark behind its flag or under `personality=user` alone, with `test-kernel-symbols` showing `present`, `desktop` and `personality=none` unchanged, its own consultant review, and Cut 0's ratchet moved
down in the same commit. Sizes are estimates, AArch64 `max` / x86-64 `fast`. Points are *guesses*, §8's unit.

| Step | What | Crates | Libraries | Count A after | Points | Stands alone? | Review |
|---|---|---:|---:|---:|---:|---|---|
| **Cut 0** | The ratchet, below. No kernel line | 0 | 0 | 97,631 / 105,089 | 3 | **Yes**: holds the rest, whoever stops where | **May be built** |
| **Cut 1** | What needs only a `cfg` or a split: IOMMU units by architecture (**the first kernel commit: 1,934 lines on AArch64**), `msrlog`, `inspect`, `framebuffer`, SMBIOS, `oom`, signal frames, vDSO, board glue; no-client crates, errno split | -7,027 / -5,591, of which about 2,400 already under a `cfg` | -17,745 / -22,208 | 72,859 / 77,290 | 10 | **Yes**, by count the most | No objection; each its own |
| **Cut 2** | K2's clone, K3, K10, K13 and their handlers, `claim`, `hooks`, and by D8 K4 and K7 | -3,064 / -3,016 | -400 | 69,395 / 73,874 | 8 | **Yes**; the check lines (about 3,000) go too | OK IF TK.2, TK.5. K4, K7 NOT YET |
| **Cut 3** | `kernel_vm=placed`: placed spaces, Memory, demand ranges, retirement path, `rt` and process builder map first. **The mechanism** | -6,606 / -6,700 | -975 | 61,814 / 66,199 | 30, low | **Yes**: no fault resolution in a safety partition is worth having at any count | OK IF TK.7, TK.8, TK.10. Region's half NOT YET |
| **Cut 4** | Jobs and quotas by reservation; `Host` and `Bare` become one process. Needs `retyped` `built`: R.1-R.5 | -3,271 / -3,219 | -526 | 58,017 / 62,454 | 15 | Yes | Its own |
| **Cut 5** | `windows`, `edf`: FLIP's own work, this only budgets it | -3,014 / -1,754 | -1,303 / +397 | 53,700 / 61,097 | FLIP's | Yes, it is owed anyway | 7011 |
| **Cut 6** | Prose out (D4), `arch/` trims of §3.8, console, audit, smp, main's hand-off | -4,782 / -6,274 | -542 | 48,376 / 54,281 | 15 | Yes; **the softest, and partly data** | Its own |
| **Cut 7** | `device_bus=devmgr`: traversal and matching leave. D7 | -4,611 / -4,651 | -5,219 / -3,748 | 38,546 / 45,882 | 25 | Yes, if its design holds | **NOT YET**, TK.11 |
| **Cut 8** | `first_process=one`. D6 | -1,242 | -1,042 | **36,262 / 43,598** | 10 | Small; do last | OK IF TK.12 |
| | **Total** | **-33,617 / -32,447** | | | **about 115, my guess** | M6's guess was 70, "re-sized after K16" | |

**The points are probably too low.** 250 to 350 new ids with checks and controls and 3,000 to 4,000 lines of checks are in that 115. K7 was 12 points for 7 ids: at that rate the ids alone are 400 to 600. I cannot say which is right; plan on several hundred, and Cut 3's 30 does not look it.

**Where it sits: M6, re-sized.** Cuts 0 to 2 now, beside FLIP: they change nothing a row sees, though K4's and K7's parts wait. FLIP's three words: `fast` and `max` are refused until each value is `built`, and four new values join those refusals when they enter a profile. M5's flip comes with M7's evidence, so Cut 3 onward must enter the profiles **before M7's evidence is written**, and that will delay M7. Build each dark under `--set` meanwhile, since FLIP's B.3 rows should not chase a moving kernel. BACKLOG owes the window's W2 to W8 at the flip, so W7 gets built once before Cut 3 and again in it. If the customer stops after Cut 2: AArch64 count A 69k, count B 48.5k, a usable result. After Cut 4: 58k and 38.6k, with the mechanism. Cut 7 is most of what is left and the least sure.

**Cut 0, built (2026-11-05).** `cargo xtask test-ring0-budget`, a row of the image row's. It runs `test-kernel-shape --release` once for each build `tools/common/data/ring0-budget.json` lists, three flag sets on x86-64 and AArch64, each a child process so its flags resolve afresh, then `check-kernel-shape.py --budget` on that image. **It holds today's measured figures and says nothing about whether 36k is reached.**

* **What it counts.** Count B: every file rustc's dep-info lists for the three ring-0 crates, less check files, whole files, generated ones included. Count A: B plus every library file the **release** image's `.debug_line` names, whole file (M.7). Checks, count C on x86-64 (AArch64: "not computed", the method does not see `adrp`) and `.text` are printed and never held. `readelf -W` is needed: without it a unit's own file is printed by its base name and drops out, which also undercounted §1's opt-level 1 line tables a little.
* **What fails.** A rise of A, B or the library part. A fall past 1% with no edit of the baseline. Against the file's last different version in git, without a new `rise.ledger`: a baseline that rose, a check file added, a set, architecture or baseline entry removed, or a set or profile changed (ledger 7448 CZ.3). A sets list that names no build, or does not match the baseline. A file read as a check that the reviewed `check_files` list does not name: the tool's own rule is a file name. A listed check file that is gone. An image with no `.debug_line`. A kernel file in the line table that rustc's list lacks ("the dep-info is short"). A `.rs` rustc read that is not there.
* **Its baseline, measured on develop b50e88907 with Cut 0 on it, release**, in the file's one version (7448 CZ.1, CZ.2). First measured on 3ff0b533b; the window (98e1d8df1), the ordinary path (610f8d2e2) and b50e88907 then landed under it, and the row read their growth at once, **+753 kernel lines on x86-64 and +337 on AArch64** in every set, and one new check file, `arch/fast_check.rs`, which the reviewed list now names:

| Set | x86-64 A | x86-64 B | AArch64 A | AArch64 B | Checks, x86-64 / AArch64 | Count C, x86-64 |
|---|---:|---:|---:|---:|---:|---:|
| `--personality user` | 92,047 | 64,918 | 86,170 | 58,936 | 18,120 / 16,728 | 13,090 |
| `fast`'s values | 92,846 | 65,230 | 86,760 | 59,039 | 18,392 / 16,977 | 13,475 |
| `max`'s values, less `windows` | 92,086 | 64,987 | **86,209** | 59,005 | 18,181 / 16,789 | 13,134 |

  **Read against §1 and §4.** The release image links about 27,200 lines of library files where opt-level 1 linked about 39,000: LTO drops what only the checks reached. So AArch64 `max`'s count A reads **86,209** by the ratchet against §1's 97,631 (an older tree, opt-level 1), and count C 13,134 against 21,991. **The 36k target is read at opt-level 1** (7383 TR.21 (d), 7448 CZ.5): the release figures are not that reading, and "target met" on them is refused. The budget of §4 was priced on the opt-level 1 figures; its library target of 11,287 is against 39,039 linked, and was not re-priced here. **The 36k target is therefore at least as easy by the ratchet's reading as when it was accepted, by about the 12,000 library lines LTO already drops; I report that rather than count it as progress.**
* **Seconds.** Through gate.sh on the landing hash (`mkt0c-budget`): **447 s** for the six builds, x86-64's three 100 to 118 s each because develop's `check-linuxd-binary` step builds linuxd in each, AArch64's 39 to 40 s. By hand earlier, before that step: 230 s warm. **It adds nothing to `cargo xtask check`**: it is not a step of it. Put in `check`, it would add about 450 s, against the customer's rule that test time comes first; hence a row, run by `gate-rows` for anything the image contains.
* **Controls, each `gate.sh control <hash> <tag> <file> <old> <new> --expect-host <text> -- test-ring0-budget`.** Seven on 3ff0b533b, carried to the landing hash by 7448 CZ.4's test (the tool's diff since touches only `budget_problems` and `budget_sets`, and the budget call is unchanged); three on **020c78f96**, the landing hash:

| Tag | Plant | gate.sh's verdict |
|---|---|---|
| `mkt0-c-module` | a 31-line module planted in `kernel_core.rs` | control: FIRED (host): ring0-budget: user x86_64 release: count A rose to 91,325 against 91,294 (+31) |
| `mkt0-c-library` | `main.rs` takes the address of `ferrix_paging::mtrr::range_type`, a library file not linked before | control: FIRED (host): ring0-budget: user x86_64 release: library files rose to 27,224 against 27,129 (+95) |
| `mkt0-c-max-alone` | `include_str!("sched/reserve.rs")` under `cfg(ferrix_kernel_memory = "retyped")`, so under `max`'s values alone | control: FIRED (host): ring0-budget: max aarch64 release: count B rose to 58,773 against 58,668 (+105) |
| `mkt0-c-aarch64-alone` | `include_str!("arch/x86_64/framebuffer.rs")` under `cfg(target_arch = "aarch64")` | control: FIRED (host): ring0-budget: user aarch64 release: count B rose to 58,697 against 58,599 (+98) |
| `mkt0-c-no-line-table` | `debug = false` in `[profile.release]` | control: FIRED (host): ring0-budget: user x86_64 release: no .debug_line in /home/sebastian/.local/share/ferrix/target-gate-slot-2/x86_64-unknown-none/release/ferrix-kernel: the budget fails closed |
| `mkt0-c-short-dep-info` | the tool's dep-info list drops `/object/` | control: FIRED (host): ring0-budget: user x86_64 release: the dep-info is short: the image's line table names src/kernel/src/object/bare.rs, rustc's list does not |
| `mkt0-c-relabel` | `include_str!("arch/stage_check.rs")`: a file whose path holds `_check` read anew. gate.sh cannot rename a file, so this stands for a product file renamed: the same reviewed list catches both (TK.4 (b), not met to the letter) | control: FIRED (host): ring0-budget: user x86_64 release: arch/stage_check.rs: read as a check, and tools/common/data/ring0-budget.json's list of check files does not name it |
| `mkt0c-c-fall` | the user x86-64 baseline's A raised past 1% | control: FIRED (host): ring0-budget: user x86_64 release: count A fell to 92,047 against 93,800 (-1.9%), past 1% with no edit of tools/common/data/ring0-budget.json: lower the baseline |
| `mkt0c-c-rise-no-ledger` | `max` AArch64's baseline A raised by one, `rise.ledger` unchanged | control: FIRED (host): ring0-budget: user x86_64 release: tools/common/data/ring0-budget.json: the baseline rose (max aarch64 a 86209 to 86210) and names no new ledger line: a rise is an explicit edit naming one (M.7) |
| `mkt0c-c-check-files-gain` | `check_files` gains `arch/stage_check.rs`, `rise.ledger` unchanged (CZ.3) | control: FIRED (host): ring0-budget: user x86_64 release: tools/common/data/ring0-budget.json: the baseline rose (check_files gained arch/stage_check.rs) and names no new ledger line: a rise is an explicit edit naming one (M.7) |

  The 3ff0b533b runs of `fall` and `rise-no-ledger` (`mkt0-c-fall`, `mkt0-c-rise-no-ledger`) also FIRED; their edits name baseline figures, so they ran again on the landing hash.

* **Rows.** On 3ff0b533b, all `gate-rows --since origin/develop` named: `mkt0-r00` `check`, `r01` and `r02` `check-stack-depth`, `r03` to `r06` `check-frames`, `r07` `build --arch all --release`, `r08` `test-boot` x86-64 under TCG, `r09` x86-64 under KVM, `r10` AArch64 (TCG), `r11` and `r12` ARMv7-A (TCG) at four and two processors, `r13` `test-ring0-budget`: **PASSED**, each. The rebase onto b50e88907 conflicted, so on **020c78f96** again (CZ.4): `mkt0c-check` run: PASSED; `mkt0c-shape`, `test-kernel-shape --personality user`, run: PASSED; `mkt0c-budget` run: PASSED; and `test-msync`, which develop added to the image row since, `mkt0c-msync-x86` run: PASSED [accel x86_64:kvm], `mkt0c-msync-a64` run: PASSED [accel aarch64:tcg].
* **What it does not see (7448 C1-C3).** A `.rs` file in the line table from outside the tree and not in the dep-info (a registry or vendored crate in ring 0) is dropped without a word: a BACKLOG row. Nothing but review checks that `rise.ledger` increases. Product code moved into a listed check file, under 1%, goes unseen until the slack runs out.

About 300 lines of Python, one JSON, one gate row, 80 lines of xtask. No kernel code, no id, no flag. Then Cut 1's IOMMU `cfg`: one `#[cfg(target_arch)]` per `mod` line and the callers' arms: 1,934 lines on AArch64, 396 on x86-64, seen at once by the ratchet.

## 8. Decisions only the customer can make

| # | Decision | Recommendation |
|---|---|---|
| D1 | **Answered, 2026-11-05. The customer: "ok then 36kLoc seems reasonable".** The target is **36k by count A on AArch64 `max`**. 25k by count B and 33k with a static board are figures beside it, not further decisions: D2 to D11 stay open. The question as put: **what "20k" counts**: count A, ring 0's product lines, AArch64 `max` as the headline, checks in the same row, B, C and bytes beside. The target is under 36k by A, under 33k with D2, under 25k by B. **By A that is a little over a third of today; by lines with code and by bytes about half; about a third of A's fall removes no executed product code.** ±15% is optimistic: 31k to 41k | **Yes**, and 20k is not the target |
| D2 | `max` on a static board description, seL4's way: no FDT or ACPI parser in ring 0, a board is a build. -3,384. **The platform is not chosen**, so "33k" rests on this | **Yes, once the Lock-mode platform is chosen.** Ask then whether `max` needs a run-time bus at all. `fast` keeps tables |
| D3 | Is about 20,200 by count B worth a one-processor `max`, with static devices? | **Not now.** Decide after Cut 4 on real figures. Nothing here prevents `smp=one` later |
| D4 | The panic catalog's prose leaves ring 0 for a host tool (`OPEN-QUESTIONS.md` asks this too). A serial panic prints the code and the numbers. Data, not risk | **Yes** for `fast` and `max`. `desktop` keeps it |
| D5 | Does `fast` keep EEVDF, or a plain round-robin under its CBS? About -1,500 lines. A real change for Linux workloads on `fast` | **Keep for now**; a row after Cut 5 decides |
| D6 | The kernel starts `partmgr` alone, against `MICROKERNEL.md` §5's three. About -840 in ring 0, +800 in a certified item, so the gain is structure, not lines. `partmgr`'s death stops even a Linux-only `fast` machine | **Yes, last.** The first to drop if time is short |
| D7 | PCI traversal and matching leave §2.1's list, into QM devmgr, a smaller parser and every check staying in ring 0. **Not asked until Cut 7's design returns: the review refused the first form** (TK.11) | Withheld. It is worth about 8,000 to 10,000 by count A |
| D8 | K3, K10, K13 are compiled out of `fast` and `max`, though §3 lists them. **And K4, one of: whole; report and kill; or out, with §5 and ledger 886's wording rewritten to supervision by end notice and a fault meaning death.** **And K7: out only with a named replacement** (a per-client bound in `btrfsd`, or a checked rule that no shared server spans levels). No caller today means `btrfsd` and `netd` have that hole now | K3, K10, K13 **yes**. K4: **out, for a safety partition**, the consultant's view too. K7: **the bound in the server**, since the hole exists anyway. They stay in the hybrid |
| D9 | §3.1's two implementations in one tree, for good, with fixes sometimes owed twice | **Accept**: it follows from `desktop` staying hybrid and one source |
| D10 | This becomes M6: its points are probably several hundred against the old 70, §7. It delays M7. F1 and F2 start on what is left | **Yes**, if the customer wants the count: doing it after M7 would discard the evidence |
| **D11** | **Restoration is given up.** Ledger 5714 (18) offered two ways before the flip: restore the kernel's charging and visible separation of Linux processes, or rewrite ST, ASR-1 and §2.3 and re-home F-35, F-37 and F-40. Cuts 2 and 3 take the rewrite, for `fast` and `max`, and K3 does not come back. The customer's "linuxd arena yes" of 2026-11-04 took the shape, not this | **Yes**: it is the shape already running, and the larger part of what makes ring 0 small. But it is a smaller promise, and it should be said as one |

## 9. What I did not verify

* **No budget figure below "Today" is built.** No step is prototyped. Every per-file target comes from reading its function inventory, not from writing it; `windows` has no code; the placed-space `Memory` has only `place` to go on.
* **Reachability is x86-64 only.** AArch64's `adrp` references are not visible to my method, so AArch64's "without checks" is not computed; linked figures are. Its method is conservative: an address in data is a root, and `Object`'s drop keeps much alive.
* **Grep is by name** for §2's second finding: a caller by another name, or a trait, would be missed; `lode-viewer`'s `Pager` is a false friend I read past. The consultant's grep of the call numbers' names over `src` and `tools` agreed: kernel checks, host tests and `check-level-doors.py` only.
* **The brief's 64,777 against 64,158** is explained only by inference.
* **All rows are older or by hand.** `partition` figures are on `feature/mk-perf`'s tree; AArch64 has no row. Nothing is a §7 baseline.
* **Cut 0's build time** in §7 is from these hand runs, not from `xtask check`.
* **I read:** `CONVENTIONS.md`'s start; `MICROKERNEL.md` §1-§7 and §8's table, not §6.5 whole; `OPEN-QUESTIONS.md`, `M6.md`, `FLIP.md`, `K16.md`, `RESTRICTED-EXIT.md`, `PERFORMANCE.md` whole; K1's and K11's heads; K9 §2a; M5-LIFT §3; the ledger lines cited, some by first 500 characters.
  **In code:** `syscall/native.rs`'s every function by size and its head; `user/space.rs`'s and `vmo.rs`'s inventories, `place`, `resolve`'s head; `lift/core/fill.rs`; heads and inventories of `object/*`, `device.rs`, `iommu.rs`, `discovery/*`, `mm.rs`, `smp.rs`, `sched/mod.rs`, `queue.rs`. **Not read:** `sched/mod.rs`'s bodies, `iommu/*.rs`, `arch/*` beyond sizes, K2, K3, K4, K8, K12, K14, K15, K17 past their reviews' lines, `BOOT-AUTHORITY.md` past BA, the foreign windows.
* **Not checked:** that `lode` can take pstore and machine-check decode; that eager mapping fits `test-partitions`' memory; how an ECC statement reads on the Lock-mode board.
* **seL4's 18,627** is one configuration without MCS or SMP, and preprocessed C's lines are not Rust's. Rust's `?`, `match` and multi-line signatures take more lines per decision; I have not normalised, and count C is the nearest thing to it.

## 10. Review

**OK IF TK.1-TK.14 (ledger 7256), internal review, not independent verification (F-27): Cut 0 (T0) may be built; Cut 2's K4 and K7, Cut 3's region half and Cut 7's claim NOT YET.** Nothing was built or booted for it. Its conditions, 7257-7270, are in the ledger.

| Part | Verdict | Status here |
|---|---|---|
| Count A | OK IF TK.3 | Folded, §1, and each table prints checks |
| Cut 0 | OK IF TK.4 | Folded, §7 |
| Cut 1 | No objection; each landing its own review | |
| Cut 2: K2's clone, K3, K10, K13 | OK IF TK.2, TK.5 | Rule folded, §3.2; D11 |
| Cut 2: K4, K7 | NOT YET, TK.6 | Choice and replacement put in D8 |
| Cut 3: restricted, native | OK IF TK.7, TK.8, TK.10 | Corrections folded, §3.3, §3.4. **The evidence they list is owed** |
| Cut 3: region's half | NOT YET, TK.9 | **Owed before Cut 3's code**. A candidate only, §3.3 |
| Cut 7 | NOT YET, TK.11 | First form withdrawn, re-priced, §3.6. **Own design owed before D7** |
| Cut 8 | OK IF TK.12 | Listed, §3.9 |
| Rulings table | TK.1 | Corrected, §6 |
| §5 | TK.13 | Reworded |
| The flattery | TK.14 | Written: the answer, §4.4, §6, §7, D1 |

**Cut 0's diff check, 2026-11-05: OK IF CZ.1-CZ.6 (ledger 7448)**, internal review, not independent verification (F-27): one JSON version (CZ.1), the rebase onto b50e88907 with its conflict kept both ways and `arch/fast_check.rs` listed (CZ.2), a shrunk measure counted as a rise (CZ.3), the landing hash's rows and three controls (CZ.4), the target's reading in the JSON and BACKLOG (CZ.5), these records (CZ.6).

## Where it stands

2026-11-05: design on `feature/mk-tiny-design`, unlanded. No flag, id or kernel code exists.

* **D1 answered, 2026-11-05**: "ok then 36kLoc seems reasonable" (the customer, relayed by the coordinator). 36k by count A on AArch64 `max` is the target. D2 to D11 are open; D7 is held back until its design returns. Nothing past Cut 0 is started.
* **Cut 0 built and diff-checked, OK IF CZ.1-CZ.6 (ledger 7448), folded**: `cargo xtask test-ring0-budget`, §7, on 020c78f96 with this section after it. It holds the landing base's measured figures and claims nothing about the target.
