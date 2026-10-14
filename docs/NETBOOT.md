# Network boot for a bare-metal test machine

Version 1, written 2026-10-15. The ask: boot the latest build on a small
PC (the first is a SOYO M4 Pro with an Intel N150) without reflashing its
disk or carrying a stick back and forth after every change. The machine
has no serial port, so it also has to say on its screen, and to the host,
what it found.

A USB stick holding iPXE is written once. At every power-on it takes an
address from the network's own DHCP server and fetches everything else
from `cargo xtask netboot` on the development host, so every boot runs
whatever that tree last built. Nothing on the machine's disk is touched.

| Command | What it does |
|---|---|
| `tools/common/fetch/fetch-ipxe.sh` | fetch iPXE 2.0.0's release tarball, pinned by SHA-256, into `~/.local/share/ferrix/netboot/pool` |
| `cargo xtask netboot-stick --server HOST[:PORT]` | write `build/x86_64/ferrix-netboot-stick.img` for a server at that address (port 8086 if none) |
| `cargo xtask netboot [--listen ADDR:PORT\|PORT]` | build the image as `build` does and serve it until Ctrl-C, on this host's default-route address, port 8086 unless told |
| `cargo xtask test-netboot` | the whole path under QEMU, and its negative control (§4) |

All three are x86-64 only, and need a Linux host: iPXE is built with
`make`, `gcc` and `perl`.

## 1. The stick

`netboot-stick` writes a 66 MiB image: a GUID partition table whose one
partition is a 64 MiB FAT32 EFI system partition named `FERRIX-NETBOOT`,
holding iPXE as `EFI/BOOT/BOOTX64.EFI` and a `NETBOOT.TXT` that says what
it is. The image is the same for the same server, byte for byte (fixed
GUIDs).

The iPXE program is `snp.efi`: iPXE driving the network card through the
firmware's own UEFI driver (the Simple Network Protocol), so it needs no
driver of its own for the card. Every PC sold with UEFI network boot has
such a driver for its own card; iPXE's own drivers do not cover every card
(the N150's Motorcomm YT6801 is not among them). The price is that the
firmware must have its network stack switched on (§2).

Built into it is a script:

```
:retry
dhcp || goto failed
chain http://HOST:PORT/ferrix/boot.ipxe || goto failed
:failed
echo Network cards iPXE drives (none: the firmware's UEFI network stack is off):
ifstat ||
echo FERRIX-NETBOOT-FAILED: the error is above; trying again in 10 seconds
sleep 10
goto retry
```

so a machine switched on before the server is started boots once it is,
and a failure shows iPXE's error and the cards it can see on the screen.

## 2. Once, for a machine

1. On the host: `tools/common/fetch/fetch-ipxe.sh`, then
   `cargo xtask netboot-stick --server <host's LAN address>:8086` (a name
   the machine's DHCP-given DNS resolves works too, and survives the host's
   address changing).
2. Write `build/x86_64/ferrix-netboot-stick.img` to a USB stick, whole
   (`dd`, Etcher, or Rufus in "DD image" mode). Everything on the stick is
   lost.
3. In the machine's firmware setup: Secure Boot **off** (iPXE and the
   loader are not signed); the **UEFI network stack on, with IPv4** (often
   "Network Stack", "IPv4 PXE Support"), which is what gives `snp.efi` a
   card to drive; the USB stick **first** in the boot order. Leave the
   internal disk's entries as they are.
4. A wired connection to the network the host is on.

## 3. Every boot

On the host, `cargo xtask netboot`. It builds the image, prints the commit
it serves (`+changes` when the tree differs from it), and logs every
request with its status, size and speed. Then switch the machine on.

The served `boot.ipxe` first walks the machine's PCI bus: each function's
location, vendor:device and class goes on the screen and, as a request
for `report/pci/...`, into the server's log, marked `<- network`,
`<- storage` or `<- display` for those classes. That is how a machine with
no serial port says which chips it has before Ferrix has a driver for
them. Then it fetches the image's files and starts the loader.

The kernel is served stripped, as `flash` strips it: every byte comes
through the network at every boot. `netboot` prints where the kernel with
its symbols is, for a panic's addresses.

The server is plain HTTP with no authentication: anything on the network
that answers at that address boots on the machine. Serve on a network you
trust, and stop it when it is not needed.

## 4. The test

