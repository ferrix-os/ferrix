# Stage 17 — Display and input ✅  ·  *74 points, spent*

The first stage of the goal after `rustc`: a Hyprland-shaped Wayland
compositor, written in Rust, running on Ferrix. The compositor is a user
program on the Linux ABI, so what it needs from the kernel is what a Linux
compositor needs, and this stage provides it the way stage 10 provides disks:
a kernel core with a ring-3 driver on stage 10's device objects, exposed
through the Linux ABI so that Rust's existing compositor crates run unchanged.

* **A display core and a virtio-gpu driver in ring 3.** The core owns
  connectors, modes and scanout buffers; the driver drives virtio-gpu's 2D
  commands (resource create, attach backing, set scanout, transfer, flush)
  through a translated domain, the way virtio-blk drives the disk. The Linux
  ABI is `/dev/dri/card0` with the DRM/KMS subset a software-rendered
  compositor uses: `GET_RESOURCES`, connectors and modes, dumb buffers
  (`MODE_CREATE_DUMB`, `MODE_MAP_DUMB` served by file-backed `mmap`),
  `MODE_ADDFB2`, the legacy page flip (atomic commit stays refused), and the
  vblank event read from the descriptor. No GEM import, no PRIME, no render
  node at this stage: rendering is on the CPU into a dumb buffer. PRIME and
  the render node came with stage 19's GPU path (`docs/GPU.md` §3).
* **An input core and a virtio-input driver in ring 3,** exposed as evdev:
  `/dev/input/event*` with `EVIOCGBIT`, `EVIOCGNAME`, `EVIOCGABS` and the
  `input_event` stream, keyboard, mouse and tablet as QEMU offers them.
* **The calls a Rust event loop needs:** `epoll_create1`/`epoll_ctl`/`epoll_pwait`,
  with an epoll descriptor another epoll can wait on, `eventfd2`, `FIONBIO`,
  `timerfd_create`/`timerfd_settime` (called by `polling`, which tolerates
  their absence), `memfd_create`
  with sealing, and `AF_UNIX` sockets with `SCM_RIGHTS`, pulled forward from
  the networking stage because Wayland is a Unix socket carrying descriptors
  and `wl_shm` is a sealed memfd mapped by both sides. The `AF_INET` half
  stays where it is.
* **Nothing is drawn by the kernel.** `docs/ARCHITECTURE.md` keeps its rule:
  the firmware framebuffer is a panic's, once. The display core hands scanout
  to whoever opened the card; a panic after that still writes text over it.

**Done — iteration 1, a colour on the screen (2026-09-16).** `docs/DISPLAY.md`
is the design. The display core (`src/kernel/src/interfaces/display`) takes a ring-3
driver's HELLO on a control channel `DISPLAY_CONTROL_CREATE` (0x104C) makes,
checks it through `ferrix-displayctl`'s session, refuses it when the firmware
framebuffer is memory the allocator owns, and publishes `/dev/dri/card0`: a
256 MiB card VMO whose ranges are dumb buffers, mapped by the program through
the card's inode and pinned read-only by the driver. `src/user/system/native/drivers/display/virtio-gpu`, started by
devmgr for 0x1050, drives virtio-gpu's 2D commands through
`ferrix-virtio-gpu` behind VT-d or the SMMUv3. The card answers the legacy
DRM subset — resources, connector, encoder, CRTC, dumb buffers, `ADDFB`,
`ADDFB2`, `SETCRTC`, `PAGE_FLIP` with its event, `DIRTYFB` — to one open at a
time. `cargo xtask test-display` boots `src/user/system/linux/compositor/blank` as init on x86-64
and AArch64 and requires every pixel of QEMU's screendump of the virtio-gpu
to be its colour, and its negative control to fail at exactly pixel (0, 0).

**Done — L1 of the input iteration, evdev's numbers (2026-09-16).**
`docs/INPUT.md` is the design, approved by os-f6 the same day. Its first
landing is `ferrix-linux-abi::input`: the evdev ioctls `/dev/input/eventN`
will answer, with the sized ones as functions and a request's parts taken
apart as the kernel's `_IOC_*` macros do, the event types and the codes the
test and QEMU's keyboard and tablet use, and `input_event`, `input_id` and
`input_absinfo`. A committed probe, `probe/input.c`, prints every number and
layout from linux-libc-dev 7.0.0-29.29's headers natively and under
`qemu-arm`, including the three views of `input_event` a 32-bit libc can
take, and the tests name each line the module disagrees with.

