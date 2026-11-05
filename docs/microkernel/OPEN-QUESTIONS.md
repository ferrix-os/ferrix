# Four open questions, looked into

*mk-look, 2026-11-05, on nazuna, develop f3ea80a3f. The customer said "look into it". This page
recommends and decides nothing. **Nothing here is gated evidence**: every run was by hand at
`nice -n 19`, no threshold moved, no slot's file was written, the 3090 was only read by `nvidia-smi`,
and `logs/queue/vram-closed` stays. "By inference" marks what was reasoned and not measured. Scripts,
logs and raw figures: `~/.local/share/ferrix/mk-look/`.*

## 1. The `max` row

**Recommendation.** Report 680 to 690 ns as "`max`'s flags less `schedule=windows`, with a fast path
`max` does not yet allow", and 1,480 ns as what `max.toml` says today. Of the 40 ns against the by-hand
650, 30 ns is real and in develop's tree. Find those 30 ns first; the window is the only lever now in
sight that clears 660 with room, and it owes W2 to W12.

### What was measured

One tree, f3ea80a3f, bench-rows' own image and QEMU command, boots alternated on one pinned core. `f2`
is the image the by-hand 650 came from, feature/mk-ibpb. Each cell is the median of five boots, fastest
to slowest beside it, host load about 3, one boot in a few with a busier core in the raw log.

| Build, all `partition_speculation=exit` | `null-getppid` | `null-general` |
|---|---:|---:|
| `f2`, the by-hand tree | 650 (640 to 650) | 1,180 |
| develop, `restricted_fastpath=on`: the row's flags | 680 (670 to 680) | 1,010 |
| develop, flag off | 1,460 (1,450 to 1,460) | 1,840 |
| develop, flag on, plus `fs=server kernel_memory=retyped fault_ahead=off smt=off runtime_settings=fixed` ("near") | 670 (670 to 690) | 1,020 |
| The same with the flag off: `max.toml` less `schedule=windows` | 1,480 (1,460 to 1,500) | 1,880 |

Near's other rows, flag on, against the same table's develop: `mmap-munmap-4K` 6,090 (5,911),
`mmap-touch-munmap-4K` 13,310 (13,250), `signal-self` 10,280 (10,322), `futex-thread-pingpong` 22,470
(22,250), `fork-exit-wait` 201.7 us (211.5), `fork-exec-wait` 461.5 us (426.9), `pagecache-miss-4K`
96.7 us (85.3, its noisy row). These are about three times `PERFORMANCE.md`'s `partition` figures: every
crossing of the general path pays the exit's barrier and runs cold. **The 30 ns is the smallest part of
what `exit` costs.**

### The 40 ns

* **30 ns is the tree.** `f2` and develop alternated on one core: 650 against 680 on core 11, 650 against
  680 on core 10, 650 against 680 on core 7, 660 against 680 at load 7 to 14.
* **It is not the core or the clock.** Those three cores read alike, though their CPPC ranks differ (171,
  166, 186). `scaling_cur_freq` during the boots read 5,303 to 5,384 MHz on all of them. The earlier
  by-hand boots moved among cores 6 to 11, and that is not what made them lower. Cores 7, 10, 11 and 23 are
  `powersave` with `balance_performance` on this boot; the IPC skill's "cores 11 and 23 run at `performance`" does
  not hold on this boot, and no second clock mode showed here. Whether seL4's 440 was read under the same
  governor is not known.
* **About 10 ns is the row against a by-hand boot of the same build**: 690 (690 to 700) through `gate.sh`,
  680 (670 to 680) here, 690 (680 to 700) here at load 6 to 10. Every figure of this row is a multiple of
  10 ns, so that is one step. It is not explained further.

Guest counters a crossing, in the row's own boot:

| | Cycles | Instructions | Taken | Mispredicts | dTLB walks | iTLB walks |
|---|---:|---:|---:|---:|---:|---:|
| `f2` (`RESTRICTED-EXIT.md`) | 3,505 | 722 | 20 | 12.5 | 9 | 7 |
| develop | 3,675 | 736 | 28.6 | 17.4 | 11 | 8 |
| Bare assembly, barrier, frame, `TS` | 2,895 | 145 | 6 | 7 | 1 | 2 |

