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

## What `cargo xtask flags` refuses today (f3ea80a3f)

| Profile | Value | `built` | On which arch it fails | Next step |
|---|---|---|---|---|
| both | `personality=user` | `adhoc` | x86-64: rows B.3. AArch64: `ferrix-linuxd` does not compile (144 errors, most in `lift/core/restricted.rs`'s register frame; `lift/core/arch` has `x86_64` only) | Port lift-core and linuxd to AArch64; run B.3's rows |
| both | `fs=server` | `yes` | AArch64 not shown | `built_on = ["x86_64"]` now; AArch64's rows, then widen |
| `fast` | `partition_speculation=partition` | `adhoc` | no check has a user host | B.4: a host for restricted-check; none at an exit and one at a partition switch, with `test-netd`'s second partition; control swapping them |
| `max` | `partition_speculation=exit` | `adhoc` | as above | B.4: barriers issued equal exits, on KVM; AArch64 reads "decided" on TCG, labelled |
| `fast` | `restricted_fastpath=on` | `adhoc` | x86-64 only: no AArch64 fast path | B.5: Z2, Z3, Z4; the fast path's code review; 6093 (1)-(12); `test-linuxd`, `test-threads --smp 2`, `test-shell`, `test-rustc`; the flag's text says AArch64 holds none, nm shows it |
| `max` | `restricted_fastpath=off` | `adhoc` | | B.5: general path, nm shows no fast function |
| `fast` | `fault_ahead=on` | `adhoc` | E1, E3, E4, A6 | B.6: build E1 and E4, E3 on and off at the tip, A6's case |
| `max` | `fault_ahead=off` | `adhoc` | | B.6: `test-threads`, `test-container`, the walk's control silent |
| `fast` | `schedule=edf` | `adhoc` | code landed dark | At the flip: `test-flags` and AArch64's `test-shell`, `test-threads --smp 2` under it; a program row of the fast path's decline on a channel, with its control (decline removed); `test-kernel-symbols` re-read. `G.1`'s generic set over all three tiers with `windows` |
| `max` | `schedule=windows` | `no` | no code | S.1, S.2, S.3, S.6: `ferrix-sched-windows`, fixed priorities inside windows (C6), no task on a fair run queue (C3) |
| `max` | `kernel_memory=retyped` | `adhoc` | `Load(Copy(Fault))` unexplained; K16.5, K16.7, K16.9, F.8 | R.1 to R.5 |

## What refuses them after every value is `built`

| What | Next step |
|---|---|
| Done (this landing). LV.1: rule 3's second half fails `check`: `core` and `item` are at `present`, fsproto and blkclient at none | `level = "none"`, `target_level = "sc2"`/`"sc4"`; values, requires, stand-ins and boot options judged against the target; `component_problems` at the target printed as a note; line reads `level=none`; hand-editing `level` to the target fails `check`, controls FIRED |
| Done (this landing). LV.2: `fast`'s `slot_signing=required`, families `aarch64-uefi-virt` only | Read the rows' boot path; a development-signed slot at level none; x86_64-uefi-pc joins the list if x86-64 boots `fast`, with a check that a family lies inside `claim_architectures` at level != none |
| LV.3: `flag_nodes` keyed on `ferrix_level` would go dark at level none | Keyed on the profile: done. **Owed before the profile builds on AArch64**: the certified-list writer and K14.12's framing test, development-only |
| LV.4: `settings_writer=absent` needs lode reload's writer out (3623 R18) | Done: a build value of `lode`, refused in an `absent` image |
| LV.5: `test-flags`, `test-kaslr`, `test-netboot` under the profiles; a row's boot options against a profile's permitted set | Read and run each; `check-stack-depth` on `personality=user`, both architectures, at the first boot |
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

## §12's failing rows

Not owed for `built` (B.8) but each is a row in the sweep, by name, with cause: test-sud, test-wine's 32-bit half,
test-compositor, test-clipboard, test-usb-serial, test-quarantine, test-log-budget, test-log-pstore,
test-log-remote, test-removable, test-mce-driver, test-postmortem, test-selfhost, test-gnome-apps, test-powerfail.

## Complexity owed

`restricted::run`, +3 from AArch64's per-architecture helpers, joins 6923 (6)'s row of the worst three (ledger 7126 L1.8).

## Where it stands

* 2026-11-05: `fast` and `max` are still refused, by every line of the first table but `fs=server` on x86-64. Landed: the profiles' target, `built_on`, the signed slot, this file (9b5fa6957); `schedule=edf` dark and `lode`'s reload as a build value (fb32c5f77). AArch64's shim builds and runs Linux programs: `test-linuxd`, `test-shell`, `test-threads --smp 2` read PASSED under `--personality user` (`mkf-5-*`), `test-seat`, `test-init`, `test-btrfs` under `fs=server` (`mkf-s1-*`, older tree). None of it flips a `built`.
* What stands before `fast`, in order: B.3's rows on the flip's hash; B.4, which needs a user host for `restricted-check`; B.5's Z2-Z4, the fast path's code review and 6093 (1)-(12); B.6's E1, E3, E4 and A6; edf's program row on a channel; LV.3's writer and framing test for AArch64; LV.5. Before `max`, beside those: `windows` (ledger 7088 W.1-W.4, S.1-S.3: no code yet, ids reserved), and `retyped`'s R.1-R.5.
* Under `fast`'s values, ad hoc, x86-64 KVM, 732aa199d, through `gate.sh`: `test-btrfs`, `test-seat`, `test-threads --smp 2` PASSED; `test-container` FAILS, its own UTS namespace's `sethostname` (`runtime_settings=fixed`, to read); `test-vfs` FAILS, `dmesg`'s `klogctl` EPERM, 2 of 24 (the same to read); `test-net` FAILS, 1 of 16, the paced fetch's `/tmp/paced` (BACKLOG's `paced` row). Not `fast` itself: an ad-hoc build has neither its loader nor its image. Rows of the profile itself: a side commit that flips the values with no review, `feature/mk-flip-trial`, is the only way to read them today, and is not a flip.
