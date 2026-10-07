# The live installer

Version 1, written on 2026-09-28 and approved by the customer that day
(§11).
The customer asked that day for "a live installer like we know it from
Linux", and chose, from four questions:

* **real PCs too**, not only virtual machines;
* **graphical from the start**, a desktop app on hyprix;
* **beside other operating systems**, not only a wiped disk;
* **a raw `.img`, a hybrid `.iso`, and both attached to GitHub releases.**

This document is the design those answers ask for, written before anything
is built. It follows `docs/YSERVER.md`'s shape: what it is, what exists,
the design, the tests, the slices and their points, and what is left for
the customer to decide.

## 1. What this is, and what it is not

What a person does with it, start to finish:

1. Download `ferrix-<version>-x86_64.iso` (or `.img`) from a GitHub release.
2. Write it to a USB stick with `dd`, Rufus or balenaEtcher, or attach it to
   a virtual machine as a CD or a disk.
3. Boot it. Ferrix starts from the stick with `/` in memory -- the *live
   session* -- and the desktop comes up with the installer open. Nothing on
   the machine's disks has been touched, and the live session is a working
   Ferrix to try first.
4. In the installer: pick a keyboard layout; pick a disk; choose **Erase the
   disk** or **Install beside** what is on it (§4.4); make a user and a
   password; read the summary; press **Install**.
5. The installer partitions, formats, copies the system, installs the boot
   loader, and says to remove the stick and restart.
6. The machine boots Ferrix from its own disk -- through Ferrix's boot menu,
   which also offers the other system when there is one.

It is not a package manager, an updater or a recovery tool. It installs the
system the live medium carries and nothing else; network installs, updates
of an installed system and repair are later work, not in this design.

It boots with Secure Boot on, through Ubuntu's Microsoft-signed shim and
a key of Ferrix's own that the person enrolls once (§5.7).

**Exit of this design:** on x86-64, two gates and one machine.

* `cargo xtask test-install` (VM, in `check`): the live ISO boots under QEMU
  with a blank virtio disk and a second disk holding a fake "other system";
  the installer, driven by an answer file, installs beside it; the guest
  powers off; QEMU then boots the target disk alone, the boot menu chooses
  Ferrix, and Ferrix reaches `FERRIX-BOOT-OK` with `/` on the installed btrfs
  root. The other system's partitions are compared byte for byte, unchanged.
* `cargo xtask test-compositor --boot installer`: the graphical installer's
  pages match their screenshots, and a click-through with a virtual pointer
  ends in the same installed disk.
* The customer's reference PC (§10 decision 3), booted from a USB stick made
  from the release's ISO, installs beside its existing system and boots both.

## 2. What exists today (surveyed 2026-09-28)

Ferrix has half an installer already, and none of the other half.

**What is there:**

* Every `run` boot *installs* already: the kernel starts on the initramfs
  in tmpfs, finds a btrfs volume labelled `ferrix-root`, unpacks the
  initramfs onto it, and switches `/` there (`src/kernel/src/fs/root_disk.rs`,
  `docs/GUIDE.md` §Getting started). An installed system is exactly that volume
  plus something that boots it.
* `ferrix.root=tmpfs` (and `--tmpfs-root`) is a live session: `/` in memory,
  no disk written.
* The UEFI loader `src/boot/common/uefi` reads `FERRIX/KERNEL.ELF`, `FERRIX/INITRD.IMG`
  and `FERRIX/CMDLINE.TXT` from the FAT volume it was loaded from, and hands
  the kernel the firmware's GOP framebuffer.
* `tools/common/xtask/src/fat.rs` writes FAT32 in plain Rust, with no mtools.
* `src/lib/fs/btrfs-write` writes btrfs that `btrfs check` accepts, including
  allocating new chunks (`grow.rs`) -- what a fresh mkfs'd volume needs to
  grow into its device.
* ACPI, PCI with MSI, and an IOMMU are in the kernel; ring-3 drivers sit
  behind `blkring` (`docs/BLOCK-RING.md`), so a new disk driver is a new
  process speaking an existing protocol.
* hyprix, and `src/user/system/linux/compositor/toolkit` with `render` and `text` for
  drawing a client's window; `authd` for accounts and passwords
  (`docs/AUTH.md`).

**What is missing:**

