# nazuna as a virtual machine

`cargo xtask run-nazuna` boots Ferrix on a QEMU machine shaped like the
customer's PC, nazuna, so that what the PC does to Ferrix can be looked at
and debugged without running it on the PC. The customer asked for it on
2026-10-12, after Ferrix was installed on nazuna's WD SN720 (2026-10-11) and
did not boot from it.

## 1. What is copied

From what nazuna reported on 2026-10-12 (`lsblk`, `lspci -nn`, `lscpu`,
`/sys/class/dmi/id`):

| nazuna | The machine |
|---|---|
| ASUS ProArt B650-CREATOR, BIOS 3206 | q35 with OVMF (§2), its ACPI tables as the board's (§1.1) |
| Ryzen 9 9900X, two chiplets of six cores of two threads, APIC IDs 0-11 and 16-27 | `-cpu host` under KVM on nazuna, `-smp 24,sockets=1,dies=2,cores=6,threads=2`, which numbers them the same |
| 64 GB | 16 GiB unless `--memory` |
| AMD IOMMU (AMD-Vi, IVRS) at 00:00.2, registers at `0xf7500000` | `AMDVI-PCI` at 00:00.2 and `amd-iommu,intremap=on`, in place of the VT-d unit every other machine here has |
| Lexar NM790 2 TB on a processor root port: Ubuntu's ESP, `/boot`, LVM | an NVMe drive on a root port, its real size and partitions, empty |
| WD PC SN720 256 GB behind the chipset (`M.2_3`): `nazuna-swap` 40 GiB, `ferrix-esp` 512 MiB, `ferrix-bootctl`, `ferrix-slot-a`/`-b` 4 GiB each, `ferrix-root` | an NVMe drive below a switch, laid out to the MiB, Ferrix installed on it by `ferrix-install --slots`, booted from (`bootindex=0`) as the PC's `UEFI OS` entry boots `EFI/BOOT/BOOTX64.EFI` |
| Lexar NQ790 1 TB behind the chipset: Windows | a second NVMe drive below the switch, its partitions empty |
| USB keyboard and mouse on xHCI | `qemu-xhci` with `usb-kbd` and `usb-mouse` |
| no serial port | a serial port is kept, so the boot can be read |

The disks are sparse files of the drives' real sizes in `build/x86_64/`
(`nazuna-sn720.img`, `nazuna-nm790.img`, `nazuna-nq790.img`); the SN720's
is made and installed again on every run, from the tree's loader, kernel and
initramfs, so a change to any of them is what boots. The live medium the
install runs from carries `ferrix.root=tmpfs ferrix.fbcon`, and
`--kernel-option` words after it; the installer makes it
`ferrix.root=btrfs ...` on the disk, as the customer's install had.

None of the test machine's own devices are there: no virtio random number
generator, `edu`, `pci-testdev`, fixture disks or network, and no 8042.

### 1.1 The board's ACPI tables

QEMU writes its own tables, so the machine runs a QEMU with
`tools/common/data/qemu/0006-*` (`-machine x-nazuna=on`, installed by
`fetch-qemu-linux.sh` into `~/.local/share/ferrix/qemu-nazuna`, beside the
QEMU every other boot runs) and an OVMF whose ACPI PM base is the board's
(`tools/common/fetch/fetch-ovmf-nazuna.sh`, into
`~/.local/share/ferrix/ovmf-nazuna`). With them the tables Ferrix reads say
what nazuna's AMI firmware says, and the devices they name are there:

| Table | As on nazuna |
|---|---|
| MADT | revision 6; 32 local APIC entries, each core's first thread before any second thread, 8 of them disabled; I/O APIC 32 at `0xfec00000` (GSI 0) and 33 at `0xfec01000` (GSI 24), whose ID registers read 32 and 33; IRQ 0 on GSI 2, IRQ 9 level and active low; LINT1 NMI |
| FADT | PM1a `0x800`/`0x804`, a 32-bit PM timer at `0x808`, GPE0 `0x820`, flags `0x3c5a5` (no forced APIC cluster model), reset by `0xBE` to port `0xB2`, which resets, LEGACY_DEVICES without an 8042 |
| HPET | `0x10228201`: AMD's, three timers, a 32-bit main counter at 14.31818 MHz, minimum tick 14318 |
| MCFG | ECAM `0xe0000000`, buses 0-255 |
| IVRS | the IOMMU at 00:00.2, registers `0xf7500000`; the HPET and both I/O APICs as its special devices, with the board's handles and requester ids |

Checked on 2026-10-13 by dumping the guest's tables with the UEFI shell's
`acpiview` and decoding them with the script that decoded nazuna's own: the
two agree in every field above. They differ only in what no driver reads:
the headers' OEM revision and creator (`BXPC` against `AMI `), the FADT's
revision (3, 244 bytes, against 6, 276), the PM timer's access size, and
the IVRS's revision, IVinfo and length.

## 2. What is not, and why

* **The board's firmware.** ASUS's BIOS is AMI Aptio V for AM5: on that
  platform AMD's security processor trains the memory and releases the
  cores, and the firmware's early phases program the chipset, the memory
  controller and the SMU. QEMU emulates none of them, so the image cannot
  run; OVMF runs instead. A bug in AMI's own code is therefore not
  reproduced here, and the firmware is read instead (§4).
