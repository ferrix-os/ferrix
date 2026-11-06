# Pixel 7 USB device driver: handover

**Status, 2026-09-26 night (ferrix-9c): done.** During a native boot
the phone presents a USB serial port and the monitor streams the boot
and `ferrix-statd` from it live, proven on the phone (run `usblog2`).
§8 is where it stands and what the phone said. The rest of this
document is the brief as it was written: why it is wanted, what the
hardware is, what Ferrix already has to build it from, the rules that bound
it, and the order to do it in. Read `src/boot/vendor/google/pixel7/HANDOVER.md` first;
it is the phone's own state, and its rules apply here unchanged.

**Who decides what, since 2026-09-26:** hardware use is the product owner
session's (`ferrix-2c`) alone, as the owner said. Each boot needs its OK on
an address list checked against `panther.dts`; each phase that writes a
register needs a new one. A power-domain or PHY isolation write would need
the owner's own word, which the survey shows is not needed.

## 1. Why, and what done looks like

When the Pixel 7 boots Ferrix natively (`fastboot boot` from example), nothing
reaches the PC until the run is over. Android is gone, Ferrix has no USB, and
so the phone is off USB for the whole run. Ferrix's console and the stat
service's samples (`src/user/system/linux/statd/`, `ferrix-statd`) go to the `ramoops` record in RAM,
which Android reads back after the watchdog reset. `tools/vendor/google/pixel7/monitor` then
loads the record and fills its graphs after the fact. The owner wants them
live.

Done means: **during a native boot, Ferrix presents a USB serial port (CDC-ACM)
on the phone's USB-C port, example sees it as `/dev/ttyACM*`, and the monitor
streams the boot's stages and `ferrix-statd`'s lines from it live**, as it
already does for a crosvm guest's 16550. The `ramoops` record stays as the
record of a run that fails before USB is up.

A guest of the phone's own crosvm (the monitor's "Run in a VM") already
streams live. This is only for native boots.

## 2. The rules

These come from the owner, who has lost a device's touchscreen calibration to
an agent before. `src/boot/vendor/google/pixel7/HANDOVER.md`, "Never write anything that
survives a reset", is the full text.

* **Only volatile state may be written**: RAM, and SoC controller registers
  such as DECON's, or here the DWC3's and the USB PHY's own.
* **Never** the PMIC or regulators, **power domains**, fuses, security
  blocks, UFS, or the USB-C port controller (the MAX77759 TCPC at I²C `0x25`).
  Never `fastboot flash`, `erase` or `oem`.
* The PMU's USB PHY isolation bit and the HSI0 power domain (§3) are power
  domains. **Writing either needs the owner's word first.** Ask before that
  step, not after.