**Done — L2 of the input iteration, the virtio-input protocol (2026-09-16).**
`ferrix-virtio::input` asks a device virtio 1.2 §5.8's configuration queries
(name, serial, ids, property and event bitmaps, each axis's range) and reads
the 8-byte event. It refuses an answer over 128 bytes or running past the
configuration block, which QEMU shortens to its longest answer, a structure
answer shorter than its structure, an axis whose minimum lies above its
maximum, and a completion that wrote anything but one event. Its tests read
QEMU 9.2.4's keyboard, mouse, tablet and multi-touch devices whole, beside
devices that lie, and require the bits QEMU sets to be L1's `KEY_*`,
`BTN_*`, `REL_*`, `LED_*` and `ABS_MT_*` codes. It no longer writes evdev's
numbers down itself: `EV_*` and `SYN_REPORT` are L1's, re-exported, so the
driver and the evdev nodes read one copy the probe pins. Its fuzz target,
`virtio_input`, ran 50,283,653 inputs in ten minutes without a failure.

**Done — L3 of the input iteration, the input control protocol and evdev's
queues (2026-09-16).** `src/lib/proto/inputctl` holds `docs/INPUT.md` §3.2's messages
between the kernel's input core and a ring-3 driver, the core's side of that
conversation, and §3.1's per-open queue, host-tested (23 tests) and fuzzed
(3,038,857 inputs in ten minutes without a failure). Where the design left a
rule to Linux it follows `drivers/input/evdev.c` and `input.c`, and
`docs/INPUT.md` §5 lists the four places where Linux answers differently from
the design's first text, for L6 to settle. No kernel code uses it yet.

**Done — L4 of the input iteration, the virtio-input driver (2026-09-17).**
`src/lib/drivers/input/virtio-input` is the driver a `src/user/system/native/drivers/input/virtio-input` process will run: the logic
over a [`Transport`], pinned pages and an event area the process hands it, as
`src/lib/drivers/display/virtio-gpu` is written. Bring-up negotiates features, reads the device's
description through L2's configuration queries and builds the event queue, and
then stops with `FEATURES_OK` set and no buffer posted, because QEMU discards
every event until `DRIVER_OK` and `docs/INPUT.md` §3.2 has the core judge the
device before it is set; READY sets it, posts a buffer in every descriptor and
rings the doorbell, and a device the core refuses never sees it. The event
queue is kept full before a single event is forwarded, since QEMU drops a
whole report without a word when a buffer is missing. What the core would not
publish is dropped here rather than breaking the session, and a report that
reaches one event short of the core's limit is cut with a `SYN_REPORT` of the
driver's own and counted: `docs/INPUT.md` §6 decisions 9 and 10. Memory the
device may still write into comes back `Teardown::Wedged` and is never
dropped. Its 28 tests run it against a device copying QEMU 9.2.4's keyboard,
mouse, tablet and multi-touch tables and against devices that lie; the
`virtio_input_driver` fuzz target plays the device and the glue against a real
`inputctl` session and requires the core never to refuse a message the batch
made, no event to be lost or reordered, and every message to fit and to end at
a report boundary unless it is full. It ran 10,523,605 inputs in ten minutes
without a failure. No kernel code uses it yet: L5 is the process.

**Done — L5, L6 and L7 of the input iteration: events, end to end
(2026-09-17).** The input core (`src/kernel/src/interfaces/input`) takes a ring-3 driver's
HELLO on a control channel `INPUT_CONTROL_CREATE` (0x104D) makes, judges it
through `ferrix-inputctl`'s session, and publishes `/dev/input/eventN` with a
boot line naming the device and what it publishes -- `input    event0 QEMU
Virtio Keyboard: keys, LEDs, repeat`. `src/user/system/native/drivers/input/virtio-input`, started by devmgr for
0x1052, drives the device through `ferrix-virtio-input` behind VT-d or the
SMMUv3, as `src/user/system/native/drivers/display/virtio-gpu` drives the card. The nodes answer the evdev subset
`docs/INPUT.md` §2.4 reads out of the `evdev` crate -- `EVIOCGVERSION`,
`EVIOCGID`, `EVIOCGNAME`, `EVIOCGUNIQ`, `EVIOCGPROP`, `EVIOCGBIT` of each
type that has a bitmap, `EVIOCGKEY`, `EVIOCGLED`, `EVIOCGSW`, `EVIOCGABS`,
`EVIOCGREP`/`EVIOCSREP`, `EVIOCGRAB`, `EVIOCREVOKE`, `EVIOCSCLOCKID` -- with
a queue per open following `evdev.c`'s size, drop and `SYN_DROPPED` rules.
`src/user/system/linux/compositor/evecho` is the consumer, and it runs on a Linux host's own
`/dev/input` as well as on Ferrix, which is where it found that `EVIOCGBIT`
of `EV_REP` is `EINVAL` on Linux. `cargo xtask test-input` boots it as init
on x86-64 and AArch64, sends a key press, a key release, an absolute position
and a button through QMP's `input-send-event`, and requires each one back out
of the right node as a finished report; its negative control reports every
key as `KEY_RESERVED` and must fail the same check. `--display` now brings
the keyboard and the tablet too, so the compositor `run --display` starts has
a seat to read, which it has since grown.

**Estimate, re-baselined by os-f6 on 2026-09-16:** 74 points, from 55. The
display's iteration 1 took 37, the input iteration is 23 (`docs/INPUT.md`
§5), and the prerequisites of iteration 2 are 14: nested `epoll` (E1),
`eventfd2` (E2) and `FIONBIO` (E3), which os-26 landed on 2026-09-16, and
`card0`'s planes and properties (E4, `docs/DISPLAY.md` §2.3), which landed
the same day.
Nested `epoll`, `FIONBIO` and the plane objects were not counted before
`docs/INPUT.md` §2 read Smithay's event loop.

