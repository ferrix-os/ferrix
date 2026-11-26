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

## What `cargo xtask flags` refuses today (9ebe69358: `fast` builds on AArch64; on x86-64 too since the x86-64 flip of 2026-11-26; `max` is refused)

| Profile | Value | `built` | On which arch it fails | Next step |
|---|---|---|---|---|
| both | `personality=user` | `yes`, x86-64 and AArch64 (`built_on`) | ARMv7-A: in no claim (7016 B.1) | Flipped at 9ebe69358 (ledger 7363, 7485). BACKLOG 603, the paced fetch, traced and fixed 2026-11-26 (ledger 10333, F-126): linuxd's bridge ended its "port out" carrier on a bell refused as full, and the net driver was never rung again; now the bell is dropped and counted. Under `fast`'s values ad hoc on x86-64 KVM, `test-net` 6 of 15 failed before (`n27c-p-*`, each with the bell line), 10 of 10 PASSED after (`n27c-f6-net-x86-*`, five with bells dropped), AArch64 TCG under `fast` 3 of 3. The four earlier sightings (`mkf-s3-net`, `mbl-g5e-net`, `mbl-g7-c1-net`, `mkf3b-net2-x86`) each printed the bell line. x86-64 joined `built_on` on 2026-11-26 by its own data-and-test commit (ledger 10492): its sweep is the table *`fast`'s x86-64 sweep* below. What it made buildable: `fast` on x86-64; LV.2's `x86_64-uefi-pc` family, already in `fast.toml`'s `slot_families`, now reached, right at level none; above none `check-item-boundary.py`'s profile judge refuses a slot family outside `claim_architectures` (2858-2862, ledger 7011 LV.2), so raising `fast`'s level with `claim_architectures = ["aarch64"]` fails `check` until x86-64 joins the claim; 7485 D.1 (a). K14's held arm on VT-d and AMD-Vi (10291 K10) is due with K14's landing, not on develop yet. The bridge's "port in" (F-127, reserved, parked) costs latency, not a row, there as on AArch64. B.3 on that hash, `mkf3b-*`: `test-flags`, `test-linuxd`, `test-init`, `test-shell`, `test-vfs`, `test-threads --smp 2`, `test-seat`, `test-btrfs` PASSED on x86-64 KVM and AArch64 TCG; `test-net` on x86-64 FAILED 1 of 4 (above); B.3's controls re-FIRED per architecture: `mkf3b-tables-*` (5714 (16), case 52), `mkf3b-r5-*`, the copy-miss row's `mkf3b-copymiss-*`. AArch64's shim and linuxd build and run there (the old 144 errors are gone). `max` is refused by its other values |
| both | `fs=server` | `yes`, x86-64 and AArch64 | ARMv7-A, as `personality=user` | Widened at 9ebe69358 with B.3's rows on its hash: `mkf3b-btrfs-*`, `mkf3b-restart2-*`, `mkf3b-init-*` PASSED |
| `fast` | `partition_speculation=partition` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358. Host: `syscall::region_check` on a Bare caller, every image (7184 B4.1). None at an exit: the regions line, held by `regions_problem`, `mkf2j-c-partition` FIRED. One at a partition switch: cases 193, 194, `domains.rs` (ledger 7216 T.1, 7249 N.1), which replace 7011 B.4's `test-netd` for this row; `test-netd`'s second partition stays M8's. No shipped job is marked (`JOB_SPECULATION_DOMAIN`), so every switch between spaces takes the full barrier, safe and stronger than the flag says; partmgr marking is an M5 row |
| `max` | `partition_speculation=exit` | `yes`, x86-64 and AArch64 | | Flipped (ledger 10144), rows ad hoc, below (*`max`'s ad-hoc values*). The regions line says `exit` and 51 barriers on 51 exits, x86-64 KVM; AArch64 TCG reads the same count, of barriers decided and none issued, labelled "(decided: this processor model has no barrier to issue)": TCG measures no speculation, and the real core's row stays BACKLOG's (*AArch64's restricted exit on a real core*). The swap (the exit's barrier skipped) FIRED on both, case 82; the regions line made to say `partition` FIRED `regions_problem`. nm: `partition`'s `switch_barrier_offered` out, by the gate, with its plant FIRED. `exit`'s own barrier is inline at `restricted_left`: no symbol of its own |
| `fast` | `restricted_fastpath=on` | `yes`, x86-64 and AArch64 | AArch64 holds no fast path: the general way, as its `why` says | Flipped at 9ebe69358. Code review given (ledger 7216). 6747 (2b)'s "drops the word term" withdrawn (7275). V.1-V.9, G.1, Z2-Z4 built: K1 §7.1's table, `check-linuxd-binary.py`, `check-kernel-shape.py`'s absent symbol. Rows on the flip's hash: `test-linuxd`, `test-threads --smp 2`, `test-shell`, `test-rustc` (x86-64) PASSED. nm: `ferrix_syscall_fast` and `fast_check` in under `fast` on x86-64, out under `off`, none on AArch64. `L.x86_64.272`-`.275` tagged, out of the baseline, with `.282` split from `.275` (7363 F.4); the anchors are the 20 units of 7363 (7008's "six" was never counted); coverage carried, none dropped; VULNERABILITY-ANALYSIS's entry (7249 (2), 7008 (4)) |
| `max` | `restricted_fastpath=off` | `yes`, x86-64 and AArch64 | | Flipped (ledger 10144), rows ad hoc, below. The general path: `test-linuxd`, `test-threads --smp 2`, `test-shell` PASSED on x86-64 KVM and AArch64 TCG, `test-rustc` on x86-64. nm by the gate (`check-value-symbols.py` in `test-kernel-shape`): `ferrix_syscall_fast` and `fast_check::restricted_fast_counts` out under `off`, development and release, both architectures; in under `on` on x86-64, out on AArch64; the plant (build.rs giving the code `on`) FIRED |
| `fast` | `fault_ahead=on` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358. B.6: E1 and E4's W+X built, controls FIRED; E3 on and off (`mkf3c-cont`, `-cont-off`, `mkf3f-threads2`, `mkf3c-threads-off`). E4's foreign half by argument, A6 a BACKLOG row at linuxd's first adoption (7275). nm: `fault_around`, `write_ahead` in `linuxd` under `on`, out under `off`, by the gate since ledger 10144 (`n27c-r-shapein-*`). **F-122, closed (ledger 10178)**: E3's walk control (`write_ahead`'s room ignored) stopped firing at 4120b4def, the walk's end moved among a fault's pages by the program's own charges; no charge past a limit seen. The walk now ends at each of the eight places, and the control FIRED on x86-64 KVM and AArch64 TCG (`n27c-g-room-*`), the walks PASSED under `on` and `off` on both |
| `max` | `fault_ahead=off` | `yes`, x86-64 and AArch64 | | Flipped (ledger 10144 X7), rows ad hoc, below. B.6's off half as amended by 10144 X7, which withdraws "the walk's control silent": `test-threads --smp 2` and `test-container` PASSED under `off` on x86-64 KVM and AArch64 TCG; nm by the gate: `fault_around`, `write_ahead` out of `linuxd` in the development and release builds on both, in under `on`, the plant (build.rs giving linuxd `on`) FIRED on both. The walk control was dead under `on` as well, `on`'s, F-122, closed since |
| `fast` | `schedule=edf` | `yes`, x86-64 and AArch64 | | Flipped at 9ebe69358: `test-flags` both, AArch64's `test-shell`, `test-threads --smp 2` PASSED; `test-kernel-symbols` re-read (`mkf3b-sym-*-present`). The fast path's decline on a channel: case 197, `restricted-check`, with its control. nm: `ferrix_sched_edf` in under `edf`, out under `fair`. `G.1`'s generic set over all three tiers with `windows` |
| `max` | `schedule=windows` | `adhoc` | both, not yet (ledger 10101): AArch64 TCG `test-threads --smp 2` fails under a host CPU burst, the order cases reading interrupts QEMU delivers late as missed turns (BACKLOG 671); under a starved TCG host a partition's task was seen executing in another partition's window (n27a-burst4-1), the cause not yet named, and no AArch64 `test-threads --smp 2` boot ran on the landing hash (ledger 10241 T6, 10361 D5); x86-64 waits on the same judge, which is common code | K9 §4, ledger 7460, 7540, 7549, 7562, 7732, 10087, 10101: `ferrix-sched-windows`, the kernel's default frame sealed before any first process (by order, WZ.12), `windows_check` with each id its failure line; the code is in, dark, `--set schedule=windows` ad hoc. Owed for `built`: the order cases judged from the class's own picks on AArch64 with a control that fires on a broken rotation, then the row 5 of 5 on the landing hash (10101 B4); S.6's list per architecture (10087 A1). The idle-at-end start delay is unjudged under every hypervisor, a halted vCPU's wake being the host's latency (BACKLOG 678, M5's flip, 10087 A8). WZ.6's check-only waits are done (ledger 10222): each check's `yield_now` is answered in K9 §4.1, a spin-wait on another task gives way, `check-yield-sites.py` holds the list. Owed beside it (10361 D3): `.text` and count A of a `--personality user --set schedule=windows` release kernel on both architectures beside the same without windows. `max` claims AArch64 alone |
| `max` | `kernel_memory=retyped` | `adhoc` | K16.5, K16.7, K16.9, F.8 (R.1, R.3); R.4's `platform/` read before AArch64 joins `built_on` | R.2 closed by cause (ledger 10111, 10191); R.5's sizing landed (ledger 10391): the promise after the initramfs's return, one drivers' promise, a machine too small refused by name. **Every retyped row names its machine:** `--memory 640` under `max`'s values (`fs=server`) on x86-64 and AArch64, the default 512 MiB being under that minimum; 480 MiB (x86-64) and 464 MiB (AArch64) without `fs=server` (K16.md §4.1). These are debug images' minimums: a release image is refused by name at 640 MiB under `fs=server` (`n27c-k9-s2-rel-dev`), its minimum not yet found (ledger 10735 M2, M3). Rows of this sizing on both architectures, x86-64 KVM and AArch64 TCG, ad hoc, not `max`. F-128 closed by fix (a): a promise the kernel made is not changed through a call (L.object.350, K16.md §4.2; ledger 10673, 10688, 10787). Next: K16.9's tables and F.8's count (agent B), K16.5's static check and R.4 (agent E), R.3 |

## What refuses them after every value is `built`

| What | Next step |
|---|---|
| Done (this landing). LV.1: rule 3's second half fails `check`: `core` and `item` are at `present`, fsproto and blkclient at none | `level = "none"`, `target_level = "sc2"`/`"sc4"`; values, requires, stand-ins and boot options judged against the target; `component_problems` at the target printed as a note; line reads `level=none`; hand-editing `level` to the target fails `check`, controls FIRED |
| Done (this landing). LV.2: `fast`'s `slot_signing=required`, families `aarch64-uefi-virt` only | Read the rows' boot path; a development-signed slot at level none; x86_64-uefi-pc joins the list if x86-64 boots `fast`, with a check that a family lies inside `claim_architectures` at level != none |
| LV.3: `flag_nodes` keyed on `ferrix_level` would go dark at level none | Keyed on the profile: done. The certified-list writer and K14.12's framing test: done, development-only, refused above level none (`mkf2k-c-level`). Real nodes by a plant: `mkf2d-k14-pass`, `mkf2d-k14-off` |
| LV.4: `settings_writer=absent` needs lode reload's writer out (3623 R18) | Done: a build value of `lode`, refused in an `absent` image |
| LV.5: `test-flags`, `test-kaslr`, `test-netboot` under the profiles; a row's boot options against a profile's permitted set | Read and run each. `check-stack-depth` on `personality=user`, both architectures: done 2026-11-26 (ledger 10274), read under `max`'s values ad hoc; again on the profile in B.8's sweep: a region's process made, 13,240 of 20,480 on x86-64 and 13,840 of 24,576 on AArch64, a lower bound, 4,096 left on each; the OOM emptier 13,288 and 13,872. The walk named a closure's instantiation for `process_create_shared` and left the region's answer, which holds its inlined body, out of the figure (10,664 and 12,160 before); a judged path now names functions themselves, and the region's answer is on it (F-124); a 12 KiB plant there FIRED on both (`n27c-l-c-*`), where the old walk's did not (`n27c-sd-c-*`). `present`'s paths read the same before and after on all three. A Linux call's paths are linuxd's, on its own stack |
| B.7: `fast_and_max_stay_not_buildable_with_the_lift_in` | The shrinking list is in: done. Owed at the flip commit: a flag-file copy with one value flipped fails it, control FIRED, and rule 3 on raising `level` FIRED; on empty, both profiles build at level none with their targets |
| B.8: one sweep per profile and architecture on one hash, in this file | Every row PASSED, FAILS with cause and a BACKLOG row, or NOT RUN, never absent; `check-stack-depth` on x86-64 and AArch64 among them, on the profile's own image (LV.5, ledger 10274 L7). Under `kernel_memory=retyped`, `check-fatal-alloc` on both architectures among them (K16.5, ledger 10834 L6). `max`'s AArch64 rows boot on QEMU's `neoverse-n1` (`max.toml`'s `qemu_cpu`, named on each boot's `qemu:` line), which reads CSV2, because `max` stops by name on a processor that neither issues the exit barrier nor shows itself unaffected (ledger 10312 J1-J3, 10522 (D)); the model shows the decision's path, not any hardware's immunity. A `max` AArch64 row is one whose `qemu:` line reads "cpu neoverse-n1 (profile max)" and nothing else; `FERRIX_ARM_CPU_OVERRIDE` is for the stop's own control alone, and its line says "override of profile max". The model follows the base profile, so an ad hoc `--set` row stays on Cortex-A72; once the stop's code is in, an AArch64 row that sets the value carrying it boots on neoverse-n1 through `FERRIX_ARM_CPU` and says so, evidence for the value and never one of `max`'s rows. The Neoverse N1 shows guest features Cortex-A72 does not (PAN among them), so figures found tonight on the default model under `max`'s values (the minimum machine, K16's peaks, timings) are found again in this sweep under `--kernel-profile max`: the sweep pays, not tonight's landings. Tonight's ad hoc rows with `max`'s values on the default Cortex-A72 read "Spectre v2 NOT covered" and stay what they are; no `max` row shows the model yet (ledger 10626) |

## Every "before the flip" in the tree, sorted

| Where | What | Word |
|---|---|---|
| BACKLOG 259 | `restricted_fastpath=on`: Z2-Z4, K1 fast path review, 6093 | `built` of `on` |
| BACKLOG 339 | check-stack-depth on the user kernel | the profile builds (LV.5): done 2026-11-26, both architectures, read ad hoc (ledger 10274) |
| BACKLOG 343, 344 | `KERNEL_JOBS`, `DEBUG_WRITE` | M5's flip |
| BACKLOG 415 | K14's certified list has no writer | the profile builds (LV.3) |
| BACKLOG 423 | netd's network row, N.7 | netd entering a profile, M5's flip |
| BACKLOG 426 | `Load(Copy(Fault))` under retyped: traced, the arena's sizing, which stays open (ledger 10111) | `built` of `retyped` (R.2 by cause; the sizing R.5) |
| BACKLOG 428 | a real device's write outside its domain, SH.2 | M5's flip |
| BACKLOG 429, 436 | K16.5, K16.9, F.8 | `built` of `retyped` (R.1); K16.2 to M7 (R.4) |
| BACKLOG 436 | K16.7 | the refusal by name for `built`, the rest to M7 (R.3) |
| BACKLOG 551 | R3 charging, `TRANSFER`, rows with no check, §7's rows | M5's flip |
| BACKLOG 569 | `fault_ahead`'s A7 controls | `built` of `on` (B.6): done 2026-11-26, on develop on both architectures (ledger 10259, `n27c-fa-*`) |
| BACKLOG 577 | the window's W2-W8 | M5's flip |
| BACKLOG 636 | `max` without IBPB | M5's flip |
| BACKLOG 653 | `test-driver-model` on AArch64: done 2026-11-26 on `personality=user` and `fast` (`DRIVER-MODEL.md` §12); `max`'s run comes with its sweep | M5's flip |
| BACKLOG 678 | `schedule=windows`' idle-at-end start delay, judged on bare metal (ledger 10087 A8, moved from `built`; unjudged under every hypervisor) | M5's flip |
| BACKLOG 679 | `schedule=windows`: a partition-0 worker's wait on a sleep lock held across a window's end, bound owed (ledger 10087 A5, W.3) | M7 |
| BUILD-FLAGS 557 | `linux_abi=off` in a profile | no profile carries it |
| MICROKERNEL 379 | coverage and rows at the flip | M5's flip |
| K16 9, 11, 17, 56 | as BACKLOG 426, 429, 436 | `built` of `retyped` |
| M5-LIFT 466, 498, 563 | flip preconditions, test-powerfail, the window | M5's flip |
| M6 75 | a user kernel's coverage | M7 |

M5's flip and M7 keep all of theirs: nothing here is dropped by being moved off `built`.

## K14 at no memory (ledger 7166 F.3)

`flag_nodes` runs first, so a later `attach` failure gives `Domain::untranslated` with no flag, on all three units: K14 fails open at no memory. Done 2026-11-26 (sebastian-27e, L.iommu.110): under `fast` and `max`, and at a certified level, a refused attachment gives no domain (`iommu::domain_placed`), and any other kernel keeps the untranslated one. A user kernel's boot check makes the refusal (the attachment's first allocation failed once) and requires each answer: under `fast` on AArch64 TCG, that the refused attachment gave no domain; under `--personality user`, the untranslated domain. `flag_nodes` reads the same `iommu::HELD_TO_K14`, so what K14 holds is one list; the check requires nothing left attached in the unit, and a boot of `fast` must print its line (F-125, ledger 10291).

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
| `test-net --init busybox` | FAILED 1 of 4 (on this hash; BACKLOG 603's cause fixed since, ledger 10333, F-126): `-net2-x86`, 1 of 16 programs, the paced fetch; `-net3-x86` to `-net5-x86` PASSED, and `present`'s `-net-present` PASSED, which compares nothing. The four sightings of one signature (`paced-fetch 143`, the restart program's `wget` given no byte in 60 s, at about 76 s), never traced: `mkf-s3-net` 732aa199d, `mbl-g5e-net` 4d06c97ba, `mbl-g7-c1-net` 8b7e9226b, `mkf3b-net2-x86` 9ebe69358. BACKLOG 603. Option (b) of ledger 7485 D.1 taken: `personality=user` is built on AArch64 alone, `fast` refused on x86-64 by that name | NOT RUN: netd's way on AArch64 is B.3's excused row (7016 B.3) |
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

## `max`'s ad-hoc values: B.2, B.4-B.7 (2026-11-26, ledger 10144)

`max` itself does not build (`windows`, `retyped`), so these rows are **ad hoc, not `max`**: the values set
by hand with `--no-claim --personality user --set fs=server --set partition_speculation=exit --set
restricted_fastpath=off --set fault_ahead=off` (M below), and `fast`'s three for the "in" side (F:
`partition`, `on`, `on`). The rest are `present`'s (`schedule=fair`, `kernel_memory=heap`), as
`feature/mk-flip-trial`'s rows were for `fast`. Through `gate.sh`, `GATE_PRIORITY=1`, tags `n27c-r-*`, logs in
`~/.local/share/ferrix/logs/queue/`, all on 8cb1eced0, the same flip before the review: an ad-hoc row reads no
`built`, and what changed since under `src/` is nothing; the rows carry by the splitting rule (ledger 10144 (a), X4).
x86-64 on KVM, AArch64 on TCG.

| Row | x86-64 KVM | AArch64 TCG |
|---|---|---|
| `test-flags`, M: build line names the values; regions line `under \`exit\``, 51 barriers on 51 exits | PASSED `-flags-x86` | PASSED `-flags-a64`, labelled "(decided: this processor model has no barrier to issue)" |
| `build --release`, M | PASSED `-rel-x86` | PASSED `-rel-a64` |
| `test-kernel-shape`, M: nm, every value's symbols out | PASSED `-shape-x86` | PASSED `-shape-a64` |
| `test-kernel-shape --release`, M: "out" judged on the LTO image | PASSED `-shaperel-x86` | PASSED `-shaperel-a64` |
| `test-kernel-shape`, F: `ferrix_syscall_fast`, `restricted_fast_counts` (x86-64 only), `switch_barrier_offered`, `fault_around`, `write_ahead` in | PASSED `-shapein-x86` | PASSED `-shapein-a64` (no fast path symbol, as `on`'s `why` says) |
| `test-linuxd`, M | PASSED | PASSED |
| `test-threads --smp 2`, M | PASSED | PASSED |
| `test-shell`, M | PASSED | PASSED |
| `test-rustc`, M | PASSED `-rustc2-x86` with `FERRIX_RUSTC_SYSROOT` at the 1.99.1 volume. `-rustc-x86` FAILED first: the default volume `~/.local/share/ferrix/rustc` holds rustc 1.97.1 and xtask wants 1.99.1 ("rustc version false"), the host's volume, not the hash | NOT RUN: x86-64 binaries on the volume |
| `test-container`, M | PASSED `-cont-x86` | PASSED `-cont-a64` |
| B.4's swap: `restricted_left`'s barrier skipped under `exit` | FIRED `-swap-x86`, case 82 | FIRED `-swap-a64`, case 82 |
| B.4's host: the regions line made to say `partition` | FIRED `-says-x86`, `regions_problem` | |
| B.2's plants: build.rs gives the code a value the run's words do not (`--expect-host`, `check-value-symbols`) | FIRED `-plant-fast-x86` (`on`), `-plant-part-x86`, `-plant-fault-x86` | FIRED `-plant-part-a64`, `-plant-fault-a64` (no `on` plant: AArch64 builds no fast path under either) |
| B.6's walk control (`write_ahead`'s room ignored) | DID NOT FIRE under M (`-walk-off-x86`) **and under F** (`-walk-on-x86`): dead, not silent, F-122; probes `-walk-probe-room`, `-walk-probe-take`, `-walk-probe-all`. Withdrawn from `off`'s conditions (10144 X7) | DID NOT FIRE under M (`-walk-off-a64`) |
| B.7: `exit` back to `adhoc` in a copy of the flag file | FIRED `-b7-x86` (`a_profile_is_refused_for_exactly_what_it_waits_on`) | |

First runs that ended in the command, not the hash: `n27c-flags-x86` (no `--no-claim`: the gate refused an ad-hoc
build line as `present`'s run), `n27c-t-flags-x86`, `-a64` (the value set passed as one word, xtask's "unknown
option"); each re-run as named.

On the landing's hash, after ledger 10144's X1-X3 changed `check-value-symbols.py` and `test-kernel-shape` (X3, X4),
tags `n27c-f-*`: `check`; `test-flags` M on x86-64 KVM and AArch64 TCG; `test-kernel-shape --personality user`
as `gate-rows` lists it (x86-64), and under `present`'s own values on x86-64, AArch64 and ARMv7-A; the five plants
and B.7's control again. The swap and the regions-line controls carry: neither file moved.

## `fast`'s x86-64 sweep (2026-11-26, ledger 10492, 10333 P7)

Through `gate.sh`, `GATE_PRIORITY=1`, `--kernel-profile fast`, x86-64 KVM, tags `n27c-x-*`, logs in
`~/.local/share/ferrix/logs/queue/`. The rows ran on 748a5d528, whose tree is the flip's (landed as 01ca70c8b; `git diff --stat`
empty: one commit, two messages). The build line reads `profile=fast`. AArch64's column is the 9ebe69358 sweep's
above: nothing of an AArch64 image changes with a `built_on` word.

| Row | x86-64 KVM |
|---|---|
| `check` | PASSED `-check` |
| `test-flags` (test-boot here) | PASSED `-flags` |
| `test-linuxd` | PASSED |
| `test-init` | PASSED |
| `test-shell` | PASSED |
| `test-vfs --init busybox` | PASSED `-vfs` |
| `test-threads --smp 2` | PASSED |
| `test-seat` | PASSED |
| `test-net --init busybox` | PASSED 10 of 10 (`-net-1` to `-10`), the n of 10333 P4 at BACKLOG 603's before-rate; `-net-5` dropped bells (at least 8) and passed |
| `test-btrfs` | PASSED |
| `test-btrfsd-restart --init busybox` | PASSED `-restart` |
| `test-kaslr` | PASSED |
| `test-netboot` | PASSED |
| `test-rustc` | PASSED, `FERRIX_RUSTC_SYSROOT` at the 1.99.1 volume (the default volume is 1.97.1, on the customer's list) |
| `test-container` | PASSED |
| `test-kernel-shape` | PASSED `-shape` |
| `check-stack-depth` | PASSED `-sd` |
| `test-kernel-symbols --release`, `present` | NOT RUN: a flip of data changes no `present` image |
| 5714 (16)'s handle control | FIRED `-tables`, case 52 |
| R5's control | FIRED `-r5`, "lift-calls FAILED" |
| The copy-miss row's control | FIRED `-copymiss` |
| B.7's controls (`--expect-host`) | FIRED `-b7` (`personality=user`'s `built_on` back to `["aarch64"]`), `-b7-fa` (`fault_ahead=on` back to adhoc), `-b7-level` (`level = "sc2"`: rule 3, core at present), on 3225f7258, the flip's tree under an earlier message |
| K14 at no memory, VT-d (10492 X5; F-125, a420f5ec9 landed first) | PASSED `-k14-x86` on the rebased hash: "pci 0000:00:00.0's refused attachment gave no domain and left nothing attached, as a profile held to K14's grant must"; the held arm made untranslated FIRED `-k14held-x86` (panic, stage 10); AMD-Vi NOT RUN: the gate's x86-64 machine has a VT-d unit, none has AMD-Vi |
| `test-driver-model` | NOT RUN under `fast`: it boots with `ferrix.unisolated`, outside `fast`'s permitted boot options (10312 M3) |
| §12's failing rows | NOT RUN, each for §12's own cause, as in the 9ebe69358 sweep |

## Where it stands

* 2026-11-26, ledger 10859 K3: a `max` image, built `checks=on` as every profile is, reads the test-only boot words compiled in with the checks (`ferrix.check_refuse_process`, F-129's; `ferrix.scrub`) though no profile permits them, until item 4's rule refuses a boot whose line carries a word its profile does not permit. That stands among what refuses `max`. `check-build-flags.py` refuses a profile that permits a `rows_only` word at any level.

* 2026-11-26, ledger 10222: **WZ.6 is done**: every `yield_now` of a check answered in K9 §4.1 (31 calls in 16 files, `check-yield-sites.py` in `check`, its control FIRED), a check's spin-wait on another task gives way under `windows`. `max` is still refused, by `schedule=windows` (the order cases judged from the class's own picks on AArch64 and the row 5 of 5, 10101 B4, 10241; `.text` and count A, 10361 D3) and `kernel_memory=retyped` (K16.5, K16.7, K16.9, F.8, K16.2, the K16 preconditions row), and on x86-64 by `personality=user`'s `built_on` for `max`'s claim, which is AArch64 alone.

* 2026-11-26, ledger 10492: `fast` builds on x86-64 too, `personality=user` built there (`n27c-x-*`), after BACKLOG 603's fix (F-126). `max` gains nothing: it claims AArch64 alone.
* 2026-11-26, ledger 10144: `max`'s `partition_speculation=exit`, `restricted_fastpath=off` and `fault_ahead=off` are `built` on x86-64 and AArch64, on rows ad hoc, not `max` (the section above). `max` is still refused, by `kernel_memory=retyped` and `schedule=windows`, and on x86-64 by `personality=user`'s `built_on` too (BACKLOG 603). F-122, `fault_ahead=on`'s walk control, closed the same night (ledger 10178). M5's flip and M7 keep all of theirs.
* 2026-11-05, ledger 7363, 7485: **`fast` builds on AArch64**, at level none held to sc2 (9ebe69358, the flip): x86-64 is refused by `personality=user` alone until `test-net`'s paced fetch is traced (BACKLOG 603); its six values flipped as data, B.7's list empty, `L.x86_64.272`-`.275` and `.282` tagged, the sweep above. `max` is still refused, by `exit`, `off`, `retyped`, `windows`. M5's flip and M7 keep all of theirs.
* 2026-11-05: `fast` and `max` are still refused, by every line of the first table but `fs=server` on x86-64. Landed: the profiles' target, `built_on`, the signed slot, this file (9b5fa6957); `schedule=edf` dark and `lode`'s reload as a build value (fb32c5f77). AArch64's shim builds and runs Linux programs: `test-linuxd`, `test-shell`, `test-threads --smp 2` read PASSED under `--personality user` (`mkf-5-*`), `test-seat`, `test-init`, `test-btrfs` under `fs=server` (`mkf-s1-*`, older tree). None of it flips a `built`.
* What stood before `fast` (met at 9ebe69358), in order: B.3's rows on the flip's hash; B.4's partition switch row (T.1); B.5's Z2-Z4, G.1, V.1-V.9 (ledger 7216); B.6's E1, E3, E4 and A6; edf's program row on a channel; LV.3's writer and framing test for AArch64; LV.5. Before `max`, beside those: `windows` (ledger 7088 W.1-W.4, S.1-S.3: no code yet, ids reserved), and `retyped`'s R.1-R.5.
* Under `fast`'s values, ad hoc, x86-64 KVM, 732aa199d, through `gate.sh`: `test-btrfs`, `test-seat`, `test-threads --smp 2` PASSED; `test-container` FAILS, its own UTS namespace's `sethostname` (`runtime_settings=fixed`, to read); `test-vfs` FAILS, `dmesg`'s `klogctl` EPERM, 2 of 24 (the same to read); `test-net` FAILS, 1 of 16, the paced fetch's `/tmp/paced` (BACKLOG's `paced` row). Not `fast` itself: an ad-hoc build has neither its loader nor its image. Rows of the profile itself: a side commit that flips the values with no review, `feature/mk-flip-trial`, is the only way to read them today, and is not a flip.
* 2026-11-05, landing of ledger 7166, 7184: under `fast`'s values through `gate.sh`, on the side commit, not `fast` itself: `test-flags` PASSED on x86-64 KVM and AArch64 TCG (`mkf2g-flags-*`), `test-init` PASSED on both (`mkf2g-init-*`), with the OOM read and fixed (authd's cost, `mkf2a-init-x86` against `mkf2g-init-x86`). `fast` and `max` are still refused.
* 2026-11-05: `syscall::region_check` runs on a user kernel, every case held, x86-64 KVM and AArch64 TCG. `test-boot` under a user kernel is `test-flags`. Under `fast`'s values on the side commit also: `test-vfs`, `test-container`, `test-net`, `test-seat`, `test-btrfs`, `test-btrfsd-restart`, `test-kaslr` PASSED as named in the fleet log; `fast` and `max` are still refused.
