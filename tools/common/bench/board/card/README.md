# The DK1 card and its rotation (B4)

One micro-SD card carries the firmware and every kernel the board matrix
compares (`docs/BOARD-BENCH.md`). Nothing about the firmware changes: TF-A,
OP-TEE and U-Boot stay as `~/.local/share/ferrix/dk1-firmware` built them,
in partitions 1 to 3. Only `bootfs` (partition 4, FAT32, 128 MiB) gets new
files.

## bootfs

| Path | What | Who builds it |
|---|---|---|
| `EFI/BOOT/BOOTARM.EFI` | Ferrix's loader, the same for every Ferrix image | B3 / `cargo xtask flash --stage` |
| `FERRIX/KERNEL.ELF`, `INITRD.IMG`, `DEFAULTS.TXT` | the active Ferrix image: the loader reads these fixed paths | copied in by U-Boot from `bench/ferrix/<tag>/` |
| `bench/ferrix/<tag>/` | one Ferrix image per tree measured (`main`, and each branch alternated with it) | B3, B6 |
| `bench/linux/` | Linux 7.2.9's `zImage`, its DK1 device tree, the initramfs with `lbench` and the B5 programs | B1, B5 |
| `bench/sel4/` | the sel4bench (release) and sel4test (debug) images | B2 |

The customer's desktop image (`FERRIX/` as it was) is copied off the card
before anything is written, and put back when the bench round ends.

## Putting files on the card

1. `gather.sh <sources> <stage>` (Git Bash) copies each listed host directory
   into a local stage laid out as bootfs. Every host directory carries its
   own SHA256SUMS, checked after the copy, and the stage gets one
   SHA256SUMS over all its files.
2. With the card in a reader, `stage-card.ps1 -Drive <letter> -Stage <stage>`
   reports what it would do. Add `-Backup <dir> -Apply` to back the card up
   and write; add `-ClearFerrix` when the desktop image must make room.
3. After the round, `stage-card.ps1 -Drive <letter> -Restore <backup> -Apply`
   puts the card back as it was.

## The boot lines

`card.plan` holds, per image, the U-Boot lines that boot it from the card.
`rotate.ps1` types them one prompt at a time (two lines back to back
overflow U-Boot's UART input while it is busy), and never writes U-Boot's
saved environment.

Ferrix's loader has fixed paths, so a Ferrix image becomes the active one by
`fatwrite` from its `bench/ferrix/<tag>/` directory before `bootefi`. That
is the only write to the card during a rotation.

## A run

Every bench prints `board-bench end <prog>` after its last line and resets
the board itself:
- Linux: `reboot -f`, PSCI SYSTEM_RESET through OP-TEE;
- seL4: the root task starts IWDG2 (0x5A002000);
- Ferrix: `ferrix.onexit=reset`.

The next autoboot then starts the next image, so a rotation runs with no hand
at the board. If a kernel powers the board off or hangs, `rotate.ps1` beeps
and asks for a reset press or a USB-C replug, and records the boot as lost.

```
powershell -NoProfile -File rotate.ps1 -Plan card.plan -Order linux,sel4,ferrix-main -Rounds 5 -Out <logs>
```

Each invocation writes a new `run-NNN/` under `<logs>`:
- `raw.log`, every byte;
- `boot-NNN-<name>.log`, one boot each;
- `INDEX.tsv`, one line per boot with its verdict (`end`, `timeout`, `died: …`, `uboot-error`);
- the plan as it was.

`cargo xtask bench-ipc --board stm32mp157d-dk1 --board-log boot-NNN-ferrix-*.log … --record`
reads Ferrix's boots.

`-DryRun` prints what would be sent and opens no port.

## The first session, in order

Each step needs the one before it; a step that fails stops the session
there, and its logs say why.

1. **Card.** `gather.sh` everything built; `stage-card.ps1` backs the card
   up and writes it (`-ClearFerrix` if the desktop must make room).
2. **Board check.** At U-Boot: `printenv kernel_addr_r fdt_addr_r
   ramdisk_addr_r` against `card.plan`'s addresses; then each image boots
   once with `-Rounds 1`: `linux`, `sel4test` (the port's correctness, debug
   build), `sel4`, `ferrix-main`. Every boot must end in `board-bench end`
   and come back to U-Boot by itself.
3. **Main table, one core.** `-Order linux,sel4,ferrix-main -Rounds 5`.
4. **Ferrix's profile.** `ferrix-prof` three times, beside one more
   `ferrix-main` for the uninstrumented figure.
5. **Both cores.** `linux-smp2`, seL4's SMP build and Ferrix without
   `nosmp`, five rounds.
6. **B6 changes,** each alternated with `ferrix-main` as its base, five
   rounds a side.
7. **Restore** the desktop with `stage-card.ps1 -Restore`.

Every run directory stays as written. The records go through
`bench-ipc --board stm32mp157d-dk1 --board-log … --record` for Ferrix,
and the Linux and seL4 lines are kept beside them.