**Follow-up surface, not part of stage 17's met exit:** `timerfd` (wanted,
not required; done 2026-09-24, below), `signalfd` (done the same day, below),
atomic commit and per-open windows onto the card
VMO. E1–E3 (os-26) and E4 are done, so iteration 2's prerequisites are all
in. E1–E3's paragraphs below record
what they do not yet do as Linux does: `EPOLLRDHUP` and `EPOLLPRI` are never
reported, and a zero-length write to an eventfd returns 0 rather than
`EINVAL`. The third, that `poll` and epoll waits rechecked every 5 ms instead
of waking on the event, was fixed on 2026-09-16 (c2129a68).

**Exit:** in the boot test on x86-64 and AArch64 (and on ARMv7-A since
2026-09-23; the DK1's LTDC is a hardware row, P3), a user program opens
`/dev/dri/card0`, sets the mode, draws a known pattern into a dumb buffer and
page-flips it; `cargo xtask` reads QEMU's screendump and requires the pattern
pixel for pixel. A key and a pointer motion sent through QEMU's monitor arrive
as `input_event`s on `/dev/input/event0` and are echoed on the console. Two
processes exchange a sealed memfd over an `AF_UNIX` socket and both see the
other's writes through `MAP_SHARED`.

**Done — the hardware row: the DK1's HDMI output (2026-09-23).** The
STM32MP157D-DK1's LTDC and its SiI9022 HDMI bridge are a card like any
other: the kernel clocks and muxes them and publishes a device-tree node,
`src/user/system/native/drivers/display/stm32-ltdc` drives both, and the core fills the card's buffers with
contiguous memory and cleans the caches for the LTDC, which does not snoop
them. On the board `src/user/system/linux/compositor/blank` put a colour on a monitor through
`/dev/dri/card0`, and `hyprix` ran as init at 1280x720 on `HDMI-A-1` with a
terminal window. `docs/DISPLAY.md` §6 has the design.

**Done — the hardware row's other half: the DK1's USB keyboard and mouse
(2026-09-23).** The kernel clocks, powers and releases the STM32MP157's USB
host and starts its PHY, and publishes a device-tree node; `vmo_pin`'s
`PIN_COHERENT` gives a driver memory a device that does not snoop sees as the
CPU does, mapped past the caches; the input core lets a USB host's node hold
a control channel per keyboard or mouse (and, since 2026-10-10, for a PC's
xHCI controllers, gated under QEMU by `cargo xtask test-usb` and not yet run
on a PC: `docs/XHCI.md`). `src/user/system/native/drivers/usb/usbhid`, over `src/lib/drivers/usb/usb-host`
and tested against a model of EHCI and the board's bus, drives the
controller, the USB2514B hub and HID boot-protocol devices through the hub's
transaction translator. On the board a G502 mouse became `event0` and a
keyboard `event1`, and keys, buttons, motion and the wheel read back from
them. The same evening devices were read through their own report
descriptors -- the mouse's side buttons and wheel tilt, the keyboard's media
keys -- and `write` of `EV_LED` to an event node reached the keyboard's
LEDs through a new core-to-driver message, `STATUS`. `docs/INPUT.md` §7 has
the design.

**Done — the board's desktop without the boot test in front of it
(2026-09-24).** The DK1 took some 20 s from reset to a usable desktop, 6.5 s
of it the kernel's self-checks and 2.5 s the loader reading 47 MB off the
card. `ferrix.checks=skip` brings every stage up and checks none
(`src/kernel/src/checks.rs`), ending in `FERRIX-BOOT-UNCHECKED`, which no boot
test accepts; `flash --compositor` and `run-compositor` put it in the image's
own `FERRIX/DEFAULTS.TXT`, beside and below the card owner's `CMDLINE.TXT`,
and `test-compositor --boot desktop` gates it. Under QEMU at two processors
the kernel's banner to its marker is 0.34 s against 4.94 s. `flash` strips
the card's programs and kernel, and oh-my-zsh loses its documentation and
pictures: the desktop's card is 26 MB where it was 47 MB. On the board the
loader's banner to hyprix's first frame is 3.6 s where it was 11.7 s: the
initramfs read in 0.9 s, the kernel's banner to its marker 1.5 s. hyprix
opens input devices that arrive after it starts (the keyboard behind the
hub, 0.8 s later, on every boot since). **Still to do:** the firmware's
5.6 s -- TF-A and OP-TEE 3 s, of which OP-TEE's finding of its device tree
is 1.4 s, and U-Boot's 2 s autoboot countdown, which is the saved
environment's `bootdelay`.

**Done — the board's desktop at the speed of a hand (2026-09-24).** The
customer found the DK1's desktop lagging -- windows switching visibly, a
command a second to come back, the terminal a minute to its first prompt --
and the pointer drifting. hyprix's frame report now says where its slowest
frame went and what it drew, and each cause was found by it on the board:
the portrait monitor's turn (148 ms of a frame, now tiled), shadows and
translucent surfaces blended through floating point (both now exact tables
and the arithmetic written out), a focus change redrawing both windows
(now their rings), clients repainting for every configure (now none), a
test client printing each mouse motion to a console polled with interrupts
masked (now quiet, and the console transmits by interrupt), input read once
a frame from a queue of Linux's 35 ms (now drained, from a second's queue),
and zinc's start (31 s cold, 3 s warm, 0.45 s a command; now 5.3 s, 1.2 s
and 0.11 s). Circling the pointer between two terminals ran at 43 to 46
frames a second, the slowest of each second 26 to 30 ms, most of it the
flip's wait, where frames had been 300 to 650 ms. **Still to do:** pointer
motion off the frame thread, so a frame being drawn cannot hold the
pointer (the cursor plane below moves it only between frames); the first
blur of a translucent window, still 0.6 to 1.9 s in f32 on this core; the
Cortex-A7 at 800 MHz, which the STM32MP157D is rated for and firmware runs
at 650, a raise of VDDCORE through the PMIC first.

