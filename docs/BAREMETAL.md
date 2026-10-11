# Bare metal: boot breadcrumbs

`ferrix.breadcrumbs` on the command line makes the loader and the kernel draw a
40x40 px block per early step in a row at the top-left of the GOP framebuffer
(8 px in, 48 px apart). The last block drawn is the last step passed; a red
block after it means a panic, red at position 1 means ExitBootServices never
succeeded (orange there = it needed a retry). Off by default: no draw, no mapping.

| Pos | Step | Colour |
|---|---|---|
| 0 | loader: framebuffer + cmdline, before ExitBootServices | blue 2040FF |
| 1 | loader: ExitBootServices ok (orange FF8000 = retried, red = failed, loader spins) | cyan 00C0FF |
| 2 | loader: about to load CR3 and jump | white |
| 3 | kernel: boot info valid, framebuffer mapped | green 00FF00 |
| 4 | kernel: early console configured | yellow FFFF00 |
| 5 | kernel: hand-off printed | magenta FF00FF |
| 6 | kernel: stage 1 self-check passed (panic screen exists from here) | dark green 008000 |
| 7 | kernel: trap vectors installed | olive 808000 |
| 8 | kernel: memory (stage 2) up, boot console started | purple 800080 |
| 9 | kernel: vmap arena up | spring green 00FF80 |
| 10 | kernel: interrupts and clocks (stage 3) | light yellow FFFF80 |
| 11 | kernel: timer seen | pink FF80FF |
| 12 | kernel: processors (stage 4) | lime 80FF00 |
| 13 | kernel: scheduler (stage 5), last crumb | lavender 8080FF |

## State (2026-10-11, branch baremetal-hang, WIP, unlanded)

1. Done and builds: loader+kernel breadcrumbs; EBS retried 4x with 64-descriptor map slack; loader refuses CR4.LA57, clears CR4.CET at the jump, prints CR0/CR4/EFER when breadcrumbs are on; 16550 probe (LSR!=0xFF + scratch) skips an absent COM1 and leaves its IRQ unrouted.
2. Proven under QEMU/OVMF KVM: normal `cargo xtask test-boot` passes (stages 1-12); a breadcrumbs+fbcon boot draws all 14 blocks and boots OK; `-serial none` boots; a bochs-display boot did not get a framebuffer above 4 GiB (OVMF still put it at 0x80000000), so the >4 GiB aperture path is not reproduced.
3. Not done: certification consultant's conditions Q1-Q9 (ledger line 745: ITEM.md/reference config entry, off-path check + gate control, mapping attribute checks, check_early_mapper must still map itself, panic block after first report line, G3/R5 "skipped: no COM1", A.FIRMWARE AoU for LA57/CET, L.* ids + coverage, all-arch test-boot rows, BACKLOG row). Not run: cargo xtask check, aarch64/armv7a boots.
4. Most likely causes from reading the code: a stage-1 fatal or a fault before init_traps (silent before the panic screen exists), or ExitBootServices refused twice with the old 8-descriptor slack (error lost after the console was shut). The crumbs will tell which.
5. Next: build a slot/ESP image with `--kernel-option ferrix.breadcrumbs --kernel-option ferrix.fbcon`, install on the WD the way the coordinator did, photograph the top-left row; then meet Q1-Q9 and land.
