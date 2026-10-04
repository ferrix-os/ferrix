# Input: `/dev/input/eventN` over a ring-3 virtio-input driver

Version 1. Written by the GUI session (os-e5) for the input iteration of the
compositor's path (`docs/DISPLAY.md` §4: after iteration 1's colour and
iteration 2's two tiled pattern clients, input). Approved as drafted by the
product owner (os-f6) on 2026-09-16, whose answers are §6. It has the shape
of `docs/DISPLAY.md` on purpose: an input core in the kernel, a ring-3 driver
started by devmgr, the Linux ABI (evdev) with its subset taken from a UAPI
probe, and QEMU's monitor as the test's stimulus.

## 1. What this is, and what it is not

Stage 17 gives a Linux compositor what it needs to read a keyboard and a
pointer: `/dev/input/event*` answering evdev. `docs/ARCHITECTURE.md` §7 puts
the driver in ring 3. This document specifies:

* **what the compositor's event loop calls**, verified from the sources of the
  crates it will be built on (§2), because the input iteration is the first
  one whose consumer waits on several descriptors at once, and iteration 2's
  protocol server is the first that runs Smithay's loop;
* **the kernel's input core**, which owns the event nodes and their queues;
* **the control protocol** between the core and the ring-3 virtio-input
  driver, in a host-tested crate `src/lib/proto/inputctl`;
* **the evdev subset** the nodes answer, its numbers and layouts from a probe;
* **the test**: QMP `input-send-event` in, the events read back out.

It is not the virtio-input protocol, which the driver speaks to the device
(`src/lib/drivers/virtio::input`, on `main` since 493fd843). It is not the
keymap: evdev delivers key codes, and turning them into keysyms is
xkbcommon's job in the compositor. It is not udev, libinput or a seat
manager, none of which Ferrix will have (§2.3). It is not hotplug, force
feedback, multi-touch or writing to the device (LEDs); §6 says when each
comes.

**Exit of the input iteration:** on x86-64 and AArch64, with a virtio
keyboard and a virtio tablet, a user program finds both under `/dev/input`,
identifies them through evdev's ioctls, and prints every `input_event` it
reads. `cargo xtask test-input` sends a key press and release, a pointer
position and a button click through QMP, and requires exactly those events,
in order, each report ended by `SYN_REPORT`. `cargo xtask run --display`
gives a person the same devices. This is stage 17's input exit as the roadmap
words it ("a key and a pointer motion sent through QEMU's monitor arrive as
`input_event`s on `/dev/input/event0` and are echoed on the console").

## 2. Prerequisites: what the event loop calls

An earlier answer to the product owner said iteration 1 needs no `epoll`, and
iteration 2 (Smithay and calloop) needs `epoll` and `eventfd` but neither
`timerfd` nor `signalfd`. That answer was not checked against source. This
section is the check: the crates were fetched from crates.io at the versions
Smithay's current release (0.7.0) resolves to, and read.

| Crate | Version | Why |
|---|---|---|
| `smithay` | 0.7.0 | the compositor base (`docs/BACKLOG.md`, assumed) |
| `calloop` | 0.14.4 | Smithay's event loop (`calloop ^0.14.0`) |
| `polling` | 3.11.0 | calloop's poller (`polling ^3.0.0`) |
| `rustix` | 1.1.4 | the system-call layer of all of the above |
| `wayland-server`, `wayland-backend` | 0.31.14, 0.3.17 | the protocol server, pure-Rust backend (no `server_system` feature, so no libwayland) |
| `drm`, `drm-ffi` | 0.14.1, 0.9.1 | Smithay's DRM backend (`drm ^0.14.0`) |
| `xkbcommon` | 0.8.0 | keymaps (`xkbcommon ^0.8.0`, a non-optional Smithay dependency) |
| `evdev` | 0.13.2 | the evdev reader an input backend without libinput would use |

rustix picks its raw Linux backend on Linux unless `use-libc` or
`--cfg=rustix_no_linux_raw` says otherwise (`rustix/build.rs:120`–`129`), so
the calls below are made directly, not through ferrousli. Rust's `std` goes
through ferrousli.

### 2.1 The event loop

* **`polling` on Linux is epoll** (`polling/src/lib.rs:83`–`89`). `Poller::new`
  calls `epoll_create1(EPOLL_CLOEXEC)` (`epoll.rs:45`), makes a notifier with
  `eventfd2(EFD_CLOEXEC | EFD_NONBLOCK)` and, only if that fails, `pipe2`
  (`epoll.rs:444`, `462`), and **makes a timerfd**:
  `timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK)`
  (`epoll.rs:50`–`54`). The timerfd is optional, since its result goes through
  `.ok()`, but when it exists every wait calls `timerfd_settime` and
  `epoll_ctl(MOD)` before waiting (`epoll.rs:181`–`203`). Without it, the
  timeout is passed to the wait itself (`epoll.rs:210`–`214`). Registrations
  use `EPOLLONESHOT`, `EPOLLET` or neither, with `EPOLLIN | EPOLLHUP |
  EPOLLERR | EPOLLPRI` for reading and `EPOLLOUT | EPOLLHUP | EPOLLERR` for
  writing (`epoll.rs:293`–`320`).
* **The wait is `epoll_pwait`, not `epoll_wait`.** rustix calls
  `epoll_pwait` with a null mask whenever the timeout fits in an `int` of
  milliseconds, and `epoll_pwait2` otherwise or under its `linux_5_11`
  feature (`rustix/src/backend/linux_raw/event/syscalls.rs:286`–`334`).
* **calloop** adds one more eventfd per `Ping` source, and `LoopSignal` is one
  (`calloop/src/sources/ping/eventfd.rs:41`). That one has no pipe fallback.
  Timers are a wheel in user space whose next deadline becomes the poller's
  timeout (`sources/timer.rs`, `sys.rs:223`–`237`), not timerfds. Signals use
  `signalfd` only under calloop's `signals` feature (`Cargo.toml`
  `signals = ["nix"]`, `sources/signals.rs:19`), which Smithay does not turn
  on. `set_nonblocking` in its async I/O adapter is `fcntl(F_GETFL/F_SETFL)`
  (`io.rs:396`–`403`).
* **wayland-backend's server has an epoll of its own.** `Backend::new`
  calls `epoll_create1` (`rs/server_impl/common_poll.rs:39`), each client is
  added with `EPOLLIN` (`rs/server_impl/handle.rs:366`), and dispatching
  waits with a zero timeout (`common_poll.rs:89`). The compositor registers
  *that* epoll descriptor in calloop's epoll, so **an epoll descriptor must
  itself be pollable and report readiness to another epoll.**
* **The socket.** `ListeningSocket::bind` takes `$XDG_RUNTIME_DIR`, opens a
  lock file and `flock(LOCK_EX | LOCK_NB)`s it (`wayland-server/src/socket.rs:92`),
  binds a `UnixListener` (`:133`) and calls `set_nonblocking(true)` on it
  (`:135`). In `std` on Linux that is **`ioctl(FIONBIO)`**, for sockets and
  for any descriptor alike (`std/src/sys/net/connection/socket/unix.rs:583`–`585`,
  `std/src/sys/fd/unix.rs:608`–`611`), not `fcntl`. `accept` is
  `accept4(SOCK_CLOEXEC)` (`unix.rs:259`) and sockets are made with
  `SOCK_CLOEXEC` (`unix.rs:84`). Messages go out with `sendmsg` carrying
  `SCM_RIGHTS` under `MSG_DONTWAIT | MSG_NOSIGNAL` (`rs/socket.rs:45`–`62`) and
  come in with `recvmsg` under `MSG_DONTWAIT | MSG_CMSG_CLOEXEC` (`:77`–`87`).
  Client credentials are `getsockopt(SO_PEERCRED)` (`server_impl/client.rs:314`).