**Done — the board's pointer on the LTDC's second layer (2026-09-24).**
The DK1's card has a cursor plane: `src/user/system/native/drivers/display/stm32-ltdc` offers the LTDC's second
layer, shows the compositor's 64 × 64 image from its own buffer blended as
premultiplied colour, and moves it by rewriting the layer's window at the
next vertical blanking, clipped at the screen's edges and put back after a
mode switch. `hyprix` turns the plane's image, hotspot and place with a
turned monitor, which the board's portrait monitor is and which kept the
pointer in the frame on every card before. The pointer then moves at the
screen's 60 Hz while frames take their 25 to 150 ms. Host-tested against
the LTDC's register model and for all eight transforms (`docs/DISPLAY.md`
§6). **Still to do:** the board's own run.

**Done — epoll, iteration 2's first kernel row.** `epoll_create1`,
`epoll_create`, `epoll_ctl`, `epoll_wait`, `epoll_pwait` and `epoll_pwait2`
answer on all three architectures, with `struct epoll_event` packed to 12
bytes on x86-64 and 16 elsewhere. A set is an anonymous file on
`anon_inodefs`, named `anon_inode:[eventpoll]`. Its registrations are keyed by
the open file and the number, as Linux keys them, and hold the file weakly, so
closing a number leaves the registration while a `dup` keeps the file open.
Level-triggered, edge-triggered and one-shot registrations report only the
events they asked for. A wait asks each file's readiness at the moment it
waits, and sleeps and asks again as `poll` does. For edge-triggered mode every
pollable object (pipe, terminal, `/dev/dri/card0`, `AF_UNIX`, `AF_INET`,
netlink and packet sockets) reports how often its wait queues were woken, and a registration is
due a report when that count moved or a readiness bit appeared. A set is
itself pollable, so a Wayland server's set can sit in its toolkit's. A set
added to itself is `EINVAL`, one that would contain itself `ELOOP`, and a chain
of sets deeper than `EP_MAX_NESTS` `ELOOP`. A regular file or a directory is
`EPERM`, and `EPOLLEXCLUSIVE` is refused where Linux refuses it. Not yet:
`EPOLLRDHUP` and `EPOLLPRI` are never reported, because a file's readiness
does not say either. A wait woke within 5 ms of its file becoming ready, not
at once, until c2129a68 made it wake on the event. The boot check makes sets by number and watches pipes. Level,
edge-after-drain, one-shot re-armed by `EPOLL_CTL_MOD`, reporting by turns
with room for one event, a registration outliving its number through a `dup`,
a set inside a set and a chain of six are each required. Twenty-two refusals
are checked in Linux's order, and the run is done twice with no frame kept.
The line reads `epoll    13 events delivered ..., 22 calls refused as Linux
refuses them; 0 frames leaked`. With edge-triggered mode blind to the wake
count, the boot panics with "an edge-triggered set did not report more data
written into a readable pipe". With the loop check looking for nothing, it
panics with "a set added to a set it holds was not ELOOP".