* Before any test that writes hardware, list the exact addresses, check them
  against `~/.local/share/ferrix/pixel7/panther.dts`, tell the owner, and make
  each write conditional on the hardware being as expected (as
  `src/boot/vendor/google/pixel7/src/display.rs`'s `take_over` does).
* Architecture: the kernel enumerates devices and drives none of them
  (`docs/ARCHITECTURE.md` §1 and §7; `tools/common/data/device-access-allowlist.json` is
  the gate). The controller is driven by a ring-3 driver. The kernel does only
  what the chip shares, as `src/kernel/extensions/board/stm32mp1/src/platform/st/stm32mp1/usb.rs` does for the DK1.

The phone is shared with other sessions (`phone-link-9f`'s PhoneLink app,
`dev.phonelink`, stays installed, with its data). `adb devices` listing it
does not mean it is free: list the peer sessions, and ask the owner before
every native boot.

## 3. The hardware

From ABL's device tree (build CP2A.260705.006), saved as
`~/.local/share/ferrix/pixel7/panther.dts` (and `.dtb`). Line numbers are that
file's.

| Block | Where | What the tree says |
|---|---|---|
| USB wrapper | `usb@11210000` (l. 14335) | `samsung,exynos9-dwusb`, 64 KiB at `0x1121_0000`, SPI `0x17b` (379) level-high, `power-domains` pd-hsi0, clocks `aclk`/`sclk`/`bus` |
| DWC3 core | child `dwc3` (l. 14362) | `synopsys,dwc3`, same window and interrupt, **`dr_mode = "peripheral"`**, `maximum-speed = "super-speed-plus"`, no `dma-coherent`, `memory-region` `xhci_dma@97000000`, and the quirks `snps,dis-u1-entry-quirk`, `snps,dis-u2-entry-quirk`, `snps,usb2-gadget-lpm-disable`, `snps,quirk-frame-length-adjustment = <0x20>` |
| USB PHY | `phy@11200000` (l. 14388) | `samsung,exynos-usbdrd-phy`, windows `0x1120_0000`+`0x200`, `0x110f_0000`+`0x2800` (combo PHY), `0x1110_0000`+`0x800`, and the core's; SPIs `0x178` and `0x176`; reference clock 19.2 MHz (`0x124f800`); `phy_version 0x301`, `sub_phy_version 0x404`, `has_combo_phy`; `hs_tune`/`ss_tune` tables (hs disabled) |
| PMU | `system-controller@18060000` (l. 12798) | `samsung,gs101-pmu`. The PHY's `pmu_offset = 0x3eb0`, so its isolation control is **`0x1806_3eb0`** (DP's `0x1806_3eb4`). Power domain. |
| Power domain | `pd-hsi0@18062080` (l. 7219) | `samsung,exynos-pd`, 32 bytes at `0x1806_2080`. Power domain. |
| Clocks | `clock-controller@1e080000` (l. 12845) | `samsung,gs201-clock`; the core uses IDs `0x22f` (aclk), `0x227` (sclk, and the PHY's `phy_ref`), `0x3a` (bus) |
| DMA guard | `s2mpu_hsi0@11070000` (l. 9537) | `google,s2mpu`, 64 KiB: the stage-2 MPU that decides what memory HSI0's masters, the USB among them, may reach |
| DMA pool | `xhci_dma@97000000` (l. 273) | `shared-dma-pool`, 4 MiB at `0x9700_0000`, `no-map`: Android's host-mode pool |
| USB-C | `max77759tcpc@25` (l. 8152) | the `extcon` both nodes name. **Never touched.** |

The interrupt is SPI 379, so GIC INTID 411.

What the tree cannot tell, and phase 1 has to find:

* **What ABL leaves.** ABL runs fastboot over this controller, in device
  mode, until it jumps to the loader. The phone drops off USB the moment
  it does, so ABL at least stops the controller or disconnects. Whether the
  PHY is still powered and un-isolated, the clocks still on, and the power
  domain up decides the whole job. If they are, Ferrix need only reprogram
  the controller (volatile). If not, bringing them back is the
  power-domain write that needs the owner (§2).
* **The S2MPU.** Whether ABL leaves HSI0 able to reach arbitrary RAM, or only
  some region. Ferrix's IOMMU layer knows VT-d and SMMUv3, not the S2MPU. If
  it is restrictive, the driver's buffers must live where it allows. Android
  uses `xhci_dma@97000000`, and ABL may use yet another region.
* **Reading a block that is unclocked or powered down can hang the bus.**
  That is not persistent, since the watchdog resets the phone with the log
  intact, but read the power domain's and the clock gate's state before the
  controller's.

## 4. What Ferrix already has

* **The device model.** The kernel publishes a device tree node for a binding
  it knows (`src/kernel/src/device.rs`: `BoardBinding`, `BoardDevice`,
  `DmaShape`). `src/user/system/native/devmgr`'s `TREE_DRIVERS` table maps the binding to a
  driver, which `devmgr` starts in a job of its own with the node's
  apertures and interrupt (`docs/DEVMGR.md` §3). Bindings are numbered in
  `src/lib/proto/native-abi/src/types.rs`: `TREE_STM32_HDMI = 1`, `_USBH = 2`,
  `_GPU = 3`. A new one takes 4.
* **The precedent to copy.** The DK1's USB host:
  * `src/kernel/extensions/board/stm32mp1/src/platform/st/stm32mp1/usb.rs` turns on clocks, resets, regulators and the
    PHY's PLL, with values read back from U-Boot, and publishes the node.
  * `src/user/system/native/drivers/usb/usbhid` drives the EHCI controller.
  * `src/lib/drivers/usb/usb-host` is the host-testable logic, with a register model under
    `src/tests/model.rs`.
  * `docs/INPUT.md` §7 is its design.

  The DWC3 is device-side, which nothing in the tree is yet, but the split is
  the same.
* **DMA.** `DmaShape`'s coherence flag (the DWC3 has no `dma-coherent`). The
  pinned VMOs `vmo_pin`, with `PIN_COHERENT` for uncached memory.
  `arch::clean_for_device` and `arch::flush_for_device`.
* **The phone's board support.** `src/kernel/src/platform/google/gs201/watchdog.rs` covers the
  two watchdogs only. The kernel feeds them while it runs, and ends a run by
  letting one fire (`reset_now`), which keeps the `ramoops` record.
* **The console.** `src/kernel/src/arch/aarch64/console.rs` has three backends:
  PL011, 16550 over MMIO, and the phone's `ramoops` zone. `ferrix-statd`
  writes its `FERRIX-STAT` lines to its standard output, which as pid 1 is
  the console. `src/user/system/linux/statd/README.md` has the format. How a ring-3 USB driver
  gets the console's bytes is a design decision still to make (§5, phase 4).
* **The loader.** `src/boot/vendor/google/pixel7`, which runs with the MMU off, so all
  memory is Device memory: aligned, word-sized, volatile accesses only. It
  is where a read-only survey is cheapest (`display.rs` reports DECON's
  registers the same way).

## 5. The plan

Each phase ends with something run on the phone and written down here.

1. **Survey, read only.** In the loader, log to `ramoops` the power
   domain's status, the clock gates, the PMU word `0x1806_3eb0`, the S2MPU's
   control registers, the PHY's first words, and the DWC3's global and
   device registers. The DWC3 offsets below are from Linux's
   `drivers/usb/dwc3/core.h`; check them before relying on them:
   * global: `GSNPSID` `0xc120`, `GCTL` `0xc110`, `GUSB2PHYCFG(0)` `0xc200`,
     `GUSB3PIPECTL(0)` `0xc2c0`, `GEVNTADRLO(0)` `0xc400`;
   * device: `DCFG` `0xc700`, `DCTL` `0xc704`, `DEVTEN` `0xc708`, `DSTS`
     `0xc70c`.

   `GSNPSID` names the core's version. `DCTL`'s run/stop bit and `DSTS`
   say what ABL left. No writes. The result decides everything after, so
   write it here.
2. **Kernel plumbing.** A binding, `TREE_GS201_DWC3`, published by the
   Pixel's board support with the core's aperture, SPI 379 and a
   non-coherent `DmaShape`. Only the shared parts go in the kernel, and only
   if phase 1 says they need touching. The power domain and PMU writes wait
   for the owner.
3. **The driver.** A host-testable library, say `src/lib/drivers/usb/dwc3`: event buffer,
   TRB rings, event decoding, and endpoint 0's control state machine, tested
   against a register model as `src/lib/drivers/usb/usb-host` is. Device descriptors and the
   CDC-ACM class go in `src/lib/drivers/usb/usb-device`. The ring-3 program, say
   `src/user/system/native/drivers/usb/usbdev`, goes in `devmgr`'s table. Run at high speed (USB 2.0) first:
   `DCFG` can hold the core there, which keeps the combo SuperSpeed PHY out of
   the first bring-up. The endpoints are endpoint 0, one bulk IN and one bulk
   OUT for ACM data, and an interrupt IN for ACM notifications.
4. **Console over USB.** Decide how the driver gets the bytes. Kernel log
   records to a ring-3 reader (`sys_syslog` returns nothing today)? A tty
   the driver serves, which statd opens? Or both? Then send the boot's lines
   and statd's.

   *Decided, and the kernel half built (2026-09-26, branch
   `pixel7-usb-log`):* a kernel log. `src/kernel/src/console/log.rs` keeps every
   byte the console sends -- the kernel's lines and programs' output, statd's
   included, before CRLF -- in a static 128 KiB ring, less the lines that
   print the kernel's layout. The driver reads it by capability:
   `device.log_control()` (`LOG_CONTROL_CREATE`, 0x1051) on its
   `TREE_GS201_DWC3` node gives a channel, READ `{ max }` is answered with
   DATA `{ lost, bytes }` of up to `MAX_DATA` (4072) bytes from the oldest
   byte still kept (`src/lib/proto/logctl`), one reader at a time, and the claim ends
   when the channel closes. `syslog(2)` reads the same log, privileged.
5. **The monitor.** `tools/vendor/google/pixel7/monitor` reads `/dev/ttyACM*` while the
   phone is in Ferrix and feeds the same parser it uses for a guest
   (`vm-line`). The owner is in `dialout`, and `cdc_acm` loads on demand.

QEMU has no model of a DWC3 in device mode, so the tests are the register
model (phase 3) and the phone.

## 6. The phone loop

`P=~/.local/share/ferrix/pixel7`. Two scripts there, outside the repo:

* **`WT=<worktree> $P/build-run.sh <name>`** builds `<worktree>`'s aarch64
  image with `--app statd`, the loader with the payload, and
  `$P/<name>/boot.img`. It keeps the tree's diff, the commit, and a debug
  kernel beside it. It refuses an existing `<name>`.
* **`$P/boot-run.sh <name>`** runs `adb reboot bootloader`, `fastboot stage
  vendor_boot.img`, `fastboot boot`, waits for Android, and saves the
  `ramoops` record as `$P/<name>/run.log`. Nothing is flashed.

The launcher's helper (`tools/vendor/google/pixel7/helper.py`) does the same
cycle over HTTP, with `POST /boot?stats=N` for the stat service. The monitor
drives it.

A native run takes about 75 s back to Android, plus any stat service time.
Each run leaves the phone locked. adb and `su` work while it is locked, and
the unlock steps are the owner's to give. A hang ends in a watchdog reset
with the log kept.

## 7. Before starting

* This is a new roadmap item. It needs a `docs/BACKLOG.md` entry, and the
  product owner session (`ferrix-32`) has the say on where it goes. (Done:
  the P2 row, owner ferrix-9c; the PO is now `ferrix-2c`.)
* Tell the owner before phase 1's first boot, which is read-only. Ask the
  owner before any power-domain write.

## 8. Where it stands

### What the phone said (phase 1, run `usb-survey2`, 2026-09-26 18:24)

One RAM boot of the loader's survey (`src/boot/vendor/google/pixel7/src/usb.rs`),
approved by the PO with the S2MPU struck from the list (a security block:
it is not read at all). Back in Android after 77 s, `FERRIX-BOOT-OK`. The
record is `~/.local/share/ferrix/pixel7/usb-survey2/run.log`.

**ABL leaves everything on.** The HSI0 domain is up, the PHY is out of
isolation, the clocks run, and the controller answers, in device mode and
halted. So the driver needs no power-domain, isolation, clock or PHY write:
only the DWC3's own registers, which are volatile.

| Register | Value | What it says |
|---|---|---|
| PMU `0x1806_3EB0` (USB PHY) | `0x3` | bit 0 set: out of isolation, Samsung's `ENABLE` |
| PMU `0x1806_3EB4` (DP) | `0x1` | |
| `pd-hsi0` `+0..+0x10` | `1 1 0x10 0 1` | configured on, status on (as `pd-disp` reads) |
| `GSNPSID` | `0x3331_3130` | `DWC_usb31` |
| `GHWPARAMS0` | `0x4020_400A` | dual-role, AXI, 64-bit data and addresses |
| `GHWPARAMS3` | `0x1042_0086` | SuperSpeed Gen2 PHY, UTMI high-speed PHY, 32 endpoints of which 16 IN |
| `GCTL` | `0x0001_2004` | port capability device |
| `GSTS` | `0x7E80_0020` | current mode device; **`CSR_TIMEOUT` set** (bit 5), left by ABL: clear it |
| `GUSB2PHYCFG(0)` | `0x0010_2400` | `U2_FREECLK_EXISTS` clear, `SUSPHY` clear |
| `GFLADJ` | `0x0A87_F020` | 30 MHz adjustment `0x20`, as the tree's quirk asks |
| `GEVNTADRLO/HI(0)`, `GEVNTSIZ(0)`, `GEVNTCOUNT(0)` | `0xF8CD_D000`, 0, `0x200`, 0 | **ABL's event buffer, 512 bytes in RAM Ferrix now owns**: the driver must point it at its own before running |
| `GTXFIFOSIZ(0..3)`, `GRXFIFOSIZ(0)` | `0x43`, `0x0043_0493`, `0x04D6_0493`, `0x0969_0493`; `0x413` | ABL's FIFO layout |
| `DCFG` | `0x0020_0BCC` | SuperSpeed, address 121 (the PC's, for fastboot), 16 `NUMP`, no LPM |
| `DCTL` | `0x00F0_0000` | **run/stop clear**: ABL stopped the controller |
| `DEVTEN` | `0x7` | disconnect, reset, connect done |
| `DSTS` | `0x00D2_C1A4` | halted and idle, link `SS.Disabled`, last connected at SuperSpeed |
| `DALEPENA` | `0x3` | endpoint 0's two directions still enabled |

The PHY windows' first words read `0x0302_0241 ...` (link), `0x6 ...`
(combo) and `0x0009_0606 ...` (high-speed); none is decoded yet, and none
needs to be written.

The kernel binding (phase 2) was in the same image and published its node:
`usb      DWC3 0x33313130 at 0x11210000, left on by ABL`, one aperture and
one vector. devmgr started nothing, since `usbdev` was not yet built in.

### Done: the port streams the kernel's log to the monitor (2026-09-26)

What §1 asks for works on the phone. During a native boot Ferrix presents
a CDC-ACM port on the USB-C port, example sees it as `/dev/ttyACM0`
(`1209:0001`, "Ferrix console"), and `tools/vendor/google/pixel7/monitor` streams the
boot and `ferrix-statd`'s samples from it live: its boot card fills stage
by stage, the Ferrix tab graphs the samples as they arrive, and the port
going away ends the stream and keeps it as a `usb-<time>` run record.

Three RAM boots, each approved by the PO (`ferrix-2c`), every register
write logged to `ramoops` and checked against the approved list:

| Run | Image | What it showed |
|---|---|---|
| `usbdev1` | `d7110880` | First writing boot. The guard read the state the survey recorded, then 543 writes, all in the DWC3 window. Enumerated at high speed about 50 s into the run, configured; the host's line echoed back and a heartbeat came every second. The event buffer and TRBs sit in Ferrix's own pinned pages (`0x9016_7000`...), so **the S2MPU passes HSI0's DMA** |
| `usblog1` | `9b5cd5f4` | The kernel log over the port: the monitor showed the boot's twelve stages, `FERRIX-BOOT-OK` and `ferrix-statd` live. Two bugs, both fixed after it: usbdev logged every write, and with the log going over USB each line sent made five more; and the monitor, started with `setsid`, took the port as its controlling terminal and was killed by `SIGHUP` when the phone left |
| `usblog2` | `c97c4afb` | Both fixes on the phone. The monitor, started with `setsid` as before, streamed the run and outlived the port, keeping `usb-20260926-174123/run.log`: 199 lines, `FERRIX-BOOT-OK`, 122 `FERRIX-STAT` samples. 49 writes logged, where `usblog1` logged 53,697; the `ramoops` record 115 KB rather than 1.99 MB |

Every write in every run was to one of 31 offsets of the DWC3 window:
`C110 C118 C200 C400-C40C C700 C704 C708 C720 C800-C85C`. The PO's standing
OK covers further boots with that list and the same guard (`GSNPSID` a
DWC3, device mode, run/stop clear, halted), each still with the phone's
other users asked first; any other offset or block needs a new OK.

What each part is, now on `main`:

* `src/boot/vendor/google/pixel7/src/usb.rs`: the read-only survey, in every record.
* `src/kernel/extensions/board/gs201-usb/src/platform/google/gs201/usb.rs`: `TREE_GS201_DWC3 = 4`, published only when
  `pd-hsi0` reads on and `GSNPSID` names a DWC3; nothing written.
* `src/lib/drivers/usb/usb-device`: chapter 9 and the CDC-ACM function.
* `src/lib/drivers/usb/dwc3`: the controller, tested against a register model
  with a write-back cache in front of its memory, which caught one real
  missing invalidate. It refuses to write unless the controller is as
  ABL leaves it, and replaces ABL's event buffer before it runs.
* `src/user/system/native/drivers/usb/usbdev`: the ring-3 driver, devmgr's `Gadget` kind.
  It asks for the log only while a host holds the port open (DTR), and
  asks again only once the last piece has gone, so a slow or absent host
  leaves the log in the kernel's ring. What the host sends is dropped.
* The kernel log (phase 4, designed with the certification agent,
  ferrix-55): `src/kernel/src/console/log.rs`, a static 128 KiB ring of every
  console byte less the kernel's layout (the KASLR slide, trace frames,
  trap registers); `syslog(2)` reads it, privileged for every action, so
  `dmesg` works; `src/kernel/src/interfaces/logctl` serves it over `LOG_CONTROL_CREATE`,
  which only this controller's node may call, one reader at a time
  (`src/lib/proto/logctl`). Its coverage arguments are staged for ferrix-55's
  next evidence run, not committed.
* `tools/vendor/google/pixel7/monitor`: the USB watcher. A monitor started before this
  landed must be rebuilt and restarted to have it.

**ModemManager takes the start of the log.** example's ModemManager opens
every new `ttyACM` to probe it, 4 s after it appears, and holds it until
the port goes. Holding it open raises DTR, so `usbdev` streams to it, and
it drops what it reads: in `usblog2` the monitor's record began at
`usbdev`'s own start rather than at the loader's first line. The fix is
on the host, a udev rule the owner installs, which needs root:

```
# /etc/udev/rules.d/70-ferrix-console.rules
ATTRS{idVendor}=="1209", ATTRS{idProduct}=="0001", ENV{ID_MM_DEVICE_IGNORE}="1"
```

### Left open

Each is a row in `docs/BACKLOG.md`, owner open: the udev rule above;
SuperSpeed (the port is
held at high speed, and the combo PHY is untouched); input from the host
(a shell or a tty over the port); devmgr restarting `usbdev` if it dies;
the PHY's suspend and LPM, kept off; and reading the core's release
(`VER_NUMBER`, `0xC1A0`), which decides reset-timing quirks the driver now
covers by always waiting.
The log core's REFUSED once went missing on an aarch64 boot; that is a
row too, and its boot check prints `logctl   SIGHTING: ...` each time it
happens again.

**The "Boot Android" line.** Since 2026-09-26 `usbdev` restarts Ferrix
when the serial port's host sends the line `ferrix-usbdev: reboot`, which
is the monitor's "Boot Android" button. On the phone that is the watchdog
reset every run ends with, so no register access changed. Run
`bootandroid1` proved it: the line restarted Ferrix within a second and
Android was back 79 s later. The monitor sends a newline first, because
the host's echo can leave a piece of Ferrix's log in `usbdev`'s line. It is an
unauthenticated reboot for anyone with a USB cable to the phone, acceptable
for a debug console, and it goes when adb's `reboot:` replaces it
(`docs/ADB.md`).

### Next agent: start here

ferrix-9c wound down on 2026-09-26 at 6f5090f6, with nothing unlanded
and every worktree, branch and target directory of its own removed.

* **Before any phone boot**, ask the product owner session (`ferrix-2c`
  that evening; ask ListAgents who holds the role now) and the phone's
  other users (then `phone-link-9f` and `ferrix-d4`), and wait for an
  explicit "free". The PO's standing OK covers only boots whose writes are
  the 31 offsets above, behind the same guard. Anything new, such as the
  combo PHY's window for SuperSpeed, needs its own OK with the address list
  checked against `panther.dts` first. After each run, check its
  `ramoops` record's `usbdev: write` offsets against the list.
* **A run:** `FERRIX_PIXEL7_CMDLINE_EXTRA="ferrix.init=/sbin/ferrix-statd
  ferrix.statd.seconds=60" WT=<worktree> $P/build-run.sh <name>`, then
  `$P/boot-run.sh <name>` (`P=~/.local/share/ferrix/pixel7`). Give your
  worktree its own `CARGO_TARGET_DIR` (a copy of `build-run.sh` with `T`
  changed). The port comes up about 50 s into the run and stays about a
  minute with those settings. Run records: `$P/usbdev1`, `usblog1`,
  `usblog2`; the monitor's own is `$P/usb-20260926-174123`.
* **To watch it**, rebuild and restart `tools/vendor/google/pixel7/monitor` from `main`:
  a monitor built before 6f5090f6 has no USB watcher. Until the owner
  installs the udev rule above, ModemManager takes the log's first
  seconds.
* **Where the parts are:** `src/boot/vendor/google/pixel7/src/usb.rs` (survey),
  `src/kernel/extensions/board/gs201-usb/src/platform/google/gs201/usb.rs` (binding), `src/lib/drivers/usb/dwc3` and
  `src/lib/drivers/usb/usb-device` (host-tested, `cargo test -p ferrix-dwc3
  -p ferrix-usb-device`), `src/user/system/native/drivers/usb/usbdev` (driver),
  `src/kernel/src/console/log.rs` and `src/kernel/src/interfaces/logctl` (the log and its
  reader, in the certified item: ask the certification session before
  changing either), `tools/vendor/google/pixel7/monitor/src/usb.rs` (watcher).
* **Still in someone else's hands:** the log's coverage arguments, in
  `~/.local/share/ferrix/pixel7-usb-log/coverage-argued-x86_64.pending.json`,
  for the certification session (ferrix-55) to merge into its next
  evidence run. Leave the file where it is.
