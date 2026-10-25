# USB serial adapters: the FT232H and the USB-Blaster, from ring 3

Ferrix drives FTDI's USB UARTs -- the FT232H (`0403:6014`, "Single
RS232-HS"), the FT232B/R (`0403:6001`, which QEMU's `usb-serial` emulates)
-- and Altera's original USB-Blaster (`09fb:6001`, an FT245B in front of a
CPLD) without one line of the kernel changed. This document says how, what
works, what does not, and -- because the customer asked for it plainly --
what in Ferrix's design made the job harder than it should be and what the
smallest kernel change that would fix it is.

Branch `ftdi-driver`, 2026-10-25. File and line references are to
bb2934faa, the commit the branch starts from.

## 1. Where the driver is

Ferrix's kernel knows nothing of USB. devmgr hands the xHCI controller, a
PCI function of class 0C:03:30, to one ring-3 process, `usbhid`
(`src/user/system/native/devmgr/src/main.rs:1935`), which maps its
registers, takes its interrupt and pins its DMA memory through the native
device ABI, and runs the whole USB stack itself: `ferrix-usb-host`
(`src/lib/drivers/usb/usb-host`). Keyboards, mice and sticks are class
drivers inside that one process. So is this one: there is no way to hand
one interface of one device to another process (§6.1).

| Piece | Where | What it does |
| --- | --- | --- |
| Bulk IN streams | `usb-host/src/xhci.rs`, `host.rs` | Four 512-byte requests kept queued on a bulk IN endpoint, completions held until taken, a request re-queued only when its chunk is taken (back-pressure to the chip instead of loss). OUT is a bulk pair with no IN half. |
| FTDI arithmetic | `usb-host/src/ftdi.rs` | Family by `bcdDevice`; the B-family divisor (3 MHz / n.eighths) and the H-family one (12 MHz, bit 17 set); the `wIndex` port byte (0 on B/R, interface+1 on H/X); `RESET`, purge, `SET_MODEM_CTRL`, `SET_FLOW_CTRL`, `SET_BAUD_RATE`, `SET_DATA`, `SET_LATENCY_TIMER`, `SET_BITMODE`; `termios` to a line; the two status bytes stripped a max-packet at a time. |
| Binding | `usb-host/src/bus.rs` | A device whose ids `ftdi::identify` knows, behind a controller that moves bulk data, becomes a serial port: reset, purged, latency 16 ms (2 ms for a Blaster), DTR/RTS up, a UART at 9600 8N1. Unplug closes the pipes and says so. |
| JTAG | `usb-host/src/blaster.rs` | The Blaster's CPLD protocol (bit-bang bytes TCK/TMS/nCE/nCS/TDI/LED/READ, byte-shift headers of up to 63 bytes with read-back), TAP reset, IR and DR scans from and back to Run-Test/Idle, an IDCODE chain read, and the terminal's command lines. |
| Exposure | `usbhid/src/tty.rs`, `serial.rs` | A pseudoterminal per adapter, linked as `/tmp/ttyUSB<N>` (UART) or `/tmp/jtag<N>` (Blaster). |
| Gate | `tools/common/xtask/src/usb_serial.rs` | `cargo xtask test-usb-serial`. |

The FTDI requests and divisors follow what Linux's
`drivers/usb/serial/ftdi_sio.c` sends; the Blaster protocol is the one
OpenOCD's `usb_blaster` driver speaks. Neither was copied: the code is
written from the behaviour, and the tests hold it to the chips' numbers
(the B divisor for 9600 is 0x4138, the H divisor for 115200 is 0x2C068,
every rate from 184 baud to 3 Mbaud within 3%).

## 2. How a program reaches it

### 2.1 Why a pseudoterminal

A Linux program expects `/dev/ttyUSB0`. Ferrix cannot give it one from
ring 3:

* `/dev` is a fixed table in the kernel, "not a directory nodes can be
  created in" (`src/kernel/src/fs/devfs.rs:3`, `:101`). Only block devices
  can be added, by the kernel's own glue (`devfs.rs:574`).
* The one forwarding of a character device to a ring-3 driver,
  `ferrix-chardevctl`, serves only NVIDIA's major 195 and its fixed names
  (`src/lib/proto/chardevctl/src/node.rs:17`, `:83`), and forwards open,
  ioctl, mmap and release -- `read` and `write` answer `EINVAL` and `poll`
  is never ready (`src/kernel/src/interfaces/chardev/file.rs:6`, `:174-189`).
  A serial port is nothing but read, write and poll.

What the kernel does give every process is a pseudoterminal
(`src/kernel/src/fs/pty.rs`), and a pty's slave is a terminal like any
other: the same line discipline as the console, `termios`, `VMIN`/`VTIME`,
job control, hang-up. So `usbhid` opens `/dev/ptmx`, unlocks it, sets it to
9600 baud as Linux opens a `ttyUSB`, and links the slave from `/tmp`
(`/tmp`, which the kernel mounts at boot, `src/kernel/src/fs/mod.rs:578`).
The master is the driver's end:

* bytes the chip received are written to the master -- a terminal's input;
* bytes programs write to the slave are read from the master and sent out
  of the bulk OUT endpoint;
* `TCGETS2` on the master reads the settings programs set on the slave.
  They are compared before every write and every 50 ms, and a change is
  sent to the chip as the three FTDI requests; `B0` drops DTR and RTS;
* unplugged, the master is closed: the slave reads end of file and writes
  `EIO`, which is what a hung-up `ttyUSB` does on Linux.

While a terminal is open, `usbhid`'s wait is a `ppoll` on its port's
descriptor (`port_fd`) and every master, since a port wait does not hear a
descriptor.

### 2.2 The USB-Blaster's terminal

JTAG is not a byte stream a program can usefully own, so `/tmp/jtag<N>`
takes lines, and answers each with one line, `= ...` or `! ...`:

```
idcode              = 1 0x031820dd
reset               = reset
ir 10 0x006         = 0x..1          (what IR captured: ...01 in its low bits)
dr 32 0             = 0x031820dd
```

`ir`/`dr <bits> <hex>` shifts up to 512 bits, least significant first, and
answers TDO the same way. When a cable is plugged in, the driver reads the
chain and says it on the console:

```
usbhid: jtag 0: 1 taps
usbhid: jtag 0 tap 0: idcode 0x031820dd (version 0, part 0x3182, maker 0x06e)
```

That is the shape a ring-3 JTAG service would have: the cable's protocol
and the TAP state machine in the driver, scans as requests. A program like
OpenOCD would want the raw cable instead; §6.4 says what that needs.

## 3. What works, and how it was shown

**On QEMU** (`cargo xtask test-usb-serial`, x86-64, KVM): QEMU's
`usb-serial` -- an FT232BM, full speed, 64-byte packets -- on `qemu-xhci`,
its line a Unix socket the gate holds. Busybox as init runs `stty -F
/tmp/ttyUSB0 115200 raw -echo`, writes a line, and reads one back. The gate
requires the driver's lines, the guest's line on the socket and the gate's
line in the guest (each with a nonce new for the run), and QEMU's own trace
of the divisor it received decoded to 9600 and then to 115384 (3 MHz / 26,
within 3% of 115200). The run on this branch:

```
usbhid: serial 0 0403:6001 (FT232B UART, 64-byte packets) is /tmp/ttyUSB0 (/dev/pts/0)
serial-gate: ready
usbhid: serial 0 set to 115200 baud, 8 data bits, parity None, 1 stop bits, flow None
serial-gate-in=host-to-ferrix-1f733ae1ccf06bd3
x86_64: stty set the FT232BM to 115200 baud (QEMU decoded the divisor to it),
  "ferrix-to-host-18c48fb99d70e678" came out of /tmp/ttyUSB0 onto the socket,
  and "host-to-ferrix-1f733ae1ccf06bd3" went the other way
```

