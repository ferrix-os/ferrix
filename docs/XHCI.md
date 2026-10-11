# xHCI: a PC's USB ports

Version 1, 2026-10-10, session `xhci-driver`. Slice H3 of
[INSTALLER.md](INSTALLER.md) §6 and §9 ("xHCI with HID"), and the part of
H4 ("USB mass storage") that fits behind it today. The customer's PC, an
ASUS ProArt B650-CREATOR with a Ryzen 9 9900X, has four USB controllers and
every one is xHCI; it has no PS/2 keyboard to fall back on.

## 1. What this is, and what it is not

A ring-3 driver for an xHCI host controller, behind the host stack the
STM32MP15's EHCI already had ([INPUT.md](INPUT.md) §7): a keyboard and a
mouse plugged into a PC's USB port become `/dev/input/event<N>`, come and go
as they are plugged and unplugged, and survive their driver being killed.
A memory stick is found, sized and read.

It is **not** proved on the PC. Everything below was run against QEMU's
`qemu-xhci` and against a model; §8 says what that leaves open, and §9 is
the list for the first run on the board, which is the product owner's to
decide and nobody's to do from this branch.

Not built: a disk served to the block core (§6), isochronous transfers,
streams, SuperSpeed hubs, link and port power management, and more than
one interrupter.

## 2. Where it sits

| Piece | Where | In the certified item |
|---|---|---|
| The controller, the bus, HID, the disk | `src/lib/drivers/usb/usb-host` (`xhci.rs`, `host.rs`, `bus.rs`, `storage.rs`) | no |
| The process | `src/user/system/native/drivers/usb/usbhid`, the same program that drives the board's EHCI | no |
| The match | `devmgr`'s `PCI_DRIVERS`: class 0C:03:30 of any vendor, kind `Host` | no |
| Eight input claims on the node | `DeviceNode::input_functions`, `L.device.21` | **yes** (core) |
| The gate | `cargo xtask test-usb` | no |

`usbhid` is in every image's `/lib/drivers` already, so the image gains no
file. `devmgr` starts it on each xHCI function as it starts it on the
board's EHCI node: its device and START over the bootstrap channel, a wait
of at most five seconds for the driver's word that what was plugged in at
boot is published, and no PUBLISHED. The PC's four controllers are four
processes.

The driver gets what every PCI driver gets and nothing else:

* **Registers.** The apertures of the function's first BAR. The kernel
  withholds the pages of the MSI-X table and its pending bits, so one BAR
  may be several apertures; the driver maps each at its place and reads a
  register in a gap as ones. The capability registers must be in the first.
* **Interrupt.** The function's vector 0: MSI-X entry 0, or its MSI
  message. The driver binds it to its port, reads the event ring when it
  fires, and acknowledges it, which is what unmasks the vector. All four of
  the PC's controllers have MSI-X (read from the host's sysfs, 2026-10-10).
* **Memory.** One VMO, pinned before it is mapped. How large is read from
  the controller first: seven pages, as many scratchpad pages as
  `HCSPARAMS2` asks for, and a pool of 96. Its addresses are whatever the
  pin gives, a page at a time, past 4 GiB if the controller takes 64-bit
  pointers (`AC64`) and refused if it does not and a page is. Bus mastering
  goes on at the pin, as for every function; behind an IOMMU the addresses
  are the domain's.
* **Input.** One input control channel per keyboard or mouse, up to eight
  on the node (`USB_INPUT_FUNCTIONS`). This is the one change inside the
  certified item, and §10 says what it is.

## 3. What changed in the host stack

`Bus` was written against `ehci::Controller`. It is now generic over a
trait, `host::Host`, which has a step only where the two controllers part
ways:

| Step | xHCI | EHCI |
|---|---|---|
| `open(place)` | Enable Slot, the slot's contexts, Address Device: the **controller** sends `SET_ADDRESS` | nothing: a device in the default state answers at address 0 |
| `assign(target, address, max_packet)` | Evaluate Context, if the control endpoint's packet size is not the speed's default | `SET_ADDRESS`, as the bus sent it before |
| `set_hub(target, ports, think_time)` | Configure Endpoint with the slot's hub flag: the controller routes through a hub only once told | nothing |
| `release(target)` | Disable Slot, and the slot's pages back to the pool | nothing |
| `ports_changed()` | a port status change event brings the next poll forward | never |
| `open_bulk`, `bulk`, `close_bulk` | bulk pairs | `Unsupported` |