| Gap | Where it shows |
|---|---|
| Userland cannot open a disk | `src/kernel/src/fs/devfs.rs:1355`: every block device is `ENXIO` on open |
| No partition tables | root is found by label on a *whole* virtio disk, and only `vdd`..`vdg` (`root_disk.rs:78`) |
| No mkfs for btrfs | every volume is unpacked from a host fixture |
| No FAT in the running system | the ESP can be written on the host only |
| No image with a partition table, no ISO | `ferrix.img` is a bare FAT32 volume; releases carry notes only |
| No boot menu | the loader boots Ferrix and nothing else |
| No UEFI variables from the OS | the kernel does not keep the firmware's runtime services |
| Only virtio disks | no NVMe, AHCI, or USB mass storage |
| No USB 3, no PS/2 | `usb-host` is EHCI; a modern PC's keyboard and sticks sit on xHCI or i8042 |
| No display without virtio-gpu | on a PC with no driver for its GPU, hyprix has no screen |
| No widgets | the toolkit draws surfaces; buttons, lists and text fields are to write |

## 3. The medium

### 3.1 One layout for both files

The `.img` and the `.iso` carry the same partitions, so there is one thing
to test:

```
GPT (protective MBR; on the .iso also an ISO 9660 volume and El Torito)
 1  ESP, FAT32, 64 MiB   EFI/BOOT/BOOTX64.EFI      the loader
                         FERRIX/KERNEL.ELF
                         FERRIX/INITRD.IMG          the whole system, installer included
                         FERRIX/CMDLINE.TXT         "ferrix.live ferrix.root=tmpfs"
```

The live system *is* the initramfs, as every test boot is today, so the
medium needs no root partition and no squashfs. The initramfs is also the
installer's source: what it copies to the target is what the live session
is running. A live session keeps the medium's ESP label `FERRIX-LIVE` so
the installer knows which disk not to offer.

The `.iso` is the `.img` with an ISO 9660 filesystem and an El Torito catalog
added in the space GPT leaves free, the layout `xorriso -as mkisofs
-isohybrid-gpt-basdat` makes: firmware booting it as a CD finds the ESP
through El Torito's EFI entry, and firmware booting it as a disk (after
`dd`) finds it through the GPT. It is written by `tools/common/xtask/src/iso.rs` in Rust,
as `fat.rs` is, so neither Windows nor Linux build hosts need `xorriso`.
The ISO 9660 side carries a single `README.TXT`; nothing boots from it.

### 3.2 Which architectures

x86-64 is the target of every choice above. AArch64 gets the same `.img` and
`.iso` for virtual machines (UTM, Parallels, QEMU `virt`), since its loader
is the same code and virtio is its only hardware. ARMv7-A, the DK1 and the
Pixel keep their `xtask flash` paths; an installer on them is not asked for.

### 3.3 Building and releasing

`cargo xtask live --arch x86_64 --release` writes
`build/x86_64/ferrix-live.img` and `ferrix-live.iso`. The release workflow
(`.github/workflows/release.yml`) builds both for x86-64 and AArch64, names
them `ferrix-<tag>-<arch>.{img,iso}`, writes `SHA256SUMS`, and attaches all
of it to the release it creates. The release notes gain a short "Install"
section pointing at `docs/INSTALL.md`, the user-facing guide (§8).

## 4. The installer

### 4.1 Two programs, one engine

```
src/user/system/linux/installer/
  engine/   ferrix-installer-engine: a library, no I/O of its own beyond a Disk trait
  cli/      /bin/ferrix-install: runs an answer file, prints progress; the gates use it
  gui/      /bin/ferrix-installer: the desktop app, on toolkit + render + text
```

The engine takes a **plan** and turns it into a list of **steps**, each
of which reports progress and can be dry-run:

```rust
pub struct Plan {
    pub disk: DiskId,                // by-path and serial, never "vda"
    pub layout: Layout,              // Erase | Beside { free: Range } | Replace { partition }
    pub esp: EspChoice,              // New | Existing(PartitionId)
    pub root_size: Option<u64>,      // default: all of it
    pub user: NewUser,               // name, full name, password (goes to authd, never stored)
    pub hostname: String,
    pub keyboard: String,            // an xkb layout, as hyprix's config takes it
    pub timezone: String,
}
```

`Plan::validate(&disks)` refuses a plan that would write outside the space
it names, and `Plan::steps()` is pure, so the rule "no byte outside the
chosen space is written" (§4.4) is tested on the host with no disk at all.
An answer file is the plan in TOML; the GUI builds the same struct.

### 4.2 The steps

1. **Partition.** Write the new GPT entries (both headers, both entry
   arrays, the protective MBR) with `src/lib/fs/partition`. On *Erase*: ESP
   512 MiB, root the rest. On *Beside*: root in the chosen free range, and a
   new ESP only if the disk has none. Then `BLKRRPART` and wait for the
   kernel to list the new partitions.