So 170 cycles, about 30 ns at that clock, come with 8.6 more taken branches, 5 more mispredicts and 3 more
walks, in about the same instructions. `RESTRICTED-EXIT.md`'s 3 ns a branch would make the branches 25 ns
of it, by inference.

**Tried and no gain.** The linuxd half is visibly worse than `f2`'s: `ferrix_lift_hot_answer` is a call
about 2.4 MiB away from `straight_run`, though its comment expects LTO to take it in line. Forcing it in line
(the symbol is gone), and placing it beside `straight_run`, read 680 and 690 against develop's 680 (five boots,
load 7 to 14, `logs/ab-hot.*`). **So the far call is not the 30 ns**, and this was the leading guess. The TCG
trace (`trace/dev2.*`) cannot settle the rest: its model has no ERAPS, so it takes the refill and the
barrier's branches that KVM's row does not. Next step: bisect `f2`'s tree against develop under the guest
counters, K1's two landed forms first.

### Which levers are legal

| Lever | Size | Standing |
|---|---:|---|
| Whatever the 30 ns turns out to be | 30 ns, to about 650 by hand | Legal, unnamed |
| Remaining taken branches in the two-root path | At most develop's 690 less the bare 570 (row-shaped, stamp in) | Legal. The bare figure has no kernel and no linuxd, so this is a bound on software, not a target |
| 2 MiB pages for linuxd's text | Unmeasured; 8 iTLB walks a crossing against the bare 2 | Legal, named in `RESTRICTED-EXIT.md` |
| The window (`feature/mk-ibpb`, 0f237e9be) | 420 by hand, one processor | Customer's W1 yes (ledger 6826); W2 to W12 owed; unsafe on two processors today. May not be called better than seL4 (6787 (e)) |
| `TS` off | About 60 to 70 ns, calculated | Answered for the window's stub only (6826). **Not for the two-root path**: NOT YET, 6464, 6753 |
| Deferred IBPB | | Refused for `max`, 6752 |
| `hardened`, no exit barrier | | Refused, ledger 941. nazuna does not qualify for the skip: SRSO_NO clear |
| SBPB | 250 ns tight, against IBPB's 150 | Slower here, and not enough while SRSO_NO is clear |
| PCID | | Not on this processor: `/proc/cpuinfo` has `invpcid` and no `pcid` |
| Global pages | | Excluded by §7 |

### What is reached, and what each figure rests on

No floor is claimed. These are the best readings so far.

| Figure | How | Rests on |
|---|---|---|
| 690 (690 to 700) | `gate.sh`, five quiet boots, `mkm-row-3` | `personality=user`, flag on, not `max`; no delay control; `fs=lift`, `heap`, `fair` |
| 670 to 690 | Here, five boots, near | The same flags as `max` except `schedule` and the fast path; by hand |
| 650 | By hand, `f2` | A tree not on develop, whose K1 predates mk-restricted's; flag on |
| 570 | Bare assembly, a row-shaped program | No kernel, no linuxd: nothing a real call does |
| 500 | Bare assembly, barrier, frame, `TS` | As above, and no program |
| 420 | By hand, window | One processor; prototype; no root write, no linuxd instruction; IBPB and `TS` kept |

