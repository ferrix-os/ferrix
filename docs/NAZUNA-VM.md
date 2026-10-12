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
| ASUS ProArt B650-CREATOR, BIOS 3206 | q35 with OVMF (§2) |
| Ryzen 9 9900X, 12 cores of two threads | `-cpu host` under KVM on nazuna, `-smp 24,sockets=1,cores=12,threads=2` |
| 64 GB | 16 GiB unless `--memory` |
| AMD IOMMU (AMD-Vi, IVRS) | `amd-iommu,intremap=on`, in place of the VT-d unit every other machine here has |
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
generator, `edu`, `pci-testdev`, fixture disks or network.

## 2. What is not, and why

* **The board's firmware.** ASUS's BIOS is AMI Aptio V for AM5: on that
  platform AMD's security processor trains the memory and releases the
  cores, and the firmware's early phases program the chipset, the memory
  controller and the SMU. QEMU emulates none of them, so the image cannot
  run; OVMF runs instead. A bug in AMI's own code is therefore not
  reproduced here, and the firmware is read instead (§4).
* **ACPI tables.** QEMU writes q35's own (with an IVRS for the AMD IOMMU).
  nazuna's tables are dumped in `~/nazuna-profile/acpi` on nazuna, for the
  parser to be run against on the host.
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
FERRIX_QEMU=~/.local/share/ferrix/qemu/bin cargo xtask run-nazuna
FERRIX_QEMU=~/.local/share/ferrix/qemu/bin cargo xtask run-nazuna --timeout 240
```

The first is watched in the terminal (Ctrl-A x quits); the second boots
headless for that many seconds and keeps the serial log in
`build/x86_64/serial.log`. `--memory`, `--smp` and `--kernel-option` are
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