2. **Format.** `mkfs.btrfs` of our own (§5.3) on root, label `ferrix-root`,
   a new filesystem UUID. `mkfs.fat` on a new ESP.
3. **Copy the system.** Mount the new root read-write and unpack the running
   initramfs onto it, which is the kernel's `root_disk::install` done from
   userland -- the same function, moved into a library both call, so the
   installed system is what a `run` boot's would be, stamp and all.
4. **Configure.** `/etc/hostname`, the keyboard layout in the desktop's
   config, the time zone, and the account through `authd` (a `useradd`-like
   request with `--root` pointing at the new volume, `docs/AUTH.md`).
5. **Boot files.** Copy `BOOTX64.EFI`, `KERNEL.ELF` and `INITRD.IMG` into
   `EFI/ferrix/` on the ESP, and write `EFI/ferrix/CMDLINE.TXT` with
   `ferrix.root=PARTUUID=<root's>`. On an ESP of its own, also the fallback
   `EFI/BOOT/BOOTX64.EFI`; on a shared one, never (it belongs to the other
   system).
6. **Boot entry.** Add a `Boot####` variable "Ferrix" pointing at
   `\EFI\ferrix\BOOTX64.EFI` and put it first in `BootOrder`, through
   `/sys/firmware/efi/efivars` (§5.5). A firmware that forgets it (some do)
   still finds Ferrix through the fallback path on an ESP of its own; on a
   shared ESP the summary says to pick "Ferrix" in the firmware's boot menu.
7. **Sync and unmount**, then say to remove the medium and restart.

A failure before step 1 finishes changes nothing. After it, the installer
says which step failed and what is on the disk; it does not try to undo a
partition table, which is a second chance to lose data.

### 4.3 Which disks it offers