The rest -- root ports, control transfers, interrupt IN pipes, `service`
-- is what `ehci::Controller` always offered, now behind the trait.
`Target` gains the slot, the route string and the root port; `Speed` gains
`Super`; `RootReset::HighSpeed` became `Ready(speed)`, since an xHCI root
port takes a device of any speed where EHCI hands a slow one to a
companion, and gained `Failed`; an endpoint carries its `bInterval`, which
xHCI schedules by and EHCI ignores. The types moved to `host.rs` and are
re-exported from `ehci`, so `ehci::Error` is still a name.

EHCI's transfers are the same, in the same order. Its 37 tests pass with
three lines changed in two files: the bus's type alias in `tests/bus.rs`,
and `Speed::Super` added to two matches in `tests/model.rs` (which also
declares the new model as a child module).

## 4. The controller

`xhci.rs`, written against the eXtensible Host Controller Interface
specification, revision 1.2; each constant names the table it was read
from. Facts about what real controllers need beyond the text were read from
Linux 6.18's `drivers/usb/host` (`xhci-pci.c`, `pci-quirks.c`,
`xhci-ext-caps.h`) and are marked as Linux's where they are used; no code
was taken from it.

* **Hand-over.** The USB legacy support capability (§7.1 of the
  specification): the OS-owned semaphore is set, the BIOS-owned one waited
  for a second, and taken regardless if firmware does not let go, as Linux
  does; then every SMI enable is cleared and the three change bits
  acknowledged.
* **Reset and start.** Halt, `HCRST`, wait for it and for `CNR`, require
  4 KiB pages, enable at most 32 slots, the device context base address
  array with the scratchpad array in entry 0, the command ring, one event
  ring segment (size, dequeue pointer, then the table's address, which is
  what starts it), interrupter 0 with a 40 µs moderation, run. Ports are
  powered if the controller switches power.
* **Ports.** The supported-protocol capabilities say which root ports are
  USB 2 and which USB 3, and the slot type each needs. A USB 2 port is
  reset to enable it. A USB 3 port enables itself as its link trains and is
  reset (warm) only if it did not. Every write to `PORTSC` carries the
  power, indicator and wake bits as read and never the enabled bit, which
  disables the port when written as one. The speed is `PORTSC`'s, by the
  default speed IDs.
* **Slots and contexts.** 32 or 64 bytes as `CSZ` says. One input context
  page serves every command, since commands run one at a time. A slot's
  context names the route string, the root port, and for a full- or
  low-speed device behind a high-speed hub that hub's slot and port.
* **Rings.** A page each, the last TRB a link back with the toggle bit.
  Each TRB's control word is written last. The command ring and the event
  ring wrap in the tests (800 reports through one pipe).
* **Control transfers.** Setup (immediate data, the transfer type), data,
  status with an interrupt; a short data stage is taken from its own event
  and the transfer ends at the status stage's.
* **Interrupt IN.** Eight requests queued, each with a buffer of its own;
  one more is queued as each completes. The interval is Table 6-12's.
* **Bulk.** A transfer of at most a page, waited for. The disk layer above
  it cuts a command's data into pages.
* **Recovery.** A stalled or failed endpoint is brought back with Reset
  Endpoint and Set TR Dequeue Pointer, and the device's halt cleared with
  `CLEAR_FEATURE`; a transfer that times out is stopped with Stop Endpoint
  first. An interrupt pipe is recovered four times in a row and then left
  stopped, which the driver says. A command that is not answered in five
  seconds, or a controller that reports a host system or controller error,
  ends the driver, which devmgr then starts again.
* **Hubs.** A USB 2 hub is walked by its port status, as before, 250 ms
  apart or at once after a port status change event on a root port. The
  controller is told it is a hub before anything behind it is addressed.

