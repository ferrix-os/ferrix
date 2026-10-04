# Clipboard: the host's selection and the guest's, joined over vdagent

Version 1, drafted. Written by ferrix-d3 on 2026-09-19 at the user's asking
("shared copy and paste between QEMU Ferrix and the host"). **Not yet approved
by a product owner:** §7 is the list of decisions that are the owner's and not
this document's, and no owner session was running when this was written --
`ferrix-32`, whom `docs/BACKLOG.md` names, has ended. Until one answers, §7's
draft answers are what the code assumes and each is marked where it is used.

It has the shape of `docs/INPUT.md` on purpose, because it is the same kind of
thing: a small, rare byte stream between a ring-3 virtio driver and a program
that wants it, with a kernel core in between and a host-tested crate for every
protocol. Where a rule here differs from that document's, the item says why.

## 1. What this is, and what it is not

A person watching Ferrix in a QEMU window copies a line in their host editor
and pastes it into a Ferrix program, and copies in a Ferrix program and pastes
it on the host. This document specifies the whole path:

* **the transport**, virtio-serial: a `virtio-console` device with the
  multiport feature, whose port named `com.redhat.spice.0` is the wire every
  SPICE guest agent has used since 2010;
* **the protocol on it**, SPICE's vdagent, of which this implements the
  clipboard messages and nothing else;
* **the driver**, `src/user/system/native/drivers/console/vport`, which owns the device and offers the port as
  a Unix socket. There is no kernel in this path at all, and §5 is why;
* **the agent**, a program that is a `ext-data-control` client on one side and
  the port's reader on the other;
* **the test**, which needs no window and no person: QEMU's own host half of
  vdagent, driven from `xtask` over a socket.

It is **not** the SPICE display, the SPICE server, or a SPICE client. QEMU's
`qemu-vdagent` chardev (since QEMU 6.1) is the host half of the agent protocol
with no SPICE anywhere: it bridges the port straight to whatever clipboard the
active UI has — GTK's, SDL's, or a VNC client's through the RFB extended
clipboard. So Ferrix speaks vdagent and gets every viewer QEMU has, and this
project does not gain a SPICE dependency.

It is not the mouse, the monitors configuration, the file transfer or the
audio volume of the same protocol. It is not images on the clipboard: §7(b)
asks the owner whether `image/png` is in version 1 or not. It is not a
clipboard between two Ferrix machines, and it is not the guest's clipboard
persisting after the program that owns it exits, which is Wayland's rule and
stays Wayland's rule (`docs/ROADMAP.md`, stage 18: "Wayland's clipboard is a
promise and not a buffer").

**Exit:** on x86-64 and AArch64, with `cargo xtask test-clipboard`, a string
copied by the host half arrives in the guest's selection and is printed by
`clip paste`; a string given to `clip copy` is grabbed, requested by the host
half, and arrives there byte for byte. `cargo xtask run --display --clipboard`
gives a person the same thing with their own keyboard.

## 2. Why this transport and not another

Three were considered and two rejected, and the reasons are worth keeping
because the rejected ones look cheaper until the viewer is asked about.

1. **vdagent over virtio-serial** — this document. It costs a device driver
   Ferrix does not have. It is the only one of the three that reaches the
   clipboard of a viewer that is not on the QEMU host: the person watching
   Ferrix from another machine, over VNC or from a QEMU window on a host whose
   build has GTK, has their clipboard joined by QEMU itself.
2. **A bridge over the network**, guest TCP to a listener in `xtask`, which
   already terminates the guest's TCP on the host (`tools/common/xtask/src/gateway/`). It
   needs no driver and would have been days rather than weeks — but the
   clipboard it joins is the *QEMU host's*, which is the wrong machine for
   anyone whose viewer is elsewhere. It also works only on a boot `xtask`
   started, with `--net`, which is not a clipboard a system has.
3. **A shim in `xtask`**, shuttling text through a channel that exists. A
   development convenience, not a clipboard programs share.

The decision, on 2026-09-19, was the user's, told the viewer question: their
QEMU window is on another host. (1) is the only one that reaches it.

## 3. The transport

### 3.1 The device

`-device virtio-serial-pci` with one port:

```
-chardev qemu-vdagent,id=vdagent,name=vdagent,clipboard=on
-device virtio-serial-pci,disable-legacy=on,iommu_platform=on
-device virtserialport,chardev=vdagent,name=com.redhat.spice.0
```

