# USB serial adapters: the FT232H and the USB-Blaster, from ring 3

Ferrix drives FTDI's USB UARTs -- the FT232H (`0403:6014`, "Single
RS232-HS"), the FT232B/R (`0403:6001`, which QEMU's `usb-serial` emulates)
-- and Altera's original USB-Blaster (`09fb:6001`, an FT245B in front of a
CPLD) from a class driver process of their own. This document says how,
what works on what evidence, what is still worse than Linux and why, and
-- because the customer asked for it plainly -- what in Ferrix's design
made the job harder than it should be.

Branch `ftdi-driver`, 2026-10-25. Built so far: the ring-3 driver and its
interface export (R1, R2), and three kernel items under the certification
consultant's OK IF, ledger 4317: numeric speeds (K3), back-pressure and
flow control (K1), hang-up on the master's close (K6). Designed and not
built: the driver packet mode (K2), the published node (K4'), the
libusb-style claim (R3) and the by-id names (R4); each has a
`docs/backlog/p2.md` row.

## 1. Where the driver is

Ferrix's kernel knows nothing of USB. devmgr hands the xHCI controller to
`usbhid`, which runs the USB stack (`ferrix-usb-host`). Keyboards, mice and
sticks are class drivers inside it; **every other interface is exported**
(§6.1, R2):

```
 usbhid (owns the xHCI controller)          devmgr                 usbserial
 ── interface found ──► ATTACH{identity} + channel end ─► table ─► BIND{number} + channel
 ◄── CONTROL / OPEN / SEND / FREE ────────────────────────────────── (shared VMO)
 ──► DONE / IN / SENT / EMPTY / LOST ──────────────────────────────►
```

| Piece | Where | What it does |
| --- | --- | --- |
| Protocol | `src/lib/proto/usbif` | ATTACH, BIND, HELLO with a 32 KiB VMO (control 1 KiB, two 4 KiB OUT slots, sixteen 512-byte IN slots), CONTROL, OPEN, SEND, FREE, DONE, IN, SENT, EMPTY, LOST; `allowed`, usbfs's rule for a claimed interface. |
| Export | `usb-host/src/bus.rs` (`Bus::export_*`) | An interface that is no hub, HID or disk is exported with its identity and strings; control requests filtered; streams only on its own bulk endpoints; whatever a driver opened closed when it goes. |
| Streams | `usb-host/src/xhci.rs` | Bulk IN: four 512-byte requests queued, re-queued only as read (back-pressure). Bulk OUT (R1): at most six requests in flight per endpoint, never waited for; a stall stops and resets the endpoint before any buffer is reused and counts what was lost. |
| Server | `usbhid/src/export.rs` | Each export's channel and VMO; the host owns the DMA memory and copies, no class-driver page is pinned. A driver's end closing re-attaches the interface. |
| Matching | `devmgr/src/usb.rs` | An image-time table (FTDI's vendor, the Blaster) names `usbserial`; a sealed job per driver; the number of the class (`ttyUSB<N>`) stays a device's across restarts; the same `ferrix-restart` policy and budget as every restarted driver. |
| Chips | `src/lib/drivers/usb/usb-serial` | `ftdi` (families, divisors, requests, status bytes), `port` (the chip as `ftdi_sio` sets it up, per-byte flags as `ftdi_process_packet`, `TIOCGICOUNT` counters, break, latency, DTR/DSR flow), `blaster` (the CPLD protocol, scans, IDCODE chain, command lines), `mpsse` (the loopback). |
| Class driver | `usbserial` | The link over the channel, the chip, a pseudoterminal linked as `/tmp/ttyUSB<N>` or `/tmp/jtag<N>`. |
| Gates | `xtask/src/usb_serial.rs`, `ft232h.rs` | `test-usb-serial` (QEMU), `test-ft232h` (the real chip). |

The FTDI requests and divisors follow what Linux's `ftdi_sio.c` sends, the
Blaster protocol what OpenOCD's `usb_blaster` speaks; neither was copied.

## 2. How a program reaches it

`/dev` is a fixed table (`src/kernel/src/fs/devfs.rs`) and the kernel's one
forwarding of a character device to a ring-3 driver serves NVIDIA's major
alone, so `usbserial` serves its port through a pseudoterminal: it holds the
master and links the slave as `/tmp/ttyUSB<N>`. Bytes the chip receives are
written to the master; what programs write to the slave is read from it
and sent; settings are read back with `TCGETS2` before every write and
every 50 ms and sent to the chip when they change (`B0` drops DTR and
RTS); the interface going -- the device unplugged, or `usbhid` gone --
closes the master, which hangs the slave up (K6). With K4' the slave will
be published as `/dev/ttyUSB<N>` by devmgr (§5).

A USB-Blaster's terminal, `/tmp/jtag<N>`, takes command lines (`reset`,
`idcode`, `ir <bits> <hex>`, `dr <bits> <hex>`), answered `= ...` or
`! ...`; the chain is read and said on the console when the cable comes.
This stays a Ferrix-only name (the consultant's R-e); Linux parity for JTAG
tools is R3's claim.

## 3. What works, and the evidence

**QEMU, `cargo xtask test-usb-serial`** (x86-64, KVM): QEMU's `usb-serial`
(an FT232BM) and a `usb-kbd` on `qemu-xhci`. Three rounds through
`/tmp/ttyUSB0`, each a line each way with a nonce of the run. Between the
first and second the guest kills `usbserial` with SIGKILL: usbhid attaches
the interface again, devmgr starts the driver again under the same number
(status 137, "1 of 8" of its budget), and a key typed over QMP meanwhile
reaches `/dev/input/event0`, usbhid never restarted. Between the second
and third the guest kills `usbhid`: the kernel's T0 restart starts it
again, it exports the interface again, a new `usbserial` takes it. QEMU's
own trace shows the divisor decoded to 9600 then 115384 by each of the
three drivers. Negative control (gate.sh, `ftdi-r2-ctl`): usbhid not
re-attaching an interface whose driver died, `control: FIRED (line, no
panic): - no usbserial linked /tmp/ttyUSB0 again after the kill`.

**The real FT232H, `cargo xtask test-ft232h`** (the customer approved
passing it through; QEMU's `usb-host` takes `0403:6014` on bus 1 only and
gives it back to the host's `ftdi_sio`, which the gate checks): the chip
enumerates at high speed with 512-byte packets and no serial number
string; takes 9600, 115200 (115246 on the chip), 1000000 and 3000000 baud
and 7O2 framing, each request answered; reports its modem lines (CTS DSR
RI DCD all 0, nothing wired); and loops 4096 bytes through its MPSSE
engine back as sent, then is a UART again. Its TX and RX are wired to
nothing, so bytes on the pins and the rate on the wire are not shown.

**The model, `cargo test -p ferrix-usb-host -p ferrix-usb-serial -p
ferrix-usbif`**: the export filter, release and rebind, unplug under a new
key, the FT232H's requests, 12 Mbaud and 250000 exact, line errors flagged
as Linux flags them, modem-line counters, a megabyte through a reader
taking a chunk every 3 ms with nothing lost, a chip that NAKs OUT while a
keyboard on the same controller types (R1; negative control: the old
waiting write, which failed at "the write waited for the chip"), an OUT
stall, the MPSSE loopback, broken loopback and absent engine, and the
Blaster: the MAX 10 10M08's IDCODE 0x031820dd from a model CPLD and TAP
chain, IR and DR scans, BYPASS, a three-tap chain, an empty chain and TDO
held low.

**Not shown on hardware**: the USB-Blaster (bulk transfers to it failed on
the host after Quartus's `jtagd`; it waits for the customer's replug), and
`/tmp/jtag<N>` through `usbserial` (QEMU has no Blaster).

## 4. What is still worse than Linux

| Linux `ftdi_sio` + tty | Ferrix now | Verdict |
| --- | --- | --- |
| Binds by id table, per interface | devmgr's image-time table, per interface, own process (R2) | better: a fault ends only its interface |
| Driver restart after a crash | devmgr restarts it, same number, budgeted | better (Linux has none for a crashed module) |
| Every rate the chip can do | K3: `BOTHER` kept as a number; the H divisor up to 12 Mbaud | equal |
| Throttle when the reader is slow, no loss | K1: master write back-pressure, IXOFF; IN streams hold the chip back | equal |
| IXON/IXOFF/IXANY | K1, on a pty | equal |
| In-chip RTS/CTS, DTR/DSR, XON/XOFF | `port::configure` (`CRTSCTS`, `IXON`/`IXOFF`) | equal (DTR/DSR has no termios flag on Linux either) |
| Hang-up on unplug: SIGHUP, EIO, POLLHUP | K6 | equal |
| One chip's held-off TX stalls no other device | R1: a bounded OUT stream per endpoint | equal (Linux's URBs are asynchronous too) |
| `TIOCMGET`/`TIOCMSET`/`TIOCMIWAIT`/`TIOCGICOUNT` | the driver keeps the lines and counters; no ioctl reaches it until K2 | **worse**, until K2 |
| Break, `tcdrain`, `tcsendbreak` | `port::set_break`; no `TCSBRK` until K2 | **worse**, until K2 |
| Per-character errors (PARMRK, INPCK, IGNPAR, BRKINT) | flags computed per byte, not delivered until K2 | **worse** with INPCK/PARMRK set; equal at the default (INPCK off) |
| Settings applied at `tcsetattr` | polled every 50 ms and before each write | **worse**, until K2's records |
| `/dev/ttyUSB0`, `ttyname` | `/tmp/ttyUSB0`, `ttyname` says `/dev/pts/N` | **worse**, until K4' |
| `/dev/serial/by-id` | none | **worse**, until R4 |
| `/sys/class/tty/<name>/device` (pyserial `list_ports`) | none | **worse**; K7, no design yet (NOT YET, ledger 4317) |
| libusb/libftdi tools (OpenOCD) | none | **worse**, until R3 |
| MPSSE (JTAG/SPI/I2C) for programs | loopback diagnostic only | **worse** (§7) |

## 5. The kernel items and their load statements

The consultant's ruling (ledger 4317): no USB in the kernel, no class
table in `device.rs`; every item in the load ring, carried as numbered load
statements, each with its check and a negative control run through
`fleet/gate.sh control`. The checks are in `src/kernel/src/fs/pty_check.rs`,
run from `syscall::check`'s handler checks on every `test-boot`.

1. **serial-1** (K3, numeric speeds): a pseudoterminal keeps a `BOTHER`
   speed as the number `TCSETS2` gave -- 250000 reads back 250000 both ways
   from slave and master with `CBAUD` still `BOTHER`; `TCSETS` naming
   `BOTHER` keeps the numbers; split speeds stay split; a `B` code is its
   own speed. The console keeps its own speed (its path untouched, held by
   `check_the_console_answers_as_a_terminal`). Check:
   `check_a_pty_keeps_its_speeds_as_numbers`. Control: `TCGETS2` reporting
   the old code-only speeds, FIRED at "did not read back 250000 both ways".
2. **serial-2** (K1, back-pressure): a master write takes only what the
   line discipline has room for -- non-blocking the short count, `EAGAIN`
   at none; blocking it waits killably (`ERESTARTSYS`) and ends `EIO` once
   every slave that was opened has closed; the master polls writable only
   with room; 64 KiB through a slave read 300 bytes at a time arrive whole,
   0 bytes lost. An echo never makes a master write wait. Check:
   `check_a_master_write_takes_only_what_fits`. Control: the room test
   removed, FIRED at "bytes lost".
3. **serial-3** (K1, flow control): on a pty `IXON` stops the slave's
   output at `VSTOP` and starts it at `VSTART`, which go no further;
   `IXANY` restarts at any character; `IXOFF` sends `VSTOP` below 128 bytes
   of room and `VSTART` below 128 held. Check:
   `check_flow_control_acts_on_a_pty`.
4. **serial-4** (K1, canonical): a full canonical line with no terminator
   does not block the master; further bytes are taken and dropped, and a
   newline still ends the line. Check:
   `check_a_full_canonical_line_does_not_block_the_master`.
5. **serial-5** (K6, hang-up): the master's last close takes the terminal
   from its session, sends the leader `SIGHUP` and `SIGCONT`, and the slave
   reads end of file, writes `EIO`, polls hung up. Check:
   `check_a_masters_close_hangs_its_slave_up`. Control: the signal
   skipped, FIRED at "no SIGHUP".

Not built, with the conditions they carry (ledger 4317 (5)-(10), (15)-(19)):

* **K2, driver packet mode**: a Ferrix ioctl of its own on the master;
  coalesced state records master-bound (settings with numeric speeds,
  DTR/RTS, break, drain, throttle, HUPCL's drop) and slave-bound (CTS, DSR,
  RI, DCD, the driver's `TIOCGICOUNT` counters, the drain acknowledgement,
  the rate actually set); per-byte flags under `IGNBRK`, `BRKINT`,
  `IGNPAR`, `PARMRK`, `INPCK`; `TIOCMIWAIT`, `tcdrain` and timed break
  killable and ending `EIO` at hang-up; `CLOCAL`/DCD as `tty_port`. The
  ring-3 side is ready for it: `port` already computes the per-byte flags,
  the counters and the actual rate.
* **K4', the published node**: one class-free master ioctl, PUBLISH(name,
  major, minor), checked fail-closed in a stated order (privilege, name
  rule, no shadowing, number range, one name per pair, a bound); a 0660
  `dialout` node whose open is the slave and whose `ttyname` is
  `/dev/<name>`, gone with the pair; devmgr publishes and hands the master
  to the unprivileged class driver; VULNERABILITY-ANALYSIS V-17 and a
  SECURITY-TARGET note. **K7** (`/sys/class/tty/<name>/device`) is NOT YET:
  this is the gap, and R4's by-id names cover udev-style users meanwhile.

## 6. What the design gets wrong

### 6.1 One process was the whole USB bus (fixed by R2)

The process that owns the controller owned every device behind it: a fault
in the FTDI code stopped the keyboard, a restart dropped every device, a
new class was an edit to the host's driver, and no program could claim a
device. R2 exports each interface to a class driver process devmgr starts
by id, needing no kernel change: devmgr is told of what usbhid finds on
usbhid's own bootstrap channel. What is still missing is the claim for
programs (R3): an interface channel given by a broker on image-time policy,
exclusive, detaching the class driver as `USBDEVFS_DISCONNECT` does, the
host filtering what usbfs filters.

### 6.2 Every class of device needs its own kernel core

What programs see is decided in the kernel, class by class: the input core,
the block ring, the net ring, display, render, audio, logctl, and the
NVIDIA-only chardev forwarding. A ring-3 driver of a class the kernel has
no core for has no way into `/dev`, so moving a driver out of the kernel
still needs a kernel change per kind of driver -- the opposite of keeping
the kernel slim. K4' is the class-free fix for terminals: the kernel checks
generic invariants and the names are devmgr's data. For classes without a
kernel discipline (CAN, SPI, sensors) a CUSE-like forwarding (K5) is the
remaining design, NOT YET.

### 6.3 Terminals were only consoles and ptys

The terminal layer had no notion of hardware behind it: no throttle, no
flow control, no speed it did not know a name for, no hang-up to a
session, no modem lines, break or drain. K1, K3 and K6 add the first four
to the pty; K2 adds the rest.

## 7. MPSSE and the FT232H's JTAG

`mpsse::loopback` already enters the engine (`SET_BITMODE` 2), sets the
clock and clocks bytes through it. JTAG over MPSSE (0x4B for TMS,
0x39/0x3B for TDI with TDO) is a second implementation of `blaster::Jtag`'s
scan interface -- a `JtagCable` trait, a third `ftdi::Role`, the same
command lines -- and SPI/I2C the same engine; for programs that speak MPSSE
themselves (OpenOCD's `ftdi` driver) R3's claim is the way.

## 8. Running it

```
cargo test -p ferrix-usb-host -p ferrix-usb-serial -p ferrix-usbif
cargo xtask test-usb-serial --timeout 300     # QEMU, end to end, with the kills
cargo xtask test-ft232h --timeout 240         # the host's FT232H, passed through
```

On a booted Ferrix with an adapter plugged in:

```
stty -F /tmp/ttyUSB0 115200 raw -echo
cat /tmp/ttyUSB0 &
echo hello > /tmp/ttyUSB0
touch /tmp/usbserial-loopback-0               # as root: the MPSSE loopback, on an FT232H

echo idcode > /tmp/jtag0; head -n 1 /tmp/jtag0
```