### 2.2 Smithay's own calls

* **Clocks:** `clock_gettime` of `CLOCK_MONOTONIC` and `CLOCK_REALTIME`
  (`smithay/src/utils/clock.rs:12`, `22`, `46`).
* **Keymaps** are a sealed memfd: `memfd_create(MFD_CLOEXEC |
  MFD_ALLOW_SEALING)`, then `F_ADD_SEALS` of seal, shrink, grow and write
  (`utils/sealed_file.rs:34`–`45`). If that fails the keymap is written to a
  temporary file in `$XDG_RUNTIME_DIR` instead (`input/keyboard/keymap_file.rs:73`–`82`),
  so the memfd is wanted, not required.
* **`wl_shm` pools** are `mmap(MAP_SHARED)` of the client's descriptor, and a
  client that shrinks its file under the compositor is survived by a
  `SIGBUS` handler (`sigaction(SIGBUS, SA_SIGINFO | SA_NODEFER)`) that maps
  anonymous memory `MAP_FIXED` over the faulting pool, or re-raises
  (`wayland/shm/pool.rs:261`–`342`).
* **`/proc/self/fd/N`** is read with `readlink` for log messages
  (`utils/fd.rs:70`); a failure is tolerated.
* **Sessions.** Smithay 0.7.0 has one session implementation, libseat
  (`backend/session/mod.rs:22`–`27`). `Session` is a trait whose `open` returns
  an `OwnedFd`; without libseat, the compositor implements it with `openat`
  and does nothing on `change_vt`. No calls beyond `openat` and `close`.
* **Input.** Smithay's only input backend is libinput
  (`backend_libinput = ["input"]`); `backend::input` is traits. Without
  libinput the compositor implements `InputBackend` over evdev descriptors
  itself (§2.4).
* **xkbcommon** links `libxkbcommon` (`xkbcommon/src/xkb/ffi.rs:114`), whose
  keymap compiler `stat`s and `eaccess`es its include paths and `mmap`s or
  reads the files it opens (libxkbcommon `src/context.c:46`–`56`,
  `src/utils.c:30`–`33`, `104`–`120`). The XKB data must be in the image.

### 2.3 Smithay's DRM backend, legacy path

`DrmDeviceFd::new` asks for master (`backend/drm/device/fd.rs:77`), and
`DrmDevice::new` reads `st_rdev` through `fstat` (`:97`), asks for the
universal-planes client capability, the monotonic-timestamp and cursor-size
capabilities (`device/mod.rs:198`–`209`), and falls back to legacy when the
atomic capability is refused (`:250`–`256`). `card0` answers all of that
today. **Then it goes further than `card0` does:**

* **`create_surface` enumerates planes even on the legacy path**
  (`device/mod.rs:331`, `mod.rs:152`–`216`): `MODE_GETPLANERESOURCES`,
  `MODE_GETPLANE` for each, then `MODE_OBJ_GETPROPERTIES` and
  `MODE_GETPROPERTY` of each plane's properties to find `type`, `zpos`,
  `IN_FORMATS` and `SIZE_HINTS`. A plane with no `type` property reaches
  `unreachable!()` (`mod.rs:244`): the compositor panics. With universal
  planes refused, Smithay keeps only the primary planes from that list
  (`mod.rs:210`–`214`), and with none, `create_surface` fails with `NoPlane`.
* **Connectors' properties are read** to find `DPMS`
  (`device/legacy.rs:159`–`220`), on the device's reset and on every
  connector change: `MODE_OBJ_GETPROPERTIES` must answer for the connector,
  though an empty list is fine and then nothing is set.
* **Resetting the device** calls `MODE_CURSOR` with no buffer, ignoring the
  answer, and `SETCRTC` with no mode (`device/legacy.rs:101`–`119`), which
  `card0` already treats as off.
* **Vblank events** are `read` from the card (`drm/src/control/mod.rs:995`–`1003`).

So `card0` needs, before Smithay can drive it: one primary plane with a `type`
property, `GETPLANERESOURCES`, `GETPLANE` (format `XRGB8888`,
`possible_crtcs` 1), `OBJ_GETPROPERTIES` for planes and the connector, and
`GETPROPERTY`. `GETPROPBLOB` and `OBJ_SETPROPERTY` are not needed if the plane
has no `IN_FORMATS` or `SIZE_HINTS` and the connector has no `DPMS`.

Smithay has no renderer that writes into a dumb buffer on the CPU without a C
library (its CPU renderer is pixman), so the tiny-skia renderer is the
compositor's own `Renderer`; that is iteration 2's business and not a call
(a stage 18 item of its own, 5 points).

### 2.4 An evdev consumer