`clipboard=on` is what makes the chardev a clipboard peer of QEMU's UI;
without it the chardev exists and carries nothing. `mouse=off` is left as it
defaults, since the agent announces no mouse capability and QEMU then sends no
mouse state (`ui/vdagent.c`, `have_mouse`).

**A window is not a peer unless QEMU says so.** The chardev hands the
clipboard to QEMU's UI, and QEMU's VNC server always takes it -- but its GTK
window only in a QEMU built with `gtk_clipboard`, which is off by default and
off in both QEMUs the tree has been run with (Ubuntu's 10.2.1 and a build
from source: `CONFIG_GTK_CLIPBOARD` undefined). In a window the port then
carried nothing, and `run-compositor --everything` had no clipboard. So on a
Wayland host a watched boot (`run`, `run-compositor`) gives the port a socket
chardev instead, and xtask is the host half for the whole run
(`tools/common/xtask/src/clipboard/host.rs`): what `wl-paste --watch` reports is grabbed
into the guest -- or on GNOME, whose compositor offers `wl-paste --watch` no protocol, what the X11 selection holds, read through GTK as an Xwayland client, which needs no focus -- and what the guest copies goes to `wl-copy`. The serials are
kept as `ui/vdagent.c` keeps them, and the guest's own copy coming back from
`wl-paste --watch` is not grabbed back into the guest. Without
`WAYLAND_DISPLAY`, or without wl-clipboard, the boot keeps `qemu-vdagent`, and
says so: a VNC viewer's clipboard still reaches it. Proved on 2026-09-27 in
a `run-compositor --everything` window on GNOME: a host `wl-copy` reached the
guest's `clip paste`, and the guest's `clip copy` the host's `wl-paste`.

virtio-console is device id 3, so the modern PCI device id is `0x1043` and the
transitional one `0x1003` — the pair `devmgr`'s table takes (`docs/DEVMGR.md`
§3). The features the driver takes are `VIRTIO_CONSOLE_F_MULTIPORT` (bit 1),
without which there is no port naming and no control queue and the device is
one nameless console; `VERSION_1`; and `ACCESS_PLATFORM`, without which a
device behind the machine's IOMMU refuses `FEATURES_OK`, as every other Ferrix
virtio driver takes it. `F_SIZE` and `F_EMERG_WRITE` are not asked for.

### 3.2 The queues, and why the numbering is the awkward part

Virtio 1.2 §5.3.2. Port 0 is queues 0 (receive) and 1 (transmit). With
multiport, queues 2 and 3 are the control receive and control transmit. Every
port *N* above 0 is queues `2N + 2` and `2N + 3`. So the one port this device
has — port 1, the only one `virtserialport` adds — is queues 4 and 5, and
queues 0 and 1 exist, must be set up, and are never used. A driver that
assumes its port's queues are 0 and 1 gets a device that accepts everything
and delivers nothing, which is the failure this paragraph exists to prevent.

### 3.3 The control conversation

The control message is `struct virtio_console_control` — `le32 id`, `le16
event`, `le16 value` — verified against QEMU 9.2.4's
`include/standard-headers/linux/virtio_console.h`. The opening exchange, in
the order it happens:

1. driver → `DEVICE_READY` (0) with `value` 1, once the queues are up;
2. device → `PORT_ADD` (1) for port 1;
3. driver → `PORT_READY` (3) with `value` 1;
4. device → `PORT_NAME` (7), whose payload after the header is the name
   `com.redhat.spice.0`, and `CONSOLE_PORT` (4) it does not send for this one;
5. device → `PORT_OPEN` (6) with `value` 1 when the host end is ready;
6. driver → `PORT_OPEN` (6) with `value` 1, and only now does data flow.

The name is how the port is identified, not the number: a port's number is
QEMU's to choose, and a guest that hardcodes 1 is a guest that breaks the day
somebody adds a second port before it. The driver matches the name and calls
the port it found "the agent port"; §7(c) asks whether a second port is worth
carrying at all in version 1.

`PORT_OPEN` with `value` 0 from the device means the host end went away — the
viewer's window closed, the chardev disconnected — and the agent must forget
its grabs and stop offering a selection it can no longer satisfy.

## 4. The protocol on the port

SPICE's vdagent, from `/usr/include/spice-1/spice/vd_agent.h` (BSD-licensed
header; the numbers are cited here, none of its code is copied) and checked
against what QEMU 9.2.4 actually sends and accepts in `ui/vdagent.c`.