Its negative control, run by hand: `ftdi::payload` handing on each packet
with its status bytes; the guest read a status byte before the gate's line
and the gate failed on it.

**On the model** (`cargo test -p ferrix-usb-host`, 19 tests in
`src/tests/serial.rs`): the xHCI model behind an FTDI chip model that NAKs
when it has nothing, sends status-only packets as a latency timer does, and
records what it was sent; for the Blaster, a CPLD model and an IEEE 1149.1
TAP chain behind it. What QEMU cannot show is here:

* the FT232H itself -- high speed, 512-byte packets, the H divisor in
  `wValue`/`wIndex` with the port byte 1, every request in order;
* 5000 bytes in through a reader with room for one chunk, none lost;
* a new line as the chip's three requests; a rate it cannot have refused
  and the old line kept; unplug and replug;
* the Blaster: the MAX 10 10M08's IDCODE 0x031820dd read from a
  one-tap chain; IR capture and IDCODE by `ir`/`dr` scans; BYPASS one bit
  long; a three-tap chain with a bypassed tap in the middle, nearest-TDO
  first; an empty chain as no taps; TDO held low as a fault. A mutation of
  the driver (no `READ` on the last bits) fails four of them.

**Not shown**: anything on the real chips. The FT232H on the host is
`/dev/ttyUSB0` there, and the USB-Blaster is on the customer's board (OpenOCD
on the host reads a single tap, IDCODE 0x031820dd, MAX 10 10M08 -- the
number the model is built to), but no host device was passed to a VM: that
waits for the customer's word. The `/tmp/jtag<N>` command lines have run
only against the model's cable through the library, not through `usbhid`'s
terminal, since QEMU has no Blaster to boot beside (its `usb-serial` takes
no vendor id).

## 4. What does not work, and why

Each of these is a limit of the pseudoterminal the kernel gives, not of the
driver, and each has a file and line:

1. **Received bytes can be lost.** A pty master's write takes everything
   (`src/kernel/src/fs/pty.rs:556-561`) and the slave's line discipline
   drops what does not fit its 4096 bytes (`src/kernel/src/fs/terminal.rs:71`,
   `:506`, `:515`). Linux throttles a `ttyUSB`: the flip buffer fills, the
   driver stops submitting IN URBs, the chip NAKs, and with RTS/CTS its RTS
   falls. Here the driver cannot see that the slave's reader is behind --
   `FIONREAD` on the master counts the other direction (`syscall/tty.rs`
   `pty_ioctl`) -- so the back-pressure the streams build (§1) stops at the
   pty. A program that reads its serial port slower than 4 KiB per pause
   loses bytes silently.
2. **No modem lines, no break, no drain.** `TIOCMGET`, `TIOCMSET`,
   `TIOCMBIS`/`BIC`, `TIOCMIWAIT`, `TIOCGICOUNT`, `TCSBRK` (and so
   `tcdrain` and `tcsendbreak`), `TIOCSBRK`/`TIOCCBRK` all end at
   `_ => Err(Errno::ENOTTY)` (`src/kernel/src/syscall/tty.rs:496`). The
   driver reads CTS/DSR/RI/DCD from every packet and keeps them, and can set
   DTR/RTS, but no program can ask for either. A program that toggles DTR
   to reset a board (avrdude, esptool) fails; one that calls `tcdrain`
   gets `ENOTTY`.
3. **Only the `B` rates.** The terminal keeps a speed only as a `B` code;
   `BOTHER` with a rate no code names keeps the old one
   (`src/kernel/src/fs/terminal.rs:201`). The FT232H's 6 and 12 Mbaud, and
   rates like 250000 (DMX, some 3D printers), cannot be asked for, though
   `ftdi::divisor` computes them.