The `evdev` crate's `Device::open` opens read-write, falling back to
read-only (`evdev/src/raw_stream.rs:99`–`111`), then **requires** answers to
`EVIOCGBIT(0)`, `EVIOCGID`, `EVIOCGVERSION` and `EVIOCGPROP`, and to
`EVIOCGBIT(type)` for every type the device reports, plus `EVIOCGREP` when it
reports `EV_REP` (QEMU's keyboard does) and `EVIOCGEFFECTS` when it reports
`EV_FF` (`:114`–`230`). `EVIOCGNAME`, `EVIOCGPHYS` and `EVIOCGUNIQ` may fail;
their answer must be the length copied *including* the NUL, which the crate
asserts (`:17`–`35`). After `SYN_DROPPED` it re-reads the state with
`EVIOCGKEY`, `EVIOCGABS(axis)` for each axis, `EVIOCGSW` and `EVIOCGLED`
(`:506`–`549`, `sync_stream.rs:308`–`322`). Events are `read` as whole
`struct input_event`s (`raw_stream.rs:429`–`445`). `EVIOCGRAB` and
`EVIOCSCLOCKID` are called only when asked for (`:648`, `sys.rs:76`); a
compositor asks for both, as libinput does, to own the device and to get
monotonic timestamps.

### 2.5 The table

"Ferrix today" is `main` at `aa47c0c6`, read from `src/kernel/src/syscall` and
`src/lib/proto/linux-abi`; `card0` is branch `display-core`.

| Call | Needed by | Ferrix today | Iteration |
|---|---|---|---|
| `epoll_create1`, `epoll_ctl` (`ONESHOT`, `ET`) | polling, wayland-backend | **no**: numbers and `epoll_event` in `linux-abi` (`nr.rs`, `types.rs`), no handler | 2 |
| `epoll_pwait` (and `epoll_wait` on x86-64 and ARMv7-A) | rustix, musl | **no** | 2 |
| an epoll descriptor pollable by another epoll | wayland-backend inside calloop | **no** | 2 |
| `epoll_pwait2` | rustix, timeouts over `INT_MAX` ms or `linux_5_11` | **no**, not in `nr.rs` | not needed |
| `eventfd2` | polling (else `pipe2`), calloop `Ping` (no fallback) | **no**: number only | 2 |
| `timerfd_create`, `timerfd_settime` | polling, optional | **no**, not in `nr.rs` | wanted for 2 (sub-millisecond timeouts), not required |
| `signalfd4` | calloop `signals` feature only | no | not needed |
| `pipe2` | polling's fallback | yes (`syscall/fsctl.rs:99`) | — |
| `ioctl(FIONBIO)` | `std`'s `set_nonblocking`, wayland-server's listener | **no**: `sys_ioctl` answers `ENOTTY` for everything but the console, sockets' `SIOCINQ`/`SIOCOUTQ` and interface requests (`syscall/fd.rs:442`) | 2 |
| `fcntl(F_GETFL/F_SETFL)` `O_NONBLOCK` | calloop, evdev | yes (`syscall/fd.rs`) | — |
| `socket`/`accept4` with `SOCK_CLOEXEC`, `bind`, `listen` on `AF_UNIX` | wayland-server | yes (`syscall/sockets.rs:115`, `fs/socket.rs`) | — |
| `sendmsg`/`recvmsg` with `SCM_RIGHTS`, `MSG_DONTWAIT`, `MSG_NOSIGNAL`, `MSG_CMSG_CLOEXEC` | wayland-backend | yes, `SCM_RIGHTS` since `aa47c0c6` (`syscall/sockets.rs:1059`) | — |
| `getsockopt(SO_PEERCRED)` | wayland-backend | yes (`fs/socket.rs:74`) | — |
| `flock` | wayland-server's lock file | yes (`syscall/mod.rs:438`) | — |
| `memfd_create`, `F_ADD_SEALS` | Smithay keymaps (tempfile fallback) | yes (`syscall/mod.rs:439`, `fs/memfd_check.rs`) | — |
| `mmap(MAP_SHARED)` of a file, `SIGBUS` past its end, `SA_SIGINFO` handler remapping `MAP_FIXED` | Smithay `wl_shm` | partial: the mapping and `SIGBUS` are there (`syscall/memory.rs`); a handler that repairs the fault and returns is unproven | 2 |
| `clock_gettime` `MONOTONIC`, `REALTIME` | Smithay, `std` | yes (`syscall/mod.rs:376`) | — |
| `poll`, `ppoll` | the input test program (§4) | yes (`syscall/mod.rs:386`, `syscall/signal.rs:1004`) | — |
| `readlink /proc/self/fd/N` | Smithay logging, tolerated | yes (`fs/procfs.rs`, `fd`) | — |
| `DRM_IOCTL_MODE_GETPLANERESOURCES`, `GETPLANE` | Smithay `create_surface` | **no** (`card0` answers `ENOTTY`) | 2 |
| `DRM_IOCTL_MODE_OBJ_GETPROPERTIES`, `GETPROPERTY` | Smithay plane `type`, connector `DPMS` | **no** | 2 |
| `DRM_IOCTL_MODE_GETPROPBLOB`, `OBJ_SETPROPERTY`, `CURSOR` | only if the properties exist; `CURSOR`'s answer is ignored | no | not needed |
| `EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPHYS`, `EVIOCGUNIQ`, `EVIOCGPROP`, `EVIOCGBIT`, `EVIOCGREP` | evdev `open` | **no** | 3 (this document) |
| `EVIOCGKEY`, `EVIOCGABS`, `EVIOCGSW`, `EVIOCGLED` | evdev after `SYN_DROPPED` | **no** | 3 |
| `EVIOCGRAB`, `EVIOCSCLOCKID` | a compositor's input backend | **no** | 3 |

**What the earlier claim got right and wrong.**

* *Iteration 1 needs no epoll:* right. `src/user/system/linux/compositor/blank` calls the card's
  ioctls directly and waits on nothing.
* *Iteration 2 needs epoll and eventfd:* right, and incomplete. It also needs
  an epoll descriptor that is itself pollable, `ioctl(FIONBIO)`, and four DRM
  ioctls `card0` lacks, without which Smithay's DRM backend fails or panics.
* *Not timerfd:* right that it is not required, wrong that it is not called.
  `polling` creates one in every `Poller::new` and arms it on every wait
  when it exists; `ENOSYS` makes it fall back to a millisecond timeout.
* *Not signalfd:* right, for Smithay with calloop's default features.

**The input iteration itself needs none of the missing event-loop calls.** Its
test program waits with `poll(2)`, which exists, and the event nodes answer
`Inode::poll` like any other file, so they will work under epoll when it
lands without further change.

## 3. The design

### 3.1 Who owns the events: the core, per device, with a queue per open

**The input core** (`src/kernel/src/interfaces/input`) has one task per input device, as the
display core has one per card. It holds what the driver said the device is,
the device's current state (keys down, axis values, LEDs, switches), and the
opens of its node, each with a queue of `input_event`s.

* **A report is the unit.** Events arrive in reports ended by `EV_SYN` /
  `SYN_REPORT`. The core keeps a report's events aside until its
  `SYN_REPORT` arrives, stamps the whole report with one time, updates the
  device state, and then appends it to every open's queue, or only to the
  grabbing open's (§3.3). A reader never sees half a report, which is what
  Linux's evdev does with its packet head.
* **A full queue drops, and says so.** When a report does not fit, the open's
  queue is emptied and a `SYN_DROPPED` event is queued, which tells the reader
  to re-read the state with `EVIOCGKEY` and the others. Each queue's size
  follows Linux's `evdev_compute_buffer_size`, with 256 packets where Linux
  has 8: a compositor drawing in software on the DK1 read its devices once
  every 100 ms and more, and a 1000 Hz mouse overflowed eight packets in
  35 ms, losing motion and making the pointer drift (2026-09-24,
  `src/lib/proto/inputctl/src/queue.rs`). L6 checks that behaviour against
  `drivers/input/evdev.c` of the header version the probe records, not from
  memory.
* **Undeclared events are dropped.** An event whose type or code the device
  did not declare is not delivered, as Linux's input core does. The driver
  drops them first (§3.2); one reaching the core means the driver lied, which
  is a refusal (§3.2).
* **No software autorepeat.** Linux's input core repeats a held key as value-2
  events for devices that declare `EV_REP`. Compositors ignore those and repeat
  keys themselves (`wl_keyboard.repeat_info`), so the core does not generate
  them. `EVIOCGREP` answers a stored delay and period, and `EVIOCSREP` stores
  new ones. This is a written deviation (§6).
* **Timestamps.** Each report is stamped from the monotonic clock once, and a
  read converts it to the open's clock: realtime by default, as on Linux, or
  the clock `EVIOCSCLOCKID` chose (`CLOCK_REALTIME`, `CLOCK_MONOTONIC` or
  `CLOCK_BOOTTIME`).

The queue, report assembly, grab and drop rules are pure logic. They go in
`src/lib/proto/inputctl` beside the protocol (`inputctl::queue`), host-tested and
fuzzed, so the kernel's part is glue.

### 3.2 The driver protocol: a control channel, `src/lib/proto/inputctl`

As for the display, events are small and rare next to a disk's traffic, so
there is no data ring: one control `Channel` per device, with
`src/lib/proto/displayctl`'s message shape. Each message is fixed-size and
little-endian, starts with a type and a length, has reserved bytes that must be
zero, and is validated in the order its fields are read. Handles travel
alongside.

**Bring-up follows the display.** devmgr's table gets `(0x1AF4, [0x1052],
b"input", Kind::Input)` (`DEVMGR.md` already names 0x1052). `start_input` asks
the kernel for the control channel with a new native call,
`INPUT_CONTROL_CREATE` (0x104D, the next free number after
`DISPLAY_CONTROL_CREATE`), sends `START` with blk's layout to
`/lib/drivers/input`, and waits for `PUBLISHED`. QEMU's keyboard and tablet
are two PCI functions, so there are two driver processes, as there is one
blk driver per disk.

| Type | Direction | Body | Handles |
|---|---|---|---|
| `HELLO` | driver → core | version; location; device ids (bus type, vendor, product, version); name and serial, each with its length; property bits; for each event type the core supports, its code bitmap; for each declared absolute axis, minimum, maximum, fuzz, flat and resolution | driver port (`WRITE \| TRANSFER`) |
| `READY` | core → driver | the node's index `N` | core port (`WRITE`) |
| `REFUSED` | core → driver | reason | — |
| `EVENTS` | driver → core | count; up to `MAX_EVENTS` events of 8 bytes each, virtio-input's own layout (type `u16`, code `u16`, value `i32`) | — |
| `STOP` / `STOPPED` | as blk | | |
| `STATUS` | core → driver | laid out as `EVENTS`: the `EV_LED` events a program wrote to the node that changed the device's state, for the driver to light (§7.4) | — |

`HELLO` holds virtio-input's answers as the device gave them. Its bitmaps are
as long as the probe's `*_CNT` for each type, so the message is fixed-size and
well under a channel's 64 KiB. The event types the core supports in this
iteration are `EV_SYN`, `EV_KEY`, `EV_REL`, `EV_ABS`, `EV_MSC`, `EV_SW`,
`EV_LED` and `EV_REP`. A device declaring `EV_FF`, `EV_SND` or multi-touch
axes is published without them, and the boot line says what was left out.

`EVENTS` may end in the middle of a report. The next message continues it,
and the core only delivers at `SYN_REPORT` (§3.1). `MAX_EVENTS` is 64, which
holds any report QEMU's HID devices make.

**The driver** (`src/user/system/native/drivers/input/virtio-input`, logic in `src/lib/drivers/input/virtio-input` over
`src/lib/drivers/block/virtio-blk`'s traits as `src/lib/drivers/display/virtio-gpu` does) negotiates features,
asks the configuration queries through `src/lib/drivers/virtio::input`, sends `HELLO`,
and on `READY` sets `DRIVER_OK`. It then keeps every descriptor of the event
queue posted with an 8-byte buffer: **QEMU drops a whole report without
telling anyone when the queue lacks buffers for it**
(`hw/input/virtio-input.c:47`–`56`), so the driver refills after each
interrupt before it forwards anything. It checks each event against what the
device declared, drops those it did not declare, and forwards the rest in
`EVENTS`. The status queue is not used in this iteration.

**Doorbells** are the channel's own signal both ways, as for the display.

**What the core never trusts:** a `HELLO` with a length over its field, a
bit beyond its type's `*_MAX`, an axis whose minimum lies above its maximum,
an `EVENTS` count over `MAX_EVENTS`, an event of a type or code not in
`HELLO`, or a report longer than the core will hold (256 events). Any of these
gets `REFUSED`, then quiesce, the same as a display driver that lies. The
device's word is checked by the driver first (`src/lib/drivers/virtio::input`'s trust
section); the core checks the driver's.

**A driver that goes away** takes the node with it. Every open's `read`
answers `ENODEV` once its queue is drained, and `poll` reports `POLLHUP |
POLLERR`, as a Linux evdev client sees an unplugged device. The node leaves
`/dev/input`, and its index is not given out again this boot, so a program
never opens a different device under a name it remembered.

**`PUBLISHED`** is sent when the core has registered `eventN`.

### 3.3 The evdev subset

`/dev/input/eventN` is a character device, major `INPUT_MAJOR` (13,
`linux/major.h`) and minor 64 + N, the range Linux's evdev takes for its first
32 devices. The node is `0660` and root's, as `card0` is, since Ferrix has no
`input` group. `/dev/input/` becomes devfs's second subdirectory after
`/dev/dri/`. Any number of opens is allowed, as on Linux, each with its own
queue, clock and grab state. Nothing here stands in for DRM master's exclusive
open.

Numbers and layouts go into `src/lib/proto/linux-abi` (`input` module), from a probe
compiled on example against `/usr/include/linux/input.h` and
`input-event-codes.h` (`linux-libc-dev` 7.0.0-29.29 today), at both pointer
widths. The probe is committed as `src/lib/proto/linux-abi/probe/input.c` and
`input.sh`, its output as `input-64.txt` and `input-32.txt`, and the numbers
are pinned by `src/lib/proto/linux-abi/src/tests/input.rs`, exactly as `drm.c`,
`drm.sh`, `drm-64.txt`, `drm-32.txt` and `tests/drm.rs` did for the display.
**No number in this document or in the code is written from memory.** The
behaviour in the table below that the headers do not fix (which errors, which
lengths) is Linux's `drivers/input/evdev.c`, which L6 checks line by line
rather than trusting this table.

| ioctl (`linux/input.h` line) | The input iteration |
|---|---|
| `EVIOCGVERSION` (130) | `EV_VERSION` |
| `EVIOCGID` (131) | the device ids from `HELLO` |
| `EVIOCGREP`, `EVIOCSREP` (132–133) | only for a device declaring `EV_REP`; the stored delay and period (§3.1) |
| `EVIOCGNAME(len)`, `EVIOCGUNIQ(len)` (140, 142) | the name or serial, cut to `len`, returning the bytes copied including the NUL; `ENOENT` when the device gave none |
| `EVIOCGPHYS(len)` (141) | `ENOENT`: a virtio device has no physical path |
| `EVIOCGPROP(len)` (143) | the property bits, returning the bytes copied |
| `EVIOCGKEY`, `EVIOCGLED`, `EVIOCGSW(len)` (171, 172, 174) | the current state bitmaps |
| `EVIOCGSND(len)` (173) | an empty bitmap |
| `EVIOCGBIT(ev, len)` (176) | `ev` 0: the event types; otherwise that type's codes; an undeclared type is an empty bitmap |
| `EVIOCGABS(abs)` (177) | the axis's `input_absinfo` with its current value; `EINVAL` for an undeclared axis |
| `EVIOCGRAB` (184) | 1 grabs: only this open receives events, `EBUSY` if another holds the grab. 0 releases: `EINVAL` if this open does not hold it. Closing releases |
| `EVIOCREVOKE` (185) | this open reads `ENODEV` and polls `POLLHUP \| POLLERR` from now on |
| `EVIOCSCLOCKID` (241) | `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME`; others `EINVAL` |
| `EVIOCGKEYCODE*`, `EVIOCSKEYCODE*`, `EVIOCSABS`, `EVIOCGMTSLOTS`, `EVIOCSFF`, `EVIOCRMFF`, `EVIOCGEFFECTS`, `EVIOCGMASK`, `EVIOCSMASK` | `EINVAL` in this iteration; none is called by the consumers in §2.4 for the devices QEMU offers |

**Lengths follow the caller's width.** Linux copies bitmaps in whole `long`s
of the caller's width and returns the byte count. The EVIOCG* requests that
carry a length encode it in the request number, so the core decodes direction,
type `'E'`, number and size rather than matching whole numbers. L1's probe
prints each request's number for a sample length so the decoder is checked
against the macros.

**`struct input_event` at both widths.** The kernel's own definition is two
`__kernel_ulong_t`s (seconds and microseconds), then `type`, `code` and
`value` (`linux/input.h:26`–`46`): 24 bytes on x86-64 and AArch64, 16 on
ARMv7-A. That is what `read` returns. User space built without
`__USE_TIME_BITS64` sees a `struct timeval` there instead, which on a 32-bit
libc with a 64-bit `time_t` is a different size. The probe builds three views
at both widths and records them: 32-bit `time_t`, 64-bit `time_t` with the
macro as glibc sets it, both 16 bytes on ARMv7-A, and 64-bit `time_t` with the
macro undefined, which is 24 bytes there. ferrousli's `bits/alltypes.h`
defines `__USE_TIME_BITS64`, as musl's does, so its programs should see the
kernel's 16 bytes; that is read from the header, not shown by a program
built against ferrousli. ARMv7-A has taken part in the gates since
2026-09-23: `src/user/system/linux/compositor/evecho`, a Rust program on the target's own musl
built for `armv7-unknown-linux-musleabihf`, reads the kernel's 16-byte
events, and `test-input` passes there with its negative control.

**`read`** returns as many whole events as fit in `count`, from whole
reports only. It returns `EINVAL` if `count` is smaller than one event,
blocks while the queue is empty, answers `EAGAIN` under `O_NONBLOCK`, and
`ENODEV` once the device is gone and the queue is drained. **`write`**
takes whole events as `evdev_write` does, `EINVAL` for fewer bytes than one:
`EV_LED` events change the device's LED state (`EVIOCGLED`) and those that
changed go to its driver as `STATUS`; `SYN_REPORT` does nothing; any other
type is `EINVAL`, where Linux injects it as though the device had sent it --
a written deviation, with a `docs/BACKLOG.md` row. The LED events are not
passed on to the node's readers, as Linux passes them. **`poll`** reports
`POLLIN | POLLRDNORM` when a whole report is queued and `POLLHUP | POLLERR`
when the device is gone.

**Kernel plumbing this needs:** a devfs subdirectory (the mechanism
`/dev/dri/` added), character nodes whose `open` makes a per-open object, an
ioctl branch in `sys_ioctl` beside `card0`'s (or the `Inode::ioctl` hook, if
the kernel reader of the display asked for it first), and `Inode::poll` and
`read_stream` on the per-open object, the way `card0`'s event `read` works.

### 3.4 Discovery without udev

A compositor finds devices by reading `/dev/input` and asking each
`eventN` `EVIOCGBIT(0)`: keys make a keyboard, relative axes a mouse,
absolute axes a tablet. It needs nothing from `/sys`. Devices exist before init runs in this iteration,
because devmgr starts the drivers at boot. Hotplug, and how a compositor
would learn of it without udev (an `inotify` watch on `/dev/input`), is §6.

## 4. QEMU and xtask

* **Devices.** `-device virtio-keyboard-pci,id=kbd0,disable-legacy=on,iommu_platform=on`
  and `-device virtio-tablet-pci,id=tablet0,disable-legacy=on,iommu_platform=on`,
  on x86-64 and AArch64, only under `test-input` and `run --display`, so no
  existing gate changes. On ARMv7-A the same two devices, since 2026-09-23,
  without `iommu_platform=on`: U-Boot 2025.10 resets when a PCI virtio
  device offers `VIRTIO_F_ACCESS_PLATFORM`. A tablet rather than a mouse because its absolute
  position is what a screendump test of a cursor will want later, and
  because `input-send-event`'s `abs` events are what reach it.
* **Routing.** QEMU gives an event to the first handler in its list that
  takes its kind, and a virtio-input device moves itself to the head of that
  list when its driver sets `DRIVER_OK` (`ui/input.c:63`–`67`, `101`–`123`,
  `hw/input/virtio-input-hid.c`'s `change_active`); before that it discards
  events (`virtio-input.c:28`–`30`). On q35 the PS/2 devices are behind it
  once the driver is up. So the test sends nothing until the program says
  it is ready, which is after both drivers are.
* **The program.** `src/user/system/linux/compositor/evecho`, built as init like
  `src/user/system/linux/compositor/blank`. It reads `/dev/input`, opens every `eventN` with
  `O_RDONLY | O_NONBLOCK | O_CLOEXEC`, identifies each with `EVIOCGVERSION`,
  `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPROP`, `EVIOCGBIT` and `EVIOCGABS`, sets
  `EVIOCSCLOCKID(CLOCK_MONOTONIC)` and `EVIOCGRAB(1)`, and prints one
  `evecho: device` line for each. It then prints `evecho: ready`, waits in
  `poll(2)`, and prints `evecho: event N type code value` for each event, in
  decimal. It uses `ferrix-linux-abi::input` as `blank` uses `::drm`.
* **`test-input`.** It boots the program with the two devices and waits for
  the two device lines. It requires the names QEMU gives, `QEMU Virtio
  Keyboard` and `QEMU Virtio Tablet` (`virtio-input-hid.c:19`–`21`), and the
  bits each declares. It waits for `ready`, then sends over QMP (TCP on
  localhost, as `test-display` does), one `input-send-event` per report:
  key `a` down; key `a` up; tablet `abs` x and y together; button `left`
  down; button `left` up. It requires, in order, `EV_KEY`/`KEY_A` 1,
  `SYN_REPORT`; `KEY_A` 0, `SYN_REPORT`; `ABS_X` and `ABS_Y` at the values
  sent, `SYN_REPORT`; `BTN_LEFT` 1, `SYN_REPORT`; `BTN_LEFT` 0, `SYN_REPORT`.
  The names come from `linux-abi::input`, and the values sent lie inside
  QEMU's `INPUT_EVENT_ABS_MIN`..`MAX` (`include/ui/input.h:13`–`14`), which
  the tablet passes through unscaled. If the program prints `evecho: failed`,
  the test stops at once and reports it.
* **The negative control.** `evecho` built with `negative-control` prints
  `evecho: negative control` first, and reports the first `EV_KEY` event's
  code one higher than it read. The check must see that marker line, then fail
  on exactly the `KEY_A` 1 line and report expected against actual, and on
  nothing else.
* **`run --display`** adds the same two devices, so the customer can type at
  and point into whatever the compositor of that iteration shows.

## 4a. The keymap: every level, and several layouts

Added 2026-09-17. What a key means is the keymap's business, and until this
the tables carried two levels of one layout -- so a German keyboard could not
type `@`, `|`, `~`, `[`, `]`, `{`, `}` or a backslash, all of which are on
`AltGr`, and `input:kb_layout = de,us` matched no shipped layout at all and
fell back to `us`.

**What Hyprland does, which is less than it looks.** Hyprland compiles one
keymap per keyboard, sends it to clients, feeds `xkb_state_update_key` on
every real key transition, serializes depressed/latched/locked/group, and
forwards raw evdev keycodes. It never calls `xkb_state_key_get_syms`,
`xkb_keymap_num_levels_for_key` or `xkb_state_key_get_utf8`: levels are the
client's business, resolved against the keymap it was handed. `kb_layout =
de,us` goes to libxkbcommon verbatim -- **no comma splitting** -- and comes
back as one keymap with two groups, so a layout switch changes only the group
index in `wl_keyboard.modifiers` and sends no new keymap
(`IKeyboard.cpp:66-73`, `Seat.cpp:377-380`). Hyprland's own comma-splitting
code, `IKeyboard::updateXKBTranslationState(nullptr)` at `IKeyboard.cpp:228`,
is unreachable in 0.56.2 and must not be ported: it would break the
correspondence between the group index and the keymap clients hold.

**So the compositor's job is a correct keymap and correct serialization**,
and that is what this is.

* **Every level is in the tables.** `src/user/system/linux/compositor/xkb/probe/keymap.c` prints,
  for each key, every group and every level: the keysyms, and the modifier
  masks that select each level (`xkb_keymap_key_get_mods_for_level`). So
  `Key::level` is a lookup rather than an implementation of XKB's key types.
  The rule is libxkbcommon's and it is not "the mask that matches": the
  active modifiers are first narrowed to the ones the key's type declares --
  every modifier named at any of its levels -- and a combination that then
  matches nothing is **level zero**, never nothing at all. `de` has 64 keys
  deeper than two levels, `us` 17.
* **A client reads the keymap it was handed.** The term app is a client,
  and it read the first shipped table whatever the keymap said, so
  `kb_layout = de` gave it an American keyboard. There is no libxkbcommon
  here to compile with, so `compositor_xkb::groups_of` reads the group names
  out of the text -- `name[1]="German"` is what libxkbcommon's own printer
  writes -- and matches them against the tables the binary carries.
  `Layout::label` is that name, generated from the same probe output.
  The keymap is **mapped** read-only, not read: a descriptor that arrives
  over a socket shares the sender's file offset, so reading it would leave
  the next client's keymap empty.
* **Several layouts are one keymap with a group each.**
  `compositor_xkb::layouts` reads `input:kb_layout` and `input:kb_variant` as
  comma-separated lists side by side, capped at XKB's four groups, and
  `compositor_xkb::merged` assembles the shipped single-group keymaps into
  one multi-group text. The assembly is checked against libxkbcommon: for
  `us,de` all 1285 level records of a real `de,us` keymap are reproduced
  exactly, and for `de,us` all but two -- `<KPDL>` and `<LSGT>` gain a second
  group the real keymap leaves single, because a compiled keymap does not
  record that `us` inherited those keys from `pc(pc105)`. Both values are
  what those keys type in `us`, and `merge/tests.rs` pins the list.
* **A switch moves the group and nothing else.** `hyprctl switchxkblayout
  <device> <next|prev|index>` with Hyprland's own grammar: device selectors
  `main`/`active`/`current`, `all`, or a normalised name; a 0-based index
  that is range-checked; `next`/`prev` that wrap **by modulus**, which is
  what Hyprland relies on -- it asks for the index after the last one and
  lets libxkbcommon bring it back, so a state that refused an out-of-range
  index would stop `next` cycling. It is a hyprctl command and **not** a
  keybind dispatcher; a bind reaches it with `exec, hyprctl …`. A real change
  posts `activelayout>><device>,<layout name>` on the event socket.
* **A bind resolves against group zero, at level one.** Hyprland resolves
  binds against a state it never gives a group or a modifier
  (`CKeybindManager::m_xkbTranslationState`), so a bind stays on the physical
  key group zero puts it on, whatever layout is being typed in, and no bind
  of Hyprland's resolves above the first level either. `bind = , at` is one
  Hyprland would not resolve, and neither does this; `code:NN` names such a
  key.

**The one deliberate divergence: `grp:` options.** `kb_options =
grp:alt_shift_toggle` works in Hyprland because the option is passed to
libxkbcommon and the toggle ends up in the keymap's own compat section, which
`xkb_state_update_key` then acts on. This compositor cannot compile a compat
section at runtime, so a `grp:` toggle has to be implemented in
`src/user/system/linux/compositor/xkb`'s own state rather than read out of the keymap. Same
behaviour, different mechanism, and it is the reason a shipped keymap carries
no `grp:` option: the keymaps are generated without one, and the toggles are
code. **Not yet implemented** -- `hyprctl switchxkblayout` is the way to
switch today, and a `grp:` line is read and carried without effect.

**Also not implemented, and not planned:** compose and dead keys. Hyprland
has no compose support at all and needs none, because it forwards keycodes
and produces no text; a dead key is an ordinary keycode and the client's own
`xkb_compose_state` handles it. `src/user/system/linux/compositor/xkb::character` therefore answers
`None` for `dead_circumflex` rather than the spacing character it resembles,
so a client that cannot compose types nothing rather than the wrong thing.

## 5. Landings and points

Each is a small landing on main, gated on example. The first four touch no
kernel code. **L1 comes first:** every later landing takes its numbers from
it.

| # | Landing | Kernel? | Points |
|---|---|---|---|
| L1 | `src/lib/proto/linux-abi::input`: the §3.3 ioctls (with a sample length for the sized ones), `EV_VERSION`, `INPUT_MAJOR`, event types and codes, `*_MAX`/`*_CNT`, the clock ids, and `input_event`, `input_id` and `input_absinfo` layouts at both widths. From a committed probe (`probe/input.c`, `input.sh`, `input-64.txt`, `input-32.txt`) compiled on example against `/usr/include/linux/input.h`, pinned by `src/tests/input.rs` | no | 2 |
| L2 | `src/lib/drivers/virtio::input`: configuration queries and the 8-byte event, hostile-device tests, checked against QEMU 9.2.4, fuzzed, its evdev numbers L1's. Landed (493fd843, 0bfb1de4, and the switch to L1's numbers) | no | 2 |
| L3 | `src/lib/proto/inputctl`: §3.2's messages and validation, and `queue`: report assembly, per-open queues, `SYN_DROPPED`, grab, clock conversion, state for `EVIOCGKEY`/`EVIOCGABS`. Host-tested, fuzzed | no | 3 |
| L4 | `src/lib/drivers/input/virtio-input`: driver logic over `src/lib/drivers/block/virtio-blk`'s traits (bring-up, the queries into `HELLO`, keeping the event queue full, filtering, batching), tested against a simulated device that drops short reports as QEMU does. Landed | no | 3 |
| L5 | `src/user/system/native/drivers/input/virtio-input`, devmgr's table entry and `start_input`, `INPUT_CONTROL_CREATE`, the core's per-device task; exit: the boot line names each device and its event types. Landed | yes | 5 |
| L6 | devfs `/dev/input/eventN`: subdirectory, character nodes, per-open objects, the ioctl branch and §3.3's subset, `read` and `poll`; Linux's queue size, drop rule, grab and revoke answers, string and bitmap lengths, and `read`'s errors checked against `drivers/input/evdev.c`. Landed | yes | 5 |
| L7 | `src/user/system/linux/compositor/evecho`; `xtask test-input` with QMP `input-send-event` and its negative control; the devices under `run --display`. Landed | no | 3 |
|  | **The input iteration** |  | **23** |

**The event-loop prerequisites (§2) are iteration 2's, not these 23**, and the
input iteration does not wait for them:

| # | Landing | Kernel? | Points |
|---|---|---|---|
| E1 | `epoll_create1`, `epoll_ctl` with `EPOLLET` and `EPOLLONESHOT`, `epoll_pwait` and `epoll_wait`, on every pollable file; an epoll descriptor pollable in turn. Landed (os-26, d047480d) | yes | 8 |
| E2 | `eventfd2` with `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_SEMAPHORE`. Landed with `eventfd` (os-26, 9ed8808f) | yes | 2 |
| E3 | `ioctl(FIONBIO)` (and `FIOCLEX`/`FIONCLEX`) on every descriptor. `FIONBIO` landed (os-26, 2dbeadbf); `FIOCLEX` and `FIONCLEX` landed after it | yes | 1 |
| E4 | `card0`'s primary plane and properties: `GETPLANERESOURCES`, `GETPLANE`, `OBJ_GETPROPERTIES`, `GETPROPERTY`, from `probe/drm.c` extended. In progress (GUI session) | yes | 3 |
| E5 | `timerfd_create`, `timerfd_settime`, `timerfd_gettime` (wanted, not required). Done 2026-09-24, with the `time64` forms on ARMv7-A | yes | 3 |
|  | **Required for iteration 2 (E1–E4)** |  | **14** |

Stage 17 was 55 points, of which iteration 1 took 37. The input iteration
and E1–E4 together are 37 more. The difference comes from what §2 found:
nesting epoll, `FIONBIO` and the plane objects were never counted. Neither
was the per-open queue logic, which the roadmap's one line on input did not
break down. The product owner re-baselined stage 17 to 74 points on
2026-09-16: 37 for the display, 23 for input and 14 for E1–E4.

**Where the landings stand.** L1 is done: the probe's numbers and layouts
matched this document's text, and it found `EVIOCSFF` to be a second request
whose number depends on the width, since `struct ff_effect` holds a pointer.
L2 is done: `src/lib/drivers/virtio::input` landed in 493fd843 and 0bfb1de4, and now
re-exports L1's `EV_*` and `SYN_REPORT` instead of keeping its own copy, which
agreed with them. `ferrix-virtio` depends on `ferrix-linux-abi` for that: a
`no_std` crate with no dependencies that the kernel, `ferrix-rt` and the fuzz
crate already link. Its tests now also require the bits QEMU's devices set to
be L1's codes, and its fuzz target ran 50,283,653 inputs in ten minutes on
example without a failure. L3 is done, landed on 2026-09-16 as 3aa79e19 to
456719d6: `src/lib/proto/inputctl` with §3.2's messages, the core's session and §3.1's
queue, host-tested and fuzzed. Where this document left a
rule to Linux, it follows `drivers/input/evdev.c` and `input.c`, and it
records four places where they answer differently from the text above, for
L6 to settle: a full queue keeps `SYN_DROPPED` and the newest event rather
than only `SYN_DROPPED`; the state changes as each event arrives, since
whether an event passes depends on it; `read` answers `ENODEV` as soon as the
device is gone, queued events or not; and `EVIOCGABS` of an undeclared axis
answers zeros, which the crate leaves to the glue. L4 is done: `src/lib/drivers/input/virtio-input`
is the driver logic over a transport, pinned pages and an event area the
process hands it, host-tested against a device copying QEMU 9.2.4's keyboard,
mouse, tablet and multi-touch tables (28 tests) and fuzzed against a real
session (`virtio_input_driver`, 10,523,605 inputs in ten minutes without a
failure). Two of its rules are decisions 9 and 10 below.

L5, L6 and L7 are done, and with them the input iteration. The boot line
names each device and what it publishes -- `input    event0 QEMU Virtio
Keyboard: keys, LEDs, repeat` -- `/dev/input/eventN` answers the requests
§2.4 reads out of the `evdev` crate, and `cargo xtask test-input` puts a key
and a touch in at QEMU's far end over QMP and requires them back out of the
nodes, on x86_64 and on aarch64, with a negative control that reports every
key as `KEY_RESERVED` and must fail.

Four things the three landings found, each of which would have been a silent
wrong answer rather than a failing test:

* **A kernel task's stack is four pages, and the handshake did not fit.** A
  decoded `Message` is 1680 bytes and a `Session` 4272, and the compiler
  inlined the decode, the session and the box into the task's entry and added
  every temporary up. The frame was larger than the stack and the first push
  double-faulted. `Hello::decode_into` now decodes onto the heap, and each
  large value has a frame of its own held apart by `#[inline(never)]`
  (`src/kernel/src/interfaces/input/mod.rs`, `judge`).