### 4.1 Framing

Two layers, both little-endian:

```
VDIChunkHeader   port: u32, size: u32          the port field; the bytes after
VDAgentMessage   protocol: u32 = 1, type: u32, opaque: u64, size: u32, data
```

A message is cut into chunks of at most **1024** bytes of payload each, which
is what QEMU does when it sends (`vdagent_send_msg`) and what a guest should
do when it sends, because the host reassembles by `chunk.size` and a chunk
larger than its buffer is a dropped connection rather than a big message. The
chunk's `port` is `VDP_CLIENT_PORT` (1) on the way out of the guest;
QEMU ignores the field when reading, and this is written down so that nobody
"fixes" it to 2 and wonders why nothing changes.

`opaque` is 0 for every clipboard message.

### 4.2 The capability exchange

`VD_AGENT_ANNOUNCE_CAPABILITIES` (6): `u32 request`, then the capability
bitmap as `u32`s. QEMU with `clipboard=on` announces
`CLIPBOARD_BY_DEMAND` (5), `CLIPBOARD_SELECTION` (6) and
`CLIPBOARD_GRAB_SERIAL` (17), and nothing else unless `mouse=on`.

The agent announces exactly those three and no more, and it must, because each
changes the shape of a later message:

* **`CLIPBOARD_BY_DEMAND`** is the modern clipboard at all: a grab says only
  *what types are available*, and the data moves when the other side asks. The
  old behaviour — sending the data with the grab — is what its absence means,
  and nothing here implements it.
* **`CLIPBOARD_SELECTION`** puts a one-byte selection number, then three bytes
  of padding, at the front of `CLIPBOARD`, `CLIPBOARD_GRAB`,
  `CLIPBOARD_REQUEST` and `CLIPBOARD_RELEASE`. Without it every message is
  four bytes shorter and there is only the one selection. This is the single
  most common way to get a vdagent implementation wrong, so the codec has no
  way to spell a message without saying which of the two shapes it is.
* **`CLIPBOARD_GRAB_SERIAL`** adds a `u32` serial after the selection in a
  grab, which settles a grab race: whoever's serial is newer wins, and a grab
  arriving with an older serial than the one already held is dropped rather
  than fought over.

Either side may send `ANNOUNCE_CAPABILITIES` with `request` 1, which asks the
other to answer with its own. The agent sends its own unprompted when the port
opens, with `request` 1, and answers any it receives with `request` 0.

### 4.3 The clipboard messages

Selections: `CLIPBOARD` (0) and `PRIMARY` (1). `SECONDARY` (2) exists in the
protocol, QEMU maps it to nothing, and the agent refuses it.

Types: `UTF8_TEXT` (1) is version 1's whole vocabulary, mapped to the MIME
type `text/plain;charset=utf-8` that the compositor's clipboard already
carries (`src/user/system/linux/compositor/clip`). `IMAGE_PNG` (2) is §7(b).

| Message | Number | Meaning |
|---|---|---|
| `CLIPBOARD_GRAB` | 7 | I now own this selection; here are the types |
| `CLIPBOARD_REQUEST` | 8 | send me this selection as this type |
| `CLIPBOARD` | 4 | here is the data, in answer to a request |
| `CLIPBOARD_RELEASE` | 9 | I no longer own it |

Four rules that are the protocol's and not obvious:

1. **A request is answered, always.** A `CLIPBOARD` message with type
   `NONE` (0) and no data is how a side says "I could not". A request left
   unanswered hangs the other side's paste until its own timeout, which on the
   host is a viewer that appears frozen.
2. **Line endings.** The guest announces neither `GUEST_LINEEND_LF` nor
   `GUEST_LINEEND_CRLF`, so no conversion happens in either direction and
   bytes are carried as they are. A Ferrix program that copies `\r\n` pastes
   `\r\n` on the host.
3. **A regrab is not a release.** Without
   `CLIPBOARD_NO_RELEASE_ON_REGRAB` (16), which neither side announces here,
   a new grab replaces the old with no release in between.
4. **Size.** Neither side announces `MAX_CLIPBOARD` (10), so no maximum is
   negotiated. The agent has one of its own anyway — §7(a) — because a
   selection is read into memory whole and a guest that will allocate whatever
   the host names is a guest the host can exhaust.

