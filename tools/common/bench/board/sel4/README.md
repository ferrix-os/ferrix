# seL4 on the STM32MP157D-DK1 (board bench item B2)

seL4 has no STM32MP1 platform upstream. This directory adds one, `stm32mp1`,
as patches against the seL4 pins nazuna's x86-64 runs use. It also adds
`sel4rt`, the round trip timed with `../common/board-bench.h`, unchanged. The
plan and its fairness rules are in `docs/BOARD-BENCH.md`.

**Status (2026-10-08).** Everything here builds on nazuna, and the bench's
root task ran under QEMU on `qemu-arm-virt`. **Nothing has run on the board.**
The list of what only the board can tell is near the end of this file.

## Versions

| What | Version |
|---|---|
| seL4 | c6ce4d2a0c334cc9365b2cc41ff0126d75c0ea3c |
| seL4_tools | f1f63d93301cf491abc2d38ffb1a97803c218b31 |
| seL4_libs | 262a34dcb2f3285be01df9e5404d911132d567a6 |
| util_libs | 8dd23f736664fe61aefc25ea45ffff8127bcdf2b |
| musllibc | b0005f86fecbd6d0257b15363a5b013446914265 |
| sel4runtime | 86489cf6efab9f314964e79468c036e9035394c7 |
| sel4_projects_libs | fe2647c2582cd22a83e07491bb281ce061324b81 |
| projects_libs | dfee9caa847c4cc1af8ed11d978a964cb9ec49be |
| nanopb | cad3c18ef15a663e30e3e43e3a752b66378adec1 |
| sel4bench | 18f9d5f079bc23551eccbdc2202cae95c72c69d7 (sel4bench-manifest 80add415) |
| sel4test | b00d84fca8890e34f6007f104372fec5e82a23d1 (sel4test-manifest 555edd2b, the same kernel and libraries) |
| Device tree | Linux v7.2.9 `arch/arm/boot/dts/st/stm32mp157a-dk1.dts` (mainline has no `-d-dk1`; the D-DK1 is electrically the DK1) |
| Compiler | arm-none-eabi-gcc 14.2.1 20241119, GNU ld 2.45.50 (see *Toolchain*) |
| Tools | CMake 4.2.3, Ninja 1.13.2, Python 3.14.4 (`requirements.txt`), DTC 1.7.2, QEMU 9.2.4 |

## Files

| File | What |
|---|---|
| `fetch.sh` | Clones every repository at its pin into `<root>/src` and applies the patches. `NO_PATCHES=1` leaves the trees clean. |
| `build.sh <config>` | Builds one configuration into `<root>/build/<config>`. It prints, and writes, every configuration value that differs from the defaults. |
| `run-qemu.sh <config> <tag>` | Boots a `qemu-arm-virt` image with stdin from `/dev/null` and a timeout, and keeps the serial log. |
| `make-patches.sh` | Writes `patches/` back from an edited checkout. |
| `gen-dts.py` | Makes `stm32mp157a-dk1.dts` from Linux v7.2.9, as seL4's `update-dts.sh` makes its device trees. |
| `config-diff.py`, `list-tests.py` | Used by `build.sh`, and to list the tests that `sel4test` carries. |
| `patches/` | One patch per repository (below). |
| `configs/` | The configuration differences and the sel4test list of the images built on 2026-10-08, and `SHA256SUMS`. |
| `uboot-boot.txt` | The U-Boot lines for each image. |
| `requirements.txt` | The Python packages in the build's venv. |

`<root>` defaults to `~/.local/share/ferrix/board-bench/sel4` on nazuna. The x86-64 tree at
`~/.local/share/ferrix/sel4` is not touched.

```sh
mkdir -p ~/.local/share/ferrix/board-bench/sel4/scripts   # copy common/ and sel4/ here
cd ~/.local/share/ferrix/board-bench/sel4
python3 -m venv venv && venv/bin/pip install -r scripts/sel4/requirements.txt
bash scripts/sel4/fetch.sh
bash scripts/sel4/build.sh bench-dk1     # or bench-dk1-nofpu, test-dk1, bench-dk1-smp, bench-qemu(-nofpu), test-qemu
SECURE=on bash scripts/sel4/run-qemu.sh bench-qemu smoke
```