All are x86-64 under KVM, one vCPU, qemu64 with named bits, which leaves `ibpb_brtype`, `sbpb`, `srso_no`
and `btc_no` at 0 in the guest (the row's own `LB cpuid` line). §7 allows that: the kernel treats a missing bit as
affected. The hardware's own barrier runs whatever the guest is told.

### What `max`'s profile adds

`--kernel-profile max` is refused today: "not yet buildable", naming each flag's `lands`. Flag by flag:

* **`restricted_fastpath=off`: about 800 ns.** It is the whole of `max`'s gap. `values.on` allows `sc2` and
  `none` until K1's review. K1 §7.1 says `on` follows its coverage run, vulnerability-analysis entry and code
  review; BACKLOG's Z2 to Z4 and 6093 (1)-(12) stand before that. So **a `max` that passes needs the
  flag's level raised, and that is the review's and not a figure's**.
* **`fs=server`, `kernel_memory=retyped`, `fault_ahead=off`, `smt=off`, `runtime_settings=fixed`:** nothing
  this method can see on `getppid`. `smt=off` parks nothing under one vCPU, so its effect on this row is not
  tested, by inference. `fork-exec-wait` read 8% slower, and `fs=server` is the likely reason, by inference.
* **`schedule=windows`: unknown.** `built = "no"`: K9 is not written, so no real `max` can boot, whatever
  else lands. A tick and a window switch are outside a p50 of a tight loop, by inference; the row may not
  move, the code must still exist.

### AArch64

**Not obtainable today, and no limit exists to compare it with.** `max.toml`'s `claim_architectures` is
`["aarch64"]` alone, and §7's 660 is "on nazuna".

1. `K1.md` says it: "No AArch64 row, §7 figure or isolation claim is made" (ledger 6198 A1).
2. The fast path is x86-64 only: `restricted_fastpath` appears nowhere under `arch/aarch64/`, and 6291
   keeps AArch64's out. An Arm figure today would be the general path.
3. `bench-rows --arch aarch64` does not build on nazuna: `linux-rows-test` links through the host's `cc`
   and stops at `crt1.o: file in wrong form`, so no Arm row ran here.
4. TCG has no timing, and QEMU flushes on an ASID change, so nothing it reports is a cost. A figure needs a
   real core.

**What a real figure needs.** For a development figure: `phone-boot`'s path carrying the rows on the Pixel
7's A78 cores (the PO's word for the phone), pinned, with the BHB loop on exit, and seL4 on the same core
under the same crosvm for a ratio; I found no AArch64 seL4 baseline. Then a limit from the customer: 1.5x
of that, or another. For evidence: `MICROKERNEL.md`'s Lock-mode A78AE platform, which it says is
not funded yet. Whether two ASIDs are two hardware contexts when one is reused is K1 §8.1's open question, and it
decides what the exit's barrier must be.

### What the x86-64 row needs before it is `max`'s

K9's `schedule=windows`; the fast path's review and its flag's level; `lands` satisfied so the profile
builds; then `bench-rows --kernel-profile max --fail-above null-getppid=660`, five quiet boots through
`gate.sh`, a delay control that fires, `null-general` printed beside it (1,010 to 1,020 now), and the row's
text from 6755. None of it moves the 660.

**The customer decides:** whether the window is the road (W2 to W12 is real work, and a figure that is not
like for like with seL4), or the two-root path is worked for its 30 ns and the stop rule is read at about
650 with no margin.

## 2. The rustc CI job

**Recommendation: required.** Take `rustc` out of `ADVISORY` and rewrite the comment and `REPO-FLOW.md`:
that changes no promotion's fate, and truly advisory would give up CI's one x86-64 boot under KVM to save
about 2.6 hours of holds in two weeks.

**What it is today.** `ci.yml`'s `CI result` ignores it, but Promote's `ci_state` reads the run's
`conclusion`, which a failed job makes `failure`, so a red job stops `main` while `CI result` posts
success. **It is required already, in the one place that moves `main`**, and advisory only for `CI result`
on pull requests. The comment's own exit, "when both have their fix", was met on 2026-10-24: `81e5c1f5b` and
`c153aa8f2`, and neither panic has shown since.

**History**, all 946 CI runs since 2026-09-12, 105 of them with this job failing, read from each log:

| Cause | Runs | Alone | Promotions | Dates | What it was |
|---|---:|---:|---:|---|---|
| Stage 9 XINUSE wake check | 34 | 14 | 3 | 10-08 to 10-24 | The check's assumption, fixed |
| Stage 9 "a task resumed from a blocking" | 11 | 2 | 1 | 10-06 to 10-31 | Not traced here; last seen 10-31 |
| btrfs check refused the volume Ferrix wrote | 11 | 11 | 0 | 09-23 | Not traced here |
| Stage 7 `getrusage` | 10 | 5 | 4 | 10-21 to 10-24 | The check's precision, fixed |
| **Case 138, FX-0925** | **8** | **8** | **6** | **11-04** | **A vacuous case: real** |
| Component clone | 7 | 2 | 0 | 10-03 to 10-04 | Infrastructure |
| Allocation failure check | 5 | 4 | 0 | 10-06 to 10-07 | Not traced here |
| FX-0936 "system calls were charged as user time" | 3 | 0 | 3 | 11-05 | Today's; `Boot in QEMU (aarch64)` fails too |
| Others | 16 | | | | Build failures, one net core check, timeouts |