* **The DSDT and SSDTs.** The machine's AML is q35's. Ferrix has no AML
  interpreter and reads none of it; nazuna's are dumped in
  `~/nazuna-profile/acpi` on nazuna.
* **The chipset.** The devices are q35's (ICH9's LPC, SATA, SMBus) at
  q35's places; only what the tables above name is moved to the board's
  addresses.
* **Device identities.** QEMU's NVMe controller reports itself
  (`1b36:0010`, `QEMU NVMe Ctrl`) and has no property for another model;
  the chipset's own devices, the Realtek network controllers and the three
  graphics cards are absent. Copying identities needs a QEMU patch, and
  only identities: a driver matched to a spoofed identity talks to QEMU's
  device.
* **A framebuffer above 4 GiB.** nazuna's was at `0xfc10000000`; q35's VGA
  has a 32-bit BAR. Passing the RTX 3060 through with its option ROM
  (`run-nvidia`'s domain does it without) would give the real one.

## 3. Running it

On nazuna (Linux, KVM; the images are sparse, 3.2 TB apparent):

```
FERRIX_QEMU_DIR=~/.local/share/ferrix/qemu-nazuna \
  FERRIX_QEMU_WORK=~/.local/share/ferrix/qemu-nazuna-build \
  tools/common/fetch/fetch-qemu-linux.sh        # once, and after 0006 changes
tools/common/fetch/fetch-ovmf-nazuna.sh          # once
cargo xtask run-nazuna
cargo xtask run-nazuna --timeout 240
```

The first is watched in the terminal (Ctrl-A x quits); the second boots
headless until the kernel powers off or panics, or for that many seconds,
and keeps the serial log in `build/x86_64/serial.log`. `--memory`, `--smp` and `--kernel-option` are
taken; `--display` opens the screen.

## 4. The SSD boot on nazuna

On the PC (`origin/baremetal-hang`, `docs/BAREMETAL.md` there): the loader
prints everything up to the framebuffer and the breadcrumb before
ExitBootServices, and the firmware never returns from that call (or the
GetMemoryMap before it); the kernel never runs.

**On this machine (2026-10-12)** the same install boots: OVMF starts the
SN720's `BOOTX64.EFI`, the loader boots slot A, ExitBootServices returns,
and the kernel runs every stage to `FERRIX-POWER-OFF` on 24 processors,
driving the three NVMe drives and the xHCI keyboard and mouse. So the hang
is in what the board's firmware does and OVMF does not. Seen on the way:
the AMD IOMMU is not driven (`0 VT-d units and 0 SMMUv3s translating`), so
no device's DMA is confined, which the certified item's claims rule out
on that PC until an AMD-Vi driver exists (`docs/NVME.md` §11).

**With the board's tables (2026-10-13)** it boots the same:
`FERRIX-BOOT-OK stages 1-12` and `FERRIX-POWER-OFF` 31 s after QEMU
started. The kernel calibrated the TSC against the 32-bit HPET (4399.995
MHz), took the 24 processors from the reordered MADT, booted on APIC ID 0,
and was not put off by the second I/O APIC or the IDs 32 and 33. It now
says the IOMMU gap outright: `degraded trusted mode: no IOMMU domain is
programmed, so device DMA reaches all of memory`. So nothing in the tables
nazuna's firmware writes stops Ferrix once the kernel runs. (Stage 4's
100-round IPI check took 3 s here, against 57 s and 131 s on the first two
runs, when nazuna was loaded at 11 to 20: the host's load, not the guest.)

**The board's firmware, read** (BIOS 3206 from ASUS, SHA-256 matching
ASUS's; extracted with UEFIExtract, decompiled with Ghidra; nothing of it
is kept in this tree). Its DXE core is EDK2's:

* It accepts the OS-defined memory types (it refuses only `0x0E` to
  `0x6FFFFFFF`) and never indexes a table by one: every per-type table
  is behind a `type < 16` test in GetMemoryMap and ExitBootServices.
  Ruled out as the core's fault.
* ExitBootServices checks the map key by plain comparison and refuses a
  stale one with `INVALID_PARAMETER`, which the loader's retries would
  have shown as a red breadcrumb. Ruled out.
* Before that check it signals the BeforeExitBootServices and
  ExitBootServices events, with boot services still up and interrupts
  on. 44 drivers register for them: AMD's PSP drivers (mailbox polls
  in unbounded loops), the NBIO and IOMMU drivers, the FCH's, USB, USB4,
  NVMe and the network stack. A handler that never returns there gives
  exactly what the PC showed. Which one could not be settled without
  running it.

Every one of those handlers also runs when Ubuntu boots, and Ubuntu
boots, so what makes one hang must be something the loader left behind.
The one difference a handler can see is still the memory map's
OS-defined types, which Linux's stub never uses; a handler that reads
the map and assumes a type below 16 would go wrong on them. The smallest
change that removes the difference: allocate with standard types
(`EfiLoaderData`, `EfiLoaderCode` for the image) and carry Ferrix's kinds
in the boot information, which already records the map. And disable the
boot watchdog (`SetWatchdogTimer(0, ...)`), as GRUB does when it starts;
not the hang's cause (GRUB had already disabled it on the chainloaded
boots, and no reset came), but a firmware-started boot would otherwise be
reset after five minutes. Neither can be shown to fix the hang except on the PC; this
machine shows that they break nothing.