## The patches

| Patch | Repository | Licence of what it adds | Contents |
|---|---|---|---|
| `seL4-stm32mp1.patch` | kernel | GPL-2.0-only. `libsel4`'s `constants.h` is BSD-2-Clause. The device tree is Linux's (GPL-2.0-only). | `src/plat/stm32mp1/config.cmake` and `overlay-stm32mp1.dts`; `tools/dts/stm32mp157a-dk1.dts`; `libsel4/sel4_plat_include/stm32mp1`. It also adds two small changes, listed below. |
| `seL4_tools-stm32mp1.patch` | seL4_tools | GPL-2.0-only | The elfloader prints the mode it was entered in, and SCTLR and ACTLR as entered. |
| `util_libs-stm32mp1.patch` | util_libs | BSD-2-Clause | `libplatsupport` for `stm32mp1`: the U(S)ARTs, with UART4 as the default (the STM32MP2's polled driver, which writes CR before LF), and the generic timer's ltimer when the kernel exports the physical timer. |
| `seL4_libs-stm32mp1.patch` | seL4_libs | BSD-2-Clause | `libsel4bench`'s Cortex-A7 PMU events. sel4bench had none, so no Cortex-A7 platform could build it. |
| `sel4bench-stm32mp1-sel4rt.patch` | sel4bench | `apps/ipc` BSD-2-Clause; `apps/sel4bench/src/main.c` GPL-2.0-only | `sel4rt`; the run's end line and the board reset; on `stm32mp1`, SMP means two cores. |

The two small kernel changes outside the platform's own files:
- **The boot banner.** A debug kernel prints `SCTLR` and `ACTLR` after
  `Bootstrapping kernel`. This is in `src/arch/arm/kernel/boot.c`, under
  `CONFIG_PRINTING`, so it is not in a release kernel.
- **The PMU export.** On `stm32mp1`, `check_export_pmu` skips its probe of the
  Secure-only SDER register, which is UNDEFINED in the non-secure world. That
  is `src/arch/arm/armv/armv7-a/user_access.c`. PMUSERENR.EN is still set.

The platform is the Cortex-A7 template of `imx7` and `allwinnerA20`:
- `KernelArmCortexA7`, `armv7-a`, the GICv2 driver and `l2c_nop`;
- the generic timer at 24 MHz: the kernel uses the virtual timer, PPI 27 (`KERNEL_TIMER_IRQ 27`);
- `MAX_IRQ 287`;
- `seL4_UserTop 0xe0000000`, so the kernel window holds all of the RAM.

The overlay does three things:
- It names the kernel's devices, from the device tree:
  - `serial0`, which is UART4 at 0x40010000 (`st,stm32h7-uart`);
  - the GIC-400, distributor 0xA0021000 and CPU interface 0xA0022000;
  - the timer.
- It gives the elfloader UART4, PSCI and the timer, and gives both CPUs
  `enable-method = "psci"` (for the SMP image).
- It reserves OP-TEE's secure DDR: `optee@de000000`, 32 MiB, `no-map`. That is
  `CFG_TZDRAM_START` and `CFG_TZDRAM_SIZE` in the firmware's `optee-conf.mk`.

The generated memory map:
- RAM is 0xC0000000 to 0xDE000000, which is 480 MiB.
- Device untypeds cover 0x0 to 0xA0021000, 0xA0023000 to 0xC0000000, and 0xDE000000 up.
- The GIC is the kernel's alone.

seL4 already had the drivers this platform needs:
- the kernel's and the elfloader's UART driver, `st,stm32h7-uart`, upstream since the STM32MP2 port;
- the GICv2 and generic-timer drivers.

## Image format and boot

**An ELF, booted with `bootelf -p` and `go`.** This is seL4's default for
`imx7` and `allwinnerA20`. The ELF carries its own load address, so there is
no mkimage header to keep in step with the build.
- **Not `bootm`.** A uImage of type Linux would want a device tree, or ATAGs.
- **Not `go` on a raw binary.** That would also leave U-Boot's MMU and caches on.
- **The caches go off first.** This U-Boot's `bootelf`, v2026.07 in
  `lib/elf.c`, no longer turns the data cache off before it jumps. So the lines
  run `dcache off` and `icache off` first.
- **`bootelf -p` only loads, and `go` starts.** With `autostart` unset,
  `bootelf -p` copies the segment to its physical address and returns, and
  `go` jumps.

| Image (on bootfs) | Size | Load (file) | Segment (physical) | Entry |
|---|---|---|---|---|
| `bench/sel4/sel4bench.elf` | 752,984 B | 0xC2000000 | 0xC0140000-0xC0208047 | 0xC0147000 |
| `bench/sel4/sel4bench-nofpu.elf` | 752,984 B | 0xC2000000 | 0xC0140000-0xC0208047 | 0xC0147000 |
| `bench/sel4/sel4test.elf` | 3,511,588 B | 0xC2000000 | 0xC0490000-0xC07DD047 | 0xC0498000 |

The elfloader unpacks the kernel to 0xC0000000, and the root task just above
it. The images' SHA-256 values are in `configs/SHA256SUMS`, and the exact
lines are in `uboot-boot.txt`.

## Configurations

`configs/<config>.txt` lists every value that differs from the defaults. It
gives them twice: against the kernel configured alone for the platform, and
against the project (sel4bench or sel4test) given only the platform.

- **bench-dk1** (the card's `sel4bench.elf`):
  - release kernel, no printing, fast path on;
  - one core (`KernelMaxNumNodes 1`), no MCS, no hypervisor support;
  - `KernelArmExportPMUUser ON`, `KernelArmExportVCNTUser ON`;
  - sel4bench's own settings: `KernelTimerTickMS 1000`, `KernelTimeSlice 500`, `KernelRootCNodeSizeBits 15`, `KernelFWholeProgram ON`;
  - only the IPC app;
  - `LibSel4UtilsStackSize 1048576`, as on x86-64;
  - `AllowUnstableOverhead ON`, as sel4bench sets on x86-64, so an unsteady overhead cannot abort the run before its end line;
  - `LibSel4SerialServerColoredOutput OFF`, for plain lines.
- **test-dk1** (the card's `sel4test.elf`):
  - debug kernel with printing, one core, no MCS;
  - every test the build carries: `LibSel4TestPrinterRegex .*`, and no halt on a failure;
  - 125 tests on and 49 off, listed in `configs/test-dk1.tests.txt`. The ones off are MCS, SMP, domain, timer and hardware-debug tests.
  - There are no timer tests, because no user-level timer is built. The only
    one possible is the generic timer's physical half (CNTP, PPI 30), since the
    kernel owns the virtual one. Whether the non-secure world may use CNTP
    depends on CNTHCTL, which the secure firmware sets and the non-secure world
    cannot read; a trapped access would end the run. The port carries that
    ltimer: `EXTRA="-DKernelArmExportPCNTUser=ON -DKernelArmExportPTMRUser=ON"`
    builds it.
- **bench-dk1-nofpu** (the card's `sel4bench-nofpu.elf`): bench-dk1 with
  `-DSel4rtClientFpu=OFF`, so neither sel4rt thread has the FPU. It is a
  second, reported row beside the matched one; see *The client-FPU-off row*
  below. Its one difference from bench-dk1 is `Sel4rtClientFpu ON -> OFF`
  (`configs/bench-dk1-nofpu.txt`).
- **bench-dk1-smp:** bench-dk1 with both cores (`KernelMaxNumNodes 2`), for the
  second table. It builds and is not on the card yet.
- **bench-qemu, test-qemu:** the same on `qemu-arm-virt` with a Cortex-A15 in
  AArch32, for the smoke test. sel4test runs its simulation set there.
  `bench-qemu-nofpu` is bench-qemu with the client FPU off.

**The card's `sel4bench.elf` predates the client-FPU switch.** It was built
from the patches as of bf916aff2 (`sources=...+patches-a1eeb694df47`), and that
build is reproducible: rebuilt from those patches on 2026-10-08, it came out
byte-identical (SHA-256 990b1eee...). The patches with the switch build a
bench-dk1 that differs (`out/sel4bench-dk1-patches-3c7d1fe6.elf`, 7746ad92...,
the same size and entry). The sources string in its build line names the new
patches hash, and the line gains `client_fpu=on`, which moves the root task's
code and data. What it runs is the same: `configure_fpu(ctcb, true)` with the
switch at its default. The card keeps 990b1eee... for the matched row.

## sel4rt, the matched round trip

`apps/ipc/src/sel4rt.c` in the sel4bench patch is the ARMv7 port of the x86-64
`sel4bench-ferrix-rt.patch`. It runs first in sel4bench's IPC app.
- **The two processes.** A client and a server, each a process of its own (two
  address spaces), both at `seL4_MaxPrio - 1` on one core. As on x86-64, the
  client has the FPU and the server does not (`client_fpu=on`). The
  `bench-*-nofpu` builds take it from the client too; see *The client-FPU-off
  row*.
- **The round trip.** `seL4_CallWithMRs` with one word in a register, answered
  by `seL4_ReplyRecvWithMRs`. Every reply is checked for the echoed word,
  outside the timed part.
- **The series**, each `BB_WARMUP` untimed then `BB_SAMPLES` timed, through
  `BB_TIME` and `BB_RUN`:
  - `timer-floor`;
  - `call`;
  - `call.after.w<W>`: a call, then `bb_touch(buf, W, 4096)` over the client's
    own 4 KiB pages, for W = 1, 16, 64 and 256;
  - `base.w<W>`: the same touch without a call.
- **Getting the samples out.** The client is a text-only clone, so it keeps
  each series on its stack. It ships the series to the IPC app over the result
  endpoint, then waits until it has been printed, so nothing prints while it
  measures. The IPC app prints with `bb_print`.
- **The output.** First come the build line (the sources and the
  configuration), a line saying user mode cannot read SCTLR or ACTLR, the
  `pmuserenr` line and the `clock` line. Then the series, then
  `sel4rt echo-mismatches <n>`, then `RT done`.
- **The end of the run.** After sel4bench's own JSON and `Fin`, the root task
  prints `board-bench end sel4rt`. On `stm32mp1` it waits about 0.1 s on the
  virtual counter, then writes 1 to RCC_MP_GRSTCSETR (MPSYSRST).
  - The reset uses MPSYSRST, not the IWDG2 watchdog. The watchdog's APB clock
    may be off on this firmware: OP-TEE has `CFG_STM32_IWDG=n`, and U-Boot
    `WDT_STM32MP=n`. Turning it on would be a clock write.
- **Arm state.** The IPC app is built `-marm`, and `sel4rt.c` refuses to build
  in Thumb. All of seL4's own C is compiled with `-march=armv7-a -marm`.

### The client-FPU-off row

**The matched row stays client-FPU-on.** The x86-64 runs give the client the
FPU and not the server, and `sel4bench.elf` does the same. That is the row the
comparison with Ferrix uses.

**Why a second row.** seL4 at c6ce4d2a decides FPU ownership per TCB flag.
`lazyFPURestore` runs at every switch, the ARMv7 fast path included:
- for a thread with `seL4_TCBFlag_fpuDisabled`, `disableFpu` clears FPEXC.EN;
- for the thread that owns the FPU, `enableFpu` sets it.

Each is a VMRS and a VMSR of FPEXC. With the client on and the server off, the
round trip turns the FPU on and off once each. Ferrix's bench programs never
touch VFP. A seL4 user whose code is the same would give both threads the
flag, so seL4's best case is a row of its own.

**How it is built.** The CMake option `Sel4rtClientFpu` (default ON) becomes
`SEL4RT_CLIENT_FPU`. With it OFF, `configure_fpu(ctcb, false)` runs as well as
`configure_fpu(stcb, false)`. The build line says which it is, as
`client_fpu=on` or `client_fpu=off`. `build.sh bench-dk1-nofpu` and
`bench-qemu-nofpu` build it. Neither thread uses VFP: the IPC app is
`general_regs_only` and soft-float.

**What it does not remove (argued from the source).** `disableFpu` still runs
at each switch. It reads FPEXC and writes it back with EN clear, even when EN
is already clear. So the off row makes as many FPEXC accesses as the matched
row. What it saves:
- the owner check, `nativeThreadUsingFPU`;
- turning EN on and off: each write leaves EN as it was.

Whether a VMSR that leaves FPEXC unchanged costs less on the Cortex-A7 than
one that toggles EN, only the board can tell.

**QEMU (2026-10-08, nazuna, QEMU 9.2.4, `SECURE=on ICOUNT=1`).** With
`-icount shift=0`, the cycle counter follows the instructions. Measured p50,
every sample the same:

| Series | client_fpu=on | client_fpu=off |
|---|---|---|
| `call` | 365 | 362 |
| `call.after.w256` | 1392 | 1389 |
| `base.w256` | 1032 | 1032 |

That is 3 instructions fewer a round trip, the owner check's. Under TCG
(no icount), `call` p50 was 6049 ns on and 6179 ns off. TCG timings say
nothing about either the board or the FPEXC cost.

## QEMU smoke (2026-10-08, nazuna, QEMU 9.2.4)

QEMU has no STM32MP157. The same root task, built for `qemu-arm-virt` with a
Cortex-A15 in AArch32, ran with `SECURE=on`. The first lines and the result
lines (ns at QEMU's PMU clock, 1 GHz):

```
ELF-loader started on CPU: ARM Ltd. Cortex-A15 r4p0
  entered in mode 0x13 (SVC), SCTLR=0xc50078 ACTLR=0x0
sel4rt build sources=seL4-c6ce4d2a0c33+patches-a1eeb694df47 platform=qemu-arm-virt cpu=cortex-a15 kernel=release fastpath=1 nodes=1 mcs=0 hyp=0 printing=0 pmu_user=1 vcnt_user=1 tick_ms=1000 timeslice=500 stack=1048576 isa=arm
sel4rt pmuserenr=0x00000001 en=1
sel4rt clock cpu_hz=1000260149 cntfrq=62500000
sel4rt timer-floor n=20000 min=149 p50=169 p90=169 p99=219 mean=168
sel4rt call n=20000 min=5818 p50=6128 p90=9987 p99=10517 mean=6897
sel4rt call.after.w1 n=20000 min=5868 p50=6048 p90=6158 p99=10707 mean=6325
sel4rt base.w1 n=20000 min=149 p50=159 p90=159 p99=159 mean=157
sel4rt call.after.w16 n=20000 min=7058 p50=7228 p90=7308 p99=9207 mean=7297
sel4rt base.w16 n=20000 min=159 p50=169 p90=179 p99=179 mean=173
sel4rt call.after.w64 n=20000 min=10587 p50=10877 p90=11277 p99=34371 mean=11798
sel4rt base.w64 n=20000 min=219 p50=249 p90=249 p99=269 mean=246
sel4rt call.after.w256 n=20000 min=24583 p50=25373 p90=26483 p99=51116 mean=26621
sel4rt base.w256 n=20000 min=509 p50=559 p90=769 p99=989 mean=599
sel4rt echo-mismatches 0
RT done
All is well in the universe.
board-bench end sel4rt
```

These are TCG figures and say nothing about the board. What the smoke test does show:
- **The instructions counter reads 0.** Every `.ins` line is 0, because QEMU
  prohibits event counting in the Secure world (MDCR_EL3.SPME is clear). The
  board runs non-secure.
- **The run is deterministic with `-icount`.** With `ICOUNT=1`, the cycles are
  exact: `timer-floor` 6, `call` 365, `call.after.w256` 1392, `base.w256` 1032.
- **QEMU needs `SECURE=on` for the bench.** QEMU reads DBGDSCR.NS as 0, so a
  stock PMU-exporting kernel thinks it is Secure. Started non-secure, it touches
  SDER and takes an undefined instruction at boot (seen: `KERNEL DATA ABORT`).
  The `stm32mp1` kernel skips that probe. The `qemu-arm-virt` kernel is left as
  upstream, hence `SECURE=on` there.
- **sel4test ran in HYP.** It was entered in HYP (`VIRT=on`), the state U-Boot
  could leave on some boards, so the elfloader's `leave_hyp` path ran:
  - `entered in mode 0x1a (HYP)`;
  - the kernel booted non-secure SVC: `SCTLR 0xc5387d ACTLR 0x0`;
  - in 540 s, 44 tests passed and none failed. That was the time limit, not
    the end of the suite. The image was built from these patches before their
    last edits, which changed only comments and `stm32mp1`-only code.
- **No Cortex-A7 smoke.** seL4's `qemu-arm-virt` accepts `ARM_CPU=cortex-a7` in
  CMake, but its libsel4 header stops with `unsupported core`, so there is no
  Cortex-A7 smoke test.

## What the images write (for the product owner's OK)

In the format of `tools/common/bench/board/card/REGISTERS.md` (os-07, B4).

### seL4 (B2): the STM32MP1 port

- **The elfloader**, in both images, before the kernel:
  - UART4 0x40010000:
    - CR1 (+0x00) is read-modify-written three times: UE off; TE and FIFOEN on; UE on;
    - CR2 (+0x04) once, clearing STOP for one stop bit;
    - then TDR (+0x28) once per character, polling ISR (+0x1C).
    - BRR is not touched, so U-Boot's 115200 stays.
  - CP15 only otherwise:
    - its boot page tables (TTBR0, TTBCR, DACR, CONTEXTIDR);
    - SCTLR (MMU and caches on);
    - cache, TLB and branch-predictor maintenance;
    - CNTVOFF, only if entered in HYP mode, and then the switch to SVC.
  - No GIC and no PSCI. The SMP image would also call PSCI CPU_ON, an SMC to
    OP-TEE, for CPU1.
- **The kernel:**
  - UART4 0x40010000: the debug image (sel4test) writes TDR per character,
    polling ISR. The release image (sel4bench) maps UART4 and never touches it.
  - GIC-400 distributor 0xA0021000:
    - at boot:
      - GICD_CTLR 0, then 1;
      - all-ones to every ICENABLER and ICPENDR;
      - IPRIORITYR 0 for the SPIs and for the first word;
      - ITARGETSR: every line to this CPU;
      - ICFGR from IRQ 64 up 0x55555555 (level);
      - IGROUPR 0, which is RAZ/WI from the non-secure world;
      - all-ones to CPENDSGIR;
    - while running: ISENABLER and ICENABLER bits for the interrupts in use,
      the virtual timer PPI 27, and ICFGR if a user sets a trigger;
    - GICD_SGIR in the SMP image only.
  - GIC-400 CPU interface 0xA0022000:
    - GICC_CTLR 0, then 1;
    - GICC_PMR 0xF0 and GICC_BPR 3;
    - a GICC_IAR read and a GICC_EOIR write per interrupt.
  - CP15 and CP14:
    - the virtual timer (CNTV_CVAL, CNTV_CTL);
    - CNTKCTL: PL0VCTEN in sel4bench, 0 in sel4test;
    - PMUSERENR.EN in sel4bench;
    - JMCR, JOSCR, TEECR and TEEHBR, if Jazelle or ThumbEE are implemented;
    - VBAR, TTBR0, the ASIDs, FPEXC, and cache and TLB maintenance.
    - No SDER, and no other debug register, on `stm32mp1`.
- **The root tasks:**
  - sel4bench:
    - UART4 from user mode, through a device frame: TDR per character, polling ISR;
    - the PMU from user mode: PMSELR, PMXEVTYPER, PMOVSR, PMCR and PMCNTENSET (board-bench's `bb_pmu_start` and sel4bench's own `sel4bench_init`); reads of PMCCNTR, PMXEVCNTR, PMUSERENR and CNTVCT;
    - at the end, once: **RCC_MP_GRSTCSETR 0x50000404 = 0x1 (MPSYSRST)**, a system reset, about 0.1 s after `board-bench end sel4rt`.
    - The RCC page (0x50000000, 4 KiB) is mapped from a device untyped. 0x50000000 lies in the root task's device range 0x0-0xA0021000. It is neither RAM nor a kernel-only device; only the GIC is kernel-only.
  - sel4test: UART4 from user mode as above. It holds a cap to one device
    frame at the bottom of the first device untyped. Its two FRAMEDIPC tests
    use the cap and never map it.
- No clock, power, PHY, PMIC or IWDG write. The one RCC write is a reset.

## What only the board can tell

1. **Output on UART4.**
   - the elfloader's;
   - the debug kernel's;
   - the root tasks', through `libplatsupport` and the stm32h7 driver's
     register offsets (ISR +0x1C, TDR +0x28).
2. **The mode U-Boot leaves.** It should be non-secure SVC: U-Boot runs
   "in trusted mode" with `ARMV7_NONSEC` unset. The elfloader prints the mode.
   HYP entry, through `leave_hyp`, was tested under QEMU only.
3. **OP-TEE coexistence.** seL4 never uses 0xDE000000-0xDFFFFFFF as RAM, but
   that range is in a device untyped. Neither image maps it. A user mapping
   would meet the TZC-400.
   - Also open: whether OP-TEE's monitor, PSCI and secure interrupts leave a
     running seL4 alone, and whether seL4's GIC set-up works from the
     non-secure side. The interrupts must be in group 1, set by OP-TEE: seL4's
     IGROUPR writes are ignored there, and its priority writes are banked.
     Ferrix's virtual timer works on this firmware, which suggests PPI 27 does.
4. **CNTFRQ.** It must be 24 MHz, which the kernel's `TIMER_FREQUENCY` assumes.
   The `clock` line prints `cntfrq` and the measured `cpu_hz` (650 MHz
   expected; TF-A clocks the A grade's PLL1).
5. **The PMU from user mode, non-secure.**
   - the instructions event (0x08) counting, which QEMU could not show;
   - `pmuserenr ... en=1`;
   - the `.ins` lines non-zero.
6. **ACTLR.SMP and the caches.** The elfloader prints ACTLR, and SCTLR as
   entered. The debug kernel prints both as it runs.
7. **MPSYSRST from the non-secure world** (RCC non-secure under this OP-TEE)
   resetting the board after the end line.
8. **The U-Boot lines.** `dcache off`, `icache off`, `bootelf -p` loading
   without starting (with `autostart` unset), and `go`.
9. **sel4test's 125 tests on hardware.** The cache tests run there, not under
   QEMU.
10. **The SMP image.** It is built but has never run: PSCI CPU_ON through
    OP-TEE.

## Toolchain

`arm-none-eabi-` 14.2.1, not nazuna's `arm-linux-gnueabihf-` 15.2:
- **seL4's user code is soft-float.** It builds with `-mfloat-abi=soft`: its
  `-march=armv7-a` names no FPU, so its hard-float probe fails.
- **gnueabihf cannot link it.** Its `crtbegin.o` and `libgcc.a` are hard-float
  only, and ld refuses to link them with soft-float objects.
- **arm-none-eabi can.** Its `thumb/v7-a/nofp` multilib is soft-float. Its
  libgcc helpers are Thumb-2; nothing in a timed path calls them.

Two more workarounds:
- seL4's musllibc build runs `rm -f src/.git`, so `fetch.sh` keeps musllibc's
  repository outside the tree, through `--separate-git-dir`.
- `fetch.sh` links `tools/nanopb` to `nanopb`, for sel4test.