**What it caught.** Case 138 is the case to weigh. From K1's landing no passing boot on any host had judged
that read (cf774db5b, ledger 6829): the other thread ended the process first. Only this job failed, because
it is CI's only x86-64 boot under KVM on a host that is not nazuna; `Boot in QEMU` runs TCG. Why the race
goes the other way there is unmeasured. It was traced because `main` would not move, by inference from
mk-gh-ci's notes, whose first finding was this very mismatch. An advisory job's red would have been one more
yellow line.

**What each choice costs, in promotions.** 155 promotion runs since the flow began, 75 red, 63 of those on
some other job. Rustc failed in 22 and was alone in 12. Since the two fixes (10-25 on): 114 runs, 13 failures, 8 alone: six
of case 138, one self-host over 1,800 s, one job ended with no failed step after 66 minutes.

| | Required | Truly advisory |
|---|---|---|
| Holds, against today | None more: Promote holds today | About 2.6 h fewer standing alone: 0:52 on 10-24, 1:03 on 10-28, 0:42 on 10-30 |
| Pull requests to develop | Red would block; none since 10-25 was red on this job | As today |
| Change | `ci.yml`'s list and comment, `REPO-FLOW.md` | `ci_state` reads `CI result`, not the conclusion |
| What is lost | One job's runner noise can hold `main` until the next landing, about an hour | The only foreign-host KVM verdict; stage 16's exit and the self-host reach `main` unchecked; case 138 would have |

Two longer windows are left out as misleading. 10-23's 18:40 was the flow's first day with other jobs red
too. 11-04's 8:12 did not matter: `certification/verdict` was pending since 68b54b00c, and CI's three green
runs after it (9270c159c, 14ccf6a64, c4d607c44) did not move `main` either. All red in the flow: 3 days 17
hours over 15 closed streaks, so this job is a small part.

**With it, worth doing:** `timeout-minutes` on the job (the 66-minute run had no failed step, and the job has
no timeout of its own); a retry of its fetch and build steps alone, never of a boot, so a flake stays
visible.

**Today, whatever is chosen:** `main` cannot move. develop's tip is red on a required job, FX-0936 in
`Boot in QEMU (aarch64)` as well as here, since `bare_side_check` landed at 1aa9bffbb. The same boots pass on
nazuna at that hash (`mkm-f2-kvm`, `mbt-b0-a64`); `mbt-*` and `mkf-*` rows were running on `bare` as this was written. And `certification/verdict` is pending on f3ea80a3f: "16 item commits name no review". develop
is 173 commits ahead.

**The customer decides:** required or advisory, and who owes the verdict post (`morning-questions.md`, mk-gh-ci).

## 3. Slot 8 and the 3090

**Recommendation: keep it closed, and spend the effort on disk.** A slot on the card adds about 12% at best,
and only for the rows that fit; disk is what is shedding slots today, and a slot on RAM would test the same
capacity without touching ComfyUI.

**Left to qualify**, none of it possible without VRAM (ledger 6858, BACKLOG):

* V2's kernel freshness control. `q-v2-ckern` and its disk twin both DID NOT FIRE, so **the control must first
  be shown firing on a disk slot**. If V2 ever shows a stale build, that ends the idea (6758 I3).
* V10(h): a control killed by a store fault, with a control that fires.
* V7's neighbours: the gates' GPU tests beside a full store. ComfyUI is now a neighbour no design reviewed.
* `mount-semantics.py` on the then binary.
* Capacity: rows/h with and without the slot, and a kernel built beside a disk slot's.
* Then the consultant's line for `vram-qualified`, and the customer's word for `vram-closed`.

Shown already, not citable: 403 test results as on a disk slot, the loud faults all INFRA, none a false pass.

**What it would add, by inference.** 5 to 7 slots gave 120 to 158 rows/h (`TEST-TIME.md`, different rows, a
guide): 19 rows/h a slot, each row 8% slower. So **at most about 19 rows/h, 12% on 158.** Rows it may
take (`vram_row_fits`): test-boot, init, shell, threads were 55% of 3,042 rows and 41% of their time in the
last 33 hours; with `check`, which needs the 18 GiB store, 68% and 63%.

The pool is not always full. Busy slots averaged per hour: median 3.9, 90th percentile 5.5, highest 5.8 of
7, no hour at 6. Waits were bursty: median 136 s, 90th percentile 1,009 s. And today's slots are short of
disk, not of a card: slots 4, 6 and 7 have no target directory now. Slot 7's was shed at 07:30 CET during this work, at
the 40 GB line, and this work's own builds and images, about 8 GB at their height, were part of what pushed it there.

