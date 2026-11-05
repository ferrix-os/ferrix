# What refuses `fast` and `max`

A checklist, with a next step per line and no owners. Reviewed as ledger 7011 (O, S, B, R, LV), internal
review, not independent verification (F-27). No line here claims a certified level: the evidence comes in M7.

## Three words

* **`built`**, per value and architecture: this value's code is real and its rows pass on QEMU, accelerator
  named, no timing claim. `built_on` in `tools/common/data/build-flags.toml` says which architectures.
* **The profile builds**: every value `built` for that architecture, and the profile's image rules met.
* **M5's flip**, with M7's evidence: the in-kernel load's claim moves. Every "before the flip" in BACKLOG
  means this one unless the table below moves it.

`desktop` stays hybrid and `present` stays as it is, whatever happens here.

## What a `fast` row spends (ledger 7126 L1.3)

Its loader starts only a verifying slot, so each image built for it signs one leaf of the development key `dev-slot`, a tree of 2^15, whose log held 84 lines on 2026-11-05. Each sweep leaves the log's line count here. SIGNING's 50% alarm is not in the soft signer yet (BACKLOG). A row of `fast` on CI needs `ci-sign` and its anchor first: `docs/SIGNING.md` §6.5.

## What `cargo xtask flags` refuses today (9ebe69358: `fast` builds on AArch64, x86-64 waits on `test-net`; `max` is refused)