4. **Settings are polled.** No pty event says the slave's settings changed
   (`set_termios`, `pty.rs:262`, wakes nobody, and there is no packet mode).
   The driver compares them before each write, so bytes written after a
   `tcsetattr` go at the new rate, but a change with nothing written after
   it reaches the chip up to 50 ms later, and the driver wakes 20 times a
   second while a terminal is open.
5. **The name.** Programs find `/tmp/ttyUSB0`, not `/dev/ttyUSB0`, and
   `ttyname` says `/dev/pts/0`. udev-style rules and `/dev/serial/by-id`
   have nothing to match.
6. **Writes wait.** A bulk OUT transfer is synchronous in `usbhid`'s loop
   (`Host::bulk`): a chip that NAKs -- CTS low under RTS/CTS -- holds the
   keyboards and mice on the same controller for up to the transfer's ten
   seconds. That is ferrix-usb-host's to fix (an OUT stream like the IN
   one), not the kernel's.

None of these stops the customer's use: a console on a board at a `B`
rate, or a JTAG chain read. Items 1, 2 and 3 stop a programmer that resets
a board through DTR, a fast UART link, and any heavy receive load.

## 5. The smallest kernel change that removes §4.1-5

The pty already is a serial port with a program at the far end. What it
lacks is everything a driver needs to tell the kernel and hear from it. So
the minimal change is **a driver-backed terminal: a pty whose master end
is a kernel object handed to a ring-3 driver, with the events a serial
driver needs**, rather than a new subsystem:

1. **Back-pressure** (`pty.rs`, `terminal.rs`): a master write that stops
   when the discipline is full, answering `EAGAIN` non-blocking, and the
   master's readiness reporting "writable" only with room. About thirty
   lines: `Discipline` already knows its room (`terminal.rs:515`).
