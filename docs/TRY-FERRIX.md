# Try Ferrix

Ferrix is an operating system with its own Rust microkernel that runs
ordinary Linux programs. The live ISO starts its desktop, Yoru, from memory,
so you can try it without touching your disks, and it carries an installer
if you want to keep it.

**This is a development preview.** Ferrix has run on very few PCs, it is
not signed for Secure Boot, and parts of authentication and process
isolation are still being finished. Do not put anything on it that you
cannot lose. The live ISO's command line is not a configuration any
certification claim is made of (it skips the boot self-checks, lends the
firmware's framebuffer to a driver and, where there is no IOMMU, runs
drivers unisolated).

## Download

From the release page of
[ferrix-os/ferrix](https://github.com/ferrix-os/ferrix/releases):

* `ferrix-<version>-x86_64.iso`: the live medium;
* `SHA256SUMS`: check the download with `sha256sum -c SHA256SUMS`.

The ISO boots on 64-bit x86 PCs and virtual machines with **UEFI**
firmware. There is no legacy BIOS boot.

## Drivers and the IOMMU

Ferrix runs every driver that does DMA behind the IOMMU (Intel VT-d,
AMD-Vi). The **live session** also starts drivers on a machine with no
IOMMU, and says so on its boot log: they are then trusted with all of
memory, as every driver is on Linux. The **installed system** never does:
without an IOMMU it starts no USB, disk or network driver. So turn VT-d or
AMD-Vi on in the firmware -- or the virtual machine's IOMMU -- **before
you install**.

## In QEMU (recommended)

The command below is the machine Ferrix is tested in: an Intel IOMMU
(`intel-iommu`) and virtio devices with `iommu_platform=on`. You also need
OVMF, QEMU's UEFI firmware
(`apt install qemu-system-x86 ovmf`, `dnf install qemu-system-x86
edk2-ovmf`, `pacman -S qemu-full edk2-ovmf`).

```sh
cp /usr/share/OVMF/OVMF_VARS_4M.fd ferrix-vars.fd   # a fresh variable store
qemu-system-x86_64 \
  -accel kvm -machine q35,kernel-irqchip=split -cpu host -m 4096 -smp 4 \
  -device intel-iommu,intremap=on,eim=off \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
  -drive if=pflash,format=raw,file=ferrix-vars.fd \
  -vga none \
  -device virtio-gpu-pci,disable-legacy=on,iommu_platform=on,edid=on,xres=1920,yres=1080 \
  -device virtio-keyboard-pci,disable-legacy=on,iommu_platform=on \
  -device virtio-tablet-pci,disable-legacy=on,iommu_platform=on \
  -netdev user,id=net0 \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on \
  -drive file=ferrix-x86_64.iso,if=none,id=live,format=raw,readonly=on \
  -device virtio-blk-pci,drive=live,disable-legacy=on,iommu_platform=on,bootindex=0 \
  -serial mon:stdio
```

The OVMF file names differ between distributions (`OVMF_CODE.fd`,
`edk2-x86_64-code.fd`); use the ones yours installed. Without KVM, use
`-accel tcg -cpu max` and expect it to be slow.

The ISO is attached as a virtio **disk** here, not a CD. It also boots as
a CD (`-cdrom`), and the live desktop then comes up the same way, but the
installer can only copy the system from a medium Ferrix can read as a
disk -- virtio, SATA or NVMe -- and Ferrix has no CD driver yet: booted as
a CD it says so at Install and writes nothing.

### Installing into the virtual machine

Make a disk of at least 8 GiB and add it beside the ISO:

```sh
qemu-img create -f raw ferrix-disk.img 16G
# add to the command above:
  -drive file=ferrix-disk.img,if=none,id=disk,format=raw \
  -device virtio-blk-pci,drive=disk,disable-legacy=on,iommu_platform=on
```

Then, in the live session, use the installer. When it says it is done,
quit QEMU, take the two `-drive ... live` and `-device ... drive=live`
lines out, give the disk's `-device` line `bootindex=0`, and start it
again. Ferrix starts from the disk and asks for your password.

## In VirtualBox or VMware (untested)

Nobody has run Ferrix in VirtualBox or VMware yet. What follows is what
Ferrix needs, from the same machine shape checked in QEMU (a plain display
adapter, USB keyboard and mouse on xHCI, the ISO as a SATA disk, the
install going to an NVMe disk); tell us what happens.

* **EFI** firmware on, Secure Boot off.
* **A USB 3 (xHCI) controller**, and the keyboard and mouse on USB:
  Ferrix has no PS/2 driver. In VirtualBox:
  `VBoxManage modifyvm <vm> --firmware efi --usbxhci on --keyboard usb --mouse usbtablet`.
* **4 GiB** of memory or more.
* **The ISO as a SATA hard disk**, not as a CD, if you want to install:
  `VBoxManage convertfromraw ferrix-<version>-x86_64.iso ferrix-live.vdi`,
  or `qemu-img convert -O vmdk ferrix-<version>-x86_64.iso ferrix-live.vmdk`
  for VMware, attached to the SATA controller and booted from; and a second,
  empty disk of 8 GiB or more on SATA or NVMe to install onto.
* **An IOMMU** for the installed system: VirtualBox's (Settings, System,
  Motherboard, IOMMU: Intel, with the ICH9 chipset) or VMware's
  "Virtualize IOMMU". The live session runs without one.