## 5. No kernel at all, and why

Version 1 of this document specified a kernel port core: a control protocol
in `libs/portctl`, a core owning the port, and `/dev/vport0p1` as a character
device in devfs. **That is no longer the plan, and none of it will be built.**

The reason is a property of this kernel that the first draft did not know.
Every process gets all three of a native handle table, a POSIX file
descriptor table and a VFS namespace -- `Process::with_pid` in
`src/kernel/src/syscall/process.rs` builds them for every process there is, not
for Linux ones only. And the system call dispatcher takes the native ABI
**by number range, before any Linux table is asked**
(`src/kernel/src/syscall/mod.rs`, `dispatch`). So the two ABIs are not two kinds
of process. They are two ranges of number, and one program may use both.

`ferrix-rt` has in fact been doing exactly this since it was written: a
native program's `exit` is Linux's `exit_group`, called with a Linux number
through the same instruction as every native call
(`src/user/system/native/rt/src/arch/x86_64.rs`). The trick this design turns on is already in
the tree, on every architecture, in the runtime every driver links.

So a driver may hold a device through native handles *and* create a Unix
socket through ordinary Linux calls. The port needs no kernel representation
at all, and the two landings that were kernel work are gone.

What is given up, honestly: the port is reachable only through that socket
and not as `/dev/vport0p1`, so a program written against Linux does not find
it where it would on Linux, and a second virtio-serial port would need this
design extended rather than a node appearing. If a reason appears to want the
character device -- another port, or a program that expects the Linux name --
§5 of version 1 is in the history and still correct.

## 6. The two programs

**`src/user/system/native/drivers/console/vport`** is the driver, started by `devmgr` for PCI id `0x1043` like
any other. It is a native program: it takes the device in START, maps the
register blocks, negotiates the features of §3.1, sets up the four queues of
§3.2 and walks the control conversation of §3.3 until the port named
`com.redhat.spice.0` is open. Then it binds a Unix socket at the abstract
name **`\0ferrix.vport`** and answers only a peer whose `SO_PEERCRED` uid is
0, and everything
that arrives on the port is written to whoever is connected and everything
written there goes out on the port. It understands nothing of vdagent: it is
a pipe with a device on one end.

The name is abstract because of when the driver starts. Under
`ferrix.devmgr=init` with a root volume (`docs/INIT.md` §7.3), `devmgr` and
its drivers start from the initramfs before `/` moves onto the volume, and
only pid 1 is moved. Until 2026-09-27 the socket was the path `/tmp/vport`,
bound in the initramfs's `/tmp`, and an agent started from the volume looked
in the volume's: `no clipboard port at /tmp/vport` on every desktop that
switched its root, which `run-compositor` does unless `--tmpfs-root` says
otherwise. An abstract name is in no directory, so the switch cannot hide
it. It has no file mode either, so the uid check keeps what a root-owned
socket's mode kept. Like Linux's, it is per network namespace, so a unit
given a network of its own would not see it; the agent is not run that way.

Its loop answers the device's events and stops at the port's bytes, which
are read when there is somewhere to write them. Until 2026-09-23 it asked the
device for events until there were none, and bytes waiting on the port are an
event until they are read -- so the first bytes the host sent held the loop
there for good: a processor pinned from then on, and nothing reaching the
client. A desktop booted with `--clipboard` had one of its processors busy
from its first seconds (`docs/COMPOSITOR-DAMAGE-HANDOFF.md` §2.8).

`devmgr` needs one change beyond its table, and it is not optional. A driver
that does not publish to a kernel subsystem is currently **killed** and
counted as failed (`src/user/system/native/devmgr/src/main.rs`, after `await_published`), and
`test-boot` requires `failed 0`. `vport` publishes to no subsystem because it
has none, so it needs a kind of its own that is started and not waited for.

**`src/user/system/linux/compositor/vdagent`** is the agent, an ordinary `std` program beside the
compositor's other clients. It connects to `vport`'s socket on one side and
to the Wayland socket on the other, and it is where `src/lib/drivers/console/vdagent` and
`ext-data-control` meet: a host grab becomes a `create_data_source`,
`offer`, `set_selection`; a guest `selection` event becomes a grab, and the
host's request for the data is answered from a pipe. It reuses
`src/user/system/linux/compositor/wire` and the client half of `src/user/system/linux/compositor/clip`, which is why the
agent is `std` and the driver is not.

## 6a. The terminal