**Sharing with ComfyUI.** Read now: 3090 24,576 MiB, 13,545 used, 13,526 by ComfyUI (pid 60864), 0% busy.
At its 20 GB peak about 4.5 GB is left.

| Way | What it takes | What it gets | Risk |
|---|---|---|---|
| Stays closed | Nothing | Nothing | None |
| Small store: `GATE_VRAM_GIB=4` and no `check` in `vram_row_fits` | ComfyUI started with `--reserve-vram 5` (`cli_args.py:176`), so that it stops short of it; the customer's restart; V7 with ComfyUI loaded | Boots at 2.5 to 2.8 GB peaks fit, one row's build at a time: a change of architecture is cold, 87 to 100 s against 39 s warm. So well under 19 rows/h, by inference | Without the reserve, ComfyUI's load and the store collide. The store fails to INFRA, which is safe for a verdict and not for a render |
| Night window, 18 GiB | Models unloaded by `POST /free` (`server.py:1294`) before the store starts, and no render that night | The full 19 rows/h, with `check` | The next render reloads its models. A timed job that touches ComfyUI is the customer's to allow |
| **A slot on RAM, no GPU** | A slot's target on tmpfs or a RAM-backed store, 12 to 18 GiB | The same list of rows. `TEST-TIME.md`'s table has tmpfs ahead of VRAM on every line, and FUSE on RAM level with it | 59 GB of RAM, 51 available now, 10 GB of swap in use; at load it is unmeasured. Non-ECC, so the store's CRC or an argument is owed. A new design for the consultant |

The driver's single-buffer limit (6.3 GB) and vtier's blocks are unchanged by a small store. The 3060 is not
proposed: tests never use it (customer, 2026-11-04).

**Small and available now:** `target-gate-slot-8-spill` holds 8.8 GB on the full disk for a closed slot. It
was not touched here.

**The customer decides:** whether slot 8 stays closed; if not, which way, and whether ComfyUI may be restarted
with a reserve or told to unload.

## 4. btrfs for the shared build store

**Recommendation: yes if a device can be spared: `compress=zstd` alone first, dedupe later, on `nvme1n1p6` if
what is on it may go.** Compression alone takes the slots from 130 GiB to about 42, needs no daemon, and
keeps every reflink question for later.

### What it would save

A read-only scan of the slots present (slot 1, 2, 3, 5; 7 is shed, 4 and 6 absent), targets and `build/`:
`O_NOATIME|O_DIRECT`, so no time changed and the page cache kept what the gates had. It skips holes and all-zero
blocks (4.8 GiB; the sparse images are not data), 337,560 files, 18 minutes at 200 MB/s. 236 reads failed,
files gone mid-scan by inference. It is an estimate of btrfs, not btrfs: `mkfs.btrfs` is here, `duperemove` and
`compsize` are not, and none was run.

| Way | Left of 129.9 GiB | Saved | How |
|---|---:|---:|---|
| Reflinks of identical files alone | 91.4 | 29.7% | Whole-file hash: `.rmeta`, `.rlib`, build scripts |
| Dedupe at 128 KiB | 73.5 | 43.4% | Fixed blocks |
| Dedupe at 4 KiB | about 66 | 49.2% | A 1/256 sample by hash value. Compressed extents cannot share at this size, by inference from the format |
| **`compress=zstd:3` alone** | **about 41.4** | **68%** | Each 128 KiB by itself, level 3, rounded up to 4 KiB, kept raw unless a sector is saved: 3.14x on a 1/16 sample by hash value |
| `compress=zstd:1` alone | about 43.5 | 67% | 2.99x |
| 128 KiB dedupe and zstd:3 | **about 23.6** | **82%** | 3.11x on the unique blocks |

Metadata would add under 1 GiB (1.36 million extents, a checksum a sector, duplicated), by inference. mk-gpu's
3.75x was one stream over its sample; a block at a time gives 3.14x, so its "about 20 GB" reads 24.

