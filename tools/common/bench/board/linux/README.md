# Linux 7.2.9 on the DK1 (B1)

Linux's side of the board matrix (`docs/BOARD-BENCH.md`): the kernel, the
module that gives user mode the PMU, and `lbench`, the Linux round trips timed
by `../common/board-bench.h`.

| File | What |
|---|---|
| `build.sh` | Download and check the source, configure, build, and pack the card's files. `build.sh` runs every step; `build.sh clean` then removes the source and build trees. |
| `smoke-qemu.sh` | Boots the card's zImage and initramfs on QEMU's `virt` with one Cortex-A7. |
| `lbench.c` | `/init`: loads the module, prints the clock and the setup, runs the series, prints `board-bench end lbench`, resets. Its header lists the series. |
| `pmu-user/` | `pmu-user.ko`: sets PMUSERENR.EN on every CPU and prints SCTLR, ACTLR and CNTKCTL; restores PMUSERENR on unload. Measurement only. |
| `boot-linux.cmd` | The U-Boot lines, one command per line: one core (the main table), then both cores. |

Outputs go to `~/.local/share/ferrix/board-bench/linux/` on nazuna (or
`$BB_LINUX_OUT`): the card's files in `out/card/bench/linux/`, the record in
`out/versions.txt`, QEMU logs in `logs/`.

## What was built (2026-10-08)

| | |
|---|---|
| Source | `linux-7.2.9.tar.xz`, sha256 `b4c5dfbe51a364a6c7f03869200f88c8e1f77403539005f14b7fc6bc91b8d8ba`, the line in kernel.org's `sha256sums.asc` (signed by the checksum autosigner, B8868C80BA62A1FFFAF5FDA9632D3A06589DA6B1); the tar signed by Greg Kroah-Hartman, 647F28654894E3BD457199BE38DBBDC86092693E |
| Compiler | `arm-linux-gnueabihf-gcc (Ubuntu 15.2.0-16ubuntu1) 15.2.0`, GNU ld 2.46 |
| Config | `multi_v7_defconfig`, unmodified; `.config` sha256 `b4d915368fb6946f0458d31bb2333c4c4919382ae95106ec17881f487552921b`. Voluntary preemption, HZ=100, no LPAE, ARM (not Thumb-2) kernel. |
| Device tree | `st/stm32mp157a-dk1.dtb`: mainline has no `stm32mp157d-dk1`. The firmware is the a-dk1 build, and OP-TEE reports "RCC is non-secure", which is what this tree (not `-scmi`) expects. |
| CPU clock points | None. Neither CPU node names an OPP table and the tree has none, so there is no cpufreq driver: the CPUs carry `clock-frequency = 650000000`, and the MPU runs at the 650 MHz TF-A sets (U-Boot prints "MPU : 650 MHz"). |
| Kernel targets | `zImage dtbs modules_prepare`; the defconfig's own modules are not built. `pmu-user.ko` is checked against `vmlinux.symvers`. |
| `lbench` | `-O2 -static -marm -mcpu=cortex-a7`: ARM code. The C library's `syscall`, `read` and `write` in Ubuntu's `libc.a` are Thumb-2, so the timed calls switch mode into them. gcc's default here is Thumb-2. |

The card's files (bootfs, `mmc 0:4`), as built; zImage's hash changes with
every build, because the build time is in it:

| File | Bytes | sha256 |
|---|---|---|
| `bench/linux/zImage` | 12407296 | `a56bfecfa423733b501e225eced41eae855e2d2ffc7e13cf44fc17e599b23b84` |
| `bench/linux/stm32mp157a-dk1.dtb` | 55854 | `1fe9b1fadcc777d7d8322b770ce05ff46176a01b2c21fe34ff9e472ea6220764` |
| `bench/linux/initramfs.cpio.gz` | 258960 | `e1da016727c8fa9c44bd4c115b1cc7bc035b4e418853fe5adc0cafc3dcb13631` |

## Booting it

`boot-linux.cmd` holds the lines. They load at U-Boot's stm32mp15 defaults
(0xc2000000, 0xc4000000, 0xc4400000), far below OP-TEE's 0xde000000-0xdfffffff,
and use Linux's own tree, not `${fdtcontroladdr}`. That tree reserves nothing
for OP-TEE. This U-Boot copies OP-TEE's reserved-memory nodes into the tree
it boots, and five `fdt` lines also add the `-scmi` tree's
`optee@de000000` node. Main table: `maxcpus=1 cpufreq.off=1`; the second entry
drops `maxcpus`.

The first lines of a run are the clock line, the kernel version, the module's
SCTLR/ACTLR/CNTKCTL line per CPU and `pmuserenr: ... en=1`. Then come the
clock check, `/proc/iomem`'s RAM ranges and the reserved-memory nodes the
kernel got.

## QEMU smoke (2026-10-08)

`smoke-qemu.sh` on QEMU 9.2.4 (`-M virt -cpu cortex-a7 -smp 1 -m 512`): the
module loads (`PMUSERENR=00000000->00000001`, `CNTKCTL=000000c6`, so
PL0VCTEN is set, as Linux's arch timer leaves it), every series runs, and the
run ends with `LB done`, `board-bench end lbench` and `reboot: Restarting
system` in 36 s. QEMU's cycle counter is its virtual clock at 1 GHz, and it
counts instructions only with `QEMU_ARGS='-icount shift=0'`. With that, the
touch of W pages takes 4W + 4 instructions, as the loop is written. No QEMU
figure stands for the board.

## Not yet seen

- Nothing here has run on the board (B4).
- That U-Boot copies OP-TEE's nodes, and that the `fdt` lines are accepted:
  read lbench's `reserved-memory` and `iomem` lines on the first boot.
- `pmu-user` covers the CPUs online when it loads; a CPU brought up later is
  not covered. Neither boot entry does that.