**Done — eventfd, iteration 2's second kernel row.** `eventfd2` on all three
architectures, and the older `eventfd` on x86-64 and ARMv7-A, make a 64-bit
counter on `anon_inodefs`, named `anon_inode:[eventfd]`. A write adds its
eight bytes and a read takes the counter, or one with `EFD_SEMAPHORE`. A read
of zero waits or is `EAGAIN`, and so is a write that would carry the counter to
`u64::MAX`, the value it never holds. `poll` answers writable exactly while one
more fits. A buffer shorter than eight bytes and a write of `u64::MAX` are
`EINVAL`, as in `fs/eventfd.c`, and `EFD_CLOEXEC` and `EFD_NONBLOCK` reach the
descriptor and the open file. Each read and write wakes the other side's
queue, which is also what epoll's edge-triggered mode counts. Not yet: a
zero-length write returns 0 before it reaches the eventfd, where Linux answers
`EINVAL`, because the write path answers every empty write itself. The boot
check covers the initial value, adding up, the semaphore, the ceiling and
`poll` at it. A blocking read must be ended by a write's wake, not the wait's
5 ms recheck, as the queue's count of wake-ended waits shows. An
edge-triggered registration must be reported after a second write although
the counter stayed readable, and eight refusals are checked. The run is done
twice with no frame kept. The line read `eventfd  7 values read back, a
waiting reader woken by a write, 8 calls refused as Linux refuses them; 0
frames leaked` when this landed; it has since grown the waits the wake work
checks. With the write's wake removed, the boot panics with "a waiting
eventfd reader was ended by its recheck, not by the write's wake".

