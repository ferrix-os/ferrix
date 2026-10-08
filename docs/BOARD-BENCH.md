# The board matrix: Linux, seL4 and Ferrix on one DK1

The IPC comparison runs on bare metal: three kernels on one STM32MP157D-DK1,
each given the same firmware, clock, cores, memory and bench code. This file
says what runs, how the board is shared fairly, what is measured, and who
builds what.

**Status:** being built (2026-10-08). Nothing has run on the board yet. os-07, the IPC benchmark agent, owns items B1 to B6 from os-4b's handover; the Who column says who did or does each.

## Decision (customer, 2026-10-08)

- **The main target is the DK1, bare metal.** On a board there is no
  hypervisor between any kernel and the hardware, so no exit, no host
  scheduler and no host clock policy can favour one kernel.
- **nazuna (x86-64 under KVM) is a side target.** The x86-64 work
  continues there, but it is no longer the main target.
- **The Pixel 7 is not used.**
  - seL4 has no platform for its SoC, and mainline Linux does not
    support it.
  - Ferrix runs there only as a crosvm guest under pKVM, which brings a
    hypervisor back.
- **The rule is unchanged:** Ferrix's round trip is at or below seL4's on
  the same hardware.

## The competitors

| Kernel | Version | Build |
|---|---|---|
| Linux | 7.2.9, the stable release of 2026-10-03, from kernel.org with its signature checked | `multi_v7_defconfig` unchanged; the mainline DK1 device tree |
| seL4 | c6ce4d2a with sel4bench-manifest 80add415, the commit nazuna's x86-64 runs use | Release, no printing, fast path on, single core for the main table. There is no STM32MP1 platform upstream, so we add one. The Cortex-A7 platforms `imx7` and `allwinnerA20` are the templates; the GIC-400 and generic timer drivers exist already. |
| Ferrix | the tree under test | the DK1 image, as `bench-ipc --board stm32mp157d-dk1` builds it |

**LionsOS does not run here.** It is built on seL4 Microkit, which supports
only AArch64 and RISC-V; LionsOS adds x86-64, and the DK1 is 32-bit.
LionsOS's own boards are the Odroid-C4 and the MaaXBoard. A LionsOS stage
would need one of them and a bare-metal AArch64 Ferrix port, so it is not in
this plan.

## Same resources for every kernel

| Resource | How it is made equal |
|---|---|
| Firmware | One SD card: the TF-A, OP-TEE and U-Boot of `~/.local/share/ferrix/dk1-firmware`, built once. All three kernels sit on the card. A U-Boot script boots the one named in an environment variable, which the driver sets over serial before each boot. Rotation (A, B, C, A, B, C) needs no card swap. |
| Clock | The MPU clock the firmware sets, for all three kernels. Linux boots with `cpufreq.off=1`, so it cannot raise it. Every run prints cycles against the system counter, and a run whose ratio differs from the others is reported, not used. |
| Cores | **Main table, one core:** Linux `maxcpus=1`, seL4's single-core build, Ferrix with one processor. **Second table, both cores:** cross-core figures, with seL4's SMP build. |
| Memory | The same device tree reserved regions (OP-TEE's) and the same RAM size for every kernel. |
| Caches and predictors | Each kernel's `SCTLR` and `ACTLR` are printed once per boot, to show the same caches and coherency. The Cortex-A7 is not affected by Spectre v2 or BHB, so no kernel needs a predictor flush; each kernel's setting is recorded anyway. |
| Bench code | One C source per benchmark with a thin layer per OS, built with the same compiler and flags. Linux and Ferrix run the identical static binary for the POSIX benchmarks. |
| Counters | The bench reads `PMCCNTR` (cycles) and the instructions-retired event itself, around every sample. Each kernel sets `PMUSERENR.EN` for it: seL4 through `KernelArmExportPMUUser`, Linux through a small out-of-tree module, Ferrix through a measurement-only switch. The system counter's 24 MHz ticks are about 42 ns, too coarse to time a round trip with alone. |

## What is measured

The four levels:
1. **The call.** Round-trip cycles, instructions, and p50, p90 and p99 in
   ns. It is split into the hardware floor (two entries and exits, two
   address-space switches) and the software above it.
2. **The cost after the call.** The caller's extra cycles per call while
   its own work touches 1, 16, 64 or 256 pages between calls.
3. **Calls per operation.** Round trips per POSIX operation; a monolith
   makes none.
4. **Programs.** lmbench and SQLite's `speedtest1`, as a ratio to Linux.

| | Linux 7.2.9 | seL4 | Ferrix |
|---|---|---|---|
| Round trip | null syscall; pipe, Unix socket and futex ping-pong, two processes on one core | `seL4_Call` / `seL4_ReplyRecv`: sel4bench, and the matched root task | `channel_write_read` (`domain-call`) |
| Cost after the call | yes | yes | yes |
| Calls per operation | 0 | n/a (no POSIX) | kernel counter |
| lmbench, `speedtest1` | yes | n/a | yes, the same binary |

Every run records what `.claude/skills/optimize-ipc-round-trip/SKILL.md` §7
lists. The records go through `bench-ipc --board-log ... --record`.

## Work items

| Item | Who | Points (estimate) |
|---|---|---|
| B1. Linux 7.2.9 for the DK1: kernel, device tree, initramfs, PMU module, `lbench` ported from `rdtsc` to `PMCCNTR` | os-4b's agent; os-07 since. Built, QEMU smoke passed (os4b/b1-linux); timed calls through lbench's own A32 svc (os07/b1-linux) | 3–4 |
| B2. seL4 STM32MP1 platform; sel4test (debug) and sel4bench (release) images; the matched root task on ARMv7 | os-4b's agent | 8–12 |
| B3. Bench suite: the after-the-call sweep for all three kernels, the Ferrix PMU switch (measurement only), and one output format | os-4b's agent | 8–10 |
| B4. The card: firmware and the three kernels, the U-Boot selection script, and the serial driver for rotation | os-07: the driver, staging and plan are written (os07/b4-card, tools/common/bench/board/card/); the card waits for the customer | 4–5 |
| B5. lmbench and SQLite static ARMv7 builds, run on Linux and Ferrix | os-07's agent (os07/b5-posix), musl 1.2.5 static, -marm | 3–4, more if Ferrix lacks a syscall |
| B6. Ferrix's ARMv7 round trip, in skill §6's order: counter (landed, b17462efe), baseline on the board, stub clobbers and reset, ASIDs with the lazy TLB, 3b, profile, then the fast path's Arm design | os-07's agents, the consultant first for each kernel change: ASIDs (os07/asid, OPAQUE-KERNEL §9.13), user state (os07/ustate, §9.14), the profile build (os07/prof-armv7, measurement only) | 25–40 |

B1 to B3 need no board: they build on nazuna and are smoke-tested under
QEMU where a machine exists. B4 onward needs the card in a reader, done by
the customer (this PC cannot see the board's USB-C OTG port, CN7). Every
touch of the board needs the product owner's OK and an address list
(skill §6). No write persists outside the card.