| Profile | Value | `built` | On which arch it fails | Next step |
|---|---|---|---|---|
| both | `personality=user` | `yes`, AArch64 alone (`built_on`) | x86-64: `test-net` FAILED 1 of 4 on the flip's hash, BACKLOG 603, so `fast` is refused there by this one name (ledger 7485 D.1, option (b)); ARMv7-A: in no claim (7016 B.1) | Flipped at 9ebe69358 (ledger 7363, 7485). x86-64 waits on four sightings of one signature (`paced-fetch 143`, the restart program's `wget` given no byte in 60 s, at about 76 s), never traced: `mkf-s3-net` 732aa199d, `mbl-g5e-net` 4d06c97ba, `mbl-g7-c1-net` 8b7e9226b, `mkf3b-net2-x86` 9ebe69358: traced to a cause outside the user kernel with a fix or a control and `test-net` PASSED, or a code change with its own review; then `x86_64` joins `built_on`. B.3 on that hash, `mkf3b-*`: `test-flags`, `test-linuxd`, `test-init`, `test-shell`, `test-vfs`, `test-threads --smp 2`, `test-seat`, `test-btrfs` PASSED on x86-64 KVM and AArch64 TCG; `test-net` on x86-64 FAILED 1 of 4 (above); B.3's controls re-FIRED per architecture: `mkf3b-tables-*` (5714 (16), case 52), `mkf3b-r5-*`, the copy-miss row's `mkf3b-copymiss-*`. AArch64's shim and linuxd build and run there (the old 144 errors are gone). `max` is refused by its other values |
| both | `fs=server` | `yes`, x86-64 and AArch64 | ARMv7-A, as `personality=user` | Widened at 9ebe69358 with B.3's rows on its hash: `mkf3b-btrfs-*`, `mkf3b-restart2-*`, `mkf3b-init-*` PASSED |
| `fast` | `partition_speculation=partition` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358. Host: `syscall::region_check` on a Bare caller, every image (7184 B4.1). None at an exit: the regions line, held by `regions_problem`, `mkf2j-c-partition` FIRED. One at a partition switch: cases 193, 194, `domains.rs` (ledger 7216 T.1, 7249 N.1), which replace 7011 B.4's `test-netd` for this row; `test-netd`'s second partition stays M8's. No shipped job is marked (`JOB_SPECULATION_DOMAIN`), so every switch between spaces takes the full barrier, safe and stronger than the flag says; partmgr marking is an M5 row |
| `max` | `partition_speculation=exit` | `adhoc` | a row of the value | B.4: the host and `regions_problem` are in: barriers equal exits, on KVM; AArch64 reads "decided" on TCG, labelled, BACKLOG's real core |
| `fast` | `restricted_fastpath=on` | `yes`, x86-64 and AArch64 | AArch64 holds no fast path: the general way, as its `why` says | Flipped at 9ebe69358. Code review given (ledger 7216). 6747 (2b)'s "drops the word term" withdrawn (7275). V.1-V.9, G.1, Z2-Z4 built: K1 §7.1's table, `check-linuxd-binary.py`, `check-kernel-shape.py`'s absent symbol. Rows on the flip's hash: `test-linuxd`, `test-threads --smp 2`, `test-shell`, `test-rustc` (x86-64) PASSED. nm: `ferrix_syscall_fast` and `fast_check` in under `fast` on x86-64, out under `off`, none on AArch64. `L.x86_64.272`-`.275` tagged, out of the baseline, with `.282` split from `.275` (7363 F.4); the anchors are the 20 units of 7363 (7008's "six" was never counted); coverage carried, none dropped; VULNERABILITY-ANALYSIS's entry (7249 (2), 7008 (4)) |
| `max` | `restricted_fastpath=off` | `adhoc` | | B.5: general path, nm shows no fast function |
| `fast` | `fault_ahead=on` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358. B.6: E1 and E4's W+X built, controls FIRED; E3 on and off (`mkf3c-cont`, `-cont-off`, `mkf3f-threads2`, `mkf3c-threads-off`). E4's foreign half by argument, A6 a BACKLOG row at linuxd's first adoption (7275). nm: `fault_around`, `write_ahead` in `linuxd` under `on`, out under `off` |
| `max` | `fault_ahead=off` | `adhoc` | | B.6: `test-threads`, `test-container`, the walk's control silent |
| `fast` | `schedule=edf` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358: `test-flags` both, AArch64's `test-shell`, `test-threads --smp 2` PASSED; `test-kernel-symbols` re-read (`mkf3b-sym-*-present`). The fast path's decline on a channel: case 197, `restricted-check`, with its control. nm: `ferrix_sched_edf` in under `edf`, out under `fair`. `G.1`'s generic set over all three tiers with `windows` |
| `max` | `schedule=windows` | `no` | no code | S.1, S.2, S.3, S.6: `ferrix-sched-windows`, fixed priorities inside windows (C6), no task on a fair run queue (C3) |
| `max` | `kernel_memory=retyped` | `adhoc` | `Load(Copy(Fault))` unexplained; K16.5, K16.7, K16.9, F.8 | R.1 to R.5 |

## What refuses them after every value is `built`

| What | Next step |
|---|---|
| Done (this landing). LV.1: rule 3's second half fails `check`: `core` and `item` are at `present`, fsproto and blkclient at none | `level = "none"`, `target_level = "sc2"`/`"sc4"`; values, requires, stand-ins and boot options judged against the target; `component_problems` at the target printed as a note; line reads `level=none`; hand-editing `level` to the target fails `check`, controls FIRED |
| Done (this landing). LV.2: `fast`'s `slot_signing=required`, families `aarch64-uefi-virt` only | Read the rows' boot path; a development-signed slot at level none; x86_64-uefi-pc joins the list if x86-64 boots `fast`, with a check that a family lies inside `claim_architectures` at level != none |
| LV.3: `flag_nodes` keyed on `ferrix_level` would go dark at level none | Keyed on the profile: done. The certified-list writer and K14.12's framing test: done, development-only, refused above level none (`mkf2k-c-level`). Real nodes by a plant: `mkf2d-k14-pass`, `mkf2d-k14-off` |
| LV.4: `settings_writer=absent` needs lode reload's writer out (3623 R18) | Done: a build value of `lode`, refused in an `absent` image |
| LV.5: `test-flags`, `test-kaslr`, `test-netboot` under the profiles; a row's boot options against a profile's permitted set | Read and run each. `check-stack-depth` on `personality=user`, both architectures: it walks a region's process made, a Linux call being linuxd's |
| B.7: `fast_and_max_stay_not_buildable_with_the_lift_in` | The shrinking list is in: done. Owed at the flip commit: a flag-file copy with one value flipped fails it, control FIRED, and rule 3 on raising `level` FIRED; on empty, both profiles build at level none with their targets |
| B.8: one sweep per profile and architecture on one hash, in this file | Every row PASSED, FAILS with cause and a BACKLOG row, or NOT RUN, never absent |

## Every "before the flip" in the tree, sorted

| Where | What | Word |
|---|---|---|
| BACKLOG 259 | `restricted_fastpath=on`: Z2-Z4, K1 fast path review, 6093 | `built` of `on` |
| BACKLOG 339 | check-stack-depth on the user kernel | the profile builds (LV.5) |
| BACKLOG 343, 344 | `KERNEL_JOBS`, `DEBUG_WRITE` | M5's flip |
| BACKLOG 415 | K14's certified list has no writer | the profile builds (LV.3) |
| BACKLOG 423 | netd's network row, N.7 | netd entering a profile, M5's flip |
| BACKLOG 426 | `Load(Copy(Fault))` under retyped | `built` of `retyped` (R.2) |
| BACKLOG 428 | a real device's write outside its domain, SH.2 | M5's flip |
| BACKLOG 429, 436 | K16.5, K16.9, F.8 | `built` of `retyped` (R.1); K16.2 to M7 (R.4) |
| BACKLOG 436 | K16.7 | the refusal by name for `built`, the rest to M7 (R.3) |
| BACKLOG 551 | R3 charging, `TRANSFER`, rows with no check, §7's rows | M5's flip |
| BACKLOG 569 | `fault_ahead`'s A7 controls | `built` of `on` (B.6) |
| BACKLOG 577 | the window's W2-W8 | M5's flip |
| BACKLOG 636 | `max` without IBPB | M5's flip |
| BACKLOG 653 | `test-driver-model` on AArch64 | M5's flip |
| BUILD-FLAGS 557 | `linux_abi=off` in a profile | no profile carries it |
| MICROKERNEL 379 | coverage and rows at the flip | M5's flip |
| K16 9, 11, 17, 56 | as BACKLOG 426, 429, 436 | `built` of `retyped` |
| M5-LIFT 466, 498, 563 | flip preconditions, test-powerfail, the window | M5's flip |
| M6 75 | a user kernel's coverage | M7 |

M5's flip and M7 keep all of theirs: nothing here is dropped by being moved off `built`.

## K14 at no memory (ledger 7166 F.3)

`flag_nodes` runs first, so a later `attach` failure gives `Domain::untranslated` with no flag, on all three units: K14 fails open at no memory. M5's flip: under `fast` and `max`, no domain rather than untranslated.

## `test-boot` is `test-flags` here

`gate.sh`'s verdict of `test-boot` requires `FERRIX-BOOT-OK`, which a user kernel never prints, by design (5714 (20)), so B.3's test-boot row is `test-flags`, per accelerator and architecture: it runs `test_boot` whole, and its static checks. Ledger 7184. The sweep's test-boot row names this.

## §12's failing rows

Not owed for `built` (B.8) but each is a row in the sweep, by name, with cause: test-sud, test-wine's 32-bit half,
test-compositor, test-clipboard, test-usb-serial, test-quarantine, test-log-budget, test-log-pstore,
test-log-remote, test-removable, test-mce-driver, test-postmortem, test-selfhost, test-gnome-apps, test-powerfail.

## Complexity owed

`restricted::run`, +3 from AArch64's per-architecture helpers, joins 6923 (6)'s row of the worst three (ledger 7126 L1.8).

## B.8: `fast`'s sweep on its flip's hash (9ebe69358, ledger 7011 B.8, 7363 F.9, 7485)

Through `gate.sh`, `GATE_PRIORITY=1`, `--kernel-profile fast`, tags `mkf3b-*`, logs in
`~/.local/share/ferrix/logs/queue/`. x86-64 on KVM, AArch64 on TCG. Rows that ran on 146a44cbc, the same
flip before a conflict-free rebase onto b50e88907, carry (2026-10-14's rule). The flip as landed, 4d8242298 on e39ff3dc6 (a clean rebase), differs from 9ebe69358 only by `personality=user`'s `built_on` (`["aarch64"]`), its test's x86-64 assertion and one sentence of VULNERABILITY-ANALYSIS (7485 D.1 (b)): no image's values or code. `check` and B.7's control (b) ran again on it (`mkf3b-check3`, `mkf3b-b2`).

| Row | x86-64 KVM | AArch64 TCG |
|---|---|---|
| `check` | PASSED `mkf3b-check` | (one, host) |
| `test-flags` (test-boot here) | PASSED `-flags-x86` | PASSED `-flags-a64` |
| `test-linuxd` | PASSED | PASSED |
| `test-init` | PASSED | PASSED |
| `test-shell` | PASSED | PASSED |
| `test-vfs --init busybox` | PASSED `-vfs2-x86` | PASSED `-vfs2-a64` |
| `test-threads --smp 2` | PASSED | PASSED |
| `test-seat` | PASSED | PASSED |
| `test-net --init busybox` | FAILED 1 of 4: `-net2-x86`, 1 of 16 programs, the paced fetch; `-net3-x86` to `-net5-x86` PASSED, and `present`'s `-net-present` PASSED, which compares nothing. The four sightings of one signature (`paced-fetch 143`, the restart program's `wget` given no byte in 60 s, at about 76 s), never traced: `mkf-s3-net` 732aa199d, `mbl-g5e-net` 4d06c97ba, `mbl-g7-c1-net` 8b7e9226b, `mkf3b-net2-x86` 9ebe69358. BACKLOG 603. Option (b) of ledger 7485 D.1 taken: `personality=user` is built on AArch64 alone, `fast` refused on x86-64 by that name | NOT RUN: netd's way on AArch64 is B.3's excused row (7016 B.3) |
| `test-btrfs` | PASSED | PASSED |
| `test-btrfsd-restart --init busybox` | PASSED `-restart2-x86` | PASSED `-restart2-a64` |
| `test-kaslr` | PASSED | PASSED |
| `test-netboot` | PASSED | NOT RUN: x86-64 only by design, the stick is an x86-64 UEFI program (`-netboot-a64`'s refusal) |
| `test-rustc` | PASSED | NOT RUN: x86-64 only, the volume holds x86-64 binaries (`-rustc-a64`'s refusal) |
| `test-container` | PASSED | PASSED |
| `test-kernel-shape` | PASSED `-shape-x86` | PASSED `-shape-a64` |
| `check-stack-depth` | PASSED `-sd-x86` | PASSED `-sd-a64` |
| `test-kernel-symbols --release`, `present` | PASSED `-sym-x86_64-present` | PASSED `-sym-aarch64-present` |
| 5714 (16)'s handle control: restricted mode's table made the shared one | FIRED `-tables-x86`, case 52 | FIRED `-tables-a64`, case 52 |
| R5's control: the arena given `DUPLICATE` | FIRED `-r5-x86`, "lift-calls FAILED" | FIRED `-r5-a64` |
| The copy-miss row (BACKLOG L4): `enter_with` made `enter` under `test-init` | FIRED `-copymiss-x86` | FIRED `-copymiss-a64` |
| B.7's controls (`--expect-host`) | FIRED `mkf3b-a` (fault_ahead=on back to adhoc), `-b` (built_on removed), `-c` (level sc2: rule 3, core and item at present) | |

First runs of `test-vfs`, `test-btrfsd-restart` and `test-net` without `--init` ended in xtask's own refusal
(`mkf3b-vfs-*`, `-restart-*`, `-net-x86`): the command, not the hash; re-run with it as named.

**nm, by value: local, not evidence** (6155 (12); debug builds of this hash on this host, outside `gate.sh`, `--kernel-profile fast` against `--personality user`
with `partition_speculation=exit restricted_fastpath=off fault_ahead=off schedule=fair`): `ferrix_syscall_fast`
and `fast_check::restricted_fast_counts` 1 and 1 under `fast` on x86-64, 0 under `off`, 0 on AArch64;
`speculation_check::switch_barrier_offered` 1 under `partition`, 0 under `exit`, on both; `ferrix_sched_edf` 46 (x86-64),
42 (AArch64) under `edf`, 0 under `fair`; `linuxd`'s `fault_around`, `write_ahead` 1 each under `on`, 0 under `off`, on both.
`arch_prctl` 0 everywhere (Z2). The gate's rows for it: `mkf3b-shape-a64` and `-shape-x86` (`check-kernel-shape.py`'s absent symbol on AArch64 and x86-64), and each value's own landing's rows by tag, `mkf3m-fast-part`, `-fast-exit`, `-edf`, `-window` PASSED on 49b859a0f, `mkf3m-edf-decline` FIRED (ledger 7337 RB.1).

**Every other gate row** (`cargo xtask gate-rows`'s list but those above, under `--kernel-profile fast`): NOT RUN on this hash, as B.8 is a sweep of the rows B.3 to B.6 and LV.5 name (6277 L10: named, so none is absent).

**§12's failing rows** (M5-LIFT §12): test-sud, test-wine's 32-bit half, test-compositor, test-clipboard,
test-usb-serial, test-quarantine, test-log-budget, test-log-pstore, test-log-remote, test-removable,
test-mce-driver, test-postmortem, test-selfhost, test-gnome-apps, test-powerfail: NOT RUN on this hash, each for
§12's own cause, which no value of this flip touches; not owed for `built` (B.8). They stay M5's flip's.

The dev-slot log held 216 lines after this sweep (of 2^15).

## Where it stands

* 2026-11-05, ledger 7363, 7485: **`fast` builds on AArch64**, at level none held to sc2 (9ebe69358, the flip): x86-64 is refused by `personality=user` alone until `test-net`'s paced fetch is traced (BACKLOG 603); its six values flipped as data, B.7's list empty, `L.x86_64.272`-`.275` and `.282` tagged, the sweep above. `max` is still refused, by `exit`, `off`, `retyped`, `windows`. M5's flip and M7 keep all of theirs.
* 2026-11-05: `fast` and `max` are still refused, by every line of the first table but `fs=server` on x86-64. Landed: the profiles' target, `built_on`, the signed slot, this file (9b5fa6957); `schedule=edf` dark and `lode`'s reload as a build value (fb32c5f77). AArch64's shim builds and runs Linux programs: `test-linuxd`, `test-shell`, `test-threads --smp 2` read PASSED under `--personality user` (`mkf-5-*`), `test-seat`, `test-init`, `test-btrfs` under `fs=server` (`mkf-s1-*`, older tree). None of it flips a `built`.
* What stood before `fast` (met at 9ebe69358), in order: B.3's rows on the flip's hash; B.4's partition switch row (T.1); B.5's Z2-Z4, G.1, V.1-V.9 (ledger 7216); B.6's E1, E3, E4 and A6; edf's program row on a channel; LV.3's writer and framing test for AArch64; LV.5. Before `max`, beside those: `windows` (ledger 7088 W.1-W.4, S.1-S.3: no code yet, ids reserved), and `retyped`'s R.1-R.5.
* Under `fast`'s values, ad hoc, x86-64 KVM, 732aa199d, through `gate.sh`: `test-btrfs`, `test-seat`, `test-threads --smp 2` PASSED; `test-container` FAILS, its own UTS namespace's `sethostname` (`runtime_settings=fixed`, to read); `test-vfs` FAILS, `dmesg`'s `klogctl` EPERM, 2 of 24 (the same to read); `test-net` FAILS, 1 of 16, the paced fetch's `/tmp/paced` (BACKLOG's `paced` row). Not `fast` itself: an ad-hoc build has neither its loader nor its image. Rows of the profile itself: a side commit that flips the values with no review, `feature/mk-flip-trial`, is the only way to read them today, and is not a flip.
* 2026-11-05, landing of ledger 7166, 7184: under `fast`'s values through `gate.sh`, on the side commit, not `fast` itself: `test-flags` PASSED on x86-64 KVM and AArch64 TCG (`mkf2g-flags-*`), `test-init` PASSED on both (`mkf2g-init-*`), with the OOM read and fixed (authd's cost, `mkf2a-init-x86` against `mkf2g-init-x86`). `fast` and `max` are still refused.
* 2026-11-05: `syscall::region_check` runs on a user kernel, every case held, x86-64 KVM and AArch64 TCG. `test-boot` under a user kernel is `test-flags`. Under `fast`'s values on the side commit also: `test-vfs`, `test-container`, `test-net`, `test-seat`, `test-btrfs`, `test-btrfsd-restart`, `test-kaslr` PASSED as named in the fleet log; `fast` and `max` are still refused.