* **`EVIOCGBIT` of a type with no bitmap is `EINVAL`.**
  `evdev_handle_get_bits` switches on the type, and `EV_REP`, `EV_PWR` and
  `EV_FF_STATUS` are not in the switch. A consumer that asked for every type
  a device reports lost every keyboard that reports `EV_REP`, which QEMU's
  does -- found by running `src/user/system/linux/compositor/evecho` against the host's own kernel
  before Ferrix ever ran it.
* **A directory cursor counts from `FIRST_CURSOR`, not from zero.** The two
  entries before it are `.` and `..`. Counting from zero listed nothing at
  all, because the first `getdents` arrives with the cursor already at two.
* **A copy to a program's memory may not be made with a spin lock held.** It
  may fault, and a fault may not be resolved with preemption disabled;
  `EVIOCGNAME` held the session across the copy. The text is now copied out
  of the session first.

The compositor reads those nodes as of the same day: `src/user/system/linux/compositor/xkb` carries
libxkbcommon's own keymap for the `us` layout from a committed probe,
`hyprix` opens every node through `src/user/system/linux/compositor/evecho` and turns its events
into `wl_keyboard` and `wl_pointer` ones, and `cargo xtask test-seat` types
into a window on Ferrix from QEMU's far end and fires a Hyprland keybind.
`docs/ROADMAP.md` stage 18 records it.