* The display is the adapter's firmware framebuffer, at the resolution the
  firmware chose; there is no 3D.

## On a PC

Write the ISO to a USB stick (everything on the stick is lost):

* Linux or macOS: `sudo dd if=ferrix-<version>-x86_64.iso of=/dev/sdX bs=4M conv=fsync`
* Windows: Rufus in **DD image** mode, or balenaEtcher.

Turn Secure Boot off in the firmware's settings, and start from the stick
through the firmware's boot menu. The boot log is drawn on the screen
until the desktop takes it over; if it stops there, a photo of it is the
most useful thing to put in an issue.

Be ready for it not to work. This ISO has not been booted on a real PC
yet; it was checked in QEMU shaped like one. Ferrix itself has run on the
Intel N150 mini PC it is developed on, with its AHCI SATA disk and
Motorcomm YT6801 Ethernet.

* **The screen** is the firmware's framebuffer, at the resolution the
  firmware chose, with no GPU acceleration, whatever the graphics: Ferrix
  does not drive Intel graphics in this image, nor AMD or NVIDIA ones.
* **Keyboards and mice on USB only.** Many laptops' built-in keyboards and
  touchpads are PS/2 or I2C, which Ferrix does not drive: plug in a USB
  keyboard and mouse.
* **Ethernet** only on the Motorcomm YT6801 and some of Realtek's
  RTL8111/8168 family; no Intel Ethernet, no Wi-Fi, no Bluetooth.
* **Disks** on AHCI SATA and NVMe.
* **The installer cannot install from a USB stick yet**: Ferrix does not
  serve USB disks to the system. Try the live session there, and install in
  a virtual machine. (A copy of the ISO written to a SATA or NVMe disk of
  the PC can be installed from.)

## The live session

The desktop starts logged in as the user `ferrix`, with no password, and
the installer opens. **Try Ferrix** closes it; "Install Ferrix" is in the
launcher to open it again.

* The desktop is Yoru, in the live session and on the installed system.
* SUPER+RETURN opens a terminal running zinc, Ferrix's shell.
* SUPER+Q closes a window, SUPER+1 to 4 switch workspaces.
* Try `ferrofetch`, `btop`, `curl https://example.com`, `git`.

Nothing you do in the live session is kept: `/` is in memory. Give the
machine 4 GiB: with much less, the session can run out of memory.

## The installer

It asks, page by page:

1. the language (English only, for now) and whether to try or install;
2. the keyboard layout, with a field to try it in;
3. the disk, and either **erase the whole disk** -- everything on it is
   lost, and the page says so in red -- or **install into free space or
   one partition you choose**, which leaves every other partition alone;
4. your name, user name, computer name and password;
5. the time zone;
6. a summary of every change, then **Install**;
7. the progress, and a page to restart from.

The installed system starts Yoru with your session locked: your password
opens it. Its disk has an EFI system partition and a btrfs root. It
isolates its drivers behind the IOMMU (above).

The dock's clock shows UTC whatever the time zone; programs (`date`, the
lock screen's clock) follow the zone you chose.

It cannot yet resize another system's partition, add a boot entry to the
firmware (on a disk shared with another system, pick Ferrix in the
firmware's boot menu, or make the disk boot first), or install on a disk
with an MBR partition table except by erasing it.

## Licences

Ferrix is MIT licensed. The ISO carries other people's programs under
their own licences; `THIRD_PARTY_LICENSES.MD` in the ISO's root (and
`LICENSES.MD` on its EFI system partition) lists every part with its
licence and the licence's text. No firmware is carried.

## Reporting what happened

Open an issue on [ferrix-os/ferrix](https://github.com/ferrix-os/ferrix/issues)
with the machine, what you did, and, if you can, the serial log
(`-serial mon:stdio` above prints it in the terminal).