Every disk the kernel lists, except the one the live system booted from,
with its model, size, and what is on it: the partitions, their file system
by magic (FAT, NTFS, ext4, btrfs, swap, BitLocker, Apple's APFS), their
labels, and "Windows", "Ubuntu" and so on when an ESP holds a loader Ferrix
recognises (§5.6). A disk with an MBR partition table rather than a GPT is
offered only for *Erase*: converting it would change what the other system
boots from.

### 4.4 Beside another system

The page offers three ways, as Ubuntu's does:

* **Install beside `<system>`**, with a divider between the two systems that
  the person drags. Ferrix takes free space first; when that is not enough,
  the installer **shrinks the other system's partition** to make room
  (§4.6). The divider stops where the other system's data ends plus a
  margin, and Ferrix needs at least 16 GiB.
* **Replace a partition**: pick one, its data is lost, Ferrix goes there.
  The page shows what it holds before it is picked.
* **Erase the disk.**

The rule that makes *beside* safe: **no byte outside the chosen range and
the GPT's own sectors is written**, and the ESP is written only inside
`EFI/ferrix/`. The gate (§7) checks it by hashing every other partition
before and after. A shrink is the one exception, and §4.6 is its own rule.

### 4.5 The graphical installer

A toplevel window on hyprix, 900 × 640, one page at a time with *Back* and
*Next*, as Calamares and Ubuntu's installer are laid out:

1. **Welcome**: "Try Ferrix" (closes the installer) or "Install Ferrix".
2. **Keyboard**: a list of layouts and a field to try it in.
3. **Disk**: the disks of §4.3 as bars coloured by partition, and the three
   choices of §4.4 under the chosen disk; the bar shows the result.
4. **You**: name, user name, password twice, hostname.
5. **Summary**: every change in words, what will be erased in red, and
   **Install**.
6. **Installing**: a progress bar and the current step; a *Details* toggle
   shows the CLI's log.
7. **Done**: *Restart now*.

The widgets are new: a button, a label, a list, a radio group, a text field
(with a password mode and the toolkit's clipboard), a progress bar, the
partition bar and a slider. They go in `src/user/system/linux/compositor/widgets` beside
the toolkit, so later apps get them too. They draw with `render` and `text`
as the other clients do, and follow the desktop's theme colours.

The live session's hyprix config starts the installer on login, and the live
session logs in without a password as user `ferrix`.

### 4.6 Shrinking the other system

The customer asked on 2026-09-28 for the full setup: the installer shrinks
the other system itself, as Ubuntu's does with `ntfsresize`, rather than
sending the person to Windows' *Disk Management*. It is the one step in
this design where a bug loses someone's files, so it is built as its own
library with its own gates, and it runs as the installer's **first** step,
before any partition is written, so a refused or failed shrink leaves the
disk exactly as it was.

**What it shrinks.** A partition's *end* moves toward its start; its start
never moves, so nothing the other system's boot loader points at changes.

| File system | Who has it | How |
|---|---|---|
| NTFS | Windows | `src/lib/fs/ntfs` reads the MFT, attributes and run lists; `ntfs-resize` moves every cluster past the new end below it, rewrites the run lists that named them, truncates `$Bitmap` and `$BadClus`, moves the backup boot sector to the new last sector, and marks the volume for `chkdsk` on Windows' next start, as `ntfsresize` does |
| ext4 | most Linux installs | `src/lib/fs/ext4` and `ext4-resize`, `resize2fs`'s shrink: move blocks and inodes out of the block groups being removed, rewrite extent trees and directory entries that named them, drop the groups, fix the superblock and group descriptors |
| btrfs | Fedora, openSUSE | `btrfs-write` already allocates chunks; shrinking relocates every chunk past the new end (a balance restricted to them) and then lowers the device's size, as `btrfs filesystem resize` does |
| anything else | APFS, ZFS, LUKS, LVM, BitLocker | not shrunk; the installer says what it found and offers free space and *Replace* only |

`ntfsresize` and `resize2fs` are GPL and are read as references, never
copied (Ferrix is MIT); Microsoft's published NTFS structures and the
Linux kernel's `Documentation/filesystems/ext4` are the specifications.

**When it refuses**, and says why in words the person can act on:

* NTFS: Windows is hibernated or *Fast Startup* left it half-shut (the
  `hiberfil.sys` signature, the dirty flag); BitLocker (the `-FVE-FS-`
  signature); the volume is marked dirty; `$LogFile` is not clean; the
  consistency pass below finds anything. The page tells the person to start
  Windows, turn Fast Startup off, shut it down fully, and come back.
* ext4: the journal needs recovery; `needs_recovery` or an orphan list;
  features the library does not know (it refuses by `incompat` bit, never
  guesses).
* btrfs: more than one device, or a profile other than `SINGLE`/`DUP`.
* Any of them: a bad-sector list the file system recorded, or a read error
  anywhere during the pass.

**How it stays safe.**

1. **Check before.** A full read-only consistency pass (every MFT record,
   every run list against `$Bitmap`; every inode's extents against the block
   bitmaps; `btrfs check`'s tree walk, which `src/lib/fs/btrfs` has). Anything
   it cannot account for is a refusal.
2. **Plan, then write.** The moves are computed in memory first, and the
   plan is refused if any destination is not free or overlaps a source not
   yet moved.
3. **Order the writes so a power cut loses nothing.** Data is copied to its
   new place and flushed *before* the metadata that points at it changes,
   and the old place is freed only after that metadata is flushed; a cut in
   the middle leaves a file system whose pointers all name good data, with
   some space leaked that `chkdsk`/`e2fsck` recovers. The partition entry in
   the GPT shrinks last, after the file system's own size is flushed.
4. **Check after**, with the same pass as step 1, and compare the file tree
   (names, sizes, and a hash of every file's contents) with the one read
   before. A mismatch stops the install before Ferrix writes anything, and
   says so.

A person is shown, before **Install**, in red: "Resizing `<system>` can lose
data if the power fails. Back up anything you cannot lose."


### 5.1 Block devices from userland

`devfs` opens block nodes (`devfs.rs:1355`): `read`, `write`, `pread`,
`pwrite`, `lseek` (with `SEEK_END` giving the size), `fsync`, and the
`ioctl`s the installer and busybox use: `BLKGETSIZE64`, `BLKSSZGET`,
`BLKPBSZGET`, `BLKRRPART`, `BLKFLSBUF`. I/O goes through the block core to
the driver's ring, as a mounted filesystem's does. A partition's node is
limited to its range, as on Linux (I2). Like Linux without `O_EXCL`, opening
a disk with something mounted from it is allowed, and not coherent with the
mount; the engine refuses such a disk itself (I5).

*Built (I1, 2026-09-28):* `src/kernel/src/fs/disk_file.rs`. Reads and writes of
any byte range, with a read-modify-write for a sector covered in part;
`EACCES` for writing a read-only disk; `ENOSPC` for a write at the end;
`fsync` as the disk's flush; `BLKGETSIZE64`, `BLKGETSIZE`, `BLKSSZGET`,
`BLKPBSZGET`, `BLKBSZGET`, `BLKIOMIN`, `BLKIOOPT`, `BLKALIGNOFF`,
`BLKROGET`, `BLKROTATIONAL` and `BLKFLSBUF`; `BLKRRPART` is `EINVAL` until
I2. Held by devfs's boot check on all three architectures (FX-0830).

### 5.2 Partitions

`src/lib/fs/partition`: GPT read and write (CRC-32 of headers and entry
arrays, the backup at the end of the disk, the protective MBR), MBR read.
Pure Rust, `no_std`, fuzzed like `src/lib/fs/btrfs`. The kernel's block core
reads each disk's table when the driver announces it and on `BLKRRPART`,
and makes `/dev/vda1`, `/dev/nvme0n1p1`, `/dev/sda1` with Linux's minor
numbers, plus `/sys/class/block/<name>/{partition,start,size}` and
`/dev/disk/by-partuuid/`.

Root is then found by `ferrix.root=PARTUUID=…` or `ferrix.root=LABEL=…` on
any disk or partition; with neither, as today, by the label `ferrix-root`,
now scanned on every disk rather than `vdd`..`vdg`. The live medium is found
the same way by its ESP label, so the loader need not say which disk it was.

### 5.3 mkfs.btrfs

`src/lib/fs/btrfs-mkfs` and `/sbin/mkfs.btrfs`: what `mkfs.btrfs` makes by
default for one device -- `SINGLE` data, `DUP` metadata, skinny metadata,
`NO_HOLES`, the free-space tree, CRC-32C -- built as trees in memory and
written in one pass, then grown by `btrfs-write`'s existing chunk
allocation. Tested by `btrfs check` on the host over sizes from 256 MiB to
2 TiB (sparse files), and by mounting what `mkfs.btrfs` of btrfs-progs
makes of the same size and comparing their trees item by item.

### 5.4 FAT

`tools/common/xtask/src/fat.rs` moves into `src/lib/fs/fat`, and gains the half it lacks:
opening an existing FAT32 (or FAT16) volume, walking directories, long
names, and adding files to it -- which is what writing into another
system's ESP needs. It is a library the installer links, not a kernel
filesystem: nothing mounts the ESP while Ferrix runs. `xtask` keeps using it
for its images.

### 5.5 UEFI variables

The loader passes the firmware's runtime services table and the memory map's
runtime regions to the kernel (`ferrix-bootinfo`), which calls
`SetVirtualAddressMap` and keeps a ring-0 call path for `GetVariable`,
`GetNextVariableName` and `SetVariable`, serialised by one lock, with the
calls' memory mapped only while one runs. `/sys/firmware/efi/efivars` is
Linux's interface to them, so the installer's `Boot####` code is what
`efibootmgr` does. On a machine where the firmware faults in a call, the
kernel turns the variables off and says so, as Linux's `efi=noruntime` does,
and the installer falls back to §4.2 step 6's second path.

### 5.6 A boot menu

The loader gains a menu, shown for three seconds (and until a key is
pressed) when the ESP it is on holds another system's loader:
`EFI/Microsoft/Boot/bootmgfw.efi`, `EFI/ubuntu/shimx64.efi` or
`grubx64.efi`, `EFI/fedora/…`, `EFI/debian/…`, `EFI/arch/…`, any
`EFI/*/BOOTX64.EFI`. Choosing one starts it with `LoadImage` and
`StartImage`. It draws on the GOP framebuffer with `fbtext`'s font and
takes the firmware's keyboard, so it works before any Ferrix driver.
`EFI/ferrix/MENU.TXT` sets the default and the time-out. With no other
loader there is no menu and no wait, as today.

### 5.7 Secure Boot, through Ubuntu's shim

A PC with Secure Boot on starts only what Microsoft's UEFI CA signed.
Ferrix cannot get its loader signed, and it cannot borrow Ubuntu's GRUB:
in Secure Boot mode that GRUB starts only kernels signed with Canonical's
key. What it can borrow is **shim**, the small first stage every Linux
distribution has Microsoft sign. Shim starts a second stage signed by its
vendor *or by any key the machine's owner has enrolled* in its MokManager.
That second path is the one Ferrix takes, as Ventoy does (the customer's
decision of 2026-09-28):

* `tools/common/fetch/fetch-shim.sh` takes `shimx64.efi` and `mmx64.efi` (and the
  `aa64` pair) from Ubuntu's `shim-signed` package, pinned by version and
  SHA-256, into `~/.local/share/ferrix/shim`. The repository carries none
  of it. A build without it makes images that need Secure Boot off, and
  says so.
* The medium's ESP becomes `EFI/BOOT/BOOTX64.EFI` = shim,
  `EFI/BOOT/grubx64.efi` = Ferrix's loader (shim starts its second stage by
  that name), `EFI/BOOT/mmx64.efi`, and `FERRIX.CER` at the root. The
  installed ESP has the same four in `EFI/ferrix/`, and the boot entry names
  shim.
* `xtask` signs the loader Authenticode-style with a Ferrix key, in Rust, as
  it writes FAT and ISO without host tools. A checkout makes a development
  key under `~/.local/share/ferrix/keys` on first use; the release workflow
  signs with a key kept as a GitHub secret, whose certificate is the
  `FERRIX.CER` that releases carry.
* The loader checks `KERNEL.ELF` and `INITRD.IMG` before starting them,
  against a detached Ed25519 signature made with the same release key and a
  public key built into the loader. It enforces this when the firmware's
  `SecureBoot` variable is 1, and warns otherwise. Shim's own verify call
  takes PE images only, and the kernel is an ELF, hence the loader's own.
* **First boot with Secure Boot on:** shim finds the loader unsigned by
  Canonical and not yet enrolled, and opens MokManager: *Enroll key from
  disk* → `FERRIX.CER` → a password → reboot → confirm with that password.
  `docs/INSTALL.md` shows each screen. The key stays in the firmware's
  variables, so the installed system boots without asking again.
* **Revocation.** Microsoft and the distributions revoke old shims through
  SBAT and `dbx` updates, which Windows Update applies. A revoked shim stops
  booting on an updated machine, so the fetch script's pin is moved to
  Ubuntu's newest shim before each release. Ubuntu signs its current shims
  with both Microsoft's 2011 and 2023 UEFI CAs, which covers machines on
  either CA.

## 6. Real PCs

Everything above runs in a VM on virtio. A PC also needs Ferrix to see its
disk, the stick it booted from, its keyboard and mouse, and its screen.
Each is a ring-3 driver behind an existing protocol, started by `devmgr` on
the PCI class it matches:

| Slice | Driver | Behind | Why a PC needs it |
|---|---|---|---|
| H1 | NVMe | `blkring` | the disk of nearly every PC since 2018 |
| H2 | AHCI (SATA) | `blkring` | older PCs, and SATA SSDs |
| H3 | xHCI | `usb-host` | every USB port on a PC since 2012; sticks, keyboards, mice |
| H4 | USB mass storage (BOT + SCSI) | `blkring` | the stick the live system is on |
| H5 | i8042 keyboard and touchpad | `input` | laptops' built-in keyboards |
| H6 | the firmware's framebuffer | `display` | a screen when there is no driver for the GPU; stage 21 is the one for NVIDIA |

H6 is the plain answer for a PC's screen: hyprix draws into the GOP
framebuffer the loader already hands over, at the mode the firmware chose,
with no mode setting, no vsync and no GPU. That is how every Linux live
image starts before its GPU driver loads, and it is enough for an installer
and a desktop. Stage 21's NVIDIA driver is not needed for any of this.

Network on a PC (Intel and Realtek Ethernet, Wi-Fi) is not needed to
install, since the medium carries the system, and is not in this design.

The unknown is bring-up: Ferrix has never booted on an x86 PC, only under
QEMU and on two Arm boards. ACPI tables, interrupt routing, and firmware
that behaves unlike OVMF will each cost something no estimate can see. §9's
H0 is a slice for exactly that, run on the reference machine before the
drivers, with its points a guess.

## 7. Tests

| Gate | What it proves | In `check` |
|---|---|---|
| host tests of `partition`, `btrfs-mkfs`, `fat`, `installer-engine` | tables, volumes, and plans, including "no write outside the range", with `btrfs check` and `sgdisk --verify` on what they write | yes |
| `test-install --arch x86_64` | ISO boots under OVMF, answer file installs on a blank disk, power off, target boots alone to `FERRIX-BOOT-OK` with `/ is btrfs on vda2` | yes |
| `test-install --beside` | as above on a disk pre-made with an ESP holding a stub `bootmgfw.efi` (an EFI app of ours that prints `OTHER-OS`) and an NTFS-looking partition; after install, both partitions hash unchanged; the menu boots Ferrix, and with a key press the stub | yes |
| `test-install --secure-boot` | the medium under OVMF's Secure Boot build with Microsoft's keys enrolled; the gate drives MokManager's enrollment over the serial console, installs, and boots the target with Secure Boot on; an image with a tampered `KERNEL.ELF` must stop at the loader | yes, when the shim is fetched |
| `test-install --arch aarch64` | the first gate on AArch64 | yes |
| `test-install --usb-live` | the ISO attached as `usb-storage` on `qemu-xhci`, target on `nvme`: H1, H3 and H4 under QEMU | after H4 |
| `test-install --ahci` | target on QEMU's `ich9-ahci` | after H2 |
| host tests of `ntfs-resize`, `ext4-resize`, btrfs shrink | volumes made by `mkntfs`, `mke2fs` and `mkfs.btrfs`, filled by a seeded generator (fragmented files, sparse files, hard links, ADS and compressed files on NTFS, inline data on ext4), shrunk, then checked by `ntfsresize --check` and `ntfsfix -n`, `e2fsck -fn`, `btrfs check`, and the tree hash of §4.6; plus every refusal case; plus a power cut injected after each flushed write, then the checker | yes |
| `test-install --shrink` | the `--beside` gate on a disk whose "other system" fills it: NTFS, ext4 and btrfs in turn; the installer shrinks it, the other partition's tree hash is unchanged | yes |
| `test-install --windows` | a real Windows 11 install (the evaluation image, kept on the build host, never in the repository) shrunk by the installer; then Windows boots, runs its own `chkdsk`, and reaches its desktop | no: needs the image; run before every release that changes the shrinker |
| `test-compositor --boot installer` | each page's screenshot; a click-through by virtual pointer ends in the same disk as the answer file | yes |
| the reference PC | §1's exit, by hand, with a log kept in this document | no |

QEMU emulates NVMe, AHCI, xHCI, USB storage and i8042 faithfully enough
that every driver of §6 is gated before it meets the reference PC, which
then only tests the machine.

## 8. Documentation

`docs/INSTALL.md`, for someone who has never built Ferrix: where to
download, how to write a stick on Windows, macOS and Linux, enrolling Ferrix's Secure
Boot key, what "beside" can and cannot do, and how to boot a VM from the
ISO in QEMU, VirtualBox, virt-manager and UTM. `README.md` and the website
point at it before they point at `cargo xtask run`.

## 9. The slices and their points

In landing order. The VM path is first and complete on its own: after I9
anyone can install Ferrix in a VM from a release. The PC slices can start
beside it (they touch nothing the VM slices do) once H0 has shown the
reference machine boots.

| # | Slice | Points |
|---|---|---|
| I1 | Block devices from userland (§5.1) | 5 |
| I2 | `src/lib/fs/partition`, partitions in the block core, root by `PARTUUID`/`LABEL` on any disk (§5.2) | 8 |
| I3 | `src/lib/fs/btrfs-mkfs`, `/sbin/mkfs.btrfs` (§5.3) | 8 |
| I4 | `src/lib/fs/fat` with read-and-add, `mkfs.fat` (§5.4) | 5 |
| I5 | The engine and `/bin/ferrix-install`, answer files; `root_disk::install` shared (§4.1, §4.2) | 8 |
| I6 | `xtask live`: GPT `.img`, hybrid `.iso` (§3.1) | 8 |
| I7 | `test-install`, blank disk and `--beside`, x86-64 and AArch64 (§7) | 5 |
| I8 | The loader's boot menu (§5.6) | 5 |
| I9 | UEFI runtime variables and `efivars` (§5.5) | 8 |
| I10 | Widgets (§4.5) | 13 |
| I11 | The graphical installer, its screenshot gate, the live session's autostart and login | 13 |
| I12 | Release images, checksums, `docs/INSTALL.md`, README and website (§3.3, §8) | 3 |
| I13 | Secure Boot through Ubuntu's shim: fetch, signing, the loader's kernel check, `--secure-boot` gate (§5.7) | 13 |
| | **VM path** | **102** |
| S1 | `src/lib/fs/ntfs`: boot sector, MFT, attributes, run lists, `$Bitmap`; the consistency pass; the refusals (§4.6) | 13 |
| S2 | `ntfs-resize`: relocation, run-list rewrite, `$Bitmap`/`$BadClus`/backup boot sector, crash-ordered writes | 21 |
| S3 | `src/lib/fs/ext4` read and the `ext4-resize` shrink | 21 |
| S4 | btrfs shrink on `btrfs-write` | 8 |
| S5 | The engine's shrink step, the tree-hash check, the GUI's divider and warnings, `test-install --shrink` | 8 |
| S6 | `test-install --windows` against a real Windows 11 | 5 |
| | **Shrinking** | **76** |
| H0 | First boot on the reference PC: serial or screen, ACPI, interrupts, timer (a guess) | 13 |
| H1 | NVMe | 8 |
| H2 | AHCI | 8 |
| H3 | xHCI with HID | 21 |
| H4 | USB mass storage | 5 |
| H5 | i8042 keyboard and touchpad | 5 |
| H6 | Firmware framebuffer display for hyprix | 5 |
| H7 | The reference PC's exit, and what it finds | 8 |
| | **Real PCs** | **73** |
| | **Total** | **251** |

At the fleet's measured pace these are days, not weeks; the PC half's
risk is H0 and H7, which no estimate covers. Shrinking lands after I7 (it
needs the engine and the gate) and beside the GUI; S1 can start at once.

## 10. For the customer to decide

1. **Shrinking the other system.** *Decided 2026-09-28: the installer
   shrinks it itself*, NTFS, ext4 and btrfs (§4.6, slices S1–S6, 76 points).
2. **The live session's account.** A user `ferrix` with no password, logged
   in automatically, as Ubuntu's live session does. *Recommended.* Or a
   login prompt with a password printed on the boot screen.
3. **The reference PC.** Which machine H0 and H7 are run on, and whether the
   product owner may boot it from a stick (`hardware-use` rule: the product
   owner alone decides hardware use; nothing is written to its internal disk
   except in H7, and only beside what is there).
4. **Secure Boot.** *Decided 2026-09-28: supported*, through Ubuntu's
   signed shim and a Ferrix key enrolled once in MokManager (§5.7, I13).
5. **A text-mode fallback.** The CLI of §4.1 exists anyway for the gates; it
   can gain a menu for machines where the desktop does not come up (2
   points). *Recommended*, since H6 is the only screen a PC is sure of.

## 11. Where it stands

2026-09-28: this design, written, and approved by the customer the same
day. §10: decisions 2 and 5 as recommended; decision 1 the customer's
own, a full shrinker (§4.6); decision 4, Secure Boot through Ubuntu's shim
(§5.7); decision 3, the reference PC, is open, and the
VM path does not wait on it. I1 is built (§5.1).

**The MVP (2026-09-28, the customer: "land a MVP for now, we need to save
tokens").** What installs Ferrix in a VM today, built from what exists:

* `build --installer` carries `/sbin/ferrix-install` (`src/user/system/linux/installer`)
  and the packed empty root volume. The live image is the usual FAT image,
  attached as a virtio disk.
* `ferrix-install [--yes] [--from /dev/vdX] /dev/vdY` wipes the target and
  writes a GPT (`src/lib/fs/partition`): an ESP that is a byte copy of the live
  FAT volume, and a root partition holding the 1 GiB `ferrix-root` volume.
  No mkfs.btrfs (I3) and no FAT writer (I4) were needed.
* The kernel publishes GPT partitions as disks (`fs/partitions.rs`, once,
  before the root is looked for) and finds `ferrix-root` on any disk or
  partition, not only `vdd`..`vdg`. Its first boot installs the system, as
  for `run`.
* `cargo xtask test-install` (x86-64) installs from a live disk onto a blank
  one, then boots the blank one alone as a virtio disk through OVMF and
  requires `/ is btrfs on vdd2`. Not in `check` yet.

**I6a, the live medium on a GUID partition table (2026-10-07, po10-install,
5 points).** §3.1's `.img`, without the ISO yet:

* `cargo xtask live` writes `build/<arch>/ferrix-live.img` for x86-64 and
  AArch64: a GUID partition table whose one partition, at 1 MiB, is an EFI
  system partition named `FERRIX-LIVE`, holding the image `build
  --installer` writes with its BPB's hidden sectors set, and
  `FERRIX/CMDLINE.TXT` saying `ferrix.root=tmpfs`. The live session keeps
  `/` in memory, so booting the medium never mounts or installs onto a disk
  the machine already has. The partition's name does what §3.1's ESP label
  was to do. The disk and partition GUIDs are fixed, so two builds of one
  tree write the same image.
* `ferrix-install` without `--from` finds the live system by itself: the
  `FERRIX-LIVE` partition holding `xtask`'s FAT volume, or, for the MVP's
  image, a whole disk starting with it; more than one is an error. It
  refuses a target that holds the live partition (`/dev/vdd` for
  `/dev/vdd1`), and on the installed disk's copy of the volume it rewrites
  `ferrix.root=tmpfs` to `ferrix.root=btrfs` in place, with a reader of
  `xtask`'s FAT volumes that §5.4's library (I4) replaces, as it replaces
  the copied `CMDLINE.TXT` with §4.2 step 5's `EFI/ferrix/`.
* `test-install` boots the medium alone first, as a virtio disk through
  OVMF, and requires `vdd1` published and the tmpfs command line read from
  it; then asks the installer for `/dev/vdd`, which it must refuse, then
  installs on `/dev/vde` with the live system found by itself. A host test
  reads the medium's table back with `ferrix-partition`.

Left for later, in §9's order: a root file system the size of its partition
(I3), `BLKRRPART`, the ISO (I6b), `test-install` on AArch64, the boot menu (I8),
UEFI variables (I9), the graphical installer (I10, I11), Secure Boot (I13),
shrinking (S1–S6) and real PCs (H0–H7).