**Done — waits that wake on the event.** `poll`, `ppoll`, `select`,
`pselect6` and the epoll waits used to sleep 5 ms and look again, so a
compositor's frame loop and every idle client paid two hundred wake-ups a
second and up to 5 ms of latency. Now each pollable object names the wait
queues it wakes when its readiness changes, through `Inode::poll_queues`:
pipes, `AF_UNIX`, `AF_INET`, packet and netlink sockets, eventfds, the
terminal, `/dev/dri/card0` and epoll sets. An epoll set names its registered
files' queues and a queue of its own that `epoll_ctl` wakes. A wait sleeps on
all of them at once, as Linux's `poll_wait` does, and a wake of any ends it.
It still looks again by itself in case a wake is missing, every second when
every watched object vouches that each change wakes a queue, and every 5 ms
otherwise. The terminal vouches only when its input is interrupt-driven. A
readiness question to the net core no longer wakes the stack's waiters: a
socket wait that asked from inside its own wait used to wake itself, and two
such waits woke each other. The eventfd check now runs a `poll` and an
`epoll_wait` in tasks of their own, which a write must end by its wake. A
300 ms `poll` on a quiet eventfd must look at most 12 times, where looking
every 5 ms takes about 120. Three controls, each run and each failing the
boot. With waits that never trust their queues, it panics with "a poll on a
quiet eventfd kept looking instead of sleeping on its queues" after 119 looks.
(The poll is 120 ms since 2026-09-24, the check having been 0.8 s of every
boot; the same control then panicked after 49 looks on x86-64.)
With an eventfd naming no queue, it panics with the same. With an eventfd
naming only its writable queue, it panics with "a waiting poll was ended by
looking again, not by the write's wake".

**Done — `ioctl(FIONBIO)`, iteration 2's third kernel row.** `FIONBIO` is
answered for every file before any file-specific request, as `do_vfs_ioctl`
answers it. The `int` its argument points at sets `O_NONBLOCK` when not zero
and clears it when zero, and an unreadable argument is `EFAULT`. It was
`ENOTTY` for everything but the console before. The stage 8 pipe check makes a
blocking pipe non-blocking through it: an empty read is then `EAGAIN` and
`F_GETFL` reports `O_NONBLOCK`, and zero clears the flag again. It does the
same to an `AF_UNIX` socket. With the request left unanswered, the boot panics
with "FIONBIO on a pipe was refused". With E1 to E3 in, the kernel side of
iteration 2 is done.

**Done — `FIOCLEX` and `FIONCLEX`, E3's other two requests.** Rust's standard
library sets close-on-exec with `ioctl(FIOCLEX)` in its fallback paths, and E3
promised both. They are answered for every file beside `FIONBIO`, set and clear
the descriptor's close-on-exec flag as `F_SETFD` does, and read no argument.
The stage 8 pipe check sets and clears the flag through them and reads it back
with `F_GETFD`, passing an unreadable argument to show nothing is read. With
`FIOCLEX` left unanswered, the boot panics with "FIOCLEX on a pipe was
refused".