Of iteration 2's prerequisites,
E1–E3 landed (os-26) and E4 landed (the GUI session): iteration 2's
prerequisites are all in. The roadmap's stage 17 records what E1 and E2 do not yet do as
Linux does.

## 6. Decisions and open questions

**Decided by os-f6, 2026-09-16:**

1. **One driver process per input device**, as blk has one per disk, rather
   than one `src/user/system/native/drivers/input/virtio-input` serving every virtio-input function.
2. **No kernel key autorepeat** (§3.1), a written deviation from Linux: the
   core makes no value-2 events. Compositors repeat keys themselves
   (`wl_keyboard.repeat_info`), and libinput ignores `EV_REP` events.
3. **`write` was refused at first**, so the caps-lock LED did not light. LEDs
   came with an additive `inputctl` message (`STATUS`, core → driver) on
   2026-09-23 (§3.3, §7.4): a USB keyboard lights them; virtio-input takes
   STATUS but does not drive its status queue yet (`docs/BACKLOG.md`).
4. **Multi-touch, force feedback and sound are left out** of what the core
   publishes, even when a device declares them, each with a backlog row.
   QEMU's keyboard and tablet declare none of them.
5. **Hotplug is out of scope**, with a backlog row. Devices exist from boot.
   Whether a later iteration adds `inotify` on `/dev/input` (not in Ferrix
   today) or accepts a compositor that rescans is that row's question.
