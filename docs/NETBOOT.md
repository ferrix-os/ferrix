# Network boot for a bare-metal test machine

Version 2, 2026-10-15 (version 1 the same day; version 2 adds the boot
log sent back, §5 and §6). The ask: boot the latest build on a small
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
them. Then it fetches the image's files, and the URL the loader sends its
boot log to (§5), and starts the loader.

The loader sends its boot log -- every line it printed, up to `handing
over to the kernel` or the error it stopped at -- back to the server, which
prints it and keeps it as
`~/.local/share/ferrix/netboot/reports/<MAC>/<seconds>-BOOTLOG.TXT`, the
latest also as `BOOTLOG.TXT` beside it, a directory per machine by its
network card's MAC address. So a loader that stops before the kernel says
why on the host as well as on the screen.

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
* The loader's boot log must reach the server: one whole `BOOTLOG.TXT`
  from the machine, beginning with `Ferrix loader`, every line of it
  printed on the guest's console in the same order, kept on the host as it
  arrived; and the loader must say it sent it. The log of some 500 bytes
  goes in two parts, so the reassembly is part of what passes.
* A second boot against the same server answering every report `404`:
  the kernel must still reach `FERRIX-BOOT-OK`, and the loader must say
  `report   boot log not sent` -- sending never stops a boot.
* The negative control: the same stick, the same server, every file now
  under `/wrong/`. It must fail with iPXE's own error for
  `/ferrix/boot.ipxe` (a `... (https://ipxe.org/<code>)` line naming it),
  the stick's `FERRIX-NETBOOT-FAILED`, a 404 in the server's log, and no
  kernel.

The test's stick is `netboot-stick`'s, built by the same code, with only
the address different.

## 5. How it works

### The loader reads its files as it does from a disk

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

### The boot log comes back

The loader keeps every line it prints in a 32 KiB buffer (`bootlog.rs`,
`docs/POSTMORTEM.md` PM2's console tee). It sends it to the server
(`netreport.rs`) only when both hold:

* its volume has `FERRIX/REPORT.TXT`. `boot.ipxe` fetches it as
  `report/hello/${netX/mac:hexhyp}`, and the server answers with
  `http://<the Host the request named>/ferrix/report/box/<MAC>/`; no disk
  image carries the file. The loader takes it only as one line of at most
  256 bytes, `http://` and printable ASCII without a space, and otherwise
  says it ignored it;
* its device handle carries iPXE's download protocol
  (`3eaeaebd-decf-493b-9bd1-cdb2decae719`), which iPXE installs on every
  program it starts.

A loader started from a disk finds neither and sends nothing; its console
differs only by the `handing over to the kernel` line.

It sends just before it leaves firmware -- or, when it stops with an error,
from the error path, unless it had already begun to leave firmware. What
iPXE allocates for the requests is in the final memory map, which is taken
after.

**GETs, not a POST.** `snp.efi` opens the card's Simple Network Protocol
exclusively, which disconnects firmware's own IP, TCP and HTTP drivers from
it, so a program iPXE starts has no network but iPXE's; and iPXE's download
protocol only fetches a URL (a POST needs iPXE `params`, which a program it
started cannot make). So the log goes as the query of GETs, 512 bytes a
part, in hex:

```
GET /ferrix/report/box/<MAC>/BOOTLOG.TXT?part=<i>&of=<n>&data=<hex>
```

parts in order from 0, each answered `ok`; the last completes the file. The
server checks everything before it keeps anything: the MAC is six hex pairs,
the file one it allows (`BOOTLOG.TXT`), `part` below `of`, `of` at most 64,
the hex whole bytes, the report at most 64 KiB, and parts in order; anything
else is answered `400` and the report under way is dropped. A path on the
host is built only from the checked MAC and the allowed name.

**Bounded.** Each request is aborted after 5 s and the whole send after
30 s, and the first request that fails ends it: the boot goes on with one
line saying which part failed and why. A server that is gone costs 5 s; one
that refuses (`404`) costs nothing to speak of.

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

* **The post-mortem of a netbooted run is the loader's boot log, sent to
  the server** (§5). iPXE's file system is read-only and the machine's own
  disk is never written, so nothing is left on the machine, and
  `ferrix-postmortem.sh` (PM1) has nothing to read. The kernel's own log
  of the previous run (`docs/POSTMORTEM.md` PM3, `LASTKERNEL.TXT`) does not
  exist yet; when it does, the loader sends it the same way. A kernel that
  panics after the handover says so on the screen only.
* **The report is plain HTTP, with no authentication, for a LAN.**
  Anyone on the network path reads it, and it holds what the loader
  prints: this boot's KASLR layout (the kernel's and the direct map's
  addresses, the page-table roots), the firmware's clock and the
  machine's MAC. KASLR gives a netbooted machine no protection against its
  network -- but whoever can read the path can equally serve the machine a
  kernel of their own (§3), so the report tells them nothing they could not
  decide. Anyone who reaches the server can also send it reports for a MAC
  of their choosing; it keeps at most 64 KiB a report and builds no path
  from the request. A disk-booted machine sends nothing.
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
`test-netboot` passes under KVM with its negative control. The same day
(branch `netboot-pm`): the loader's boot log comes back to the server, and
`test-netboot` checks it arrives and matches the guest's console. The stick has
not yet booted the N150 (§2 is the customer's to do).