Copy and paste has to be reachable from a keyboard or it is not a feature a
person has. the term app had **no clipboard code of any kind** when this was written --
no `wl_data_device`, no paste -- so `CTRL`+`SHIFT`+`V` in a Ferrix terminal
would do nothing even with every part above built and working. That was
found by reading the term app's `src/client.rs` after a person tried exactly
that key and nothing happened.

So the terminal binds `wl_data_device`: `CTRL`+`SHIFT`+`V` asks for the
selection as `text/plain;charset=utf-8`, reads the pipe and writes what comes
back to the pseudoterminal, as a paste is; and a mouse selection over the
grid offers it. `/bin/clip copy` and `/bin/clip paste` already ship in the
compositor's image and are what the existing clipboard gate drives, so the
transport can be proven before the terminal is touched -- but it is not
finished until the key works.

## 7. What the product owner decides

* **(a) The maximum selection.** The draft says 1 MiB, refusing anything
  larger in either direction with `CLIPBOARD`/`NONE` and a line on the
  console. Large enough for any text a person copies, small enough that a
  hostile host cannot exhaust the guest.
* **(b) Images.** `image/png` both ways is perhaps 150 lines more and no new
  concepts — the type number exists, the compositor carries any MIME type
  already. In or out of version 1?
* **(c) More than one port.** ~~The core is written for many ports and the
  driver for one.~~ **Settled by §5:** there is no core and no devfs naming
  to paint into a corner, so the driver carries the one port it needs and a
  second would be a change to one program. Nothing is owed here now.
* **(d) The agent's name and its start.** **Settled by §5 and §6:**
  `src/user/system/linux/compositor/vdagent`, a `std` program beside the compositor's other
  clients, started as an `exec-once` the way the terminal and the wallpaper
  are. It exits quietly when the socket is not there, so a boot without
  `--clipboard` is a boot without a clipboard and not a boot with an error.
* **(e) Where it lands.** This is a stage 19 feature by subject and a stage 10
  feature by machinery. The draft assumes stage 19; with §5's core gone there
  is no stage 10 debt in it any more, only user-space programs.

The two that are still open are (a) and (b), and neither blocks the build:
the maximum is a constant and images are a type number and a MIME string.

## 8. The order of the landings

Each row is one landing and each is gated on its own. The first two need
nothing from the kernel and are pure host-tested logic.

| # | What | Where | State |
|---|---|---|---|
| 1 | this document | `docs/CLIPBOARD.md` | landed |
| 2 | the vdagent protocol, encode and decode | `src/lib/drivers/console/vdagent` | landed |
| 3 | the virtio-console device protocol | `src/lib/drivers/virtio/src/console.rs` | landed |
| 4 | the console driver library | `src/lib/drivers/console/virtio-console` | landed |
| 5 | the driver and its socket, and `devmgr`'s kind | `src/user/system/native/drivers/console/vport`, `src/user/system/native/devmgr` | landed |
| 6 | the agent | `src/user/system/linux/compositor/vdagent` | landed |
| 7 | paste and copy in the terminal | the term app | landed |
| 8a | `--clipboard`: the device on the bus | `xtask` | landed |
| 8b | starting the agent, and `test-clipboard` | `xtask` | landed |

**Where it stands, 2026-09-27 (ferrix-e4).** The agent landed, and with it
the clipboard works both ways through QEMU 10.2.1's `qemu-vdagent` and its
VNC server: a VNC viewer's copy reaches `clip paste` in the guest, and a
guest `clip copy` reaches the viewer. Talking to QEMU for the first time
found three things the design had not met:

* **The header is twenty bytes.** `VDAgentMessage`'s `opaque` is 64 bits;
  `src/lib/drivers/console/vdagent` wrote it as 32, so every message either way was
  misframed ("vdagent_chr_recv_chunk: Oops: 0+24 > 21" on the host). The
  crate now has a test holding it to `vd_agent.h` byte for byte.
* **QEMU closes the port on purpose** once the first capabilities have
  crossed, to reset its serial state, and opens it again only after the
  guest's end has closed. `src/lib/drivers/console/virtio-console` now answers each of
  the host's `PORT_OPEN` messages with its own of the same value, and the
  agent takes the port up again when `vport` lets its connection go.
* **QEMU 10 resets on every capabilities message after the first**, by
  closing the port again, so the agent says its capabilities once a
  connection: in answer to the host's, or unprompted after two seconds.