6. **The compositor's input backend is hand-written over
   `ferrix-linux-abi::input`**, not the `evdev` crate or `nix`: the no-C-stack
   rule, and a small surface. §2.4 stays as the check that the subset would
   serve an `evdev`-crate consumer too.
7. **os-26 holds E1** (`epoll`, with nesting), **E2** (`eventfd2`) **and E3**
   (`FIONBIO`). E5 (`timerfd`), wanted and not required, is done (2026-09-24).
8. **E4, `card0`'s planes and properties (3 points), belongs to the GUI
   session (os-e5)**, and `docs/DISPLAY.md` §2.3 describes it as iteration 2:
   `GETPLANERESOURCES`, `GETPLANE`, `OBJ_GETPROPERTIES` and `GETPROPERTY`,
   with the primary plane and `type` property Smithay needs (§2.3 here).

**Decided in L4, 2026-09-17, and open to the reader of L5:**

9. **The driver drops what the core would not publish**, rather than
   forwarding it for the core to refuse. §3.2 has the core refuse a message
   holding any undeclared event and stay broken after it, so a driver that
   passed on `SYN_MT_REPORT` or a multi-touch axis its device declared would
   break its own session at the first touch. It therefore forwards
   `SYN_REPORT`, `REP_DELAY` and `REP_PERIOD` of a device declaring `EV_REP`,
   and otherwise only a type and code in the `Capabilities` of the HELLO the
   core accepted, and counts the rest.
