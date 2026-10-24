# Ferrix on a small machine

How small a machine Ferrix can run on, what stands in the way today, and the
plan to fit it into 8 to 16 MiB. The target that started this is a Sipeed Tang
Nano 20K running a RISC-V core: a GW2AR-18 FPGA with 8 MiB of SDRAM. A trimmed
Rocket RV64IMAC fits that FPGA, at 73% of its logic and 47 MHz. The RAM is what
does not fit (§1).

Nothing here depends on the RISC-V port, which does not exist yet. Every step
is done and gated on armv7a and aarch64 under QEMU, and a RISC-V port inherits
it.

## 1. Where the memory goes (2026-09-27, release, armv7a)

Measured from the release kernel's symbols (`nm -S`, grouped by crate), on
`main` at bfc2e4d3.

| What | Size | Notes |
|---|---|---|
| Kernel text, rodata, data and bss | 5.7 MiB | aarch64: 5.2 MiB. An image built with `--init` adds the program: busybox is 0.9 MiB |
| of which self-checks | 1.44 MiB (21%) | `ferrix.checks=skip` skips running them, but every one is compiled in |
| of which `syscall` and `fs` | 0.97, 0.80 MiB | The core; they stay |
| of which display and render, audio, input | 256, 83, 57 KiB | Not on a small board |
| of which network (kernel and crates) | about 370 KiB | Optional |
| of which btrfs (read and write crates) | about 320 KiB | Optional with a RAM root |
| Fixed buffers: audit store, log, early pool | 296, 128, 64 KiB | Sized for a desktop |
| The kernel file as the loader reads it | 98 MiB | 66 MiB of it is debug information, read into memory whole |
| The initramfs | 6.4 MiB | |

## 2. Phase 0: measure (this landing)

`--strip-kernel` (`tools/common/xtask/src/fat.rs`) puts the kernel on the image as `flash`
writes it to a card: armv7a's is 8.2 MiB rather than 98, and aarch64's is
4.5 MiB. It is off unless asked for. The kernel now prints free and managed
memory at the end of boot, beside stage 2's line.

Since 2026-10-24 it strips the initramfs's programs too, as `flash` does.
Built in the release profile with full DWARF, they had grown to 38 MiB of
the x86-64 archive, lode alone 19, and the 128 MiB row ran out of memory in
the self-checks. Stripped, they come to a few MiB.

The sweep: `test-boot --release --strip-kernel --smp 1 --memory N`, with
self-checks on.

| Arch | Lowest that boots | Free at the end of boot | What fails below it |
|---|---|---|---|
| armv7a | 128 MiB | 82 of 113 MiB managed: the kernel and its checks use about 31 | 64 and 48: the loader, allocating the initramfs. 32 and below: U-Boot, out of memory before the loader starts |
| aarch64 | 128 MiB | 73 of 105 MiB | 64 and below: EDK2 refuses to start (`ASSERT [MemoryInit]`, it needs 128 MiB) |
| x86_64 | 128 MiB | 75 of 107 MiB | 64 and 48: the loader, allocating a file. 32 and below: OVMF page-faults in its own start-up, before the loader |

Under QEMU the firmware sets the floor, as much as Ferrix does. The loader's
share is its peak: it holds the kernel file, the placed kernel and the
initramfs at once, and frees none of them before the kernel starts.
CI's boot job keeps a 128 MiB row on each architecture (`Boot in 128 MiB`), so
the floor cannot rise unseen.

The kernel's own floor still has no number. Measuring it needs a boot path
without U-Boot or EDK2 (phase 1), which is also what an FPGA board has:
OpenSBI, then the kernel.

## 3. The plan, in points

| Phase | Step | Points |
|---|---|---|
| 0 | `--strip-kernel` and the end-of-boot memory line | 2, landed |
| 0 | The sweep above | 2, done |
| 0 | The 128 MiB CI row | 2, landed |
| 1 | The loader reads only the loaded segments, straight to where they go, rather than the whole file and then a copy | 3 |
| 1 | A boot path without firmware under QEMU (`-kernel`), to measure the kernel's own floor | 3 |
| 1 | A minimal initramfs (init and a shell) or none | 1 |
| 2 | The self-checks as a build option, at the seams `checks::run()` already marks: about 1.4 MiB | 5 |
| 2 | Subsystems as build options: display, render, audio, input, network, btrfs write, PCI, IOMMU: about 1 MiB | 8 |
| 2 | `opt-level = "s"` for the small profile, overflow checks kept, measured | 1 |
| 3 | Fixed buffers sized per build: audit store, log, early pool, per-CPU reserves | 3 |
| 3 | Runtime memory: heap growth, page cache, per-process costs, from phase 1's measurements | 5 |

The expected result is about 3 MiB of kernel on armv7a (2.8 on aarch64 or
RV64). That makes 16 MiB comfortable. 8 MiB would leave about 5 MiB for the
heap and programs: possible, and phase 1's numbers will say.

## 4. Open decisions (the customer's)

1. Cargo features on the kernel, or an xtask `--small` build on top of them.
2. How to gate a build without self-checks. The proposal: gate the small
   profile with its checks at 64 MiB, and use the build without them only on
   the board.
3. What the small profile may drop: display, audio and btrfs write, and
   whether network is optional.