And a viewer sees the guest's clipboard only if it speaks the extended
clipboard (pseudo-encoding `0xc0a1e5ce`): QEMU's VNC server never sends a
plain `ServerCutText`. TigerVNC and the other common viewers speak it; a
bare RFB client, like the one that first reported the clipboard broken,
does not. `clip --primary paste` with nothing selected waits its 20 seconds
and says so -- the host offers no primary selection -- which is the wait
`test-compositor`'s side-by-side `clip copy` and `clip paste` rely on.

`cargo xtask test-clipboard` is the gate (§9), on x86-64 and AArch64: xtask
listens on the port's far end as a Unix socket and speaks vdagent itself.
Its host text is three chunks long, which found one more thing -- the agent
kept only the bytes after the last chunk it had fed, not after the last
whole message, so a paste longer than one read lost its start and stopped
the agent; without the fix the gate fails with the agent's "the host's
framing: TooLong".

The terminal's control-shift-V and control-shift-C (7) had landed without
this table saying so; driven over VNC on 2026-09-27, a viewer's copy pasted
into the terminal ran as a command, and a line selected with the pointer and
copied reached the viewer. Doing that after the host had copied once found
two last things in the agent:

* **Grab serials count on.** QEMU discards a guest grab whose serial is
  below the last it saw (`vdagent_clipboard_recv_grab`); the agent sent 0
  every time, so once the host had copied, nothing the guest copied reached
  it again. It counts on from the last grab either side made now, and
  `test-clipboard`'s host refuses a grab older than its own, as QEMU does.
* **A source let go of still gets its events.** The compositor may send
  `cancelled` or `send` to a data source before it reads the agent's
  destroy; the agent stopped on "an event for object 16" when the
  terminal's copy replaced the host's selection. It reads and ignores them.

Nothing is left of the landings above.

Landings 1 to 5 and 8a are on `main`: the guest now has a driver that opens
the port and offers it on a Unix socket, and what is left is the two programs
that speak vdagent over it and the gate that drives them. What is left needs no
kernel, which is
§5's whole point, and 8a is worth its place in the order after all: `test-boot --arch x86_64
--clipboard` reaches `FERRIX-BOOT-OK` with 9 PCI functions and 4 virtio
transports where a plain boot has 8 and 3, and `devmgr` starts the same two
drivers and fails none. So the device is enumerated, a node is published for
it, and nothing claims it -- which is exactly the state landing 6 begins
from, proven rather than assumed.

## 9. The test

`cargo xtask test-clipboard`, headless, on x86-64 and AArch64. The host half is
QEMU's own: `-chardev qemu-vdagent` bridges to the UI's clipboard, and with no
UI there is nothing to bridge to — so the test does not use the UI at all. It
attaches a second chardev, a Unix socket `xtask` listens on, and speaks vdagent
on it as a viewer would, which makes the whole guest path — device, driver,
core, node, agent, compositor — the thing under test and the host clipboard not
part of the test at all.

Two directions, one boot:

1. `xtask` sends `CLIPBOARD_GRAB` for `CLIPBOARD`/`UTF8_TEXT`; the guest's
   `clip paste` must print the string, which requires the agent to have
   requested it and the compositor to have served it.
2. The guest runs `clip copy <string>`; `xtask` must receive a grab, and the
   `CLIPBOARD` answer to its request must hold the string.

A person's own check, which no gate can make, is
`cargo xtask run --display --clipboard` and their own keyboard.

**And that check can be made on this machine**, which §2 and the paragraph
above would suggest it cannot. The QEMU on `PATH` here is a headless build
whose `-display help` offers only `none`, `spice-app` and `dbus`, and that is
what made VNC the fallback (`tools/common/xtask/src/window.rs`). But there is a second
QEMU on this host, built from source at
`~/Documents/qemu/qemu/build`, and it has `gtk`, `egl-headless` and `curses`
as well as the `qemu-vdagent` chardev. `FERRIX_QEMU` names it:

```text
FERRIX_QEMU=~/Documents/qemu/qemu/build cargo xtask run --display --clipboard
```

That opens a real window whose clipboard is this host's, which is the whole
path end to end with a person at the keyboard. It does not change the gate --
a gate may not depend on which QEMU a host happens to have built, which is why
§9's test speaks vdagent over a socket instead -- but it does mean the manual
check needs no second machine and no VNC viewer.