**Done — E4, `card0`'s primary plane (2026-09-16, reviewed by os-02).** Smithay's legacy path lists planes and reads each one's `type`
property, and has nothing to draw on without a primary plane. `card0` now
accepts `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, without which Linux's
`drm_mode_getplane_res` lists only overlay planes; it implies nothing atomic,
and atomic stays refused. It lists one primary plane, id 4, whose `GETPLANE`
gives `XRGB8888`, `possible_crtcs` 1 and the CRTC and framebuffer last shown.
`OBJ_GETPROPERTIES` gives the plane its immutable `type` enum, property 5 when this landed, at
`Primary`, and the CRTC and the connector no properties; `GETPROPERTY` names
the values `Overlay`, `Primary` and `Cursor`. Each follows Linux's
`drm_plane.c`, `drm_mode_object.c` and `drm_property.c`, read before the code
(`docs/DISPLAY.md` §2.3). Framebuffer ids started at 32, so no id named two
objects. Since the second screen (2026-09-17) the card has a block of four
ids per scanout, for up to sixteen — connector, encoder, CRTC and primary
plane — so the `type` property is 65 and framebuffer ids start at 128; the
first head's plane is still id 4. The numbers come from the extended `probe/drm.c`. `src/user/system/linux/compositor/blank`
reads the planes after its modeset as Smithay does, and `cargo xtask
test-display` requires its marker line to end in `plane <id> Primary` on
x86-64 and AArch64. With the plane's `type` value set to `Overlay`, the line
ends in `plane 4 Overlay` and the test fails with "the program found no
primary plane on the card" on both.

**Done — E5, `timerfd` (2026-09-24), for foot, the first step of
`docs/CHROME.md` §6.** foot calls it about 45 times: cursor blink, flash,
delayed render and key repeat. `timerfd_create`, `timerfd_settime` and
`timerfd_gettime` on all three architectures, and `timerfd_settime64` and
`timerfd_gettime64` on ARMv7-A, whose numbers were read from the vendored
UAPI headers and checked again against ferrousli's copies of them. A timer
counts on `CLOCK_MONOTONIC`, `CLOCK_BOOTTIME` or `CLOCK_REALTIME`, lives on
`anon_inodefs` as `anon_inode:[timerfd]`, and takes `TFD_NONBLOCK` and
`TFD_CLOEXEC`. A read takes the expiration count as eight bytes, every
interval that passed included when a periodic timer is read late, and waits,
interruptibly, or is `EAGAIN` while the count is zero. `timerfd_gettime`
answers the time left and the interval, `timerfd_settime` the setting it
replaced, and a zero value disarms and keeps the interval, as in
`fs/timerfd.c`. Expirations are driven, not discovered: one kernel thread,
`timerfds`, started on demand as `itimers` is, holds every timer weakly,
sleeps until the earliest deadline, counts it and wakes that timer's queue,
and exits when nothing is armed. As on Linux, an unread expiration stops the
timer announcing more until the read that takes it, so a periodic timer
nobody reads costs one wake, not one per interval. A set of the real-time
clock moves real-time deadlines with it, and `TFD_TIMER_CANCEL_ON_SET` makes
an absolute real-time timer readable at the set and its next read, or its
next arming, `ECANCELED`. Not as Linux: `CLOCK_REALTIME_ALARM` and
`CLOCK_BOOTTIME_ALARM` are `EPERM` without privilege, as there, but
`EOPNOTSUPP` with it, where Linux would make a timer that cannot wake a
suspended machine; that is `clock_nanosleep`'s answer here too.
`TFD_IOC_SET_TICKS`, a checkpoint-restore request, is not answered. A
periodic timer read `ECANCELED` goes on expiring, where Linux leaves one
whose expiration was already pending stopped until the next setting. The boot check covers the flags and clocks, a one-shot
timer read before and after its deadline, the overrun count of a periodic
timer armed in the past, both `itimerspec` layouts, a clock set under three
timers, and 22 refusals. A blocked read, a `poll` and an `epoll_wait`, each
waiting before its timer is armed, must be ended by the thread's wake, as
the queue's count of wake-ended waits shows, and come back within a quarter
of the one-second recheck. The line reads `timerfd  8 expirations read back,
a read, poll and epoll_wait each woken at the deadline, the latest 3483 us
after it; 22 calls refused as Linux refuses them; 0 frames leaked` on x86-64;
AArch64 said 371 us and ARMv7-A 336 us. With the thread's wake removed, the
boot panics with "a waiter on a timerfd was ended by its recheck, not by the
deadline's wake". ferrousli's wrappers landed with foot the same day, and
foot's `cargo xtask test-foot` is the program on Ferrix that uses them: it
creates its timers with them as it starts, and draws its text on the
compositor (`docs/CHROME.md` §6).

**Done — `signalfd`, and `madvise` beside it (2026-09-24), for Chrome
(`docs/CHROME.md` §2.3 and §3).** Chrome's and glib's event loops take
`SIGCHLD` and `SIGTERM` through a signalfd in their epoll set. `signalfd4`
answers on all three architectures and `signalfd` on x86-64 and ARMv7-A,
numbers read from the vendored UAPI headers and again from ferrousli's
copies, over an object on `anon_inodefs` named `anon_inode:[signalfd]`,
with `SFD_NONBLOCK` and `SFD_CLOEXEC`. `fs/signalfd.c`'s rules: a mask size
other than eight bytes is `EINVAL`, an unreadable mask `EFAULT`, `SIGKILL`
and `SIGSTOP` are taken out of the mask, and a descriptor handed back gets
the new mask or is `EINVAL` if it is no signalfd. A read takes the pending
signals in the mask, the reading thread's own before its process's, as
`rt_sigtimedwait` takes them, and answers a 128-byte `signalfd_siginfo`
for each that fits; the first may wait, interruptibly, and the rest are
taken only if already there. The signals are the reader's, not the
maker's, as on Linux. Every process has a queue, `signal_arrived`, which
`notify_signal` and `notify_signal_to` wake for every signal they make
pending, so a thread that blocks a signal still learns of it: poll and
epoll trust the signalfd and sleep a second between looks, and without the
queue a waiter would have been a second late. The boot check, FX-0884, sends
signals to a process with handlers installed, reads them back with their
sender and `SI_USER`, leaves one outside the mask pending until
`signalfd4` changes the mask, takes two in one read, refuses 10 calls as
Linux does, and requires a blocked read, a `poll` and an `epoll_wait`,
each waiting before the signal is sent, to be ended by that wake; they
came back within 90 us on x86-64, 102 us on AArch64 and 73 us on ARMv7-A
(71 us at two processors). With the wake removed from `notify_signal` the boot panics with
"a waiter on a signalfd was ended by its recheck, not by the signal's
wake". Not as Linux: `ssi_uid` is zero, since a pending signal does not
record its sender's uid here, and the timer and queued-value fields are
never filled, since there are no POSIX timers and no `sigqueue`.
ferrousli's `signalfd` wrapper landed the same day, with a C test on the
host.

`madvise` had a number and no handler, so every call was `ENOSYS`, and
PartitionAlloc and V8 could never give memory back. `MADV_DONTNEED` and
`MADV_FREE` now drop a range's pages and keep it mapped: private anonymous
memory reads zeros on the next touch, a private file mapping its file, a
shared mapping what it held, and the frames go back to the allocator
inside the call, after the one shootdown that takes their translations
out of every TLB. `MADV_FREE` drops at once rather than lazily, as Linux
does without swap. `MADV_REMOVE` punches a hole in shared anonymous memory
and is `EOPNOTSUPP` on a file, which is `fallocate`'s answer to the same
hole here. The hints are accepted where Linux accepts them and change
nothing; `MADV_WIPEONFORK`, `MADV_KEEPONFORK`, KSM's pair, `MADV_POPULATE_*`
and the rest are `EINVAL`, as from a Linux built without them. The boot
check, FX-0872, counts the frames: eight written pages dropped must give
back exactly eight frames in a frame window, with a page on either side
keeping its table, and read zeros after; with the drop sabotaged to keep
its pages the boot panics with "madvise did not give the frames of the
pages it dropped back to the allocator". Not as Linux: `MADV_DONTFORK` is
accepted and not honoured, a page held for a device keeps its frame and
contents, and `MADV_WILLNEED` reads nothing ahead. Still to do for Chrome:
the vDSO -- done on x86-64 on 2026-09-26 (`docs/CHROME.md` §3).

**Done — an EDID override, and crosvm's GPU (2026-09-27).** The display
core reads `drm.edid_firmware=` as Linux's DRM core does: once per card,
when the driver's `HELLO` is accepted, it loads `/lib/firmware/<file>` for
each connector the option names by Linux's name for it (`Virtual-1`,
`HDMI-A-1`), and the connector gets Linux's `EDID` property. A name that is
absolute, empty, holds a NUL or climbs through `..` is refused before
anything is read, and the read follows no symbolic link, since the bytes go
to every program that opens the card; stage 8's boot check holds that half.
`run-compositor` uses it to give the desktop the host monitor's EDID, which
is what the user's `monitor = desc:...` lines match. And the virtio-gpu
driver sends `TRANSFER_TO_HOST_2D`'s offset as crosvm reads it on the Pixel
7's VM, where every partial update had landed shifted by its own origin;
QEMU keeps the specification's reading.

---

