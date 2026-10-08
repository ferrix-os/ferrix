# What each image on the card touches (for the product owner's OK)

The rule since 2026-09-26: hardware use needs the product owner's OK on the
exact registers a boot writes, and a power-domain or PHY write needs the
owner's own word. This is that list for the board matrix's card, complete with B2's seL4
rows (2026-10-08).

## Firmware: unchanged

TF-A v2.14, OP-TEE 4.10.0 and U-Boot v2026.07 as in
`~/.local/share/ferrix/dk1-firmware`, partitions 1 to 3, not rewritten.
U-Boot's only writes during a rotation:
- `fatwrite` into bootfs (partition 4): FERRIX/KERNEL.ELF, INITRD.IMG and DEFAULTS.TXT;
- `fdt` edits of Linux's device tree in RAM.

Its saved environment is not written (no `saveenv`).

## Ferrix (B3's image: main plus the measurement-only `ipc-bench.pmu`)

- **Unchanged from main's DK1 boots**, which ran on this board from 2026-09-13 (stages 1 to 9) and as the HDMI desktop: the drivers binding:
  - `st,stm32h7-uart` (UART4 0x40010000);
  - `arm,cortex-a7-gic` (GIC-400, distributor 0xA0021000, CPU interface 0xA0022000);
  - `st,stm32mp1-rcc` (0x50000000), `st,stm32mp1,pwr-reg`, `st,stm32mp1-exti`, `st,stm32mp157-pinctrl`;
  - `st,stm32mp1-usbphyc` and `generic-ehci` (USB host);
  - `st,stm32-ltdc` and `sil,sii9022` (HDMI);
  - `st,stm32-tamp` (the reboot mode word);
  - PSCI through OP-TEE (SYSTEM_RESET, CPU_ON at `--smp 2`).
- **New, measurement only:**
  - CP15 PMUSERENR (EN = 1), and the PMU's own control registers (PMCR, PMCNTENSET, PMSELR, PMXEVTYPER, PMOVSR) from user mode;
  - CNTKCTL.PL0VCTEN (landed, b17462efe).
  
  No memory-mapped register is added.

## Linux 7.2.9 (B1): unmodified mainline

- `multi_v7_defconfig` with Linux's own `stm32mp157a-dk1.dtb`, so every mainline driver for an enabled node probes. The enabled nodes are:
  - UART4;
  - I2C1, with the HDMI bridge, the codec and USB-C;
  - I2C4, with the STPMIC1;
  - SDMMC1;
  - ETH;
  - the USB host, OTG and PHY;
  - LTDC, the audio peripherals and the ADC;
  - the RTC, the IWDG2 watchdog and the thermal sensor;
  - the RNG, the hash and the CRC units;
  - the M4 remoteproc and its mailbox;
  - the CoreSight blocks.
- **Power: Linux's mainline STPMIC1 regulator driver writes the PMIC over I2C4** (regulator enable and voltage, per the tree's constraints). This is the one power-domain write on the card, and it needs the owner's own word. It is what every mainline Linux boot on a DK1 does. Ferrix does not drive the PMIC.
- Added: `pmu-user.ko` writes CP15 PMUSERENR (EN = 1) on each online CPU. It writes no memory-mapped register.
- It ends with `reboot -f`: PSCI SYSTEM_RESET through OP-TEE.
- `cpufreq.off=1`, and the tree has no OPP table, so the MPU clock stays at TF-A's 650 MHz.

## seL4 (B2): its STM32MP1 port (os4b/b2-sel4 bf916aff2, sel4/README.md)

- **Elfloader:** UART4 (0x40010000).
  - CR1 is read-modify-written three times: UE off, TE and FIFOEN on, UE on.
  - CR2 is written once, for one stop bit.
  - TDR is written per character, polling ISR.
  - Otherwise CP15 only. No GIC or PSCI in the one-core images.
- **Kernel:**
  - UART4 TDR, in the debug image (sel4test) only.
  - GIC distributor 0xA0021000 at boot: CTLR, ICENABLER, ICPENDR, IPRIORITYR, ITARGETSR, ICFGR, IGROUPR (ignored from non-secure) and CPENDSGIR. At runtime, ISENABLER and ICENABLER.
  - GIC CPU interface 0xA0022000: CTLR, PMR 0xF0, BPR 3, and IAR and EOIR per interrupt.
  - CP15: the virtual timer, CNTKCTL and PMUSERENR. Not SDER, which is Secure-only and whose probe the port skips.
- **Root tasks:** UART4 TDR from user mode, and the PMU from user mode.
- **The reset:** sel4bench's root task, after `board-bench end sel4rt` and about 0.1 s, writes 0x1 to RCC_MP_GRSTCSETR (0x50000404, MPSYSRST): a system reset. It is the one RCC write, a reset rather than a clock or power change. IWDG2 is not used, because its APB clock may be off on this firmware.
- sel4test does not reset: it ends with its summary, and the board needs a reset press.
- **None:** no clock, power, PHY, PMIC or IWDG write.