**Quirks.** Linux's table has nothing for the PC's four controllers that a
driver without power management needs: `1022:43f7` (the 600-series
chipset's) gets only a runtime power management default, and `1022:15b6`,
`15b7` and `15b8` (the processor's) are not in it. The AMD PLL quirk is for
older southbridges, and the 0x96-revision quirk for older controllers.

## 5. Unplugging, and a driver that dies

A device unplugged has its pipes dropped from the slot's configuration,
its slot disabled, and its input channels closed, which the input core
hears as the device going. Its pages return to the pool only if the
controller took the command that makes it forget them.

A driver that dies is started again (`docs/DEVMGR.md` §4): `devmgr`
restarts a bus host on PCI, which it did not do for any bus host before.
The quiesce turns bus mastering off and waits for every one of the dead
driver's input claims; the new driver resets the controller before it
touches anything else; and the dead driver's pinned pages stay mapped in
the quarantine until the input core accepts the next HELLO, as for every
restarted kind. A board's EHCI host is still not restarted: nobody has
tried it on the board.

## 6. Disks

`storage.rs` is the bulk-only transport and the SCSI a disk needs
(`INQUIRY`, `TEST UNIT READY`, `REQUEST SENSE`, `READ CAPACITY (10)`,
`READ (10)`, `WRITE (10)`, `SYNCHRONIZE CACHE (10)`), with the transport's
reset when the two sides fall out of step. The bus opens a disk for every
such interface, and `usbhid` says each one with its size and a sum of its
first block.

**Not built: the disk served to the block core.** That is INSTALLER.md's
H4, and it does not fit as a small step. A block ring carries DMA memory,
and `claim.rs` allows more than one claim on a node only to a core whose
channel carries none; the host's node already shares its input claims.
The certification consultant's design review of 2026-10-10 (its ledger,
line 672) asks for a design review of its own before it is built, showing:
block claims at a limit of one in the block core's table, apart from the
input claims; the block ring's pins in the host controller's domain, and
in the quarantine when the driver dies; what a parked disk's replay does
when the disk was behind a hub that is gone; and a requirement with
reserved ids and a control. `devmgr` also names disks itself (`vda`, ...)
as it starts each disk's driver, and a bus host's disks appear later.

## 7. Tests

**Host tests** (`cargo test -p ferrix-usb-host`, 59: EHCI's 37 and 22 new).
`tests/model/xhci.rs` is a model of the controller over the EHCI model's
devices: it walks the rings as written, cycle bits and link TRBs included,
and writes its events to the ring the driver's table names. It records as
violations what a real controller would get silently wrong: a `PORTSC`
write that disables a port or drops a wake bit, a slot addressed with the
wrong route, speed, packet size or translator, a device addressed behind a
hub the controller was not told is one, a setup stage whose type
contradicts its data stage, an endpoint of the wrong type or interval, a
doorbell rung on a halted endpoint, a `SET_ADDRESS` software sent itself,
a controller started while firmware owns it or with SMIs on. Its pages are
scattered, some past 4 GiB. The tests: the hand-over both ways; a
low-speed keyboard on a root port; a high-speed hub with a full-speed mouse
and a low-speed keyboard behind it, with 32- and 64-byte contexts; a
SuperSpeed device on a USB 3 port; forty rounds of unplugging and plugging
(more than the pool holds if a page leaked); a stalled request; a pipe
that stalls once, and one that stalls for good; 800 reports across the
rings' links; reports that arrive during a transfer; a controller that
stops answering; a 32-bit controller given pages past 4 GiB; and a disk at
high and SuperSpeed that reads, writes, flushes, fails a command, stalls,
and goes with its stick.

**The gate**, `cargo xtask test-usb` (x86-64, KVM by default, `--accel
tcg`): `test-restart --boot usb`. QEMU has a `qemu-xhci` with a `usb-hub`
on one socket, a `usb-kbd` behind the hub, a `usb-mouse` on another and a
`usb-storage` on a third, at SuperSpeed. The gate requires:

1. the driver's lines for the controller, the hub and the stick, the
   stick's with its block count and the FNV-1a sum of its first block as
   the gate worked them out from the image it attached;
2. `a` pressed and released, the mouse moved by (7, -3) and clicked, all
   through QMP's `input-send-event`, at the nodes `evecho` found the
   keyboard and the mouse on, with `SYN_REPORT`;
3. the keyboard unplugged with `device_del`: the driver's `unplugged` and
   the input core's `is gone`; then one plugged in with `device_add`,
   found, and typing;
4. twice: `kill -9` of `usbhid`, devmgr's DIED and its restart, everything
   found again with the same stick, the shell alive, and a key arriving.

**The negative control** is a second boot with another stick attached than
the one the gate holds the sum of, and the `evecho` that reports every key
as `KEY_RESERVED`. Both checks must fail, and for their reasons: a disk
line with another sum, and a key delivered under another name.

The kernel's boot check for `L.device.21` runs on every boot of every
gate; its two controls are `fleet/gate.sh control` runs (the arm removed;
the arm widened to every 0C:03 class).

## 8. What has only been proved on QEMU's emulated controller

Everything. Said plainly, piece by piece:

* **The hand-over from firmware** ran against the model only. QEMU's
  `qemu-xhci` has no legacy support capability. On the PC it is mandatory,
  and it is the first thing that can go wrong.
* **Scratchpad pages**: QEMU asks for none. The model asks for two.
* **64-byte contexts**: QEMU's are 32 bytes. The model's are both.
* **MSI-X on a real controller, and the interrupt's acknowledgement**:
  QEMU's works. Whether a real controller raises again for an event that
  arrived between the driver's last look and its acknowledgement was
  designed for (the ring is read once more after the busy flag is cleared)
  and not seen.
* **A BAR cut around its MSI-X table**: QEMU's table is at the BAR's end,
  so the registers are one aperture. A table in the middle was never met.
* **Real reset times.** QEMU's `HCRST` and port resets finish at once. The
  waits are Linux's, and none was ever waited out by a real controller.
* **USB 3 link training.** QEMU's USB 3 port is enabled the moment a
  device is there. A real port trains for tens to hundreds of milliseconds
  and can fail; the warm reset for a port that did not enable ran against
  nothing.
* **Transaction translators.** QEMU's hub is full speed, so no device in
  the gate is reached through one; the model's is high speed and checks
  the slot's translator fields, which a real controller uses and the model
  only compares.
* **Low-speed devices.** QEMU's keyboard is full speed.
* **Error paths against real devices**: stalls, babble, transaction errors
  and timeouts ran against the model. QEMU produced none.
* **A real disk.** QEMU's `usb-storage` is ready as soon as it is asked
  and never stalls; a disk that spins up, or stalls its data stage, ran
  against the model only.
* **Hubs deeper than one tier, and SuperSpeed hubs**: one tier ran. A
  SuperSpeed hub is refused by design, so a USB 3 device behind a USB 3
  hub is not found; the same hub's USB 2 half works as a USB 2 hub.
* **AArch64.** Under QEMU's `virt` and EDK2 the controller is left with
  memory decoding off, so the kernel mints it no aperture and the driver
  exits (`usbhid: stopped at Registers`). The kernel's rule is deliberate
  -- nothing says firmware placed a BAR it does not decode -- and it was
  not changed. If the PC's firmware leaves a controller that way, the same
  happens there, and the `devices` line counts it ("with decoding off").
* **More than two input functions on one node at once.** The gate holds
  two. The limit is eight a controller; a keyboard often has two or three
  HID interfaces.
* **Timing.** Every run was on a host at a load of 30 to 46.

## 9. The first run on the board

Not this branch's to do. Hardware use is the product owner's decision
(`AGENTS.md`), with the list of addresses below per boot. The driver writes
nothing persistent: no flash, no firmware setting, no disk block (the disk
layer's `WRITE (10)` is called by nothing).

**What the driver writes, on each of the four controllers** (`0b:00.0`
`1022:43f7`, BAR0 32 KiB; `0e:00.3` `1022:15b6`, `0e:00.4` `1022:15b7` and
`0f:00.0` `1022:15b8`, BAR0 1 MiB each): the registers inside BAR0 --
`USBCMD`, `USBSTS`, `DNCTRL`, `CRCR`, `DCBAAP`, `CONFIG`, each port's
`PORTSC`, interrupter 0's `IMAN`, `IMOD`, `ERSTSZ`, `ERSTBA` and `ERDP`,
the doorbells, and `USBLEGSUP` and `USBLEGCTLSTS` in the extended
capabilities. The kernel writes the function's command register (bus
mastering) and its MSI-X capability and table. Nothing outside the
function.

**Before it.** INSTALLER.md's H0 (the PC boots Ferrix at all: ACPI,
interrupts, timer, a console) is a prerequisite and is not part of this
work. Without a serial port or a screen there is nothing to read.

**The list, in order.** Stop at the first line that is not as expected and
keep the log.

1. The boot's `devices` line: the four functions counted, none "with
   decoding off". If one is, stop: §8's AArch64 case.
2. `devmgr`'s REPORT line counts four more started and none failed.
3. `usbhid: xHCI running`, four times. If instead `the xHCI controller
   would not start: Unresponsive`, the hand-over or the reset failed; if
   `Memory`, the scratchpad count is past 512 or a page has no address the
   controller takes; `stopped at Registers` is step 1 or no vector.
4. With nothing plugged in but the keyboard: one `device ... N input
   functions` line and one `is event<N>` line per function, within a second
   of the controller's line. The Keychron K8 Pro is a full-speed device
   with several HID interfaces.
5. Keys arrive: `evecho` or the console. Caps Lock lights.
6. Unplug it and plug it in again, in the same port and in a port of each
   of the other three controllers: `unplugged`, `is gone`, then found.
7. A mouse; then a USB 2 hub with both behind it.
8. A memory stick in a USB 3 port: the `disk 0` line with a plausible
   size. Then the same stick in a USB 2 port.
9. `kill -9` of one `usbhid`: devmgr's DIED and restart lines, and the
   devices back.
10. An hour idle with the keyboard in, then a key: no `Stopped` note, no
    `Faulted` exit.

A keyboard of the customer's own is plugged into the PC for this, by the
customer. Nothing here is a reason to attach a host device to a VM.

## 10. The change inside the certified item

One: `DeviceNode::input_functions` answers eight for a PCI function whose
class code is exactly 0C:03:30, as it does for the STM32 board's host. The
argument for sharing a node's claims (`claim.rs`) does not depend on how
the node was found: an input control channel carries ports and never
memory, and every channel is made by the holder of the node's `MANAGE`
handle, which `devmgr` gives one driver. `L.device.21` and `H.DEV.1` say
so, `L.device.21` left the baseline for a boot check, and the input core
announces PUBLISHED to devmgr only for a node with one input function.
The consultant's design verdict is OK IF X1-X9 (its ledger, line 672).

## 11. Deviations from INSTALLER.md

INSTALLER.md decides little about H3: a ring-3 driver behind `usb-host`,
started by `devmgr` on its PCI class, gated under QEMU before it meets the
PC. All of that is as built. Where this differs:

* **H4 is half built** (§6): the disk is found and read, and not served.
  `test-install --usb-live`, which INSTALLER.md §7 plans after H4, is
  therefore not possible yet.
* **The gate is `test-usb`**, a row of its own, rather than part of
  `test-install`: it needs no installer, and the driver's restart belongs
  with `test-restart`.
* **H0 has not run**, so H3 was built before the PC was shown to boot,
  which §9 of INSTALLER.md allows ("the PC slices can start beside it once
  H0 has shown the reference machine boots") only loosely: the customer
  asked for the driver now.

## 12. Where it stands

2026-10-10: built and gated under QEMU on branch `xhci-driver`, not landed.
Next, in order: the first run on the PC (§9, after H0); the disk served to
the block core (§6, a design review first); SuperSpeed hubs; the kernel's
answer for a function firmware left with decoding off, if the PC shows it.
