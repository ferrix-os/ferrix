# Handover: stage 20's matrix built by Ferrix, and one kernel for every test (2026-10-04)

Wound down on the customer's order, relayed by the product owner (ferrix-d7).
Branch `selfhost-matrix`, pushed to GitHub; resume from its head (this commit).
Base: `main` 528cea144. 17 commits on top.

## What the branch holds

**Item change (needs the consultant; OK IF, ledger lines 328, 331-333, 340, 343).**
Each test's init program, `sh -c` script and command list moved out of the
kernel into the image's initramfs under `.ferrix/init/`, so one kernel serves
every test (`src/kernel/build.rs` embeds nothing):

- `src/kernel/src/init.rs`: `InputEntry`, `set_inputs` (taken once),
  `judge` (refusals, each printed and taken as absent), `inputs()`;
  `init::run`'s order unchanged.
- `src/kernel/src/init_check.rs`: the `inputs` boot check (FX-1503), the
  verifier of `L.init.2`, `L.init.3`, `H.BOOT.15`.
- `src/kernel/src/fs/mod.rs` hands the archive's `.ferrix` entries to
  `set_inputs` (an empty set when there is no initrd) and refuses a root where
  `/.ferrix` exists after the unpack; `fs/root_disk.rs` refuses the same on the
  volume.
- `src/lib/fs/vfs/src/initramfs.rs`: `unpack` never creates `.ferrix/**`;
  `init_entries` lists them (SAFETY-MANUAL AoU-24, its host test).
- xtask: `cargo::Kernel` carries its init into `fat::write_image*` and
  `flash::run` (the card and Pixel/DK1 staging path), which add the inputs
  and read them back, refusing an image that lacks one.
- Requirements: `H.BOOT.15` (part 13), part 25 with `L.init.1-3`
  (`L.init.4` reserved and not written, released with the rest), AoU-24,
  ITEM.md §2, MEMORY-AND-TIMING, VULNERABILITY-ANALYSIS V-12, A.FIRMWARE,
  TEST-TIME.md item 1. TRACEABILITY.md and the coverage files regenerated.

**Stage 20 harness (outside the item).** `FERRIX_BUILDS=plan:` records every
build without booting; `selfhost-matrix.sh plan|record|replay` (57 rows,
`build-apps` first, logs in `DIR/<mode>-run`); script apps recorded as builds;
`test-selfhost --plan` vendors the plan's workspaces and a workspace whose git
crate collides apart.

## What passed

- Host: xtask tests (502+), kernel clippy on three targets, ferrix-vfs tests,
  check-traceability, check-item-boundary, the coverage and panic-catalog
  generators' `--check`.
- Boots by hand: test-boot x86_64 (the `inputs` line), test-init x86_64
  (installs on the root disk with the new check), test-shell x86_64 --init musl
  busybox, test-vfs x86_64 (32 commands).
- Measured (plan mode, 57-row matrix): 271 builds to 188, 66 kernel builds to
  6, 36 min with no boots; the gap: plan mode misses 23 of 153 distinct
  non-kernel builds (made after a test's first boot).
- Controls on 5e2eca4b2 (spec `~/.local/share/ferrix/logs/linit-controls.md`,
  invocations `linit-launch.sh` beside it on nazuna): b, c1-c7 and e FIRED.

## What is owed, in order

1. **Every control again on the landing hash.** 5e2eca4b2 failed `cargo fmt
   --check` in `src/lib/fs/vfs/src/tests.rs` (fixed in 0eda22d00), which is
   why e-host and g did not fire; a, f and f2 still need their re-runs as the
   spec's "Re-runs" section says (a: expect-line `before the guest printed`;
   f, f2: the slot needs `build-apps --arch x86_64 --app sshdt` first, pin with
   `GATE_SLOT`). Run `cargo xtask check` on the head first (it was stopped at
   the wind-down, not seen to pass).
2. The batch: `batch.sh join <session> selfhost-matrix selfhost-matrix --gate
   ~/.local/share/ferrix/selfhost-matrix/batch-gate.txt` (the extra lines the
   PO asked for), then `batch.sh wait`. The PO lands a PASSED stack only after
   the one-line report: all controls FIRED and the four boots' transcripts
   equal main's apart from the `inputs` line and the sizes.
3. Consultant conditions of line 343: as above, plus carry-coverage after the
   final rebase, boot-21b's `H.BOOT.10-14` reconciled by whichever lands
   second (line 333), and the landing message naming the harness commits as
   outside the item.

## Next, after it lands

- Tests build every variant before their first boot (approved by the PO), so
  plan mode is complete.
- The weekly CI workflow: plan mode, Ferrix makes the builds in a guest of
  about 8 GB, replay of the rows a runner boots; `shards.sh` in
  `~/.local/share/ferrix/selfhost-matrix/` runs a replay in 5 shards.
- The Arm C programs (cross gcc and three Rust targets in the toolchain) and
  the script apps (Mesa's mako and glslang, btop's LLVM runtime) on Ferrix.

## Traps

- A plan belongs to its tree: any change, a docs edit included, means
  recording again.
- `test-init` in a gate slot needs `sshdt` built in that slot.
- The generated architecture files conflict on every rebase: take either side
  and run `tools/common/gen/gen-arch-doc.py`.
- carry-coverage after a commit that wrote the coverage files: `--from HEAD`.