2. **Events to the master** (`pty.rs`, `syscall/tty.rs`): a packet mode
   (Linux's `TIOCPKT`, extended) in which the master's reads carry, ahead
   of the data, a record for each settings change, each `TIOCMSET`/`BIS`/
   `BIC`, each break and each drain request; and on the slave, `TIOCMGET`,
   `TIOCMIWAIT`, `TCSBRK` and `TIOCSBRK`/`CBRK` answered from state the
   master sets (`TIOCMSET` on a master in this mode sets the lines the
   slave reads) instead of `ENOTTY`. `tcdrain` waits for the master to
   acknowledge.
3. **Any rate** (`terminal.rs:201`): keep `c_ispeed`/`c_ospeed` as numbers
   beside `c_cflag`, so `BOTHER` holds.
4. **A name** (`devfs.rs`): a `register_char` beside `register_block`
   (`devfs.rs:574`) that lets the native device ABI publish a pty slave
   under a name from a short table the kernel owns -- `ttyUSB<N>` at 188:N,
   `ttyACM<N>` at 166:N -- given to a USB host's node as `input_control`
   is today (`src/kernel/src/device.rs:1902`).

Items 1 to 3 change only the terminal code, which is generic; item 4 is
the only one that names a device class. None of them puts USB in the
kernel.

The general alternative is Linux's CUSE: let `ferrix-chardevctl`'s
forwarding carry read, write and poll, and let its node table name more
than NVIDIA's major. That serves every ring-3 character driver, not only
terminals, but then the line discipline would have to live in ring 3 too
(as it does for CUSE), and every serial program's `tcsetattr` would be a
round trip to the driver. For terminals, items 1-4 are smaller and
faster.

## 6. What the design gets wrong

The customer asked for a candid answer. These are the decisions that made
a few hundred lines of driver need a document.

### 6.1 One process is the whole USB bus

devmgr starts drivers only for what the kernel publishes: PCI functions
and device-tree nodes (`devmgr/src/main.rs:313`, `:1935`). A USB device is
not one, so the process that owns the controller owns every device behind
it, and every class driver is compiled into it. The consequences are not
theoretical:

* a fault in the FTDI code -- or a Blaster scan stuck in a 500 ms read --
  stops the keyboard and mouse on the same controller;
* restarting the driver to recover one device drops all of them;
* a driver for a new USB class is an edit to the host controller's
  driver, not a new program; there is no interface to write against;
* a program cannot claim a device (libusb-style), so OpenOCD, flashrom,
  dfu-util, sigrok and avrdude's USB programmers cannot run at all.

Linux binds `usb_driver`s to interfaces by id table; Fuchsia publishes
each USB interface as a device a separate driver binds to. Ferrix needs
the same in ring 3: the host controller process exporting each interface
as a channel (control, bulk, interrupt transfers and their completions as
messages, with buffers in a shared VMO), and devmgr starting class drivers
for them by id. That needs no kernel change except letting devmgr be told
of devices a ring-3 driver found -- devmgr's protocol, not the kernel's --
and it is the change that would have made this a separate program.

### 6.2 Every class of device needs its own kernel core

What programs see is decided in the kernel, class by class: the input core,
the block ring, the net ring, display, render, audio, logctl, and the
NVIDIA-only chardev forwarding. A ring-3 driver of a class the kernel has
no core for -- a UART, a JTAG cable, CAN, SPI, a GPIO expander, a sensor --
has no way to appear in `/dev`, because `/dev` is a fixed table
(`devfs.rs:3`) and the one generic forwarding is pinned to one vendor's
numbers (`chardevctl/src/node.rs:17`). That inverts the aim the customer
set ("keep the kernel slim: policy and device knowledge in ring 3"): moving
a driver out of the kernel still needs a kernel change for each new kind
of driver, and the kernel ends up holding exactly the per-class, even
per-vendor, knowledge that was meant to leave it. A generic forwarding
(read/write/poll/ioctl over a channel, like CUSE) with names from a policy
the kernel checks rather than hard-codes is the missing piece.

### 6.3 Terminals are only consoles and ptys

The terminal layer was built for the console and for terminal emulators,
so it has no notion of hardware behind it: no modem lines, no break, no
drain, no flow control, no throttle, no speed it does not know a name for
(§4). Every one of those is part of what a tty *is* for a serial port, and
POSIX programs use them. §5 is the fix.

### 6.4 No raw access for tools that bring their own protocol

OpenOCD, flashrom and friends drive FTDI chips themselves, through libusb
or libftdi. Ferrix offers them neither, so the only way to support a JTAG
or SPI adapter is a driver in `usbhid` that knows the adapter's protocol --
what `/tmp/jtag<N>` is. §6.1's interface channels, given to a program by a
broker that checks a policy, would let those tools run unmodified over a
libusb backend.

## 7. MPSSE and the FT232H's JTAG

The FT232H's MPSSE engine (JTAG, SPI, I2C) fits beside the Blaster: enter
it with `SET_BITMODE` mode 2 (`ftdi::bitmode`, already there), set the
clock divisor (opcode 0x86) and the pin directions (0x80/0x82), then send
MPSSE opcodes on the same bulk OUT pipe -- 0x4B for TMS, 0x39/0x3B for TDI
with TDO read, 0x19 for TDI only -- and read the answers from the same
stream, status bytes stripped as now. That is a second implementation of
the scan interface `blaster::Jtag` offers (reset, IR/DR scan, IDCODE
chain), so the right shape is a `JtagCable` trait with a Blaster and an
MPSSE implementation, a third `ftdi::Role`, and the same terminal command
lines. Out of scope here; nothing in the transport needs to change.

## 8. Running it

```
cargo test -p ferrix-usb-host                # the model, the chips and the Blaster
cargo xtask test-usb-serial --timeout 240    # QEMU's FT232BM, end to end (x86-64)
```

On a booted Ferrix with an adapter plugged in:

```
stty -F /tmp/ttyUSB0 115200 raw -echo
cat /tmp/ttyUSB0 &
echo hello > /tmp/ttyUSB0

echo idcode > /tmp/jtag0; head -n 1 /tmp/jtag0
```