**Why compression first.** A slot rewrote 10.7 to 13.2 GB of its target in 40 busy minutes (`TEST-TIME.md`).
Seven busy slots are about 130 GB/h, so dedupe's sharing decays as fast as a daemon can rebuild it, by
inference, and that daemon reads what the gates are reading. Compression is there at write time, at about 1.5 GB/s a
core (mk-gpu's sample). The rest of the 82% can be added later without another format.

**What it fixes, and what stays open.** Per-inode times: yes by design, unlike hard links, but ledger 6859's
stale-dependent test must be run on it, not assumed. A checksum on every read: yes, crc32c, and a bad read is
`EIO`, which `gate.sh` would read as FAILED until it learns INFRA, as slot 8 did. Unmeasured, and only a mount
can say: rustc's fsync and small-file times on btrfs, images rewritten in place under copy-on-write, a crossing
from a target on btrfs to a worktree on ext4 (V3's `EXDEV` note). Any gate change goes through the consultant.

### The device

| Candidate | Size | State | Verdict |
|---|---:|---|---|
| **`nvme1n1p6`**, on the WD SN720 | 190 G | `lsblk`: NTFS "New Volume", partition label "Basic data partition", not mounted, by-partlabel link dated 15 minutes after this boot | **The candidate.** But it was `ferrix-root` btrfs on 2026-10-11, the root of nazuna's bare-metal plan, so someone changed it since. Only the customer knows what is on it |
| LVM on `nvme2n1` | | The LV fills the PV, within 5,000 sectors | None free. ext4 `/` shrinks only offline |
| Loop file on `/` | | 60 GB free, shed line 40 | No room, and each write would pay two file systems, by inference. Mounting it needs root too |
| `nvme0n1` | 931 G | BitLocker, live Windows | Never (customer, 2026-10-11) |
| A new NVMe | | | The cleanest. A free slot is not known from here |

At about 42 GiB for these four slots, about 10 GiB a warm slot, 190 GiB holds well past seven, and CPU would be
the limit, by inference. **Without btrfs at all**, `aosp17` (297 GB) or `comfy` (208 GB) moved off `/` removes
the limit as well; it brings no checksum and no time-safe sharing.

### What the customer would do

1. Say what `nvme1n1p6` holds, and whether nazuna's bare-metal plan still roots there.
2. With root, once:
   ```
   mkfs.btrfs -f -L ferrix-store /dev/nvme1n1p6
   mkdir -p /mnt/ferrix-store
   # fstab: LABEL=ferrix-store /mnt/ferrix-store btrfs compress=zstd:1,noatime,discard=async 0 0
   mount /mnt/ferrix-store && chown sebastian: /mnt/ferrix-store
   apt install duperemove   # only if dedupe follows
   ```
3. Agree the slots get cold once.

Then an agent, no root: one slot first, its target and `build/` moved, 6859's stale test and a control on it, rows
against a disk slot, then `gate.sh`'s `slot_target` for the rest, through the consultant.

### What is left in making the kernel tiny

`cargo xtask test-kernel-shape --personality user` at f3ea80a3f, debug-optimised, a figure and no evidence:
**x86-64 63,654 product lines, 61,194 under no cfg this shape rules out, 16,981 of checks. AArch64 58,007, 55,678,
15,560**, and AArch64 is `max`'s only claimed architecture. `M6.md`'s 63,046 was earlier: 608 lines have come
since. Rings: 52,768 core, 10,886 item. `check-item-boundary.py --report` still prints sizes for the whole tree
(124,008 lines of item and core): M6 §3's per-profile size ratchet is not built, so nothing holds this figure down.

**Not in that count:** library crates in ring 0, up to 39,684 lines by rustc's lists, far less linked. Debug text
bytes: `ferrix_fdt` 64,568, `ferrix_native_abi` 47,588, `ferrix_paging` 45,922, `ferrix_sync` 35,057,
`ferrix_objects` 28,116, `ferrix_acpi` 27,689, `ferrix_fallible` 26,132, `ferrix_pci` 25,328, `ferrix_sched` 21,788,
`ferrix_linux_abi` 19,757. Whole text: x86-64 1.79 MB, AArch64 1.57 MB.

| Module (x86-64) | Lines | Largest files |
|---|---:|---|
| `arch/` | 11,761 | `x86_64/mod.rs` 1,272, `mce*` 1,570, `speculation.rs` 885, `cpu.rs` 850, `restricted.rs` 826, `syscall.rs` 714 |
| `object/` | 7,726 | `quota.rs` 1,304, `job.rs` 875, `process.rs` 790, `channel.rs` 766, `pager.rs` 632, `pin.rs` 580, `exception.rs` 555 |
| `user/` | 6,855 | `space.rs` 3,491, `vmo.rs` 2,178, `address_wait.rs` 666, `cache.rs` 408 |
| `sched/` | 6,097 | `mod.rs` 2,619, `task.rs` 916, `queue.rs` 685, `wait.rs` 495 |
| `syscall/` | 5,582 | `native.rs` 4,446, `native/region.rs` 434 |
| `iommu.rs`, `iommu/` | 4,746 | `iommu.rs` 1,865, `vtd.rs` 998, `amdvi.rs` 653, `remapping.rs` 473, `smmuv3.rs` 396, `ivrs.rs` 283 |
| `panic/` | 3,970 | `catalog.rs` 3,919, of which 2,227 are built |
| `discovery/` | 3,191 | `devmgr.rs` 1,114, `pci.rs` 856, `tree.rs` 630 |
| `device.rs` | 2,298 | |
| `mm.rs`, `mm/` | 2,571 | `mm.rs` 1,773, `scrub.rs` 375 |
| `main.rs` | 1,576 | 1,087 built |
| `console.rs`, `console/` | 1,404 | `log.rs` 396, `output.rs` 316, `pstore.rs` 209, `input.rs` 199 |
| All the rest | 5,877 | `smp.rs` 1,014, `audit.rs` 625, `trap.rs` 546, `boot_authority.rs` 496, `linuxd_start.rs` 456, `vmap.rs` 384 |

**What could leave next, in order**, sizes read from the files, effects by inference, none built:

| Candidate | Lines | Needs |
|---|---:|---|
| VT-d, AMD-Vi and IVRS on AArch64: `nm` of the Arm kernel finds both drivers (27 KB of text). SMMUv3 on x86-64 | 1,934 on AArch64, 396 on x86-64 | A `cfg` by architecture. **The cheapest** |
| `user/cache.rs` (the name in `feature/mk-boot`'s notes and BACKLOG): its list is empty here, and a machine check's retirement runs through `drop_poisoned` | 408 | A retirement path of its own; the consultant, for it is poison handling |
| FDT on x86-64, ACPI where a board needs none: `ferrix_fdt` is 64 KB of x86-64's text, `tree.rs` 630 | Up to about 3,200 with the crate | Why `iommu.rs` and `device.rs` reach it there, to be read first |
| `panic/catalog.rs`'s explanations, kept by code in a host tool | Up to 2,227 built | The customer: the panic report loses its prose in ring 0. Data, no logic, so it flatters a count more than it shrinks risk |
| `msrlog` and its decoder: `ferrix.msrlog`, a diagnostic for the N150's GT clock | 321 | Out under this shape |
| SMBIOS: M.1 keeps `smbios.rs` and `ferrix-smbios` for type 16's one answer | 102, and 553 in the crate | The loader hands the answer over; L.mm.106 moves |
| `arch/x86_64/signal.rs`: only `UserContext` is asked for here, and `vdso.rs` | About 300, 200 | Reading what `restricted.rs` needs; M.2 |
| `object/oom.rs` (cgroup `memory.max`, a policy), `sched/inspect.rs` | 326, 158 | Whether a user kernel has either at all |
| PCI enumeration to devmgr: `pci.rs`, `devmgr.rs` | About 2,000, and `ferrix_pci` | A design: §2.1 keeps PCI host bridges, and ECAM would be granted before any IOMMU rule covers it. The customer's "keep the kernel slim" favours it |

Counting only `src/kernel/src` and leaving FDT where a board needs it, that is about 7,000 lines on either
architecture, to perhaps 56,500 on x86-64 and 51,000 on AArch64, by inference.
`schedule=windows` may take the fair scheduler out and adds K9, net unknown.

**What it does not reach.** `object/`, `user/`, `sched/` and `syscall/native.rs` are 25,000 lines of what §2.1 keeps:
spaces, VMOs, threads, jobs, handles, channels. seL4 is about 8,700 lines of C (`MICROKERNEL.md`'s effort note). Moving modules will not make Ferrix's ring 0 that kind of small; a smaller mechanism would, and nobody has
designed one. Whether 51,000 is "truly tiny" is the customer's word, and no count above is a limit.

**The customer decides:** what `nvme1n1p6` is; whether the panic catalog's prose may leave; whether PCI leaves
§2.1's list. Everything else in that table is an agent's, with the consultant.