10. **A report the core would refuse for length is cut, not dropped.** §3.2
    caps a report at `MAX_REPORT` events. When one reaches a single event
    short of that, the driver ends it with a `SYN_REPORT` of its own and
    starts the next with the following event, counting the split. QEMU never
    does this -- a report longer than its 64-entry queue is one it can never
    deliver (`virtio_input_send`) -- so only a device outside QEMU's shape
    meets the rule, and cutting keeps every event where dropping the rest
    would lose them. The alternative, refusing the device, would turn one
    over-long report into a dead keyboard.

**For others:**

* **ferrousli (32-bit):** its `bits/alltypes.h` defines `__USE_TIME_BITS64`,
  so its view of `struct input_event` should be the kernel's 16 bytes (§3.3).
  A program built against it for ARMv7-A has not shown it. This does not
  block x86-64 or AArch64.
* **The kernel reader of L6:** `sys_ioctl` already special-cases the
  console, sockets and `card<N>`; `/dev/input/eventN` would be a fourth. The
  `Inode::ioctl` row in `docs/BACKLOG.md` says so; whether it lands first is
  the reader's call.
* **The XKB data** (`/usr/share/X11/xkb`) must be in the image for
  xkbcommon's keymap compiler, or the compositor builds its keymap from a
  string it carries. It is a stage 18 item: a pinned subset of
  xkeyboard-config in the image (2 points).