`cargo xtask test-netboot` runs a server on the host's loopback, writes a
stick for `10.0.2.2:<port>` (the address at which xtask's gateway hands
the guest the host's loopback), and boots it under OVMF with `--net`:

* the stick's `snp.efi` must take an address from the gateway's DHCP, run
  `boot.ipxe`, report the PCI bus (the virtio-net card it boots through,
  `1af4:1041` class 02, must be among the functions reported), and fetch
  every file of the image; and the kernel must reach `FERRIX-BOOT-OK`.
  The stick holds no kernel, so a kernel that boots read it through iPXE.
* The negative control: the same stick, the same server, every file now
  under `/wrong/`. It must fail with iPXE's own error for
  `/ferrix/boot.ipxe` (a `... (https://ipxe.org/<code>)` line naming it),
  the stick's `FERRIX-NETBOOT-FAILED`, a 404 in the server's log, and no
  kernel.

The test's stick is `netboot-stick`'s, built by the same code, with only
the address different.

## 5. How it works

### The loader needs nothing new

The loader reads `\FERRIX\KERNEL.ELF`, `INITRD.IMG` (and `CMDLINE.TXT`
when the image has one) through the `SimpleFileSystem` of the device it was loaded from. When iPXE
starts an EFI program it installs a file system of its own on that device,
listing every image it has fetched by its name, and compares the whole of a
name after the leading backslash (`efi_file.c`). So `boot.ipxe` fetches
each file under the name of its path on the image's volume
(`imgfetch --name FERRIX\KERNEL.ELF FERRIX/KERNEL.ELF`) and starts the
loader with `chain`; the loader finds them where it always looks. Proven by
`test-netboot` 2026-10-15: the loader says `boot esp` and the kernel
reaches its boot marker. No `sanboot` of the whole image is needed, so the
server needs no HTTP range requests.

### Why the test's network card is in the boot order

The first test boot found no network at all: `ifstat` listed nothing, and
the gateway saw no frame. OVMF connects drivers only to the devices QEMU's
boot order names (`bootindex`), to boot faster; the stick had
`bootindex=0` and the card none, so no driver was ever connected to it, no
Simple Network Protocol existed, and `snp.efi` had nothing to drive. The
same held for an `e1000e` with its option ROM, and with `iommu_platform`
off: it was never the card. With the card at `bootindex=1` (after the
stick, so the stick still boots first) `snp.efi` drives it as
`SNP-0000:00:08.0`.

So `test-netboot` puts the card in the boot order, the QEMU form of the
firmware setting in §2.3. The alternative was to boot the test with
`ipxe.efi`, iPXE with its own drivers, which drives virtio-net with no help
from the firmware and would have passed at once. It was not taken: the
test would then boot a program the stick does not carry, and pass exactly
when the firmware's network stack is off, which is the most likely way the
real machine fails. Keeping `snp.efi` keeps the test about the stick.

## 6. Limitations

* **No post-mortem from a netbooted run.** iPXE's file system is
  read-only, so the loader's boot log (`docs/POSTMORTEM.md` PM2,
  `BOOTLOG.TXT`) and its copy of the previous kernel's log (PM3,
  `LASTKERNEL.TXT`) cannot be written to the device the loader came from,
  and `ferrix-postmortem.sh` (PM1) has no disk to read: the machine's own
  disk is never written. A logging failure never stops a boot (PM2's
  rule), so the boot goes on; what is lost is the record. Until the loader
  can send it to the server (an HTTP `POST` of `LASTKERNEL` to `netboot`,
  or printing it on the screen at the next boot), the screen and the
  server's log are all a netbooted run leaves.
* **No network once Ferrix runs on the N150.** Ferrix drives virtio-net
  only; the box's Motorcomm YT6801 and Realtek 8852BE have no driver yet.
  The image arrives through the firmware's driver, and nothing after
  `ExitBootServices` can use it.
* **Plain HTTP, no authentication** (§3).
* **Firmware the stick depends on**: a firmware with no UEFI network
  driver for its card, or with the stack switched off, gives `snp.efi`
  nothing; the stick then prints an empty card list and retries.

## 7. Where it stands

2026-10-15: `netboot-stick`, `netboot` and `test-netboot` built;
`test-netboot` passes under KVM with its negative control. The stick has
not yet booted the N150 (§2 is the customer's to do).