## 7. The DK board's USB host

The STM32MP157 DK boards have no input device QEMU's virtio could stand in
for: their four USB-A sockets hang off a Microchip USB2514B hub on port 1 of
the chip's EHCI controller. `src/user/system/native/drivers/usb/usbhid`, over `src/lib/drivers/usb/usb-host`, drives that
controller, the hub, and every keyboard and mouse behind it, and serves each
to the input core as §3.2's protocol says. It ran on an STM32MP157D-DK1 on
2026-09-23 with a Logitech G502 at full speed and a keyboard at low speed.

### 7.1 What the kernel does, and what it gives

As for the LTDC (`docs/DISPLAY.md` §6), the kernel does what is shared with
the rest of the chip and nothing more (`src/kernel/src/platform/st/stm32mp1/usb.rs`): it
turns on the USBH and USBPHY clocks, releases both resets, brings up the PWR
block's 1.8 V and 1.1 V regulators, and starts the USB PHY's PLL from the
HSE. Each value is what U-Boot's `usb start` writes on this board, read back
with `md`. It then publishes a device-tree node, binding `TREE_STM32_USBH`,
with the EHCI page and its interrupt, and says so:

```
  usb      EHCI at 0x5800d000, PHY PLL 0xd400003c from 24 MHz
```

The PHY's analogue tuning (`st,tune-*`) is left at its reset value.

### 7.2 Memory the controller and the program see alike

EHCI polls descriptors in memory, and the STM32MP1's does not snoop the
caches. A program's mappings were write-back cached, and ARMv7 gives ring 3
no cache maintenance, so `vmo_pin` takes `PIN_COHERENT`: on a device whose
DMA is not coherent, the pin must cover a VMO nothing maps yet, the VMO is
marked coherent, every frame is cleaned and invalidated to the point of
coherency, and every mapping of it after is Normal non-cacheable (MAIR
attribute 2, `MapFlags::uncached`). `vmo_read` and `vmo_write`, which copy
through the kernel's cached view, refuse such a VMO. On a device that snoops,
which is every device on x86-64 and QEMU, the option changes nothing. The
driver still orders its writes with `dsb sy` before the controller may look.

### 7.3 One host, several input devices

A USB host is not one input device but as many as are plugged in, and only
its driver knows how many. So devmgr starts it as a *bus host* (`Kind::Host`):
with its device and START, like the virtio-serial port's driver, and without
waiting for a PUBLISHED. The driver makes an input control channel for each
keyboard or mouse it finds with `device.input_control()`, which a USB host's
node allows eight times at once (`USB_INPUT_FUNCTIONS`) where every other
node allows one. A tree node's HELLO carries `DEVICE_NOT_PCI` for its
location, as the display core's does, and sends devmgr no PUBLISHED, since
every tree node shares that word. A device unplugged has its channel closed,
which the core hears as the device going (`event<N> is gone`); its number is
free again, and `event<N>` numbers are always the lowest free, so a device
whose driver dies alone and is started again comes back under the number it
had. When two die together, the first to come back takes the lower number. Each
channel is one of the node's claims (`src/kernel/src/claim.rs`), and a quiesce
waits until every one of a dead driver's has gone. devmgr does wait, a
bounded five seconds, for the driver to say on its bootstrap channel that
its first enumeration has settled -- what was plugged in at boot found and
published -- before it sends REPORT, which the kernel starts init at: a
compositor that reads `/dev/input` once at start found none of the board's
devices without it (hyprix as init, 2026-09-23). This is the
hotplug decision 5 of §6 left out, for this driver: devices come and go; how
a compositor learns of one that came later is still that backlog row's.

**The wait does not hold on the board as it boots today (2026-09-24).**
usbhid says it has settled after its first poll, and on the DK1 that poll
comes as the controller starts and finds nothing: init was running 60 ms
after `usbhid: EHCI running`, the hub was published half a second after it
and the keyboard behind the hub after 0.9 s. hyprix still found all four
devices because it opens `/dev/input` after its screens and its socket, at
least 0.7 s after its first line. That is a race the compositor
wins by being slow, and a faster start-up will lose it. Making the settle
honest -- ports powered and debounced, hubs walked -- would hold init back by
the whole enumeration, about a second on every boot; the fix that costs
nothing is the row above: hyprix rescanning `/dev/input` and telling bound
seats their new capabilities, which also serves a keyboard plugged in later.

### 7.4 The bus

`src/lib/drivers/usb/usb-host` is written against registers, DMA memory and a clock, and
tested against a model of the controller walking the real schedules frame by
frame, with the DK1's bus behind it as U-Boot's `usb tree` showed it.

* **Control transfers** run on an asynchronous schedule holding one queue
  head, switched on for the transfer and off after, so the head can be
  rewritten for any device without the doorbell handshake.
* **Interrupt IN pipes** hang after one inactive anchor every frame-list
  entry points at, each a queue head with two qTDs pointing at each other:
  the controller runs one while the driver reads and re-arms the other. Every
  pipe is polled every frame.
* **Split transactions:** a full- or low-speed device behind a high-speed hub
  is reached through the hub's transaction translator, each pipe starting
  in a microframe of its own among the first four, with complete-splits two
  to four microframes after. All four starts in microframe 0 overran the
  translator on the board: the last pipe linked, the mouse's, halted until
  it was stopped. The model fails a test for a wrong hub, port, speed or
  mask, or for two starts in one microframe behind one hub, and a pipe
  stopped after repeated halts says so on the console.
* **Hotplug by polling:** every 250 ms the driver reads the root ports and
  asks each hub for each port's status. A device that cannot be set up is
  left alone until it is unplugged.
* **HID, by the device's report descriptor:** every HID interface with an
  interrupt IN endpoint has its report descriptor read (`src/lib/drivers/usb/usb-host`'s
  `report`: main, global and local items, report IDs, push and pop), and one
  whose fields map to anything is a function, in the report protocol.
  Usages become codes by Linux's `hid-input.c`: the keyboard page by
  `hid_keyboard`, buttons from `BTN_MOUSE` in a mouse (sixteen) and
  `BTN_MISC` elsewhere, the relative desktop axes, `AC Pan` as `REL_HWHEEL`,
  the system controls, and the consumer page's media and application keys.
  Events come in Linux's order (modifiers, keys let go, keys pressed,
  `SYN_REPORT`). A boot interface whose descriptor cannot be read falls back
  to the boot protocol, whose fixed layouts are two built-in descriptors
  read the same way. A function is named as Linux names an input per
  application: the manufacturer's and product's strings, and the
  application's suffix unless the name already ends with it ("SEM USB
  Keyboard Consumer Control"). The bus is `BUS_USB`.
* **LEDs:** a keyboard declares the LEDs its output reports hold. `STATUS`
  from the core becomes `SET_REPORT` of each output report holding an LED,
  sent when the LEDs change. hyprix writes Caps Lock and Num Lock from the
  keymap's locked modifiers to every keyboard that declares LEDs, as
  libinput's compositors do, so the lights follow the keys on the desktop.

### 7.5 Not done

Absolute axes (tablets, touch screens), multi-touch and force feedback;
vendor pages (the G502's HID++ report); bulk and isochronous
transfers, and a full- or low-speed device on a root port, which
EHCI hands to its companion OHCI controller -- not reachable on a DK board,
whose root port has the hub. A process writing the 115200-baud console flat
out starves the driver: hexdumping both event nodes to it lost the clicks
made meanwhile, where recording them to tmpfs lost nothing (`docs/BACKLOG.md`).
